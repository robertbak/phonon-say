// SPDX-License-Identifier: MIT OR Apache-2.0

//! Speak text with a local Phonon model, streaming straight to the speakers.
//!
//! The model generates 80 ms audio frames as it goes; this tool hands each one
//! to the audio device immediately instead of waiting for the whole utterance.
//! A small warm daemon (`--serve`) can keep the model, voices and audio device
//! ready so repeated commands start speaking in tens of milliseconds.

mod audio;
mod config;
mod model_store;
mod models;
mod service;

use std::io::{IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use clap::Parser;
use ptts::checkpoint::{Checkpoint, ResolveOptions};
use ptts::preprocess::{Normalize, Rules};
use ptts::synth::{DeviceKind, Quant, SpeechOptions, Synth};
use ptts::voice_catalog::{CatalogVoice, DEFAULT_VOICE_CATALOG, VoiceCatalog};

/// Name file-based voices are registered under.
pub(crate) const CUSTOM_VOICE: &str = "custom";

/// Built-in defaults, used when neither a flag nor the config sets a value.
const DEFAULT_MODEL: &str = "31m";
const DEFAULT_LANG: &str = "en";
const DEFAULT_QUANT: &str = "q8";
const DEFAULT_DEVICE: &str = "auto";
const DEFAULT_TEMPERATURE: f32 = 0.3;
const DEFAULT_SEED: u64 = 4_242_424_242_424_242;
const DEFAULT_DURATION_DELTA: f32 = 0.0;
const DEFAULT_MAX_MODELS: usize = 1;
const DEFAULT_MAX_VOICES: usize = 1;

#[derive(Parser, Debug)]
#[command(
    name = "phonon-say",
    version,
    about = "Stream Phonon text-to-speech straight to your speakers",
    after_help = "Defaults can live in ~/.config/phonon-say/config.toml (see --init-config,\n\
                  --show-config). Command-line flags override the config.\n\
                  Models: --list-models shows licenses, gating and cache status. 31m is\n\
                  the public default; the 40m family is gated and non-commercial.\n\
                  Voices: --list-voices shows the catalog; --voice picks one by name.\n\
                  Warm service: --install-service starts a user daemon so later runs skip\n\
                  model loading and voice conditioning (first audio in ~60 ms).\n\
                  Examples:\n  \
                  phonon-say \"Hello from Phonon\"\n  \
                  phonon-say --model 40m --voice Zoey \"Hello again\"\n  \
                  echo \"piped text\" | phonon-say --voice Juniper\n  \
                  phonon-say --voice /path/to/voice.safetensors \"Custom voice\""
)]
pub(crate) struct Args {
    /// Text to speak. If omitted (or "-"), text is read from stdin.
    #[arg(value_name = "TEXT")]
    pub(crate) text: Vec<String>,

    /// Model: 31m (default), 40m, 40m-fr/de/es/pt/multilingual,
    /// hf://owner/model[@revision], or a local directory.
    #[arg(short, long, value_name = "MODEL")]
    pub(crate) model: Option<String>,

    /// Language for text normalization: en (default), fr, de, es, pt, or none.
    #[arg(short, long, value_name = "LANG")]
    pub(crate) lang: Option<String>,

    /// Voice: a name from the checkpoint or voice catalog, or a .safetensors file.
    #[arg(short, long)]
    pub(crate) voice: Option<String>,

    /// List the voice catalog and exit.
    #[arg(long)]
    pub(crate) list_voices: bool,

    /// With --list-voices, show only this language (en, fr, de, es, pt).
    #[arg(long, value_name = "LANG")]
    pub(crate) voices_lang: Option<String>,

    /// Voice catalog: hf://owner/name[@revision], or a local voices.jsonl or folder.
    #[arg(long, value_name = "SOURCE")]
    pub(crate) voice_catalog: Option<String>,

    /// Weight format: q8 (default, small and fast on CPU) or f32.
    #[arg(long, value_name = "QUANT")]
    pub(crate) quant: Option<String>,

    /// Compute device: auto (default), cpu, cuda, vulkan, or metal.
    #[arg(long, value_name = "DEVICE")]
    pub(crate) device: Option<String>,

    /// Output device name (case-insensitive substring); default is the system default.
    #[arg(long, value_name = "NAME")]
    pub(crate) output_device: Option<String>,

    /// List audio output devices and exit.
    #[arg(long)]
    pub(crate) list_devices: bool,

    /// List the Phonon models, licenses, gating and cache status, then exit.
    #[arg(long)]
    pub(crate) list_models: bool,

    /// Also save the generated audio to this WAV file.
    #[arg(short, long, value_name = "FILE")]
    pub(crate) out: Option<PathBuf>,

    /// Sampling temperature [default: 0.3].
    #[arg(long, value_name = "T")]
    pub(crate) temperature: Option<f32>,

    /// Sampling seed; fixed by default so repeated runs sound the same.
    #[arg(long, value_name = "N")]
    pub(crate) seed: Option<u64>,

    /// Speaking-rate condition: negative is faster, positive slower [default: 0].
    #[arg(long, value_name = "D", allow_hyphen_values = true)]
    pub(crate) duration_delta: Option<f32>,

    /// Re-resolve the model from the Hub and refresh the local copy.
    #[arg(long)]
    pub(crate) refresh: bool,

    /// Bypass the warm service and synthesize in this process.
    #[arg(long)]
    pub(crate) local: bool,

    /// Run the warm speech service in the foreground.
    #[arg(long)]
    pub(crate) serve: bool,

    /// Show whether the warm service is running and what it has loaded.
    #[arg(long)]
    pub(crate) status: bool,

    /// Ask the warm service to exit.
    #[arg(long)]
    pub(crate) stop: bool,

    /// Unix socket for the warm service (default: $XDG_RUNTIME_DIR/phonon-say.sock).
    #[arg(long, value_name = "PATH")]
    pub(crate) socket: Option<PathBuf>,

    /// Config file to read instead of $XDG_CONFIG_HOME/phonon-say/config.toml.
    #[arg(long, value_name = "FILE")]
    pub(crate) config: Option<PathBuf>,

    /// Ignore the config file.
    #[arg(long)]
    pub(crate) no_config: bool,

    /// Write a commented example config and exit.
    #[arg(long)]
    pub(crate) init_config: bool,

    /// Print the effective defaults and where they came from, then exit.
    #[arg(long)]
    pub(crate) show_config: bool,

    /// With --serve, models to warm at startup (default: the configured model).
    #[arg(long, value_name = "MODEL", value_delimiter = ',')]
    pub(crate) preload: Vec<String>,

    /// With --serve, voices to condition at startup (default: the configured voice).
    #[arg(long = "preload-voice", value_name = "VOICE", value_delimiter = ',')]
    pub(crate) preload_voice: Vec<String>,

    /// With --serve, how many models to keep warm [default: 1].
    #[arg(long, value_name = "N")]
    pub(crate) max_models: Option<usize>,

    /// With --serve, how many conditioned voices to keep per model (1-16) [default: 1].
    #[arg(long, value_name = "N")]
    pub(crate) max_voices: Option<usize>,

    /// Install and start a systemd user service for the warm daemon.
    #[arg(long)]
    pub(crate) install_service: bool,

    /// Stop and remove the systemd user service.
    #[arg(long)]
    pub(crate) uninstall_service: bool,

    /// Print timings and stream details.
    #[arg(long)]
    pub(crate) verbose: bool,

    /// Suppress progress messages.
    #[arg(long)]
    pub(crate) quiet: bool,
}

/// Effective settings: flag, then config, then built-in default.
pub(crate) struct Settings {
    pub(crate) model: String,
    pub(crate) voice: Option<String>,
    pub(crate) lang: String,
    pub(crate) quant: String,
    pub(crate) device: String,
    pub(crate) output_device: Option<String>,
    pub(crate) temperature: f32,
    pub(crate) seed: u64,
    pub(crate) duration_delta: f32,
    pub(crate) voice_catalog: String,
    pub(crate) max_models: usize,
    pub(crate) max_voices: usize,
    pub(crate) refresh: bool,
    pub(crate) quiet: bool,
    pub(crate) verbose: bool,
}

impl Settings {
    fn resolve(args: &Args, config: &config::Config) -> Result<Self> {
        let model = args
            .model
            .clone()
            .or_else(|| config.model.clone())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string());
        let voice = args
            .voice
            .clone()
            .or_else(|| config.voice.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .map(|value| resolve_voice_value(&value))
            .transpose()?;
        let lang = args
            .lang
            .clone()
            .or_else(|| config.lang.clone())
            .unwrap_or_else(|| DEFAULT_LANG.to_string());
        let quant = args
            .quant
            .clone()
            .or_else(|| config.quant.clone())
            .unwrap_or_else(|| DEFAULT_QUANT.to_string());
        let device = args
            .device
            .clone()
            .or_else(|| config.device.clone())
            .unwrap_or_else(|| DEFAULT_DEVICE.to_string());
        let output_device = args
            .output_device
            .clone()
            .or_else(|| config.output_device.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let temperature = args
            .temperature
            .or(config.temperature)
            .unwrap_or(DEFAULT_TEMPERATURE);
        let seed = args.seed.or(config.seed).unwrap_or(DEFAULT_SEED);
        let duration_delta = args
            .duration_delta
            .or(config.duration_delta)
            .unwrap_or(DEFAULT_DURATION_DELTA);
        let voice_catalog = args
            .voice_catalog
            .clone()
            .or_else(|| config.voice_catalog.clone())
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_VOICE_CATALOG.to_string());
        let voice_catalog = resolve_catalog(&voice_catalog)?;
        let max_models = args
            .max_models
            .or(config.max_models)
            .unwrap_or(DEFAULT_MAX_MODELS);
        ensure!(max_models >= 1, "max_models must be at least 1");
        let max_voices = args
            .max_voices
            .or(config.max_voices)
            .unwrap_or(DEFAULT_MAX_VOICES);
        ensure!(
            (1..=16).contains(&max_voices),
            "max_voices must be between 1 and 16 (the runtime's primed-voice cap)"
        );

        // Fail early on a typo in the config rather than after a download.
        lang.parse::<Normalize>()?;
        let parsed_quant = quant.parse::<Quant>()?;
        let parsed_device = device.parse::<DeviceKind>()?;
        parsed_quant.check_device(parsed_device)?;

        Ok(Self {
            model: resolve_model(&model)?,
            voice,
            lang,
            quant,
            device,
            output_device,
            temperature,
            seed,
            duration_delta,
            voice_catalog,
            max_models,
            max_voices,
            refresh: args.refresh,
            quiet: args.quiet,
            verbose: args.verbose,
        })
    }

    fn print(&self, source: &config::Source, args: &Args, config: &config::Config) {
        println!("# config: {}", source.describe());
        println!("model = {:?}", self.model);
        match &self.voice {
            Some(voice) => println!("voice = {voice:?}"),
            None => println!("# voice = (checkpoint default)"),
        }
        println!("lang = {:?}", self.lang);
        println!("quant = {:?}", self.quant);
        println!("device = {:?}", self.device);
        match &self.output_device {
            Some(device) => println!("output_device = {device:?}"),
            None => println!("# output_device = (system default)"),
        }
        println!("temperature = {}", self.temperature);
        println!("seed = {}", self.seed);
        println!("duration_delta = {}", self.duration_delta);
        println!("voice_catalog = {:?}", self.voice_catalog);
        println!("max_models = {}", self.max_models);
        println!("max_voices = {}", self.max_voices);
        if let Ok(models) = preload_models(args, config, self) {
            println!("preload = {models:?}");
        }
        if let Ok(voices) = preload_voices(args, config, self) {
            println!("preload_voice = {voices:?}");
        }
    }
}

fn main() -> Result<()> {
    // Behave like a normal Unix filter when piped into `head`.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let launch = Instant::now();
    let args = Args::parse();

    if args.init_config {
        return config::write_example(args.config.as_deref());
    }
    if args.install_service {
        return service::install_service(&args);
    }
    if args.uninstall_service {
        return service::uninstall_service();
    }
    if args.list_devices {
        return audio::print_devices();
    }
    if args.list_models {
        let quant = args
            .quant
            .as_deref()
            .unwrap_or(DEFAULT_QUANT)
            .parse::<Quant>()?;
        models::print_catalog(quant);
        return Ok(());
    }

    let socket = service::socket_path(args.socket.as_deref());
    if args.status || args.stop {
        return service::request_status(&socket, args.stop);
    }

    let (config, source) = config::load(args.config.as_deref(), args.no_config)?;
    let settings = Settings::resolve(&args, &config)?;
    if args.show_config {
        settings.print(&source, &args, &config);
        return Ok(());
    }
    if args.serve {
        return service::serve(&args, &settings, &config);
    }

    let catalog = VoiceCatalog::new(&settings.voice_catalog)?;
    if args.list_voices {
        return list_voices(&catalog, args.voices_lang.as_deref());
    }

    let text = read_text(&args.text)?;
    let voice = VoiceArg::parse(settings.voice.as_deref())?;
    let quant = settings.quant.parse::<Quant>()?;
    let device = settings.device.parse::<DeviceKind>()?;

    // Prefer the warm service: it keeps the model, the conditioned voice and
    // the audio device ready, so requests start speaking in tens of ms.
    let use_service = !args.local
        && settings.output_device.is_none()
        && settings.voice_catalog == DEFAULT_VOICE_CATALOG;
    if use_service {
        // Fetch with this process's Hugging Face credentials and copy into the
        // local store; the daemon then loads a plain directory and never needs
        // the token. Gated 40m models keep working from the service this way.
        if settings.model.starts_with("hf://") {
            model_store::ensure(&settings.model, quant, settings.refresh, settings.quiet)?;
        }
        let request = service::SpeakRequest {
            op: "speak".to_string(),
            text: text.clone(),
            model: settings.model.clone(),
            lang: settings.lang.clone(),
            quant: settings.quant.clone(),
            device: settings.device.clone(),
            voice: voice.as_request_string()?,
            temperature: Some(settings.temperature),
            seed: Some(settings.seed),
            duration_delta: settings.duration_delta,
            out: args.out.as_deref().map(absolute).transpose()?,
            refresh: settings.refresh,
        };
        if let Some(reply) = service::try_speak(&socket, &request)? {
            if !reply.ok {
                bail!("{}", reply.error.unwrap_or_else(|| "service error".into()));
            }
            if settings.verbose {
                eprintln!(
                    "[phonon-say] warm service: first audio {:.0} ms after request; {:.2}s \
                     audio in {:.2}s{}",
                    reply.first_ms,
                    reply.audio_secs,
                    reply.generation_secs,
                    reply
                        .voice
                        .as_deref()
                        .map(|v| format!("; voice {v}"))
                        .unwrap_or_default(),
                );
            }
            return Ok(());
        }
    }

    // Local path.
    if !settings.quiet {
        eprintln!(
            "[phonon-say] loading {} ({}, {})…",
            settings.model,
            quant.as_str(),
            settings.device
        );
    }
    let mut tts = build_synth(
        &settings.model,
        &settings.lang,
        quant,
        device,
        settings.duration_delta,
        settings.refresh,
        settings.quiet,
    )?;
    let mut options = voice_options(&mut tts, &catalog, &voice)?;
    options = options
        .temperature(settings.temperature)
        .seed(settings.seed);

    let mut speaker = audio::Speaker::open(settings.output_device.as_deref(), tts.sample_rate())?;
    if settings.verbose {
        eprintln!(
            "[phonon-say] device: {} [{} ch, {:?}, {} Hz], model rate {} Hz",
            speaker.device_name,
            speaker.channels,
            speaker.sample_format,
            speaker.sample_rate,
            tts.sample_rate(),
        );
        eprintln!(
            "[phonon-say] device model: {}, voices: {}",
            tts.device_name(),
            tts.voices().join(", ")
        );
        eprintln!(
            "[phonon-say] voice: {}, language: {}, speed delta: {}",
            options.voice.as_deref().unwrap_or("<default>"),
            settings.lang,
            settings.duration_delta
        );
    }

    let out = args.out.as_deref().map(absolute).transpose()?;
    let stats = speak(
        &tts,
        &mut speaker,
        &text,
        &options,
        out.as_deref(),
        launch,
        || false,
    )?;
    if let Some(path) = &out
        && !settings.quiet
    {
        eprintln!("[phonon-say] wrote {}", path.display());
    }
    if settings.verbose {
        eprintln!(
            "[phonon-say] first audio {:.0} ms after launch; {:.2}s generated in {:.2}s \
             (RTF {:.2}); total {:.2}s",
            stats.first_ms,
            stats.audio_secs,
            stats.generation_secs,
            if stats.audio_secs > 0.0 {
                stats.generation_secs / stats.audio_secs
            } else {
                0.0
            },
            launch.elapsed().as_secs_f64(),
        );
    }
    Ok(())
}

/// A finished request, for timing output.
pub(crate) struct Stats {
    pub(crate) first_ms: f64,
    pub(crate) audio_secs: f64,
    pub(crate) generation_secs: f64,
    pub(crate) cancelled: bool,
}

/// Stream text into the speaker, optionally saving the PCM as a WAV.
///
/// `cancelled` is polled between chunks; when it returns true the rest of the
/// generation is dropped (which stops the runtime's workers).
#[allow(clippy::too_many_arguments)]
pub(crate) fn speak<F: Fn() -> bool>(
    tts: &Synth,
    speaker: &mut audio::Speaker,
    text: &str,
    options: &SpeechOptions,
    out: Option<&Path>,
    launch: Instant,
    cancelled: F,
) -> Result<Stats> {
    let stream = tts
        .stream_with(text, options)
        .context("starting speech generation")?;
    let started = Instant::now();
    let input_rate = stream.sample_rate();
    let output_rate = speaker.sample_rate;
    let mut resampler =
        (input_rate != output_rate).then(|| audio::Resampler::new(input_rate, output_rate));
    let mut raw = Vec::new();
    let mut converted = Vec::new();
    let mut first_ms = None;
    let mut was_cancelled = false;

    for chunk in stream {
        let chunk = chunk?;
        if first_ms.is_none() {
            first_ms = Some(launch.elapsed().as_secs_f64() * 1000.0);
        }
        if cancelled() {
            was_cancelled = true;
            break;
        }
        raw.extend_from_slice(&chunk);
        converted.clear();
        match resampler.as_mut() {
            Some(resampler) => resampler.process(&chunk, &mut converted),
            None => converted.extend_from_slice(&chunk),
        }
        if !speaker.write(&converted) {
            break;
        }
    }

    let generation_secs = started.elapsed().as_secs_f64();
    if !was_cancelled {
        if let Some(resampler) = resampler.as_mut() {
            converted.clear();
            resampler.flush(&mut converted);
            speaker.write(&converted);
        }
        speaker.finish(Duration::from_secs(5));
        if let Some(path) = out {
            ptts::wav::write_wav_file(path, &raw, input_rate)
                .with_context(|| format!("writing {}", path.display()))?;
        }
    }

    Ok(Stats {
        first_ms: first_ms.unwrap_or(0.0),
        audio_secs: raw.len() as f64 / input_rate as f64,
        generation_secs,
        cancelled: was_cancelled,
    })
}

/// Build a model, register its bundled voices and apply the rate condition.
pub(crate) fn build_synth(
    source: &str,
    lang: &str,
    quant: Quant,
    device: DeviceKind,
    duration_delta: f32,
    refresh: bool,
    quiet: bool,
) -> Result<Synth> {
    quant.check_device(device)?;
    let normalize = lang
        .parse::<Normalize>()?
        .with_rules("default".parse::<Rules>()?);
    let checkpoint = load_checkpoint(source, quant, refresh, quiet)?;
    let mut builder = checkpoint.builder(normalize).device(device);
    if duration_delta != 0.0 {
        builder = builder.condition("duration_delta", duration_delta.to_string());
    }
    let mut tts = builder.build().context("building the synthesis model")?;
    checkpoint.register_voices(&mut tts);
    Ok(tts)
}

/// Load a checkpoint, using the local model store for `hf://` sources.
///
/// The first use copies the checkpoint into `~/.cache/phonon-say/models/` and
/// later runs load that directory directly, so `hf-hub` is not on the startup
/// path at all (and works with no network).
fn load_checkpoint(source: &str, quant: Quant, refresh: bool, quiet: bool) -> Result<Checkpoint> {
    let options = ResolveOptions {
        quant,
        weights: None,
    };
    if source.starts_with("hf://") {
        let dir = model_store::ensure(source, quant, refresh, quiet)?;
        match Checkpoint::resolve(&dir, options) {
            Ok(checkpoint) => return Ok(checkpoint),
            Err(err) if !refresh => {
                if !quiet {
                    eprintln!(
                        "[phonon-say] local model in {} is unusable ({err}), refreshing",
                        dir.display()
                    );
                }
                model_store::invalidate(&dir);
                let dir = model_store::ensure(source, quant, true, quiet)?;
                return Checkpoint::resolve(&dir, options)
                    .with_context(|| format!("loading the refreshed model in {}", dir.display()));
            }
            Err(err) => {
                return Err(err).with_context(|| format!("loading {}", dir.display()));
            }
        }
    }
    Checkpoint::locate(source, options).with_context(|| format!("loading model {source}"))
}

/// The voice catalog, used for names the checkpoint does not have.
pub(crate) fn voice_options(
    tts: &mut Synth,
    catalog: &VoiceCatalog,
    voice: &VoiceArg,
) -> Result<SpeechOptions> {
    let mut options = SpeechOptions::default();
    match voice {
        VoiceArg::Default => {}
        VoiceArg::Embedding(path) => {
            tts.add_voice_file(CUSTOM_VOICE, path)
                .with_context(|| format!("loading voice {}", path.display()))?;
            options = options.voice(CUSTOM_VOICE);
        }
        VoiceArg::Name(name) => {
            let name = catalog.register(tts, name)?;
            let available = tts.voices();
            ensure!(
                available.contains(&name),
                "no voice '{name}'. The model has: {}. Try --list-voices for the catalog.",
                if available.is_empty() {
                    "no voices".to_string()
                } else {
                    available.join(", ")
                }
            );
            options = options.voice(name);
        }
    }
    Ok(options)
}

/// What `--voice` pointed at.
pub(crate) enum VoiceArg {
    Default,
    Name(String),
    Embedding(PathBuf),
}

impl VoiceArg {
    fn parse(arg: Option<&str>) -> Result<Self> {
        Ok(match arg {
            None => Self::Default,
            Some(arg) if arg.ends_with(".safetensors") => Self::Embedding(PathBuf::from(arg)),
            Some(arg) if Path::new(arg).is_file() => {
                bail!(
                    "'{arg}' looks like an audio file. The Phonon checkpoints on Hugging Face \
                     carry no speaker encoder, so cloning from a recording is not supported; \
                     pass a voice name (see --list-voices) or a .safetensors voice file instead."
                );
            }
            Some(arg) => Self::Name(arg.to_string()),
        })
    }

    /// How to name this voice in a request to the warm service.
    fn as_request_string(&self) -> Result<Option<String>> {
        Ok(match self {
            Self::Default => None,
            Self::Name(name) => Some(name.clone()),
            Self::Embedding(path) => Some(absolute(path)?.display().to_string()),
        })
    }
}

/// Map the short model names to pinned Hugging Face revisions.
pub(crate) fn model_source(model: &str) -> String {
    models::by_shorthand(model)
        .map(|info| info.source.to_string())
        .unwrap_or_else(|| model.to_string())
}

/// Resolve a model value to a pinned `hf://` source or an absolute directory.
pub(crate) fn resolve_model(value: &str) -> Result<String> {
    let value = expand_tilde(value.trim());
    let source = model_source(&value);
    if source.starts_with("hf://") {
        Ok(source)
    } else {
        Ok(absolute(Path::new(&source))?.display().to_string())
    }
}

/// Resolve a voice value: names stay names, files become absolute paths.
pub(crate) fn resolve_voice_value(value: &str) -> Result<String> {
    let value = expand_tilde(value.trim());
    let path_like =
        value.ends_with(".safetensors") || value.contains('/') || Path::new(&value).is_file();
    if path_like {
        Ok(absolute(Path::new(&value))?.display().to_string())
    } else {
        Ok(value)
    }
}

fn resolve_catalog(value: &str) -> Result<String> {
    let value = expand_tilde(value.trim());
    if value.starts_with("hf://") {
        Ok(value)
    } else {
        Ok(absolute(Path::new(&value))?.display().to_string())
    }
}

/// Models the service should warm, following flag, config, then the model default.
pub(crate) fn preload_models(
    args: &Args,
    config: &config::Config,
    settings: &Settings,
) -> Result<Vec<String>> {
    let values = if !args.preload.is_empty() {
        args.preload.clone()
    } else if let Some(list) = &config.preload {
        list.clone()
    } else {
        vec![settings.model.clone()]
    };
    values.iter().map(|value| resolve_model(value)).collect()
}

/// Voices the service should condition, following flag, config, then the voice default.
pub(crate) fn preload_voices(
    args: &Args,
    config: &config::Config,
    settings: &Settings,
) -> Result<Vec<String>> {
    let values = if !args.preload_voice.is_empty() {
        args.preload_voice.clone()
    } else if let Some(list) = &config.preload_voice {
        list.clone()
    } else {
        vec![
            settings
                .voice
                .clone()
                .unwrap_or_else(|| "default".to_string()),
        ]
    };
    values
        .iter()
        .filter(|value| !value.trim().is_empty())
        .map(|value| resolve_voice_value(value))
        .collect()
}

fn expand_tilde(value: &str) -> String {
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest).display().to_string();
    }
    value.to_string()
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))
}

fn read_text(words: &[String]) -> Result<String> {
    if let Some(first) = words.first() {
        if first == "-" && words.len() == 1 {
            return read_stdin();
        }
        let text = words.join(" ").trim().to_string();
        ensure!(!text.is_empty(), "the text is empty");
        return Ok(text);
    }
    if std::io::stdin().is_terminal() {
        bail!("nothing to say: pass text as an argument, or pipe it on stdin");
    }
    read_stdin()
}

fn read_stdin() -> Result<String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .context("reading text from stdin")?;
    let text = text.trim().to_string();
    ensure!(!text.is_empty(), "stdin contained no text");
    Ok(text)
}

fn list_voices(catalog: &VoiceCatalog, language: Option<&str>) -> Result<()> {
    let mut voices: Vec<&CatalogVoice> = catalog
        .voices()?
        .iter()
        .filter(|voice| match language {
            Some(lang) => voice.language.as_deref() == Some(lang),
            None => true,
        })
        .collect();
    if let Some(lang) = language {
        ensure!(
            !voices.is_empty(),
            "no catalog voices for language '{lang}'"
        );
    }
    voices.sort_by(|a, b| {
        a.language
            .cmp(&b.language)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    println!("{:<18} {:<5} {:<26} DESCRIPTION", "NAME", "LANG", "ID");
    for voice in &voices {
        println!(
            "{:<18} {:<5} {:<26} {}",
            voice.name,
            voice.language.as_deref().unwrap_or("-"),
            voice.voice_id.as_deref().unwrap_or("-"),
            truncate(voice.description.as_deref().unwrap_or("-"), 70),
        );
    }
    eprintln!("{} voices", voices.len());
    Ok(())
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let prefix: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        let mut argv = vec!["phonon-say"];
        argv.extend_from_slice(extra);
        Args::try_parse_from(argv).unwrap()
    }

    #[test]
    fn config_fills_defaults() {
        let config = config::Config {
            model: Some("40m".into()),
            voice: Some("Zoey".into()),
            temperature: Some(0.5),
            ..Default::default()
        };
        let settings = Settings::resolve(&args(&[]), &config).unwrap();
        assert!(settings.model.contains("phonon-40m-en"));
        assert_eq!(settings.voice.as_deref(), Some("Zoey"));
        assert_eq!(settings.temperature, 0.5);
    }

    #[test]
    fn flags_beat_config() {
        let config = config::Config {
            model: Some("40m".into()),
            voice: Some("Juniper".into()),
            temperature: Some(0.5),
            ..Default::default()
        };
        let settings = Settings::resolve(
            &args(&[
                "--model",
                "31m",
                "--voice",
                "Easton",
                "--temperature",
                "0.1",
            ]),
            &config,
        )
        .unwrap();
        assert!(settings.model.contains("phonon-31m-en"));
        assert_eq!(settings.voice.as_deref(), Some("Easton"));
        assert_eq!(settings.temperature, 0.1);
    }

    #[test]
    fn built_in_defaults_apply() {
        let settings = Settings::resolve(&args(&[]), &config::Config::default()).unwrap();
        assert!(settings.model.contains("phonon-31m-en"));
        assert!(settings.voice.is_none());
        assert_eq!(settings.lang, "en");
        assert_eq!(settings.quant, "q8");
        assert_eq!(settings.device, "auto");
    }
}
