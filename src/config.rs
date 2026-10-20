// SPDX-License-Identifier: MIT OR Apache-2.0

//! Optional user configuration: defaults for the command line and the warm
//! service.
//!
//! Precedence is command-line flag, then the config file, then the built-in
//! default. A missing default-path config is not an error; an explicitly
//! requested or malformed one is.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const EXAMPLE: &str = include_str!("../config.example.toml");

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub model: Option<String>,
    pub voice: Option<String>,
    pub lang: Option<String>,
    pub quant: Option<String>,
    pub device: Option<String>,
    pub output_device: Option<String>,
    pub temperature: Option<f32>,
    pub seed: Option<u64>,
    pub duration_delta: Option<f32>,
    pub voice_catalog: Option<String>,
    pub preload: Option<Vec<String>>,
    pub preload_voice: Option<Vec<String>>,
    pub max_models: Option<usize>,
    pub max_voices: Option<usize>,
}

/// Where the effective configuration came from.
#[derive(Debug, Clone)]
pub enum Source {
    File(PathBuf),
    Missing(PathBuf),
    Disabled,
}

impl Source {
    pub fn describe(&self) -> String {
        match self {
            Self::File(path) => format!("{} (loaded)", path.display()),
            Self::Missing(path) => format!("{} (not found, using defaults)", path.display()),
            Self::Disabled => "disabled (--no-config)".to_string(),
        }
    }
}

/// `$PHONON_SAY_CONFIG`, `--config`, or `$XDG_CONFIG_HOME/phonon-say/config.toml`.
pub fn path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("PHONON_SAY_CONFIG")
        && !path.is_empty()
    {
        return Some(PathBuf::from(path));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = std::env::var_os("HOME")?;
            PathBuf::from(home).join(".config")
        }
    };
    Some(base.join("phonon-say").join("config.toml"))
}

pub fn load(explicit: Option<&Path>, disabled: bool) -> Result<(Config, Source)> {
    if disabled {
        return Ok((Config::default(), Source::Disabled));
    }
    let Some(path) = path(explicit) else {
        return Ok((Config::default(), Source::Disabled));
    };
    if !path.exists() {
        if explicit.is_some() {
            bail!("config file {} does not exist", path.display());
        }
        return Ok((Config::default(), Source::Missing(path)));
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let config: Config =
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok((config, Source::File(path)))
}

/// `--init-config`: write the commented example, refusing to overwrite.
pub fn write_example(explicit: Option<&Path>) -> Result<()> {
    let path = path(explicit).context("no config path: HOME and XDG_CONFIG_HOME are unset")?;
    if path.exists() {
        bail!(
            "{} already exists; edit it, or remove it before running --init-config",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    fs::write(&path, EXAMPLE).with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_uses_defaults() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.model.is_none());
        assert!(config.preload.is_none());
    }

    #[test]
    fn parses_the_example() {
        let config: Config = toml::from_str(EXAMPLE).unwrap();
        assert!(config.model.is_none(), "the example only has comments");
    }

    #[test]
    fn rejects_unknown_keys() {
        let error = toml::from_str::<Config>("modle = \"31m\"").unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }

    #[test]
    fn reads_lists() {
        let config: Config = toml::from_str(
            "model = \"40m\"\npreload = [\"31m\", \"40m\"]\npreload_voice = [\"Zoey\"]\n",
        )
        .unwrap();
        assert_eq!(config.model.as_deref(), Some("40m"));
        assert_eq!(config.preload.unwrap(), ["31m", "40m"]);
        assert_eq!(config.preload_voice.unwrap(), ["Zoey"]);
    }
}
