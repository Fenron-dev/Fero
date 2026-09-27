//! # core::status_aliases
//!
//! What happens when [`crate::core::status::classify_status_text`] shrugs.
//!
//! A source's own wording is not standardized: one site's "Hiatus" is
//! another's "Paused indefinitely" or a completely different word in a
//! language `classify_status_text`'s small vocabulary does not cover. Rather
//! than guess — a wrong status is worse than none, the same reasoning
//! `titles_match` applies to AniList — an unrecognized text is left `None`
//! and surfaced for the user to classify once. This module is where that
//! answer is kept: per host, so the same wording on the same site is asked
//! about exactly once, and applied to every subscription there rather than
//! only the one that happened to be checked first.
//!
//! Fero has no database; this is one more small JSON file
//! (`status_aliases.json`) in the data directory, written through
//! `core::atomic` like every other one.
//!
//! ## Dependencies
//! - `core::status` – the four statuses an alias may resolve to
//! - `core::atomic` – crash-safe replacement of the store file

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::status::SeriesStatus;
use crate::error::Result;

const STATUS_ALIASES_FILE: &str = "status_aliases.json";

/// One user-confirmed mapping from a source's wording to a Fero status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusAlias {
    /// Host the wording was seen on (e.g. `novelphoenix.com`).
    pub host: String,
    /// The raw text, lowercased and trimmed — the lookup key.
    pub text: String,
    /// What the user said it means.
    pub status: SeriesStatus,
}

fn store_path(system_dir: &Path) -> PathBuf {
    system_dir.join(STATUS_ALIASES_FILE)
}

/// Normalizes a raw status string into the form aliases are keyed by.
///
/// The same normalization a lookup and a save both go through, so a stray
/// capital letter or trailing space can never make a saved alias miss on the
/// very text that prompted it.
pub fn normalize(text: &str) -> String {
    text.trim().to_lowercase()
}

/// Loads every confirmed alias. Missing or unreadable means "none yet" —
/// there is nothing here a fresh install needs to fail over.
pub fn load(system_dir: &Path) -> Vec<StatusAlias> {
    std::fs::read_to_string(store_path(system_dir))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// The status a host has been told a given text means, if any.
pub fn lookup(aliases: &[StatusAlias], host: &str, text: &str) -> Option<SeriesStatus> {
    let text = normalize(text);
    aliases
        .iter()
        .find(|alias| alias.host == host && alias.text == text)
        .map(|alias| alias.status)
}

/// Records (or replaces) the mapping for `(host, text)`.
///
/// # Errors
/// - [`crate::error::FeroError::Io`] if the store cannot be written
pub fn save(system_dir: &Path, host: &str, text: &str, status: SeriesStatus) -> Result<()> {
    let text = normalize(text);
    let mut aliases = load(system_dir);
    aliases.retain(|alias| !(alias.host == host && alias.text == text));
    aliases.push(StatusAlias {
        host: host.to_string(),
        text,
        status,
    });
    let body = serde_json::to_string_pretty(&aliases)
        .map_err(|error| crate::error::FeroError::Serialization(error.to_string()))?;
    crate::core::atomic::write_atomic(&store_path(system_dir), body.as_bytes())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fero-status-aliases-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    #[test]
    fn a_missing_store_loads_as_empty() {
        assert_eq!(load(&scratch("missing")), Vec::new());
    }

    #[test]
    fn saved_aliases_round_trip_and_are_looked_up_case_insensitively() {
        let dir = scratch("roundtrip");

        save(&dir, "novelphoenix.com", " Paused Indefinitely ", SeriesStatus::Hiatus)
            .expect("save should succeed");

        assert_eq!(
            lookup(&load(&dir), "novelphoenix.com", "PAUSED INDEFINITELY"),
            Some(SeriesStatus::Hiatus)
        );
    }

    #[test]
    fn a_second_save_for_the_same_host_and_text_replaces_the_first() {
        let dir = scratch("replace");
        save(&dir, "example.com", "weird status", SeriesStatus::Hiatus).expect("first save");

        save(&dir, "example.com", "weird status", SeriesStatus::Dropped).expect("second save");

        let aliases = load(&dir);
        assert_eq!(aliases.len(), 1);
        assert_eq!(lookup(&aliases, "example.com", "weird status"), Some(SeriesStatus::Dropped));
    }

    #[test]
    fn the_same_text_on_a_different_host_is_a_separate_answer() {
        let dir = scratch("per-host");
        save(&dir, "a.example", "odd", SeriesStatus::Hiatus).expect("save a");
        save(&dir, "b.example", "odd", SeriesStatus::Dropped).expect("save b");

        assert_eq!(lookup(&load(&dir), "a.example", "odd"), Some(SeriesStatus::Hiatus));
        assert_eq!(lookup(&load(&dir), "b.example", "odd"), Some(SeriesStatus::Dropped));
    }

    #[test]
    fn an_unknown_pair_answers_none() {
        let dir = scratch("unknown");
        save(&dir, "example.com", "known", SeriesStatus::Completed).expect("save");

        assert_eq!(lookup(&load(&dir), "example.com", "never seen"), None);
    }
}
