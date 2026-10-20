// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Phonon models this tool knows about: shorthands, licenses, gating and
//! cache status. Used by `--list-models` and the first-download notice.

use std::path::Path;

use ptts::synth::Quant;

use crate::model_store;

pub struct ModelInfo {
    pub shorthand: &'static str,
    pub aliases: &'static [&'static str],
    pub source: &'static str,
    pub repo: &'static str,
    pub license: &'static str,
    pub non_commercial: bool,
    pub gated: bool,
    pub size_q8: &'static str,
    pub size_f32: &'static str,
    pub languages: &'static str,
}

pub const MODELS: &[ModelInfo] = &[
    ModelInfo {
        shorthand: "31m",
        aliases: &["31", "31m-en", "phonon-31m", "phonon-31m-en"],
        source: "hf://gradium/phonon-31m-en@52433c46975bfa13144c56204c9f4ee1c5b148aa",
        repo: "gradium/phonon-31m-en",
        license: "Apache-2.0",
        non_commercial: false,
        gated: false,
        size_q8: "64.5 MB",
        size_f32: "103 MB",
        languages: "en",
    },
    ModelInfo {
        shorthand: "40m",
        aliases: &["40", "40m-en", "phonon-40m", "phonon-40m-en"],
        source: "hf://gradium/phonon-40m-en@9b95393286eaeeebf1330b2ae90a14a2074c415d",
        repo: "gradium/phonon-40m-en",
        license: "CC BY-NC 4.0",
        non_commercial: true,
        gated: true,
        size_q8: "74.6 MB",
        size_f32: "122 MB",
        languages: "en",
    },
    ModelInfo {
        shorthand: "40m-fr",
        aliases: &["phonon-40m-fr"],
        source: "hf://gradium/phonon-40m-fr@65b5b451d235a8c24245f51d2774ec9806a9443f",
        repo: "gradium/phonon-40m-fr",
        license: "CC BY-NC 4.0",
        non_commercial: true,
        gated: true,
        size_q8: "74.6 MB",
        size_f32: "122 MB",
        languages: "fr",
    },
    ModelInfo {
        shorthand: "40m-de",
        aliases: &["phonon-40m-de"],
        source: "hf://gradium/phonon-40m-de@a114872a440a0aea5e55bfd200741931536a0f70",
        repo: "gradium/phonon-40m-de",
        license: "CC BY-NC 4.0",
        non_commercial: true,
        gated: true,
        size_q8: "74.6 MB",
        size_f32: "122 MB",
        languages: "de",
    },
    ModelInfo {
        shorthand: "40m-es",
        aliases: &["phonon-40m-es"],
        source: "hf://gradium/phonon-40m-es@eceb8758b5ed899bd661febecffdc6be61d8e1e6",
        repo: "gradium/phonon-40m-es",
        license: "CC BY-NC 4.0",
        non_commercial: true,
        gated: true,
        size_q8: "74.6 MB",
        size_f32: "122 MB",
        languages: "es",
    },
    ModelInfo {
        shorthand: "40m-pt",
        aliases: &["phonon-40m-pt"],
        source: "hf://gradium/phonon-40m-pt@95272bcbf0e57fd09800d5129bdf8a682eba0d43",
        repo: "gradium/phonon-40m-pt",
        license: "CC BY-NC 4.0",
        non_commercial: true,
        gated: true,
        size_q8: "74.6 MB",
        size_f32: "122 MB",
        languages: "pt",
    },
    ModelInfo {
        shorthand: "40m-multilingual",
        aliases: &["multilingual", "phonon-40m-multilingual"],
        source: "hf://gradium/phonon-40m-multilingual@1267486cbcb55d2eaa61ba521c2ed8e4d22923c2",
        repo: "gradium/phonon-40m-multilingual",
        license: "CC BY-NC 4.0",
        non_commercial: true,
        gated: true,
        size_q8: "74.6 MB",
        size_f32: "122 MB",
        languages: "en fr de es pt",
    },
];

pub const VOICES_REPO: &str = "gradium/phonon-voices";

pub fn by_shorthand(value: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|info| {
        info.shorthand.eq_ignore_ascii_case(value)
            || info
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(value))
    })
}

pub fn by_repo(repo: &str) -> Option<&'static ModelInfo> {
    MODELS.iter().find(|info| info.repo == repo)
}

/// `hf://owner/name[@revision]` to `owner/name`.
pub fn repo_of(source: &str) -> Option<&str> {
    let rest = source.strip_prefix("hf://")?;
    let repo = rest.split('@').next()?;
    (!repo.is_empty()).then_some(repo)
}

pub fn url(repo: &str) -> String {
    format!("https://huggingface.co/{repo}")
}

pub fn looks_like_auth(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "authentication",
        "gated",
        "401",
        "restricted",
        "access token",
        "hf_token",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// Print what can be downloaded, with licenses, gating and cache status.
pub fn print_catalog(quant: Quant) {
    let size_label = if quant == Quant::F32 { "f32" } else { "q8" };
    println!(
        "{:<18} {:<14} {:<6} {:<7} {:<10} {:<17} REPO",
        "MODEL",
        "LICENSE",
        "GATED",
        "CACHED",
        &format!("SIZE ({size_label})"),
        "LANGUAGES"
    );
    for info in MODELS {
        let cached = if model_store::is_cached(info.source, quant) {
            "yes"
        } else {
            "no"
        };
        let size = if quant == Quant::F32 {
            info.size_f32
        } else {
            info.size_q8
        };
        println!(
            "{:<18} {:<14} {:<6} {:<7} {:<10} {:<17} {}",
            info.shorthand,
            info.license,
            if info.gated { "yes" } else { "no" },
            cached,
            size,
            info.languages,
            info.repo,
        );
    }
    println!(
        "{:<18} {:<14} {:<6} {:<7} {:<10} {:<17} {}",
        "voices", "Apache-2.0", "no", "-", "102", "en fr de es pt", VOICES_REPO
    );
    println!();
    println!("Models download once into ~/.cache/phonon-say/models; later runs are offline.");
    println!("CC BY-NC 4.0 models are non-commercial.");
    println!();
    println!(
        "Gated models: accept the terms once at the model page, then run\n\
         `hf auth login` or set HF_TOKEN. The command line fetches and caches the\n\
         model with your credentials; the warm service only loads the local copy."
    );
    for info in MODELS.iter().filter(|info| info.gated) {
        println!("  {:<18} {}", info.shorthand, url(info.repo));
    }
    println!();
    println!("Voices: `phonon-say --list-voices` (public, no token).");
}

/// The notice printed before the first download (and on `--refresh`).
pub fn announce_download(source: &str, quant: Quant, cache: &Path, refresh: bool, quiet: bool) {
    if quiet {
        return;
    }
    let repo = repo_of(source).unwrap_or(source);
    let info = by_repo(repo);
    let name = info.map(|info| info.shorthand).unwrap_or(repo);
    let verb = if refresh { "refreshing" } else { "downloading" };
    eprintln!("[phonon-say] {verb} {name}");
    eprintln!("[phonon-say]   repo:    {}", url(repo));
    if let Some(info) = info {
        let note = if info.non_commercial {
            " (non-commercial)"
        } else {
            ""
        };
        eprintln!("[phonon-say]   license: {}{note}", info.license);
        let size = if quant == Quant::F32 {
            info.size_f32
        } else {
            info.size_q8
        };
        eprintln!(
            "[phonon-say]   size:    ~{size} ({})",
            if quant == Quant::F32 { "f32" } else { "q8" }
        );
        if info.gated {
            eprintln!(
                "[phonon-say]   gated:   accept the terms at the URL above once, then run\n\
                 [phonon-say]            `hf auth login` or set HF_TOKEN"
            );
        } else {
            eprintln!("[phonon-say]   gated:   no");
        }
    } else {
        eprintln!("[phonon-say]   license: see the model card");
        eprintln!(
            "[phonon-say]   gated:   unknown — if this repository is gated, accept its terms\n\
             [phonon-say]            at the URL above, then run `hf auth login` or set HF_TOKEN"
        );
    }
    eprintln!("[phonon-say]   cache:   {}", cache.display());
    eprintln!("[phonon-say]   all models: phonon-say --list-models");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shorthands_resolve() {
        assert_eq!(by_shorthand("31m").unwrap().repo, "gradium/phonon-31m-en");
        assert_eq!(
            by_shorthand("40m-en").unwrap().repo,
            "gradium/phonon-40m-en"
        );
        assert_eq!(
            by_shorthand("40m-multilingual").unwrap().repo,
            "gradium/phonon-40m-multilingual"
        );
        assert_eq!(
            by_shorthand("phonon-40m-fr").unwrap().repo,
            "gradium/phonon-40m-fr"
        );
        assert!(by_shorthand("nope").is_none());
    }

    #[test]
    fn parses_hub_sources() {
        assert_eq!(
            repo_of("hf://gradium/phonon-31m-en"),
            Some("gradium/phonon-31m-en")
        );
        assert_eq!(
            repo_of("hf://gradium/phonon-40m-en@abc"),
            Some("gradium/phonon-40m-en")
        );
        assert_eq!(repo_of("/local/model"), None);
    }

    #[test]
    fn recognizes_auth_errors() {
        assert!(looks_like_auth("HTTP 401: Authentication required"));
        assert!(looks_like_auth("gated repo"));
        assert!(!looks_like_auth("connection reset"));
    }
}
