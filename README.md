# phonon-say

Local Linux text-to-speech that streams [Phonon](https://github.com/gradium-ai/xn-ptts)
audio straight to the speakers, with voice selection and a warm daemon so
repeated commands start speaking in roughly **60–100 ms**.

Models are copied once into `~/.cache/phonon-say/` (64–75 MB each at q8), so
startup never goes through `hf-hub` after the first use and works offline.

## Quick start

```sh
phonon-say "Hello from Phonon."
phonon-say --voice Zoey "A different voice."

# what can be downloaded, with licenses and gating
phonon-say --list-models

# voice discovery
phonon-say --list-voices
phonon-say --list-voices --voices-lang en

# stdin, speed, save a WAV
echo "Text can also arrive on stdin." | phonon-say --voice Easton
phonon-say --duration-delta -0.2 "Faster please."
phonon-say --out speech.wav "Save this too."

# audio device
phonon-say --list-devices
phonon-say --output-device "USB Audio" "Through the USB speakers."
```

## Defaults config

Put your usual model, voice and options in
`~/.config/phonon-say/config.toml` (or set `$XDG_CONFIG_HOME`, or point
`$PHONON_SAY_CONFIG` at another file):

```toml
model = "40m"
voice = "Zoey"
lang = "en"
# quant = "q8"
# device = "auto"
# output_device = "USB Audio"
# temperature = 0.3
# seed = 4242424242424242
# duration_delta = 0.0
# preload = ["31m", "40m"]
# preload_voice = ["Zoey", "default"]
# max_models = 1
# max_voices = 1   # per model, 1-16
```

```sh
phonon-say --init-config     # write the commented example
phonon-say --show-config     # see the effective values and their source
phonon-say --no-config ...   # ignore it for one run
```

Precedence is **command-line flag → config file → built-in default**, so
`phonon-say --model 31m "text"` still overrides `model = "40m"` for that run.
`--config FILE` reads a different file. Local paths in the config should be
absolute or start with `~/` (the daemon does not share your shell's cwd).

The warm service reads the config at startup and warms the configured model and
voice (or the `preload` / `preload_voice` lists), but only if they are already
in the local store: it never downloads at startup. The first client request
fetches anything missing with your credentials and then warms it. After editing
the config, run
`systemctl --user restart phonon-say.service` to repopulate the warm state;
one-shot commands pick up changes immediately. If the file is not at the
default path, install or run the service with `--config FILE` (a user
environment variable is not reliably inherited by systemd).

## Warm service

The first request in a fresh process has to load the model (~46 ms) and
condition the voice (~500 ms). The daemon keeps both ready, plus the primed
generation state, so later requests only generate audio:

```sh
phonon-say --install-service     # systemd --user, starts now and at login
phonon-say --status
phonon-say --stop                # stop it; start again with systemctl --user start
phonon-say --uninstall-service
```

- Unit: `~/.config/systemd/user/phonon-say.service`
- Logs: `journalctl --user -u phonon-say.service -f`
- Socket: `$XDG_RUNTIME_DIR/phonon-say.sock` (override with `--socket` or
  `PHONON_SAY_SOCKET`), mode `0600`.
- Clients find the daemon automatically and fall back to the in-process path
  when it is not running. `--local` always bypasses it; `--output-device` and a
  custom `--voice-catalog` also bypass it, because those are process-level choices.
- `--serve` runs it in the foreground. By default it warms the configured model
  and voice; `--preload 31m,40m` and `--preload-voice Zoey` override that.
- The daemon keeps `max_models` models (default 1, LRU) and `max_voices`
  conditioned voices per model (default 1, capped at 16 by the runtime). When a
  voice or model exceeds the cap, the old one is dropped and the requested one
  is rebuilt and primed; that request costs roughly another half second, and
  the next one is fast again. Raise the limits if you switch models or voices
  often. The audio device is released between requests so the sink can suspend.
  Expect roughly 200–350 MB for the warm model and its voice, plus ~30 MB per
  additional primed voice and ~200–350 MB per additional model.

Typical time to first audio on this machine:

| Path | First audio |
| --- | --- |
| Warm service, model + voice already primed | **~60–100 ms** |
| Warm service, new model or voice (one-off) | +0.5–1.5 s |
| `--local`, cached model | ~0.6–1.0 s (voice conditioning each process) |
| Very first run (download + copy) | a few seconds |

With the default `max_models = 1` / `max_voices = 1`, switching model or voice
pays the one-off cost each time because the previous one is evicted. Raise the
limits to keep several warm.

## Models and the local store

| Flag | Model | License | Gated | Notes |
| --- | --- | --- | --- | --- |
| `--model 31m` (default) | `gradium/phonon-31m-en` | Apache-2.0 | no | 64.5 MB q8, English |
| `--model 40m` / `40m-en` | `gradium/phonon-40m-en` | CC BY-NC 4.0 | yes | 74.6 MB q8, English, non-commercial |
| `--model 40m-fr` | `gradium/phonon-40m-fr` | CC BY-NC 4.0 | yes | French |
| `--model 40m-de` | `gradium/phonon-40m-de` | CC BY-NC 4.0 | yes | German |
| `--model 40m-es` | `gradium/phonon-40m-es` | CC BY-NC 4.0 | yes | Spanish |
| `--model 40m-pt` | `gradium/phonon-40m-pt` | CC BY-NC 4.0 | yes | Portuguese |
| `--model 40m-multilingual` | `gradium/phonon-40m-multilingual` | CC BY-NC 4.0 | yes | all five languages |
| `--model hf://owner/name[@rev]` | any Phonon checkpoint | its own | depends | pinned revision recommended |
| `--model /path/to/checkpoint` | a local folder | its own | — | used directly, not copied |

The shorthands resolve to pinned revisions. `--list-models` prints this table
with the current cache status, sizes and repository links.

Before the first download the command prints the repository URL, license,
gating, size and cache path. For a gated model it also prints the model page
where you accept the terms and the `hf auth login` / `HF_TOKEN` step; if the
download is refused, the error repeats that guidance.

The short names map to pinned revisions. The first time a checkpoint is used it
is downloaded through the Hugging Face client, then copied (hard-linked where
possible) into:

```
~/.cache/phonon-say/models/<owner>--<name>-<revision>-<quant>-<fingerprint>/
```

Later runs resolve that plain directory with `Checkpoint::resolve`, which takes
~0 ms and needs no network. This avoids the ~200 ms that `hf-hub` spends
resolving its own cache on every process start, and keeps working if the
Hugging Face cache is deleted. Delete a directory (or the whole `models/`) to
force a fresh download, or pass `--refresh` to re-resolve and re-copy now.

The 40m models are gated on Hugging Face: accept their terms once, then export
`HF_TOKEN` or run `hf auth login`. That token is only needed for the first
download; afterwards the local copy is used. 31m needs no token.

Gated models and the warm service: the command-line client downloads and copies
the model with your credentials before contacting the daemon, so the daemon
itself never needs the token. Preloading a gated model *at service startup* does
need `HF_TOKEN` in the service environment (systemd user services do not inherit
your shell). Without it the preload logs a warning and the first request fills
the store instead.

Weights default to `--quant q8`; use `--quant f32` for f32 (stored under its own
quant suffix). `--lang en|fr|de|es|pt|none` picks text normalization.

## Voices

`--voice NAME` resolves, in order:

1. a voice bundled with the checkpoint (all checkpoints have `default`),
2. a voice from the catalog (`gradium/phonon-voices`), downloaded on first use,
3. a path to a `.safetensors` voice embedding.

Names are matched case-insensitively (`--voice easton` finds `Easton`). Cloning
from a recording is not supported because the public Phonon checkpoints do not
ship a speaker encoder.

`--voice-catalog` overrides the catalog source (`hf://owner/name`, a local
`voices.jsonl`, or a folder containing one).

## How streaming works

- `ptts::synth` produces bounded mono f32 chunks at 24 kHz.
- A lock-free ring buffer (`rtrb`) carries them to the cpal/ALSA callback, which
  never allocates or locks. Generation runs ahead until the 2-second buffer is
  full, then waits for playback.
- If the device cannot run at 24 kHz, a stateful linear resampler converts to
  the device rate, and samples are converted to f32/i16/u16/i32/u8 as needed.
- `--out file.wav` writes the full model output while playing.
- If the client disconnects (Ctrl-C), the daemon notices between frames and
  cancels generation, releasing the device.

## Options

```
phonon-say [OPTIONS] [TEXT]...

  -m, --model <MODEL>           31m, 40m, hf://owner/model[@rev], or a local dir
  -l, --lang <LANG>             en, fr, de, es, pt, none [default: en]
  -v, --voice <VOICE>           catalog/checkpoint name or .safetensors path
      --list-voices             list the catalog and exit
      --voices-lang <LANG>      filter --list-voices by language
      --voice-catalog <SRC>     catalog repo or local voices.jsonl
      --quant <QUANT>           q8 (default) or f32
      --device <DEVICE>         auto, cpu, cuda, vulkan, metal
      --output-device <NAME>    output device name substring
      --list-devices            list audio outputs and exit
      --list-models             list models, licenses, gating and cache status
  -o, --out <FILE>              also write a WAV file
      --temperature <T>         sampling temperature [default: 0.3]
      --seed <N>                sampling seed
      --duration-delta <D>      speaking rate condition [default: 0]
      --refresh                 re-resolve the model and refresh the local copy
      --config <FILE>           config file [default: ~/.config/phonon-say/config.toml]
      --no-config               ignore the config file
      --init-config             write a commented example config and exit
      --show-config             print the effective defaults and exit
      --local                   bypass the warm service
      --serve                   run the warm service in the foreground
      --status                  show the warm service state
      --stop                    ask the warm service to exit
      --socket <PATH>           service socket path
      --preload <MODELS>        models to warm at startup (default: 31m)
      --preload-voice <VOICES>  voices to condition at startup (default: default)
      --max-models <N>          warm models kept by the service [default: 1]
      --max-voices <N>          primed voices per model, 1-16 [default: 1]
      --install-service         install/start the systemd user service
      --uninstall-service       stop/remove the systemd user service
      --verbose                 timings and stream details
      --quiet                   suppress progress messages
```

## Build and install

```sh
cargo build --release
cargo test --release
install -m755 target/release/phonon-say ~/.local/bin/phonon-say
phonon-say --install-service   # optional, for the warm daemon
```

Requires the ALSA development headers (`libasound2-dev` on Debian/Ubuntu).
On PipeWire/PulseAudio systems the ALSA `default` device routes through the
sound server.

Before a release:

```sh
cargo fmt --check
cargo clippy --release -- -D warnings
cargo test --release
python3 scripts/third-party-licenses.py   # regenerate THIRD_PARTY_LICENSES.md
```

## Releases

The version in `Cargo.toml` is the single source of truth. To cut a release:

```sh
# bump the version, then commit and tag
cargo set-version 0.2.0          # or edit Cargo.toml by hand
git commit -am "Release 0.2.0"
git tag v0.2.0
git push origin main --tags
```

Pushing the `v*` tag runs `.github/workflows/release.yml`, which builds the
release binary, packages it with the license files, and creates a GitHub
release with a SHA256 checksum and the generated release notes. The workflow
fails if the tag does not match the `Cargo.toml` version, so the two cannot
drift apart.

Install a released binary with:

```sh
tar -xzf phonon-say-<version>-linux-x86_64.tar.gz
install -m755 phonon-say-<version>-linux-x86_64/phonon-say ~/.local/bin/
```

Releases ship `linux-x86_64` and `linux-aarch64` tarballs, each with a
SHA256 checksum in `SHA256SUMS`. The binary links against ALSA
(`libasound.so.2`), so it needs `libasound2` on the target machine. On
PipeWire or PulseAudio systems the ALSA `default` device routes through the
sound server.

## Layout

- `src/main.rs` — CLI, model/voice resolution, local synthesis path.
- `src/audio.rs` — cpal output, ring buffer, resampler, device enumeration.
- `src/service.rs` — Unix-socket daemon, warm model/voice cache, systemd unit.
- `src/model_store.rs` — copy-on-first-use model store under `~/.cache`.
- `src/config.rs` — optional TOML defaults and precedence.
- `config.example.toml` — the file written by `--init-config`.
- `scripts/third-party-licenses.py` — regenerates the dependency notices.
- `LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE`, `THIRD_PARTY_LICENSES.md` — licensing.

## Licensing

### This tool

`phonon-say` is licensed under **MIT OR Apache-2.0**, at your option. See
`LICENSE-MIT` and `LICENSE-APACHE`. It links the Phonon runtime `ptts` and the
`xn` tensor library (MIT OR Apache-2.0, Gradium and contributors); the full
dependency notices and license texts are in `THIRD_PARTY_LICENSES.md`,
generated from the crates the binary actually builds.

### Models and voices (not included)

No model weights or voice files are in this repository or the binary. They are
downloaded separately and stay under their own terms:

| Asset | License | Notes |
| --- | --- | --- |
| `gradium/phonon-31m-en` | Apache-2.0 | commercial use allowed |
| `gradium/phonon-40m-en` and the 40m family | CC BY-NC 4.0 | **non-commercial only** |
| `gradium/phonon-voices` | Apache-2.0 | per its model-card metadata |
| voices from Gradium Studio | their own terms | custom/cloned voices |

The 40m models are gated: accept their terms on Hugging Face before use, and
do not ship them with a commercial product without permission from Gradium.
Copying a model into `~/.cache/phonon-say` is a local convenience; if you
redistribute those files, you redistribute the model under its own license,
not this project's. Cite the model card for the authoritative terms.

`phonon-say` is an independent project and is not affiliated with or endorsed
by Gradium. "Phonon" and the model names are used descriptively.
