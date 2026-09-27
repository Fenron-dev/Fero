//! Fero's own record inside a delivered work folder.
//!
//! Replaces the old `.fero.yaml` sidecars. Those described a work by a path
//! relative to one central library — which stopped making sense once every
//! subscription can be delivered somewhere of its own.
//!
//! The manifest sits *inside* the work folder instead, so a work stays
//! self-describing wherever it is moved, and Fero can pick up where it left off
//! even if its data directory is lost. The full description (long summary,
//! cover) still lives only in the EPUB's OPF and the CBZ's `ComicInfo.xml`,
//! one reader-visible copy each — duplicating it a third time here would just
//! be more places for it to go stale. What *is* mirrored here is every field
//! someone needs in order to judge a work from the folder alone, without
//! opening an archive: genre, author, tags, source and how many chapters the
//! series has versus how many are delivered. Fero has no database, and this
//! is the one file that is guaranteed to sit next to the files it describes —
//! Fundus, a script, or a person with a file browser reads it as ground truth.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::status::SeriesStatus;
use crate::core::subscription::{unix_now, Subscription};
use crate::deliver::targets::MediaKind;
use crate::error::{FeroError, Result};

/// File name of the manifest inside a work folder.
pub const MANIFEST_FILE: &str = "fero.info.json";

/// Current schema version.
///
/// Bumped only for changes older Fero versions cannot read; new optional fields
/// do not need it.
pub const SCHEMA_VERSION: u32 = 1;

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
    /// Chapter URL — the identity another Fero instance needs to recognise
    /// this chapter in the ToC instead of downloading it again.
    ///
    /// Optional because manifests written before this field exist; those
    /// chapters cannot be matched and are treated as unknown on import.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
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
    /// Adapter id the work was fetched with (`royalroad`, `mangatown`, …).
    ///
    /// `source_url` alone answers "which page"; this answers "which site" for
    /// a reader of the manifest who does not want to parse the URL.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,
    /// Title at the time of the last write, for human readers of the file.
    pub title: String,
    /// Author, as last reported by the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Genre names, as last reported by the source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Free-form tags, as last reported by the source.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Life cycle status as last determined.
    #[serde(default)]
    pub status: SeriesStatus,
    /// Unix timestamp of the last check against the source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_check_unix: Option<u64>,
    /// When the newest chapter went up at the source, where the source says.
    ///
    /// Distinct from `last_check_unix`, which is about Fero. This one is about
    /// the work, which is why it belongs in the manifest at all: Fundus shows
    /// the folder, and "letztes Kapitel vor drei Tagen" is a fact about the
    /// series that only the fetching side ever gets to see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_release_unix: Option<u64>,
    /// How many chapters the *series* has, per the last table of contents.
    ///
    /// Not the same question as `chapters.len()`: that counts what is
    /// delivered into this folder, this counts what exists at the source —
    /// the gap between the two is exactly what a reader of the manifest
    /// usually wants to know ("84 of 92 chapters here").
    #[serde(default, skip_serializing_if = "is_zero")]
    pub total_chapters: u32,
    /// Files Fero wrote here, newest last.
    #[serde(default)]
    pub files: Vec<DeliveredFile>,
    /// Chapters present locally.
    #[serde(default)]
    pub chapters: Vec<ChapterRecord>,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
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
            source: String::new(),
            title: title.into(),
            author: None,
            genres: Vec::new(),
            tags: Vec::new(),
            status: SeriesStatus::Unknown,
            last_check_unix: None,
            latest_release_unix: None,
            total_chapters: 0,
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

    /// Copies a subscription's current descriptive metadata and bookkeeping
    /// into the manifest. Shared by both media kinds so a field added here
    /// does not have to be added twice.
    ///
    /// Does *not* touch `files` — the caller decides whether this write also
    /// delivers something new, via [`Self::record_file`].
    pub fn sync_from_subscription(&mut self, subscription: &Subscription) {
        self.source = subscription.source.clone();
        self.title = subscription.title.clone();
        self.author = subscription.author.clone();
        self.genres = subscription.genres.clone();
        self.tags = subscription.tags.clone();
        // Ins Manifest gehoert die geltende Einschaetzung; „unbekannt" waere
        // fuer eine andere Instanz weniger wert als die Annahme, dass es
        // weitergeht.
        self.status = match subscription.effective_status() {
            SeriesStatus::Unknown => SeriesStatus::Ongoing,
            known => known,
        };
        self.last_check_unix = Some(unix_now());
        self.latest_release_unix = subscription.latest_release_unix;
        // known_chapters is the superset ever seen in a table of contents,
        // downloaded or not — chapters.len() below is the delivered subset.
        self.total_chapters = subscription.known_chapters.len() as u32;
        self.chapters = subscription
            .known_chapters
            .iter()
            .filter(|chapter| chapter.downloaded_at_unix.is_some())
            .map(|chapter| ChapterRecord {
                index: chapter.index,
                title: chapter.title.clone(),
                url: Some(chapter.url.clone()),
                downloaded_at_unix: chapter.downloaded_at_unix.unwrap_or_default(),
            })
            .collect();
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
    crate::core::atomic::write_atomic(&manifest_path(work_dir), body.as_bytes())
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

    #[test]
    fn syncing_copies_descriptive_metadata_and_both_chapter_counts() {
        use crate::core::subscription::KnownChapter;

        let mut subscription = Subscription::new(
            "https://example.com/novel",
            "novelphoenix",
            "Absolute Regression",
        );
        subscription.author = Some("No Name".to_string());
        subscription.genres = vec!["Action".to_string(), "Fantasy".to_string()];
        subscription.tags = vec!["Gods".to_string()];
        subscription.latest_release_unix = Some(1_790_208_000);
        // Zehn Kapitel bekannt, aber nur die Haelfte lokal geladen — genau die
        // Luecke, die total_chapters von chapters.len() unterscheidet.
        subscription.known_chapters = (1..=10)
            .map(|index| KnownChapter {
                index,
                title: format!("Chapter {index}"),
                url: format!("https://example.com/novel/chapter-{index}"),
                volume: None,
                page_count: None,
                downloaded_at_unix: (index <= 5).then_some(1_700_000_000),
                placeholder: false,
            })
            .collect();

        let mut record = manifest();
        record.sync_from_subscription(&subscription);

        assert_eq!(record.source, "novelphoenix");
        assert_eq!(record.author.as_deref(), Some("No Name"));
        assert_eq!(record.genres, vec!["Action".to_string(), "Fantasy".to_string()]);
        assert_eq!(record.tags, vec!["Gods".to_string()]);
        assert_eq!(record.latest_release_unix, Some(1_790_208_000));
        assert_eq!(record.total_chapters, 10);
        assert_eq!(record.chapters.len(), 5, "nur geladene Kapitel zaehlen hier");
    }

    #[test]
    fn an_old_manifest_without_the_new_fields_still_parses() {
        let dir = scratch("legacy");
        std::fs::write(
            manifest_path(&dir),
            r#"{"schema":1,"subscription_id":"abc123","media_kind":"webnovel",
               "source_url":"https://example.com/novel","title":"Ein Titel",
               "status":"ongoing","files":[],"chapters":[]}"#,
        )
        .expect("write should succeed");

        let loaded = load(&dir).expect("legacy manifest should still parse");

        assert_eq!(loaded.source, "");
        assert_eq!(loaded.author, None);
        assert!(loaded.genres.is_empty());
        assert_eq!(loaded.total_chapters, 0);
    }
}
