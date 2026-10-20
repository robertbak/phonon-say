// SPDX-License-Identifier: MIT OR Apache-2.0

//! Warm speech service over a Unix socket.
//!
//! The daemon keeps the model, the conditioned voices and the audio device
//! ready. A client sends one newline-delimited JSON request and waits for one
//! reply line; the request blocks until the audio has played, unless the
//! client disconnects, in which case generation is cancelled.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use ptts::synth::{DeviceKind, Quant, SpeechOptions, Synth};
use ptts::voice_catalog::VoiceCatalog;
use serde::{Deserialize, Serialize};

use crate::audio;
use crate::config;
use crate::{
    Args, CUSTOM_VOICE, Settings, build_synth, model_source, model_store, preload_models,
    preload_voices, resolve_model, resolve_voice_value, speak,
};

/// Requests larger than this are refused rather than buffered forever.
const MAX_TEXT_BYTES: usize = 200_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeakRequest {
    #[serde(default)]
    pub op: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_lang")]
    pub lang: String,
    #[serde(default = "default_quant")]
    pub quant: String,
    #[serde(default = "default_device")]
    pub device: String,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub seed: Option<u64>,
    #[serde(default)]
    pub duration_delta: f32,
    #[serde(default)]
    pub out: Option<PathBuf>,
    #[serde(default)]
    pub refresh: bool,
}

impl Default for SpeakRequest {
    fn default() -> Self {
        Self {
            op: String::new(),
            text: String::new(),
            model: String::new(),
            lang: default_lang(),
            quant: default_quant(),
            device: default_device(),
            voice: None,
            temperature: None,
            seed: None,
            duration_delta: 0.0,
            out: None,
            refresh: false,
        }
    }
}

fn default_lang() -> String {
    "en".into()
}

fn default_quant() -> String {
    "q8".into()
}

fn default_device() -> String {
    "auto".into()
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Reply {
    #[serde(default)]
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub first_ms: f64,
    #[serde(default)]
    pub audio_secs: f64,
    #[serde(default)]
    pub generation_secs: f64,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default)]
    pub status: Option<String>,
}

/// Where the warm service listens.
pub fn socket_path(explicit: Option<&Path>) -> PathBuf {
    if let Some(path) = explicit {
        return path.to_path_buf();
    }
    if let Ok(path) = std::env::var("PHONON_SAY_SOCKET") {
        return PathBuf::from(path);
    }
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        return Path::new(&dir).join("phonon-say.sock");
    }
    let uid = fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
    PathBuf::from(format!("/tmp/phonon-say-{uid}.sock"))
}

/// Send one request to a running daemon.
///
/// Returns `Ok(None)` when no daemon is listening, so the caller can fall back
/// to the in-process path.
pub fn try_speak(socket: &Path, request: &SpeakRequest) -> Result<Option<Reply>> {
    let mut stream = match UnixStream::connect(socket) {
        Ok(stream) => stream,
        Err(err)
            if matches!(
                err.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(None);
        }
        Err(err) => {
            return Err(err).with_context(|| format!("connecting to {}", socket.display()));
        }
    };
    let mut payload = serde_json::to_vec(request)?;
    payload.push(b'\n');
    stream
        .write_all(&payload)
        .with_context(|| format!("sending the request to {}", socket.display()))?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .with_context(|| format!("reading the reply from {}", socket.display()))?;
    if line.trim().is_empty() {
        bail!(
            "the warm service at {} closed the connection without replying",
            socket.display()
        );
    }
    Ok(Some(serde_json::from_str(&line)?))
}

/// `--status` and `--stop`.
pub fn request_status(socket: &Path, stop: bool) -> Result<()> {
    let request = SpeakRequest {
        op: if stop { "stop" } else { "status" }.to_string(),
        ..Default::default()
    };
    match try_speak(socket, &request)? {
        None => println!(
            "no warm service is running ({} is not listening)",
            socket.display()
        ),
        Some(reply) => {
            if !reply.ok {
                bail!("{}", reply.error.unwrap_or_else(|| "service error".into()));
            }
            if let Some(status) = reply.status {
                println!("{status}");
            } else if stop {
                println!("warm service stopped");
            }
        }
    }
    Ok(())
}

/// Run the daemon in the foreground.
pub fn serve(args: &Args, settings: &Settings, config: &config::Config) -> Result<()> {
    let socket = socket_path(args.socket.as_deref());
    prepare_socket(&socket)?;
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting {}", socket.display()))?;
    let _guard = SocketGuard(socket.clone());

    let mut engine = Engine::new(settings, socket.clone())?;
    if !settings.quiet {
        eprintln!("[phonon-say] warm service on {}", socket.display());
    }

    let models = preload_models(args, config, settings)?;
    let voices = preload_voices(args, config, settings)?;
    engine.preload(&models, &voices, settings);
    if !settings.quiet {
        eprintln!("[phonon-say] ready: {} model(s) warm", engine.entries.len());
    }

    loop {
        let (mut connection, _) = match listener.accept() {
            Ok(pair) => pair,
            Err(err) => {
                if !settings.quiet {
                    eprintln!("[phonon-say] accept failed: {err}");
                }
                continue;
            }
        };
        let _ = connection.set_read_timeout(Some(Duration::from_secs(10)));
        let mut reader = BufReader::new(connection.try_clone()?);
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => continue,
            Ok(_) => {}
            Err(err) => {
                if !settings.quiet {
                    eprintln!("[phonon-say] reading a request failed: {err}");
                }
                continue;
            }
        }
        let _ = connection.set_read_timeout(None);
        let request: SpeakRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(err) => {
                let _ = write_reply(
                    &mut connection,
                    &Reply {
                        ok: false,
                        error: Some(format!("bad request: {err}")),
                        ..Default::default()
                    },
                );
                continue;
            }
        };

        let op = if request.op.is_empty() {
            "speak"
        } else {
            request.op.as_str()
        };
        match op {
            "speak" => {
                let reply = match engine.speak(&request, &connection) {
                    Ok(reply) => reply,
                    Err(err) => Reply {
                        ok: false,
                        error: Some(format!("{err:#}")),
                        ..Default::default()
                    },
                };
                let _ = write_reply(&mut connection, &reply);
            }
            "status" => {
                let _ = write_reply(
                    &mut connection,
                    &Reply {
                        ok: true,
                        status: Some(engine.status()),
                        ..Default::default()
                    },
                );
            }
            "stop" => {
                let _ = write_reply(
                    &mut connection,
                    &Reply {
                        ok: true,
                        ..Default::default()
                    },
                );
                break;
            }
            other => {
                let _ = write_reply(
                    &mut connection,
                    &Reply {
                        ok: false,
                        error: Some(format!("unknown op '{other}'")),
                        ..Default::default()
                    },
                );
            }
        }
    }
    Ok(())
}

fn write_reply(connection: &mut UnixStream, reply: &Reply) -> Result<()> {
    let _ = connection.set_nonblocking(false);
    let mut line = serde_json::to_vec(reply)?;
    line.push(b'\n');
    connection.write_all(&line)?;
    connection.flush()?;
    Ok(())
}

fn client_gone(connection: &UnixStream) -> bool {
    // The client sends exactly one request line and then nothing, so a
    // readable socket here means it hung up (or, at worst, sent stray bytes
    // that are irrelevant to this request).
    let mut byte = [0u8; 1];
    match (&*connection).read(&mut byte) {
        Ok(0) => true,
        Ok(_) => false,
        Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => false,
        Err(_) => true,
    }
}

fn prepare_socket(socket: &Path) -> Result<()> {
    if socket.exists() {
        match UnixStream::connect(socket) {
            Ok(_) => bail!("a warm service is already running on {}", socket.display()),
            Err(_) => {
                let metadata = fs::metadata(socket)
                    .with_context(|| format!("inspecting {}", socket.display()))?;
                if !metadata.file_type().is_socket() {
                    bail!(
                        "{} exists but is not a Unix socket; refusing to remove it",
                        socket.display()
                    );
                }
                fs::remove_file(socket)
                    .with_context(|| format!("removing stale socket {}", socket.display()))?;
            }
        }
    }
    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    Ok(())
}

struct SocketGuard(PathBuf);

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Key {
    model: String,
    lang: String,
    quant: String,
    device: String,
    delta: u32,
}

struct Entry {
    key: Key,
    tts: Synth,
    file_voice: Option<PathBuf>,
    /// Voices primed in this model, least recently used first.
    voices: Vec<String>,
}

struct Engine {
    catalog: VoiceCatalog,
    entries: Vec<Entry>,
    speaker: Option<audio::Speaker>,
    output_device: Option<String>,
    max_models: usize,
    max_voices: usize,
    refresh: bool,
    quiet: bool,
    socket: PathBuf,
    started: Instant,
    requests: u64,
}

impl Engine {
    fn new(settings: &Settings, socket: PathBuf) -> Result<Self> {
        Ok(Self {
            catalog: VoiceCatalog::new(&settings.voice_catalog)?,
            entries: Vec::new(),
            speaker: None,
            output_device: settings.output_device.clone(),
            max_models: settings.max_models,
            max_voices: settings.max_voices,
            refresh: settings.refresh,
            quiet: settings.quiet,
            socket,
            started: Instant::now(),
            requests: 0,
        })
    }

    /// Load the requested model once, then reuse it for later requests.
    fn ensure_entry(&mut self, key: Key, refresh: bool) -> Result<usize> {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            let entry = self.entries.remove(index);
            self.entries.push(entry);
            return Ok(self.entries.len() - 1);
        }
        let quant = key.quant.parse::<Quant>()?;
        let device = key.device.parse::<DeviceKind>()?;
        if !self.quiet {
            eprintln!(
                "[phonon-say] loading {} ({}, {}) into the warm service…",
                key.model, key.quant, key.device
            );
        }
        let tts = build_synth(
            &key.model,
            &key.lang,
            quant,
            device,
            f32::from_bits(key.delta),
            refresh,
            self.quiet,
        )?;
        self.entries.push(Entry {
            key,
            tts,
            file_voice: None,
            voices: Vec::new(),
        });
        while self.entries.len() > self.max_models {
            self.entries.remove(0);
        }
        Ok(self.entries.len() - 1)
    }

    /// Rebuild one model in place to clear its primed voices.
    fn rebuild_entry(&mut self, index: usize) -> Result<()> {
        let key = self.entries[index].key.clone();
        let quant = key.quant.parse::<Quant>()?;
        let device = key.device.parse::<DeviceKind>()?;
        let tts = build_synth(
            &key.model,
            &key.lang,
            quant,
            device,
            f32::from_bits(key.delta),
            false,
            self.quiet,
        )?;
        let entry = &mut self.entries[index];
        entry.tts = tts;
        entry.file_voice = None;
        entry.voices.clear();
        Ok(())
    }

    fn preload(&mut self, models: &[String], voices: &[String], settings: &Settings) {
        if models.len() > self.max_models {
            eprintln!(
                "[phonon-say] preloading {} models, but only the last {} stay warm (max_models)",
                models.len(),
                self.max_models
            );
        }
        let voices = if voices.len() > self.max_voices {
            eprintln!(
                "[phonon-say] preloading the first {} of {} voices (max_voices)",
                self.max_voices,
                voices.len()
            );
            &voices[..self.max_voices]
        } else {
            voices
        };
        let quant = settings.quant.parse::<Quant>().unwrap_or(Quant::Q80);
        for model in models {
            if model.starts_with("hf://") && !model_store::is_cached(model, quant) {
                if !self.quiet {
                    eprintln!(
                        "[phonon-say] {model} is not cached with {quant:?} yet; \
                         the first request will download it"
                    );
                }
                continue;
            }
            let key = Key {
                model: model.clone(),
                lang: settings.lang.clone(),
                quant: settings.quant.clone(),
                device: settings.device.clone(),
                delta: settings.duration_delta.to_bits(),
            };
            let index = match self.ensure_entry(key, self.refresh) {
                Ok(index) => index,
                Err(err) => {
                    eprintln!("[phonon-say] preloading {model} failed: {err:#}");
                    continue;
                }
            };
            for voice in voices {
                if voice.is_empty() {
                    continue;
                }
                if let Err(err) = self.prime_voice(index, voice) {
                    eprintln!("[phonon-say] priming voice '{voice}' failed: {err:#}");
                }
            }
        }
    }

    /// Condition a voice once, so the first real request skips that cost.
    fn prime_voice(&mut self, index: usize, voice: &str) -> Result<()> {
        let catalog = &self.catalog;
        let entry = &mut self.entries[index];
        let name = if voice.ends_with(".safetensors") {
            entry
                .tts
                .add_voice_file(CUSTOM_VOICE, Path::new(voice))
                .with_context(|| format!("loading voice {voice}"))?;
            CUSTOM_VOICE.to_string()
        } else if Path::new(voice).is_file() {
            bail!("cannot preload a voice from an audio file; use a name or a .safetensors file");
        } else {
            catalog.register(&mut entry.tts, voice)?
        };
        if entry.voices.iter().any(|primed| primed == &name) {
            return Ok(());
        }
        entry.voices.push(name.clone());
        let options = SpeechOptions::default().voice(name);
        // Generating a short word runs the conditioning pass and leaves it
        // cached in the model's primed state. The audio is discarded.
        let _ = entry.tts.say_with("Hi.", &options)?;
        Ok(())
    }

    fn speak(&mut self, request: &SpeakRequest, connection: &UnixStream) -> Result<Reply> {
        ensure!(!request.text.trim().is_empty(), "the request has no text");
        ensure!(
            request.text.len() <= MAX_TEXT_BYTES,
            "the text is too long ({} bytes)",
            request.text.len()
        );
        let quant = request.quant.parse::<Quant>()?;
        let device = request.device.parse::<DeviceKind>()?;
        quant.check_device(device)?;
        let key = Key {
            model: model_source(&request.model),
            lang: request.lang.clone(),
            quant: request.quant.clone(),
            device: request.device.clone(),
            delta: request.duration_delta.to_bits(),
        };
        if request.refresh {
            self.entries.retain(|entry| entry.key != key);
        }
        let index = self.ensure_entry(key, self.refresh || request.refresh)?;

        if self.speaker.is_none() {
            self.speaker = Some(audio::Speaker::open(self.output_device.as_deref(), 24_000)?);
        }

        let mut options = SpeechOptions::default();
        if let Some(temperature) = request.temperature {
            options = options.temperature(temperature);
        }
        if let Some(seed) = request.seed {
            options = options.seed(seed);
        }

        let (mut options, voice_label, needs_rebuild) = {
            let entry = &mut self.entries[index];
            let name = register_voice(
                &mut entry.tts,
                &self.catalog,
                request.voice.as_deref(),
                &mut entry.file_voice,
            )?;
            if request.voice.is_some()
                && let Some(name) = &name
            {
                options = options.voice(name.clone());
            }
            let exceeds = name
                .as_deref()
                .is_some_and(|name| touch_voice(entry, name, self.max_voices));
            (options, name, exceeds)
        };

        if needs_rebuild {
            if !self.quiet {
                eprintln!(
                    "[phonon-say] voice cache full ({} per model); re-priming {}",
                    self.max_voices,
                    request.voice.as_deref().unwrap_or("the default voice")
                );
            }
            self.rebuild_entry(index)?;
            let entry = &mut self.entries[index];
            let name = register_voice(
                &mut entry.tts,
                &self.catalog,
                request.voice.as_deref(),
                &mut entry.file_voice,
            )?;
            entry.voices = name.iter().cloned().collect();
            if request.voice.is_some()
                && let Some(name) = &name
            {
                options = options.voice(name.clone());
            }
        }

        let entry = &mut self.entries[index];
        let started = Instant::now();
        let _ = connection.set_nonblocking(true);
        let outcome = {
            let speaker = self
                .speaker
                .as_mut()
                .context("the audio device is not open")?;
            speak(
                &entry.tts,
                speaker,
                &request.text,
                &options,
                request.out.as_deref(),
                started,
                || client_gone(connection),
            )
        };
        let _ = connection.set_nonblocking(false);
        let stats = match outcome {
            Ok(stats) => stats,
            Err(err) => {
                // Drop the partial/queued audio and the device, so a failed
                // request cannot bleed into the next one.
                self.speaker = None;
                return Err(err);
            }
        };

        // Release the audio device between requests so PipeWire can suspend
        // the sink (and so a cancelled request stops its queued audio).
        self.speaker = None;
        self.requests += 1;
        Ok(Reply {
            ok: true,
            error: None,
            first_ms: stats.first_ms,
            audio_secs: stats.audio_secs,
            generation_secs: stats.generation_secs,
            voice: voice_label,
            cancelled: stats.cancelled,
            status: None,
        })
    }

    fn status(&self) -> String {
        let mut out = format!(
            "phonon-say warm service\npid: {}\nuptime: {:.0}s\nsocket: {}\nrequests: {}\n\
             limits: {} model(s), {} voice(s) per model\n",
            std::process::id(),
            self.started.elapsed().as_secs_f64(),
            self.socket.display(),
            self.requests,
            self.max_models,
            self.max_voices,
        );
        if self.entries.is_empty() {
            out.push_str("models: none loaded\n");
        } else {
            out.push_str("models:\n");
            for entry in &self.entries {
                out.push_str(&format!(
                    "  {} [{} {} {}]  primed voices: {}\n",
                    entry.key.model,
                    entry.key.lang,
                    entry.key.quant,
                    entry.key.device,
                    if entry.voices.is_empty() {
                        "none".to_string()
                    } else {
                        entry.voices.join(", ")
                    },
                ));
            }
        }
        out
    }
}

/// Register the requested voice (or find the default) and return the name
/// ptts will actually use.
fn register_voice(
    tts: &mut Synth,
    catalog: &VoiceCatalog,
    requested: Option<&str>,
    file_voice: &mut Option<PathBuf>,
) -> Result<Option<String>> {
    match requested {
        None => Ok(tts.default_voice()),
        Some(voice) if voice.ends_with(".safetensors") => {
            let path = PathBuf::from(voice);
            if file_voice.as_deref() != Some(path.as_path()) {
                tts.add_voice_file(CUSTOM_VOICE, &path)
                    .with_context(|| format!("loading voice {}", path.display()))?;
                *file_voice = Some(path);
            }
            Ok(Some(CUSTOM_VOICE.to_string()))
        }
        Some(voice) if Path::new(voice).is_file() => bail!(
            "cloning from audio files is not supported; pass a voice name or a \
             .safetensors voice file"
        ),
        Some(voice) => {
            let name = catalog.register(tts, voice)?;
            let available = tts.voices();
            ensure!(
                available.contains(&name),
                "no voice '{name}'. The model has: {}. Try --list-voices.",
                available.join(", ")
            );
            Ok(Some(name))
        }
    }
}

/// Mark a voice as recently used; true when the per-model cap is exceeded.
fn touch_voice(entry: &mut Entry, name: &str, max_voices: usize) -> bool {
    if let Some(position) = entry.voices.iter().position(|primed| primed == name) {
        let value = entry.voices.remove(position);
        entry.voices.push(value);
        false
    } else {
        entry.voices.push(name.to_string());
        entry.voices.len() > max_voices
    }
}

/// `--install-service`
pub fn install_service(args: &Args) -> Result<()> {
    let unit_dir = home()?.join(".config/systemd/user");
    fs::create_dir_all(&unit_dir).with_context(|| format!("creating {}", unit_dir.display()))?;
    let unit_path = unit_dir.join("phonon-say.service");
    fs::write(&unit_path, unit_file(args)?)
        .with_context(|| format!("writing {}", unit_path.display()))?;
    run_systemctl(&["daemon-reload"])?;
    run_systemctl(&["enable", "phonon-say.service"])?;
    run_systemctl(&["restart", "phonon-say.service"])?;
    println!("installed {}", unit_path.display());
    println!("status: systemctl --user status phonon-say.service");
    println!("logs:   journalctl --user -u phonon-say.service -f");
    Ok(())
}

/// `--uninstall-service`
pub fn uninstall_service() -> Result<()> {
    let unit_path = home()?.join(".config/systemd/user/phonon-say.service");
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "phonon-say.service"])
        .status();
    if unit_path.exists() {
        fs::remove_file(&unit_path).with_context(|| format!("removing {}", unit_path.display()))?;
    }
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    println!("removed {}", unit_path.display());
    Ok(())
}

fn unit_file(args: &Args) -> Result<String> {
    let exe = std::env::current_exe().context("locating the phonon-say executable")?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    if exe.to_string_lossy().contains("/target/") {
        eprintln!(
            "warning: the service will run {}; reinstall after rebuilding or copying it \
             to ~/.local/bin",
            exe.display()
        );
    }
    let mut exec = format!("{} --serve --quiet", quote(&exe.display().to_string()));
    // Defaults come from the config at daemon startup; only flags given to
    // --install-service itself are baked into the unit.
    if let Some(config_path) = &args.config {
        let config_path = std::path::absolute(config_path).unwrap_or_else(|_| config_path.clone());
        exec.push_str(&format!(
            " --config {}",
            quote(&config_path.display().to_string())
        ));
    }
    if args.no_config {
        exec.push_str(" --no-config");
    }
    for model in &args.preload {
        let model = resolve_model(model)?;
        exec.push_str(&format!(" --preload {}", quote(&model)));
    }
    for voice in &args.preload_voice {
        let voice = resolve_voice_value(voice)?;
        exec.push_str(&format!(" --preload-voice {}", quote(&voice)));
    }
    if let Some(device) = &args.output_device {
        exec.push_str(&format!(" --output-device {}", quote(device)));
    }
    if let Some(models) = args.max_models {
        exec.push_str(&format!(" --max-models {models}"));
    }
    if let Some(voices) = args.max_voices {
        exec.push_str(&format!(" --max-voices {voices}"));
    }
    Ok(format!(
        "[Unit]\n\
         Description=Phonon text-to-speech warm service\n\
         After=pipewire.service\n\
         Wants=pipewire.service\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         NoNewPrivileges=true\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    ))
}

fn quote(value: &str) -> String {
    if value.chars().any(|c| c.is_whitespace() || c == '"') {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value.to_string()
    }
}

fn run_systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("running systemctl --user (is a systemd user session available?)")?;
    ensure!(
        status.success(),
        "systemctl --user {} failed",
        args.join(" ")
    );
    Ok(())
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}
