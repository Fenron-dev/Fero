//! # core::subscription
//!
//! Shared subscription store behind the webnovel and manga engines.
//!
//! ## Why one module for both
//! A webnovel subscription and a manga subscription differ only in what a
//! "chapter" contains — sanitized XHTML in one case, a list of page images in
//! the other.  Identity, persistence, the trash, and the host blocklist are
//! identical.  Keeping one implementation means a fix like the path
//! sanitization from the 2026-08-05 deep dive can never be applied to only one
//! of the two engines.
//!
//! ## Storage layout
//! Each subscription is stored as a single JSON file under
//! `<vault>/.fero/<store>/<key>.json`, where `<store>` is the caller's
//! store directory (`webnovels`, `mangas`) and `<key>` is a hex-encoded
//! FNV-1a-64 hash of the normalized subscription URL.  Hashing keeps the file
//! name short and filesystem-safe regardless of URL characters.
//!
//! ## Chapter identity rules
//! A chapter is identified by its **URL**, never by its title (titles are
//! routinely edited upstream).  Chapter `index` reflects the table-of-contents
//! order at the time the chapter was first seen.  If a known URL later
//! disappears from the ToC, the local record is kept — downloaded content is
//! never discarded because of upstream changes.  Newly appearing chapters are
//! appended with fresh indices.
//!
//! ## Dependencies
//! - `core::vault::Vault` – directory resolution (callers pass `system_dir()`)

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::status::SeriesStatus;
use crate::deliver::targets::MediaKind;
use crate::error::{FeroError, Result};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// A subscribed serial (webnovel or manga) and its chapter bookkeeping.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Subscription {
    /// Stable identifier — FNV-1a-64 hex of the normalized URL.
    pub id: String,
    /// Normalized overview/ToC page URL as subscribed.
    pub url: String,
    /// Source adapter id: `royalroad`, `mangatown`, `webtoons`, ….
    pub source: String,
    /// Title as reported by the source.
    pub title: String,
    /// Directory name used for this serial's files inside its media root.
    ///
    /// Pinned at subscribe time so a later upstream title change (or a scrape
    /// that returns an empty title) can never redirect file operations at a
    /// different folder.  `None` means "derive from the title" and exists only
    /// for records written before this field was introduced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder_name: Option<String>,
    /// Overrides the delivery target for this one work.
    ///
    /// Highest level of the target chain: set it and this serial goes there
    /// regardless of the media kind's default. `None` means "use the default
    /// for my media kind".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_dir: Option<String>,
    /// Author/writer, if the source exposes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,
    /// Illustrator, where the source separates it from the writer (manga).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    /// Cover image URL, if the source exposes one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cover_url: Option<String>,
    /// Synopsis/description, if available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Genre names from the source site or AniList.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub genres: Vec<String>,
    /// Free-form tags from the source site or AniList.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Goodreads book URL, when a match was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goodreads_url: Option<String>,
    /// External community rating (Goodreads 0–5), when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating_external: Option<f32>,
    /// AniList media id, when a match was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anilist_id: Option<u32>,
    /// AniList detail URL for the matched entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anilist_url: Option<String>,
    /// MyAnimeList page for the same work, when AniList knew the cross-link.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mal_url: Option<String>,
    /// Right-to-left reading order (Japanese manga).  Written into every
    /// chapter's `ComicInfo.xml`, so it is pinned on the subscription rather
    /// than re-derived per chapter.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub right_to_left: bool,
    /// Marked as finished — suppresses batch packaging and periodic checks.
    #[serde(default)]
    pub completed: bool,
    /// Abandoned upstream (never finished) — periodic checks are skipped.
    #[serde(default)]
    pub hiatus: bool,
    /// Paused subscriptions are skipped by checks but keep their data.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// All chapters ever seen in the ToC, in first-seen order.
    #[serde(default)]
    pub known_chapters: Vec<KnownChapter>,
    /// Life-cycle status as last determined from the status source.
    ///
    /// Separate from `completed`/`hiatus`, which the user sets by hand: this
    /// one is what the source said, and the two must not overwrite each other.
    #[serde(default, skip_serializing_if = "is_unknown_status")]
    pub series_status: SeriesStatus,
    /// Life-cycle status set by hand, overriding every automatic source.
    ///
    /// Its own field rather than a value written into `series_status`: a check
    /// run overwrites that one, and a setting the next run undoes is not a
    /// setting. `None` means "whatever the sources say".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_override: Option<SeriesStatus>,
    /// Whether the translation is finished, when the source says so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation_done: Option<bool>,
    /// UNIX timestamp of the last status check — drives the weekly cadence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_checked_at: Option<u64>,
    /// Category this subscription belongs to, when it is not the engine's
    /// plain default — an adult novel is scraped like a regular one, only its
    /// shelf and delivery target differ.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_kind: Option<MediaKind>,
    /// Cap on chapters downloaded in total, for trying a title out first.
    ///
    /// A 3.000-chapter manga is a lot of disk for something one might not
    /// like; ten chapters answer that question. `None` means everything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub download_limit: Option<u32>,
    /// Parent directory the files were actually delivered to.
    ///
    /// Normally equal to what the target chain resolves. It differs while the
    /// real target is offline and files are staged locally — then this is the
    /// truth about where the files are, and the chain only says where they
    /// should end up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_to: Option<String>,
    /// Chapters per block, when this serial should differ from the default.
    ///
    /// A serial with very short chapters reads better in larger blocks; one
    /// with novella-length ones in smaller. `None` means the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_size: Option<u32>,
    /// Page the status is read from, usually the NovelUpdates series page.
    ///
    /// Set automatically when the subscription itself points at NovelUpdates;
    /// otherwise the user supplies it, because only they know which entry
    /// belongs to their translation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_source_url: Option<String>,
    /// When the newest chapter went up at the source, where the source says.
    ///
    /// Distinct from `last_check_unix`, which says when *Fero* last looked.
    /// This one answers the reader's question — how long has the newest
    /// chapter been sitting there — and travels into `fero.info.json` so
    /// Fundus can answer it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_release_unix: Option<u64>,
    /// UNIX timestamp of the last completed update check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_check_unix: Option<u64>,
    /// Human-readable error from the last failed check, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// UNIX timestamp of subscription creation.
    pub created_at_unix: u64,
    /// Set while the subscription sits in the in-app trash (soft delete).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trashed_at_unix: Option<u64>,
}

/// One chapter as tracked by a subscription.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownChapter {
    /// 1-based position in ToC order at the time the chapter was first seen.
    pub index: u32,
    /// Chapter title as shown in the ToC.
    pub title: String,
    /// Chapter URL — the identity key for deduplication.
    pub url: String,
    /// Volume label when the source groups chapters into volumes (manga).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<String>,
    /// Number of page images stored for this chapter (manga only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page_count: Option<u32>,
    /// Set once the chapter content is cached locally; `None` means the
    /// chapter was seen in the ToC but not downloaded yet (resume support).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub downloaded_at_unix: Option<u64>,
    /// Set when the chapter could not be fetched and a placeholder was stored
    /// in its place.  Such chapters count as "handled" so one broken entry
    /// cannot block a run, but they stay retryable — unlike a real download.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub placeholder: bool,
}

impl KnownChapter {
    /// Whether this chapter holds real downloaded content (not a placeholder).
    pub fn is_complete(&self) -> bool {
        self.downloaded_at_unix.is_some() && !self.placeholder
    }

    /// Whether a check run should (re-)fetch this chapter.
    ///
    /// Placeholders are retried on every run: the usual cause is a transient
    /// upstream failure, and leaving a broken chapter permanently "done"
    /// silently corrupts the packaged EPUB/CBZ.
    pub fn needs_fetch(&self) -> bool {
        self.downloaded_at_unix.is_none() || self.placeholder
    }
}

impl Subscription {
    /// Creates a new subscription for a normalized URL.
    pub fn new(
        url: impl Into<String>,
        source: impl Into<String>,
        title: impl Into<String>,
    ) -> Self {
        let url = url.into();
        Self {
            id: subscription_id(&url),
            url,
            source: source.into(),
            title: title.into(),
            folder_name: None,
            target_dir: None,
            author: None,
            artist: None,
            cover_url: None,
            description: None,
            genres: Vec::new(),
            tags: Vec::new(),
            goodreads_url: None,
            rating_external: None,
            anilist_id: None,
            anilist_url: None,
            mal_url: None,
            right_to_left: false,
            completed: false,
            hiatus: false,
            enabled: true,
            series_status: SeriesStatus::Unknown,
            status_override: None,
            translation_done: None,
            status_checked_at: None,
            media_kind: None,
            download_limit: None,
            delivered_to: None,
            batch_size: None,
            status_source_url: None,
            latest_release_unix: None,
            known_chapters: Vec::new(),
            last_check_unix: None,
            last_error: None,
            created_at_unix: unix_now(),
            trashed_at_unix: None,
        }
    }

    /// The status that applies, hand setting first.
    ///
    /// The one place the three possible answers are reconciled; see
    /// [`crate::core::status::effective`] for the precedence and why.
    pub fn effective_status(&self) -> SeriesStatus {
        crate::core::status::effective(
            self.status_override,
            self.series_status,
            self.completed,
            self.hiatus,
        )
    }

    /// Number of chapters a check run would still want to fetch.
    pub fn pending_count(&self) -> usize {
        self.known_chapters
            .iter()
            .filter(|chapter| chapter.needs_fetch())
            .count()
    }

    /// Puts a serial that was considered finished back into the fast lane.
    ///
    /// Called when a run actually found new chapters. That is proof, and proof
    /// outranks even the hand setting: leaving "abgeschlossen" in place would
    /// send a work that is demonstrably still running back to the slow lane —
    /// and the user set that switch before the evidence existed.
    ///
    /// Returns whether anything changed, so the caller can say so in the log
    /// rather than silently overruling a decision the user made.
    pub fn reopen(&mut self) -> bool {
        if !self.effective_status().checks_rarely() {
            return false;
        }
        self.status_override = None;
        self.completed = false;
        self.hiatus = false;
        self.series_status = SeriesStatus::Ongoing;
        true
    }

    /// Number of chapters whose content is cached locally.
    ///
    /// Placeholders are included: the file exists and the user sees it in the
    /// packaged output, so hiding it from the count would misreport progress.
    pub fn downloaded_count(&self) -> usize {
        self.known_chapters
            .iter()
            .filter(|chapter| chapter.downloaded_at_unix.is_some())
            .count()
    }

    /// Number of chapters stored as a placeholder after a failed fetch.
    pub fn placeholder_count(&self) -> usize {
        self.known_chapters
            .iter()
            .filter(|chapter| chapter.placeholder)
            .count()
    }
}

/// Normalizes a subscription URL so equivalent inputs map to one identity.
///
/// Strips the fragment and trailing slashes and trims whitespace.  Scheme and
/// host casing are left to the caller's input; in practice pasted URLs are
/// already lowercase there.
pub fn normalize_url(url: &str) -> String {
    let trimmed = url.trim();
    let without_fragment = match trimmed.split_once('#') {
        Some((base, _fragment)) => base,
        None => trimmed,
    };
    without_fragment.trim_end_matches('/').to_string()
}

/// Derives the stable subscription id for a normalized URL.
pub fn subscription_id(url: &str) -> String {
    format!("{:016x}", fnv1a64(normalize_url(url).as_bytes()))
}

/// Length of a subscription id in hex characters (FNV-1a-64).
const SUBSCRIPTION_ID_LEN: usize = 16;

impl Subscription {
    /// Chapters per block for this serial.
    pub fn batch_size(&self) -> u32 {
        self.batch_size
            .unwrap_or(crate::core::batching::DEFAULT_BATCH_SIZE)
    }
}

/// Keeps `Unknown` out of the stored JSON — it is the default and carries no
/// information.
fn is_unknown_status(status: &SeriesStatus) -> bool {
    *status == SeriesStatus::Unknown
}

/// Whether a string is a well-formed subscription id.
///
/// Ids reach the store straight from request bodies and are interpolated into
/// file names, so anything that is not exactly the generated shape
/// (`[0-9a-f]{16}`) is rejected before it can touch the filesystem.
pub fn is_valid_subscription_id(candidate: &str) -> bool {
    candidate.len() == SUBSCRIPTION_ID_LEN
        && candidate
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Returns an error unless `candidate` is a well-formed subscription id.
fn ensure_valid_subscription_id(candidate: &str) -> Result<()> {
    if is_valid_subscription_id(candidate) {
        return Ok(());
    }
    Err(FeroError::InvalidProperty(format!(
        "invalid subscription id: {candidate}"
    )))
}

// ---------------------------------------------------------------------------
// Store operations
// ---------------------------------------------------------------------------

/// Returns the directory where a store's subscription JSON files live.
pub fn store_dir(system_dir: &Path, store: &str) -> PathBuf {
    system_dir.join(store)
}

/// Returns the file path for a specific subscription id within a store.
pub fn subscription_file_path(system_dir: &Path, store: &str, subscription_id: &str) -> PathBuf {
    store_dir(system_dir, store).join(format!("{subscription_id}.json"))
}

/// Loads a subscription by id, if one exists.
///
/// # Errors
/// - `FeroError::InvalidProperty` if `subscription_id` is not a generated id
pub fn load_subscription(
    system_dir: &Path,
    store: &str,
    subscription_id: &str,
) -> Result<Option<Subscription>> {
    ensure_valid_subscription_id(subscription_id)?;
    let path = subscription_file_path(system_dir, store, subscription_id);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path).map_err(FeroError::from)?;
    let subscription: Subscription = serde_json::from_str(&raw)
        .map_err(|e| FeroError::Serialization(format!("subscription JSON parse error: {e}")))?;
    Ok(Some(subscription))
}

/// Persists a subscription, creating the store directory if needed.
///
/// # Errors
/// - `FeroError::InvalidProperty` if the record carries a malformed id
pub fn save_subscription(
    system_dir: &Path,
    store: &str,
    subscription: &Subscription,
) -> Result<()> {
    ensure_valid_subscription_id(&subscription.id)?;
    let dir = store_dir(system_dir, store);
    fs::create_dir_all(&dir).map_err(FeroError::from)?;
    let path = subscription_file_path(system_dir, store, &subscription.id);
    let json = serde_json::to_string_pretty(subscription)
        .map_err(|e| FeroError::Serialization(format!("subscription JSON serialize error: {e}")))?;
    fs::write(&path, json).map_err(FeroError::from)?;
    Ok(())
}

/// Directory holding soft-deleted subscription records of a store.
pub fn trash_dir(system_dir: &Path, store: &str) -> PathBuf {
    store_dir(system_dir, store).join("trash")
}

fn trashed_file_path(system_dir: &Path, store: &str, subscription_id: &str) -> PathBuf {
    trash_dir(system_dir, store).join(format!("{subscription_id}.json"))
}

/// Moves a subscription record into the in-app trash (reversible).
///
/// Returns `Ok(None)` when no active subscription with this id exists.
pub fn trash_subscription(
    system_dir: &Path,
    store: &str,
    subscription_id: &str,
) -> Result<Option<Subscription>> {
    ensure_valid_subscription_id(subscription_id)?;
    let Some(mut subscription) = load_subscription(system_dir, store, subscription_id)? else {
        return Ok(None);
    };
    subscription.trashed_at_unix = Some(unix_now());
    let dir = trash_dir(system_dir, store);
    fs::create_dir_all(&dir).map_err(FeroError::from)?;
    let json = serde_json::to_string_pretty(&subscription)
        .map_err(|e| FeroError::Serialization(format!("subscription serialize error: {e}")))?;
    fs::write(trashed_file_path(system_dir, store, subscription_id), json)
        .map_err(FeroError::from)?;
    fs::remove_file(subscription_file_path(system_dir, store, subscription_id))
        .map_err(FeroError::from)?;
    Ok(Some(subscription))
}

/// Restores a trashed subscription back into the active list.
pub fn restore_subscription(
    system_dir: &Path,
    store: &str,
    subscription_id: &str,
) -> Result<Subscription> {
    ensure_valid_subscription_id(subscription_id)?;
    let path = trashed_file_path(system_dir, store, subscription_id);
    let raw = fs::read_to_string(&path).map_err(FeroError::from)?;
    let mut subscription: Subscription = serde_json::from_str(&raw)
        .map_err(|e| FeroError::Serialization(format!("subscription parse error: {e}")))?;
    subscription.trashed_at_unix = None;
    save_subscription(system_dir, store, &subscription)?;
    fs::remove_file(&path).map_err(FeroError::from)?;
    Ok(subscription)
}

/// Lists all trashed subscriptions of a store (newest first).
pub fn list_trashed_subscriptions(system_dir: &Path, store: &str) -> Result<Vec<Subscription>> {
    let dir = trash_dir(system_dir, store);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut trashed: Vec<Subscription> = fs::read_dir(&dir)
        .map_err(FeroError::from)?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                return None;
            }
            serde_json::from_str(&fs::read_to_string(&path).ok()?).ok()
        })
        .collect();
    trashed.sort_by_key(|subscription| std::cmp::Reverse(subscription.trashed_at_unix));
    Ok(trashed)
}

/// Permanently removes a trashed subscription record.
pub fn purge_trashed_subscription(
    system_dir: &Path,
    store: &str,
    subscription_id: &str,
) -> Result<()> {
    ensure_valid_subscription_id(subscription_id)?;
    let path = trashed_file_path(system_dir, store, subscription_id);
    if path.exists() {
        fs::remove_file(&path).map_err(FeroError::from)?;
    }
    Ok(())
}

/// Deletes a subscription record.  The downloaded files are NOT touched —
/// callers decide separately whether to remove the vault folder.
pub fn delete_subscription(system_dir: &Path, store: &str, subscription_id: &str) -> Result<()> {
    ensure_valid_subscription_id(subscription_id)?;
    let path = subscription_file_path(system_dir, store, subscription_id);
    if path.exists() {
        fs::remove_file(&path).map_err(FeroError::from)?;
    }
    Ok(())
}

/// Returns all subscriptions of a store, sorted by creation time (oldest first).
///
/// Files that cannot be parsed are silently skipped so one corrupt record
/// does not hide the rest of the library.
pub fn list_subscriptions(system_dir: &Path, store: &str) -> Result<Vec<Subscription>> {
    let dir = store_dir(system_dir, store);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut subscriptions: Vec<Subscription> = fs::read_dir(&dir)
        .map_err(FeroError::from)?
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                return None;
            }
            let raw = fs::read_to_string(&path).ok()?;
            serde_json::from_str(&raw).ok()
        })
        .collect();

    subscriptions.sort_by_key(|subscription| subscription.created_at_unix);
    Ok(subscriptions)
}

// ---------------------------------------------------------------------------
// Host blocklist
// ---------------------------------------------------------------------------

/// Hosts that are never fetched. Built-in entries cover domains that are
/// parked ad-landers or otherwise known to be unusable/unsafe.
const BUILTIN_BLOCKED_HOSTS: [(&str, &str); 1] = [(
    "novelive.com",
    "Geparkte Domain — leitet nur auf einen Werbe-Lander um (Stand 07/2026).",
)];

/// One blocklist entry — a host plus an optional curator note.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BlocklistEntry {
    /// Blocked host name (subdomains are blocked too).
    pub host: String,
    /// Optional note explaining WHY the host is blocked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Built-in entries ship with the app and cannot be removed.
    #[serde(default)]
    pub builtin: bool,
}

/// File format: entries are either plain host strings (legacy) or
/// `{host, note}` objects — both parse.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawBlocklistEntry {
    Plain(String),
    WithNote { host: String, note: Option<String> },
}

/// User-curated blocklist file (a JSON array of hosts or {host, note} objects).
///
/// One list covers every scraping engine — a host the user distrusts must not
/// become reachable again just because it is entered as a manga instead of a
/// novel.  The file name is kept for backwards compatibility.
pub fn blocklist_file_path(system_dir: &Path) -> PathBuf {
    system_dir.join("webnovel_blocklist.json")
}

/// Returns the merged blocklist: built-in entries plus the user's file.
///
/// The file is optional and errors are treated as an empty list — a broken
/// blocklist must never disable the built-in protections.
pub fn load_blocklist_entries(system_dir: &Path) -> Vec<BlocklistEntry> {
    let mut entries: Vec<BlocklistEntry> = BUILTIN_BLOCKED_HOSTS
        .iter()
        .map(|(host, note)| BlocklistEntry {
            host: host.to_string(),
            note: Some(note.to_string()),
            builtin: true,
        })
        .collect();
    if let Ok(raw) = fs::read_to_string(blocklist_file_path(system_dir)) {
        if let Ok(user_entries) = serde_json::from_str::<Vec<RawBlocklistEntry>>(&raw) {
            for entry in user_entries {
                let (host, note) = match entry {
                    RawBlocklistEntry::Plain(host) => (host, None),
                    RawBlocklistEntry::WithNote { host, note } => (host, note),
                };
                let host = host.trim().to_lowercase();
                if !host.is_empty() && !entries.iter().any(|existing| existing.host == host) {
                    entries.push(BlocklistEntry {
                        host,
                        note,
                        builtin: false,
                    });
                }
            }
        }
    }
    entries
}

/// Persists the user-curated part of the blocklist (built-ins are skipped).
pub fn save_user_blocklist(system_dir: &Path, entries: &[BlocklistEntry]) -> Result<()> {
    let user_entries: Vec<&BlocklistEntry> = entries
        .iter()
        .filter(|entry| !entry.builtin && !entry.host.trim().is_empty())
        .collect();
    let json = serde_json::to_string_pretty(&user_entries)
        .map_err(|e| FeroError::Serialization(format!("blocklist serialize error: {e}")))?;
    fs::create_dir_all(system_dir).map_err(FeroError::from)?;
    fs::write(blocklist_file_path(system_dir), json).map_err(FeroError::from)?;
    Ok(())
}

/// Flat host list, for the actual blocking check.
pub fn load_blocked_hosts(system_dir: &Path) -> Vec<String> {
    load_blocklist_entries(system_dir)
        .into_iter()
        .map(|entry| entry.host)
        .collect()
}

/// Checks a subscription URL against the blocklist.
///
/// Returns a user-facing German message when the URL's host (or a parent
/// domain of it) is blocked.
pub fn blocked_reason(system_dir: &Path, url: &str) -> Option<String> {
    let host = crate::api::novel::host_of(url)?;
    for blocked in load_blocked_hosts(system_dir) {
        if host == blocked || host.ends_with(&format!(".{blocked}")) {
            return Some(format!(
                "Die Seite „{host}“ steht auf der Blockliste \
                 (Datei: .fero/webnovel_blocklist.json im Vault)."
            ));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

fn default_true() -> bool {
    true
}

/// Returns the current UNIX timestamp in seconds.
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// FNV-1a 64-bit hash.
///
/// Used to derive stable, filesystem-safe keys from arbitrary strings.
fn fnv1a64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const STORE: &str = "webnovels";

    fn temp_system_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("subscription-test-{label}-{}", std::process::id()))
    }

    /// The hand setting is the whole point of `status_override`: the user
    /// says "fertig", and no later check run may quietly disagree.
    #[test]
    fn a_hand_set_status_survives_what_the_source_says() {
        let mut sub = Subscription::new("https://example.com/x", "madara", "X");
        sub.series_status = SeriesStatus::Ongoing;
        assert_eq!(sub.effective_status(), SeriesStatus::Ongoing);

        sub.status_override = Some(SeriesStatus::Completed);
        assert_eq!(sub.effective_status(), SeriesStatus::Completed);
    }

    /// New chapters are proof, and proof outranks the hand setting — otherwise
    /// a work that is demonstrably running would sit in the slow lane.
    #[test]
    fn new_chapters_reopen_a_serial_that_was_marked_finished() {
        let mut sub = Subscription::new("https://example.com/x", "madara", "X");
        sub.status_override = Some(SeriesStatus::Completed);
        sub.completed = true;

        assert!(sub.reopen());
        assert_eq!(sub.status_override, None);
        assert!(!sub.completed);
        assert_eq!(sub.effective_status(), SeriesStatus::Ongoing);

        // A serial already in the fast lane has nothing to reopen, so nothing
        // is reported and nothing is overwritten.
        assert!(!sub.reopen());
    }

    /// A licensed serial keeps releasing until the takedown: reopening it
    /// would throw away the one status that says "download now".
    #[test]
    fn reopen_leaves_the_fast_lane_statuses_alone() {
        let mut sub = Subscription::new("https://example.com/x", "novelupdates", "X");
        sub.series_status = SeriesStatus::Licensed;
        assert!(!sub.reopen());
        assert_eq!(sub.effective_status(), SeriesStatus::Licensed);
    }

    #[test]
    fn normalize_url_strips_fragment_and_trailing_slash() {
        assert_eq!(
            normalize_url("https://example.com/novel/ "),
            "https://example.com/novel"
        );
        assert_eq!(
            normalize_url("https://example.com/novel#chapter-3"),
            "https://example.com/novel"
        );
        assert_eq!(
            subscription_id("https://example.com/novel/"),
            subscription_id("https://example.com/novel#toc")
        );
    }

    #[test]
    fn subscription_ids_are_validated() {
        assert!(is_valid_subscription_id(&subscription_id(
            "https://example.com/novel"
        )));
        assert!(!is_valid_subscription_id("../../escape"));
        assert!(!is_valid_subscription_id("ABCDEF0123456789"));
        assert!(!is_valid_subscription_id("0123456789abcde"));
    }

    #[test]
    fn store_rejects_traversal_ids() {
        let dir = temp_system_dir("traversal");
        assert!(load_subscription(&dir, STORE, "../../etc/passwd").is_err());
        assert!(delete_subscription(&dir, STORE, "../../etc/passwd").is_err());
        assert!(purge_trashed_subscription(&dir, STORE, "..").is_err());
    }

    #[test]
    fn separate_stores_do_not_collide() {
        let dir = temp_system_dir("stores");
        let subscription = Subscription::new("https://example.com/x", "generic", "Shared URL");

        save_subscription(&dir, "webnovels", &subscription).expect("save should succeed");

        // Same URL, therefore the same id — but a different store must not see it.
        assert!(load_subscription(&dir, "mangas", &subscription.id)
            .expect("load should succeed")
            .is_none());
        assert_eq!(
            list_subscriptions(&dir, "mangas")
                .expect("list should succeed")
                .len(),
            0
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn placeholder_chapters_stay_retryable() {
        let complete = KnownChapter {
            index: 1,
            title: "Chapter 1".to_string(),
            url: "https://example.com/1".to_string(),
            volume: None,
            page_count: None,
            downloaded_at_unix: Some(1_700_000_000),
            placeholder: false,
        };
        let broken = KnownChapter {
            placeholder: true,
            ..complete.clone()
        };
        let pending = KnownChapter {
            downloaded_at_unix: None,
            ..complete.clone()
        };

        assert!(complete.is_complete() && !complete.needs_fetch());
        assert!(!broken.is_complete() && broken.needs_fetch());
        assert!(!pending.is_complete() && pending.needs_fetch());
    }

    #[test]
    fn save_load_delete_roundtrip() {
        let dir = temp_system_dir("crud");
        let mut subscription =
            Subscription::new("https://example.com/fiction/123", "royalroad", "Test Novel");
        subscription.known_chapters.push(KnownChapter {
            index: 1,
            title: "Chapter 1".to_string(),
            url: "https://example.com/fiction/123/chapter/1".to_string(),
            volume: None,
            page_count: None,
            downloaded_at_unix: Some(1_700_000_000),
            placeholder: false,
        });

        save_subscription(&dir, STORE, &subscription).expect("save should succeed");
        let loaded = load_subscription(&dir, STORE, &subscription.id)
            .expect("load should succeed")
            .expect("subscription should exist");
        assert_eq!(loaded.title, "Test Novel");
        assert_eq!(loaded.known_chapters.len(), 1);
        assert_eq!(loaded.downloaded_count(), 1);
        assert!(loaded.enabled);

        let all = list_subscriptions(&dir, STORE).expect("list should succeed");
        assert_eq!(all.len(), 1);

        delete_subscription(&dir, STORE, &subscription.id).expect("delete should succeed");
        assert!(load_subscription(&dir, STORE, &subscription.id)
            .expect("load should succeed")
            .is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn records_written_before_the_new_fields_still_parse() {
        // A subscription file as written by the pre-manga engine: no `artist`,
        // and chapters without `volume`/`page_count`/`placeholder`.
        let legacy = r#"{
            "id": "0123456789abcdef",
            "url": "https://example.com/novel",
            "source": "royalroad",
            "title": "Legacy Novel",
            "completed": false,
            "hiatus": false,
            "enabled": true,
            "known_chapters": [
                {"index": 1, "title": "One", "url": "https://example.com/novel/1",
                 "downloaded_at_unix": 1700000000}
            ],
            "created_at_unix": 1700000000
        }"#;
        let subscription: Subscription =
            serde_json::from_str(legacy).expect("legacy record should parse");
        assert_eq!(subscription.title, "Legacy Novel");
        assert!(subscription.artist.is_none());
        assert_eq!(subscription.known_chapters.len(), 1);
        assert!(subscription.known_chapters[0].is_complete());
        assert!(!subscription.known_chapters[0].placeholder);

        // Round-tripping must not introduce the new keys into the file.
        let json = serde_json::to_string(&subscription).expect("serialize should succeed");
        assert!(!json.contains("placeholder"));
        assert!(!json.contains("page_count"));
        assert!(!json.contains("artist"));
    }

    #[test]
    fn blocklist_merges_builtin_and_user_entries() {
        let dir = temp_system_dir("blocklist");
        std::fs::create_dir_all(&dir).expect("dir should create");
        std::fs::write(
            blocklist_file_path(&dir),
            r#"["evil-novels.example", " Mixed.Case.example "]"#,
        )
        .expect("write should succeed");

        assert!(blocked_reason(&dir, "https://novelive.com/x").is_some());
        assert!(blocked_reason(&dir, "https://www.evil-novels.example/novel/1").is_some());
        assert!(blocked_reason(&dir, "https://mixed.case.example/n").is_some());
        assert!(blocked_reason(&dir, "https://www.royalroad.com/fiction/1").is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn broken_blocklist_keeps_builtin_protection() {
        let dir = temp_system_dir("blocklist-broken");
        std::fs::create_dir_all(&dir).expect("dir should create");
        std::fs::write(blocklist_file_path(&dir), "{ not json").expect("write should succeed");
        assert!(blocked_reason(&dir, "https://novelive.com/x").is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn list_skips_corrupt_files() {
        let dir = temp_system_dir("corrupt");
        let store = store_dir(&dir, STORE);
        std::fs::create_dir_all(&store).expect("dir should create");
        std::fs::write(store.join("broken.json"), "{ not json").expect("write should succeed");

        let subscription = Subscription::new("https://example.com/fiction/9", "royalroad", "Valid");
        save_subscription(&dir, STORE, &subscription).expect("save should succeed");

        let all = list_subscriptions(&dir, STORE).expect("list should succeed");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].title, "Valid");

        std::fs::remove_dir_all(&dir).ok();
    }
}
