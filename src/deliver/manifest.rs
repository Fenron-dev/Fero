//! Fero's own record inside a delivered work folder.
//!
//! Replaces the old `.fero.yaml` sidecars. Those described a work by a path
//! relative to one central library — which stopped making sense once every
//! subscription can be delivered somewhere of its own.
//!
//! The manifest sits *inside* the work folder instead, so a work stays
//! self-describing wherever it is moved, and Fero can pick up where it left off
//! even if its data directory is lost. Descriptive metadata (title, author,
//! description, genres) deliberately does **not** live here — it belongs in the
//! EPUB's OPF and the CBZ's `ComicInfo.xml`, where every reader can see it.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::deliver::targets::MediaKind;
use crate::error::{FeroError, Result};

/// File name of the manifest inside a work folder.
pub const MANIFEST_FILE: &str = "fero.info.json";

/// Current schema version.
///
/// Bumped only for changes older Fero versions cannot read; new optional fields
/// do not need it.
pub const SCHEMA_VERSION: u32 = 1;

/// How the source describes the serial's life cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeriesStatus {
    /// Still receiving new chapters.
    Ongoing,
    /// Finished upstream.
    Completed,
    /// Paused, no new chapters for a while.
    Hiatus,
    /// Abandoned by the translator or the author.
    Dropped,
    /// Licensed — fan translations often disappear afterwards.
    Licensed,
    /// Not determined yet.
    #[default]
    Unknown,
}

/// One file Fero delivered into the work folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveredFile {
    /// File name inside the work folder.
    pub name: String,
    /// Chapter range the file covers, when it is a batch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapters: Option<(u32, u32)>,
    /// Unix timestamp of the moment the file was finalized.
    pub written_at_unix: u64,
}

/// A chapter Fero knows about locally.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChapterRecord {
    /// Running index within the serial, as ordered by the source.
    pub index: u32,
    /// Chapter title as reported by the source.
    pub title: String,
    /// Unix timestamp of the download.
    pub downloaded_at_unix: u64,
}

/// Fero's record for one delivered work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkManifest {
    /// Schema version, see [`SCHEMA_VERSION`].
    pub schema: u32,
    /// Id of the subscription that produced this work.
    pub subscription_id: String,
    /// What kind of media this is.
    pub media_kind: MediaKind,
    /// Overview/ToC URL the work was fetched from.
    pub source_url: String,
    /// Title at the time of the last write, for human readers of the file.
    pub title: String,
    /// Life cycle status as last determined.
    #[serde(default)]
    pub status: SeriesStatus,
    /// Unix timestamp of the last check against the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_unix: Option<u64>,
    /// Files Fero wrote here, newest last.
    #[serde(default)]
    pub files: Vec<DeliveredFile>,
    /// Chapters present locally.
    #[serde(default)]
    pub chapters: Vec<ChapterRecord>,
}

impl WorkManifest {
    /// Creates an empty manifest for a subscription.
    pub fn new(
        subscription_id: impl Into<String>,
        media_kind: MediaKind,
        source_url: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        Self {
            schema: SCHEMA_VERSION,
            subscription_id: subscription_id.into(),
            media_kind,
            source_url: source_url.into(),
            title: title.into(),
            status: SeriesStatus::Unknown,
            last_check_unix: None,
            files: Vec::new(),
            chapters: Vec::new(),
        }
    }

    /// Records a delivered file, replacing an earlier entry with the same name.
    ///
    /// Replacing rather than appending keeps the list truthful when the running
    /// `[WIP]` file is rewritten on every run.
    pub fn record_file(&mut self, name: impl Into<String>, chapters: Option<(u32, u32)>, now: u64) {
        let name = name.into();
        self.files.retain(|file| file.name != name);
        self.files.push(DeliveredFile {
            name,
            chapters,
            written_at_unix: now,
        });
    }

    /// Returns true when the manifest already lists a file by that name.
    pub fn has_file(&self, name: &str) -> bool {
        self.files.iter().any(|file| file.name == name)
    }
}

/// Path of the manifest inside `work_dir`.
pub fn manifest_path(work_dir: &Path) -> PathBuf {
    work_dir.join(MANIFEST_FILE)
}

/// Reads the manifest from a work folder.
///
/// A missing or unparsable file yields `None` rather than an error: a work
/// folder without a manifest is simply one Fero has not written yet, and a
/// corrupted one must not block a fresh download.
pub fn load(work_dir: &Path) -> Option<WorkManifest> {
    let raw = std::fs::read_to_string(manifest_path(work_dir)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Writes the manifest into a work folder, creating the folder if needed.
///
/// # Errors
/// - [`FeroError::Serialization`] if the manifest cannot be encoded
/// - [`FeroError::Io`] if the folder or file cannot be written
pub fn save(work_dir: &Path, manifest: &WorkManifest) -> Result<()> {
    let body = serde_json::to_string_pretty(manifest)
        .map_err(|error| FeroError::Serialization(error.to_string()))?;
    std::fs::create_dir_all(work_dir).map_err(FeroError::from)?;
    std::fs::write(manifest_path(work_dir), body).map_err(FeroError::from)
}

/// Loads the manifest for a work, or starts a fresh one.
///
/// Keeps callers from having to distinguish "first delivery" from "later
/// delivery" — both just read, amend and write back.
pub fn load_or_new(
    work_dir: &Path,
    subscription_id: &str,
    media_kind: MediaKind,
    source_url: &str,
    title: &str,
) -> WorkManifest {
    match load(work_dir) {
        // A manifest for a *different* subscription in the same folder means two
        // works collided on one directory name. Starting fresh would silently
        // adopt the other one's history, so the incoming subscription wins and
        // the record is rebuilt for it.
        Some(existing) if existing.subscription_id == subscription_id => existing,
        _ => WorkManifest::new(subscription_id, media_kind, source_url, title),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("fero-manifest-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch dir should be creatable");
        dir
    }

    fn manifest() -> WorkManifest {
        WorkManifest::new(
            "abc123",
            MediaKind::Webnovel,
            "https://example.com/novel",
            "Ein Titel",
        )
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = scratch("round");
        let mut written = manifest();
        written.record_file("Titel - 001-050.epub", Some((1, 50)), 1_700_000_000);

        save(&dir, &written).expect("save should succeed");

        assert_eq!(load(&dir), Some(written));
    }

    /// The running `[WIP]` file is rewritten on every run; the manifest must
    /// list it once, not once per run.
    #[test]
    fn recording_the_same_file_twice_replaces_it() {
        let mut manifest = manifest();

        manifest.record_file("Titel - 051+ [WIP].epub", Some((51, 60)), 100);
        manifest.record_file("Titel - 051+ [WIP].epub", Some((51, 70)), 200);

        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].chapters, Some((51, 70)));
        assert_eq!(manifest.files[0].written_at_unix, 200);
    }

    #[test]
    fn missing_manifest_reads_as_none() {
        assert_eq!(load(&scratch("empty")), None);
    }

    /// A corrupted manifest must not block a fresh download.
    #[test]
    fn broken_manifest_reads_as_none() {
        let dir = scratch("broken");
        std::fs::write(manifest_path(&dir), "{ not json").expect("write should succeed");

        assert_eq!(load(&dir), None);
    }

    #[test]
    fn load_or_new_keeps_history_of_the_same_subscription() {
        let dir = scratch("same");
        let mut existing = manifest();
        existing.record_file("a.epub", None, 1);
        save(&dir, &existing).expect("save should succeed");

        let loaded = load_or_new(
            &dir,
            "abc123",
            MediaKind::Webnovel,
            "https://example.com/novel",
            "Ein Titel",
        );

        assert!(loaded.has_file("a.epub"));
    }

    /// Two works that sanitize to the same folder name must not inherit each
    /// other's file list — the incoming subscription starts clean.
    #[test]
    fn load_or_new_discards_a_foreign_manifest() {
        let dir = scratch("foreign");
        let mut other = manifest();
        other.record_file("fremd.epub", None, 1);
        save(&dir, &other).expect("save should succeed");

        let loaded = load_or_new(
            &dir,
            "andere-id",
            MediaKind::Webnovel,
            "https://example.com/other",
            "Anderer Titel",
        );

        assert!(!loaded.has_file("fremd.epub"));
        assert_eq!(loaded.subscription_id, "andere-id");
    }
}
