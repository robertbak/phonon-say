# Changelog

All notable changes to this project are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-10-20

### Added

- Local text-to-speech that streams [Phonon](https://github.com/gradium-ai/xn-ptts)
  audio straight to the speakers.
- Voice selection from the checkpoint, the `gradium/phonon-voices` catalog, or a
  local `.safetensors` embedding.
- A warm daemon (`--serve`, `--install-service`) that keeps the model and
  conditioned voices ready, so repeated commands start speaking in roughly
  60–100 ms.
- A local model store under `~/.cache/phonon-say/` that copies each checkpoint
  once and works offline afterwards.
- `--list-models`, `--list-voices` and `--list-devices` for discovery.
- An optional defaults file at `~/.config/phonon-say/config.toml`, with
  `--init-config`, `--show-config` and `--no-config`.
- `--out` to write the generated audio to a WAV file.

[Unreleased]: https://github.com/robertbak/phonon-say/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/robertbak/phonon-say/releases/tag/v0.1.0
