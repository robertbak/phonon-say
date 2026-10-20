// SPDX-License-Identifier: MIT OR Apache-2.0

//! A small local model store.
//!
//! The first time a `hf://` checkpoint is used, it is copied out of the
//! Hugging Face cache into `$XDG_CACHE_HOME/phonon-say/models/`. Later runs
//! load that plain directory, so neither `hf-hub` nor the network is involved
//! and the model keeps working if the Hugging Face cache is cleared.
//!
//! The Phonon checkpoints are small (64–75 MB q8), so a copy per model and
//! weight format is cheap; where possible files are hard-linked instead.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use ptts::checkpoint::{Checkpoint, ResolveOptions, weight_candidates};
use ptts::synth::Quant;

use crate::models;

const META: &str = "phonon-say.json";

/// Directory holding all materialized checkpoints.
pub fn root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME") {
        Some(dir) => PathBuf::from(dir),
        None => match std::env::var_os("HOME") {
            Some(home) => PathBuf::from(home).join(".cache"),
            None => bail!("neither XDG_CACHE_HOME nor HOME is set"),
        },
    };
    Ok(base.join("phonon-say").join("models"))
}

/// The local directory for a model source and weight format.
///
/// The fingerprint keeps sources that sanitize to the same slug apart, for
/// example `hf://o/n-x` and `hf://o/n@x`.
pub fn dir_for(source: &str, quant: Quant) -> Result<PathBuf> {
    Ok(root()?.join(format!(
        "{}-{}-{:016x}",
        slug(source),
        quant.as_str(),
        fingerprint(source)
    )))
}

/// Whether a source is already materialized for this weight format.
pub fn is_cached(source: &str, quant: Quant) -> bool {
    dir_for(source, quant)
        .map(|dir| is_complete(&dir, quant))
        .unwrap_or(false)
}

/// Return a complete local checkpoint directory, materializing it if needed.
pub fn ensure(source: &str, quant: Quant, refresh: bool, quiet: bool) -> Result<PathBuf> {
    let dir = dir_for(source, quant)?;
    if !refresh && is_complete(&dir, quant) {
        return Ok(dir);
    }

    models::announce_download(source, quant, &dir, refresh, quiet);
    let repo = models::repo_of(source).unwrap_or(source);
    let info = models::by_repo(repo);
    let checkpoint = Checkpoint::locate(
        source,
        ResolveOptions {
            quant,
            weights: None,
        },
    )
    .map_err(|err| {
        let auth =
            models::looks_like_auth(&format!("{err}")) || info.is_some_and(|info| info.gated);
        let mut context = format!("could not download {repo}");
        if auth {
            context.push_str(&format!(
                "\naccept the model's terms once at {}",
                models::url(repo)
            ));
            context.push_str("\nthen run `hf auth login` or set HF_TOKEN");
        }
        anyhow::Error::new(err).context(context)
    })?;
    let origin = checkpoint
        .weights
        .parent()
        .context("resolved weights have no parent directory")?
        .to_path_buf();

    materialize(&origin, &dir, source, quant)?;
    Ok(dir)
}

/// Drop a store entry that no longer resolves (for example after an upgrade).
pub fn invalidate(dir: &Path) {
    let _ = fs::remove_dir_all(dir);
}

fn is_complete(dir: &Path, quant: Quant) -> bool {
    dir.join("config.json").is_file()
        && weight_candidates(quant)
            .iter()
            .any(|name| dir.join(name).is_file())
}

fn fingerprint(source: &str) -> u64 {
    // FNV-1a: stable across runs and Rust versions, unlike DefaultHasher.
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in source.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn slug(source: &str) -> String {
    let mut slug = String::new();
    for ch in source.strip_prefix("hf://").unwrap_or(source).chars() {
        match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '-' | '_' => slug.push(ch),
            '/' => slug.push_str("--"),
            '@' => slug.push('-'),
            _ => slug.push('_'),
        }
    }
    // Keep paths well under filesystem limits without losing the commit.
    if slug.len() > 140 {
        let keep = 140;
        slug.truncate(keep);
    }
    if slug.is_empty() {
        slug.push_str("model");
    }
    slug
}

fn materialize(origin: &Path, dir: &Path, source: &str, quant: Quant) -> Result<()> {
    let parent = dir
        .parent()
        .with_context(|| format!("{} has no parent", dir.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;

    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "model".to_string());
    let tmp = parent.join(format!(".tmp-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    if let Err(err) = copy_tree(origin, &tmp) {
        let _ = fs::remove_dir_all(&tmp);
        return Err(err)
            .with_context(|| format!("copying {} into {}", origin.display(), tmp.display()));
    }

    let meta = serde_json::json!({
        "source": source,
        "quant": quant.as_str(),
        "copied_from": origin.display().to_string(),
    });
    fs::write(tmp.join(META), serde_json::to_vec_pretty(&meta)?)?;

    if dir.exists() {
        fs::remove_dir_all(dir).with_context(|| format!("replacing {}", dir.display()))?;
    }
    if let Err(err) = fs::rename(&tmp, dir) {
        let _ = fs::remove_dir_all(&tmp);
        // Another process may have finished the same copy first.
        if is_complete(dir, quant) {
            return Ok(());
        }
        return Err(err).with_context(|| format!("installing {}", dir.display()));
    }
    Ok(())
}

fn copy_tree(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let target = dst.join(entry.file_name());
        // Follow symlinks: the Hugging Face cache points snapshot files at
        // content-addressed blobs, and we want real files in our store.
        let path = entry.path();
        let meta = fs::metadata(&path)?;
        if meta.is_dir() {
            copy_tree(&path, &target)?;
        } else {
            // Link the real blob, not the snapshot symlink, so the store
            // survives deletion of the Hugging Face cache.
            let real = fs::canonicalize(&path).unwrap_or(path);
            if fs::hard_link(&real, &target).is_err() {
                fs::copy(&real, &target)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fingerprint;

    #[test]
    fn fingerprints_are_stable_and_distinct() {
        assert_eq!(fingerprint("hf://o/n"), fingerprint("hf://o/n"));
        assert_ne!(fingerprint("hf://o/n-x"), fingerprint("hf://o/n@x"));
        assert_ne!(fingerprint("hf://o/n@a"), fingerprint("hf://o/n@b"));
    }
}
