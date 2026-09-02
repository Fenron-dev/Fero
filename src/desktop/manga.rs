//! # Manga subscriptions
//!
//! HTTP handlers and the download pipeline for manga subscriptions.
//!
//! ## Relationship to the webnovel engine
//! The endpoints mirror `/api/webnovel/*` one for one, so the frontend can
//! reuse its subscription UI wholesale.  What differs is the payload: a
//! chapter is a list of page images, and the artifact is one CBZ per chapter
//! instead of a rebuilt EPUB.
//!
//! ## Why there is no chapter cache
//! Webnovels cache chapter XHTML because the complete EPUB is rebuilt from
//! scratch on every run.  A CBZ is final once written — the chapter it holds
//! never changes — so the archive itself *is* the cache, and a failed chapter
//! simply stays undownloaded and is retried on the next check.
//!
//! ## Vault layout
//! ```text
//! <Vault>/Manga/<Serie>/
//!   cover.jpg
//!   Serie - Kapitel 0001.cbz
//!   Serie - Kapitel 0001.cbz.fero.yaml
//! ```
//!
//! ## Dependencies:
//! - `api::manga` – source adapters
//! - `core::cbz` – archive writer
//! - `core::manga` – subscription store
//! - `desktop` – shared vault/sidecar/browser-window helpers

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};

use super::{
    current_parent, debug_log, extract_query_value, impl_outcome, resolve_workspace,
    safe_folder_segment, sanitize_path_segment, Workspace,
};
use crate::api::manga::{self, status as manga_status, MangaChapterRef, MangaInfo};
use crate::api::novel::{detect_image_media_type, status::OriginalStatus, PoliteClient};
use crate::core::cbz::{cbz_page_count, write_cbz, CbzMeta, CbzPage};
use crate::core::manga::{
    blocked_reason, delete_subscription, list_subscriptions, list_trashed_subscriptions,
    load_subscription, normalize_url, purge_trashed_subscription, restore_subscription,
    save_subscription, subscription_id, trash_subscription, unix_now, KnownChapter, Subscription,
};
use crate::core::status::{self, ComicStatusFacts, SeriesStatus};
use crate::deliver::manifest;
use crate::deliver::targets::MediaKind;
impl_outcome!(
    MangaSimpleResponse,
    MangaListResponse,
    MangaSubscribeResponse,
    MangaTrashResponse,
    MangaCheckResponse,
    MangaJobResponse
);

use crate::error::{FeroError, Result};

// ---------------------------------------------------------------------------
// Job registry
// ---------------------------------------------------------------------------

/// Registry of running/finished manga check jobs, keyed by job id.
static MANGA_JOBS: LazyLock<Mutex<HashMap<String, MangaJobStatus>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Guards against overlapping check runs.
///
/// Separate from the webnovel flag on purpose: the two engines touch different
/// hosts and different vault folders, so a manga check must not be blocked by
/// a novel check that happens to be running.
static MANGA_CHECK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Monotonic part of generated job ids.
static MANGA_JOB_COUNTER: AtomicU64 = AtomicU64::new(0);
/// How long a finished job stays pollable before it is dropped.
const MANGA_JOB_RETENTION_SECS: u64 = 10 * 60;

/// How many chapters may fail back to back before the run aborts.
const MAX_CONSECUTIVE_CHAPTER_FAILURES: usize = 5;
/// Largest accepted page image; guards against a mislabelled huge download.
const MAX_PAGE_BYTES: usize = 20 * 1024 * 1024;
/// Cover file names probed inside a series folder, in preference order.
const MANGA_COVER_NAMES: [&str; 4] = ["cover.jpg", "cover.png", "cover.webp", "cover.gif"];

/// Progress snapshot of one check job, polled by the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaJobStatus {
    /// `running`, `done`, or `failed`.
    state: String,
    /// Title of the subscription currently being processed.
    manga_title: String,
    /// 1-based position of the chapter currently being fetched.
    current_chapter: usize,
    /// Chapters queued for download in the current subscription.
    total_chapters: usize,
    /// Pages fetched within the current chapter.
    ///
    /// A manga chapter is dozens of requests; without this the UI would sit on
    /// an unmoving bar for minutes on a long chapter.
    current_page: usize,
    /// Pages in the current chapter, once known.
    total_pages: usize,
    /// Chapters completed so far across the whole job.
    downloaded: usize,
    /// Set once the user asked for the run to stop.
    ///
    /// Visible to the frontend so the button can say "wird beendet …" instead
    /// of looking like the click did nothing — between two chapters there can
    /// be dozens of page requests still in flight.
    cancel_requested: bool,
    /// Final summary or error message once the job is terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    /// When the job reached a terminal state — drives registry cleanup.
    #[serde(skip)]
    finished_at_unix: Option<u64>,
}

impl MangaJobStatus {
    fn running() -> Self {
        Self {
            state: "running".to_string(),
            manga_title: String::new(),
            current_chapter: 0,
            total_chapters: 0,
            current_page: 0,
            total_pages: 0,
            downloaded: 0,
            cancel_requested: false,
            message: None,
            finished_at_unix: None,
        }
    }

    fn is_terminal(&self) -> bool {
        self.state != "running"
    }
}

/// Whether the user asked this job to stop.
///
/// Checked between chapters rather than between pages: a half-written CBZ is
/// worse than one more chapter, and a chapter is the unit the resume logic
/// already knows how to pick up again.
fn cancel_requested(job_id: &str) -> bool {
    MANGA_JOBS
        .lock()
        .ok()
        .and_then(|jobs| jobs.get(job_id).map(|status| status.cancel_requested))
        .unwrap_or(false)
}

/// Marks one running job — or every one — as asked to stop.
///
/// Returns how many jobs were flagged, so the caller can tell the user whether
/// anything was actually running.
fn request_cancel(job_id: Option<&str>) -> usize {
    let Ok(mut jobs) = MANGA_JOBS.lock() else {
        return 0;
    };
    let mut flagged = 0usize;
    for (id, status) in jobs.iter_mut() {
        if status.is_terminal() || !job_id.is_none_or(|wanted| wanted == id.as_str()) {
            continue;
        }
        status.cancel_requested = true;
        flagged += 1;
    }
    flagged
}

/// Applies a mutation to a job's status under the registry lock.
fn update_job(job_id: &str, apply: impl FnOnce(&mut MangaJobStatus)) {
    if let Ok(mut jobs) = MANGA_JOBS.lock() {
        if let Some(status) = jobs.get_mut(job_id) {
            apply(status);
            if status.is_terminal() && status.finished_at_unix.is_none() {
                status.finished_at_unix = Some(unix_now());
            }
        }
    }
}

/// Drops finished jobs the frontend can no longer be waiting for.
fn prune_jobs(jobs: &mut HashMap<String, MangaJobStatus>) {
    let now = unix_now();
    jobs.retain(|_, status| match status.finished_at_unix {
        Some(finished) => now.saturating_sub(finished) < MANGA_JOB_RETENTION_SECS,
        None => true,
    });
}

/// Clears the "check running" flag when a worker thread ends — even on panic,
/// so a crashed job can never wedge future checks.
struct CheckActiveGuard;

impl Drop for CheckActiveGuard {
    fn drop(&mut self) {
        MANGA_CHECK_ACTIVE.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Shared response shapes
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaSimpleResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl MangaSimpleResponse {
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }
    fn error(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaSubscriptionSummary {
    id: String,
    url: String,
    source: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anilist_url: Option<String>,
    /// MyAnimeList page for the same work, when AniList knew the cross-link.
    #[serde(skip_serializing_if = "Option::is_none")]
    mal_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rating_external: Option<f32>,
    /// Delivery target set for this subscription alone, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    target_dir: Option<String>,
    /// Category this subscription belongs to.
    media_kind: MediaKind,
    /// Total chapter cap, when one is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    download_limit: Option<u32>,
    /// Where the files currently live, when anything was delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    delivered_to: Option<String>,
    /// True when the files sit somewhere else than the target chain points.
    needs_relocation: bool,
    /// Whether a cached cover exists in the series folder.
    has_cover: bool,
    completed: bool,
    hiatus: bool,
    /// The status that applies, hand setting first — what the badge shows.
    series_status: SeriesStatus,
    /// The hand setting itself, so the control can show what is set rather
    /// than what happens to be true.
    #[serde(skip_serializing_if = "Option::is_none")]
    status_override: Option<SeriesStatus>,
    /// Whether this status is worth pointing out unprompted.
    needs_attention: bool,
    /// The database entry the status is read from, when one is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    status_source_url: Option<String>,
    /// When the newest chapter went up at the source, where the source says.
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_release_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    status_checked_at: Option<u64>,
    enabled: bool,
    known_chapters: usize,
    downloaded_chapters: usize,
    /// When the subscription was added — the "zuletzt hinzugefügt" sort.
    created_at_unix: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_check_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

impl MangaSubscriptionSummary {
    /// Builds the summary. `work_dir` is `None` when no target is configured
    /// yet — the subscription is still listed, just without a cover.
    fn from_subscription(subscription: &Subscription, ws: &Workspace) -> Self {
        let kind = subscription_kind(subscription);
        let desired = ws.delivery_parent(kind, subscription).ok();
        let current = subscription
            .delivered_to
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| desired.clone());
        let has_cover = current
            .as_ref()
            .map(|parent| manga_cover_path(&manga_folder(parent, subscription)).is_some())
            .unwrap_or(false);
        let needs_relocation = matches!(
            (subscription.delivered_to.as_deref(), desired.as_ref()),
            (Some(now), Some(target)) if Path::new(now) != target.as_path()
        );
        Self {
            id: subscription.id.clone(),
            url: subscription.url.clone(),
            source: subscription.source.clone(),
            title: subscription.title.clone(),
            author: subscription.author.clone(),
            artist: subscription.artist.clone(),
            description: subscription.description.clone(),
            genres: subscription.genres.clone(),
            tags: subscription.tags.clone(),
            anilist_url: subscription.anilist_url.clone(),
            mal_url: subscription.mal_url.clone(),
            rating_external: subscription.rating_external,
            target_dir: subscription.target_dir.clone(),
            media_kind: kind,
            download_limit: subscription.download_limit,
            delivered_to: subscription.delivered_to.clone(),
            needs_relocation,
            has_cover,
            completed: subscription.completed,
            hiatus: subscription.hiatus,
            series_status: subscription.effective_status(),
            status_override: subscription.status_override,
            needs_attention: subscription.effective_status().needs_attention(),
            status_source_url: subscription.status_source_url.clone(),
            status_checked_at: subscription.status_checked_at,
            latest_release_unix: subscription.latest_release_unix,
            enabled: subscription.enabled,
            known_chapters: subscription.known_chapters.len(),
            downloaded_chapters: subscription.downloaded_count(),
            created_at_unix: subscription.created_at_unix,
            last_check_unix: subscription.last_check_unix,
            last_error: subscription.last_error.clone(),
        }
    }
}

/// Resolves the active vault or produces a user-facing German error message.
// ---------------------------------------------------------------------------
// GET /api/manga/list
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaListResponse {
    subscriptions: Vec<MangaSubscriptionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Lists all manga subscriptions of the active ws.vault.
pub(crate) fn build_list_response(query: Option<&str>) -> MangaListResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let ws = match resolve_workspace(root_override.as_deref()) {
        Ok(ws) => ws,
        Err(message) => {
            return MangaListResponse {
                subscriptions: Vec::new(),
                error: Some(message),
            }
        }
    };

    match list_subscriptions(&ws.store) {
        Ok(subscriptions) => MangaListResponse {
            subscriptions: subscriptions
                .iter()
                .map(|subscription| MangaSubscriptionSummary::from_subscription(subscription, &ws))
                .collect(),
            error: None,
        },
        Err(error) => MangaListResponse {
            subscriptions: Vec::new(),
            error: Some(error.to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// POST /api/manga/subscribe
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubscribeRequest {
    url: String,
    /// Category the new subscription belongs to (`manga` or `hmanga`).
    #[serde(default)]
    media_kind: Option<String>,
    #[serde(default)]
    root: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaSubscribeResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription: Option<MangaSubscriptionSummary>,
    /// True when the URL was already subscribed and no new record was created.
    already_subscribed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl MangaSubscribeResponse {
    fn error(message: impl Into<String>) -> Self {
        Self {
            subscription: None,
            already_subscribed: false,
            error: Some(message.into()),
        }
    }
}

/// Subscribes to a manga series after verifying the URL can actually be read.
pub(crate) fn build_subscribe_response(body: &[u8]) -> MangaSubscribeResponse {
    let req: SubscribeRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return MangaSubscribeResponse::error(format!("Invalid request: {error}")),
    };

    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return MangaSubscribeResponse::error(message),
    };

    let url = normalize_url(&req.url);
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return MangaSubscribeResponse::error("Bitte eine vollständige URL angeben.");
    }
    if let Some(reason) = blocked_reason(&ws.store, &url) {
        return MangaSubscribeResponse::error(reason);
    }

    // Re-subscribing an existing URL returns the current record unchanged.
    let id = subscription_id(&url);
    match load_subscription(&ws.store, &id) {
        Ok(Some(existing)) => {
            return MangaSubscribeResponse {
                subscription: Some(MangaSubscriptionSummary::from_subscription(&existing, &ws)),
                already_subscribed: true,
                error: None,
            }
        }
        Ok(None) => {}
        Err(error) => return MangaSubscribeResponse::error(error.to_string()),
    }

    let source = match manga::require_source(&url) {
        Ok(source) => source,
        Err(error) => return MangaSubscribeResponse::error(error.to_string()),
    };
    let client = match PoliteClient::new() {
        Ok(client) => client,
        Err(error) => return MangaSubscribeResponse::error(error.to_string()),
    };

    debug_log(&format!("manga subscribe: {url} source={}", source.id()));
    let info = match source.fetch_series_info(&client, &url) {
        Ok(info) => {
            debug_log(&format!(
                "manga subscribe: OK — {} Kapitel",
                info.chapters.len()
            ));
            info
        }
        Err(error) => {
            debug_log(&format!("manga subscribe: FEHLER: {error}"));
            return MangaSubscribeResponse::error(error.to_string());
        }
    };

    let mut subscription = Subscription::new(url, source.id(), info.title.clone());
    // Pin the folder now: the title may change upstream later, and two series
    // can sanitize to the same segment.
    subscription.media_kind = req
        .media_kind
        .as_deref()
        .and_then(MediaKind::from_id)
        .filter(|kind| kind.uses_manga_engine());
    subscription.folder_name = Some(unique_manga_folder_name(&ws, &subscription));
    apply_series_info(&mut subscription, &info);
    subscription.known_chapters = info
        .chapters
        .iter()
        .enumerate()
        .map(|(position, chapter)| known_chapter(position as u32 + 1, chapter))
        .collect();

    match save_subscription(&ws.store, &subscription) {
        Ok(()) => MangaSubscribeResponse {
            subscription: Some(MangaSubscriptionSummary::from_subscription(
                &subscription,
                &ws,
            )),
            already_subscribed: false,
            error: None,
        },
        Err(error) => MangaSubscribeResponse::error(error.to_string()),
    }
}

/// Builds a store record from an adapter's chapter reference.
fn known_chapter(index: u32, chapter: &MangaChapterRef) -> KnownChapter {
    KnownChapter {
        index,
        title: chapter.title.clone(),
        url: chapter.url.clone(),
        volume: chapter.volume.clone(),
        page_count: None,
        downloaded_at_unix: None,
        placeholder: false,
    }
}

/// Copies scraped series metadata into a subscription without overwriting
/// anything the user (or an earlier, richer source) already filled in.
fn apply_series_info(subscription: &mut Subscription, info: &MangaInfo) {
    if subscription.author.is_none() {
        subscription.author = info.author.clone();
    }
    if subscription.artist.is_none() {
        subscription.artist = info.artist.clone();
    }
    if subscription.description.is_none() {
        subscription.description = info.description.clone();
    }
    if subscription.cover_url.is_none() {
        subscription.cover_url = info.cover_url.clone();
    }
    if subscription.genres.is_empty() {
        subscription.genres = info.genres.clone();
    }
    if subscription.tags.is_empty() {
        subscription.tags = info.tags.clone();
    }
    // Nur solange der Nutzer nichts von Hand gesetzt hat: sonst haette er
    // seine Entscheidung nach jedem Prueflauf erneut zu treffen.
    if info.completed_hint == Some(true) && subscription.status_override.is_none() {
        subscription.completed = true;
    }
    // Nur vorwaerts: verschwindet ein Datum aus der Seite oder liest ein Lauf
    // eine gekuerzte Liste, bleibt der zuletzt bekannte Stand stehen.
    if let Some(released) = info.latest_release_unix {
        if subscription
            .latest_release_unix
            .is_none_or(|known| released > known)
        {
            subscription.latest_release_unix = Some(released);
        }
    }
    // The adapter knows the reading direction (manga vs. manhwa/webtoon);
    // it must reach every chapter archive, not just the first.
    subscription.right_to_left = info.right_to_left;
}

// ---------------------------------------------------------------------------
// POST /api/manga/unsubscribe · trash · restore · purge
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UnsubscribeRequest {
    id: String,
    /// When false, the series' vault folder (CBZ files) is moved to `.trash`.
    #[serde(default = "default_true")]
    keep_files: bool,
    #[serde(default)]
    root: Option<String>,
}

fn default_true() -> bool {
    true
}

/// Moves a subscription into the in-app trash, optionally with its files.
pub(crate) fn build_unsubscribe_response(body: &[u8]) -> MangaSimpleResponse {
    let req: UnsubscribeRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return MangaSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return MangaSimpleResponse::error(message),
    };

    let subscription = match trash_subscription(&ws.store, &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return MangaSimpleResponse::error("Abo nicht gefunden."),
        Err(error) => return MangaSimpleResponse::error(error.to_string()),
    };

    if !req.keep_files {
        let parent = match current_parent(&ws, subscription_kind(&subscription), &subscription) {
            Ok(parent) => parent,
            Err(error) => return MangaSimpleResponse::error(error.to_string()),
        };
        let series_dir = manga_folder(&parent, &subscription);
        if series_dir.exists() {
            let trash_target = manga_trash_folder(&parent, &subscription);
            if let Some(parent) = trash_target.parent() {
                fs::create_dir_all(parent).ok();
            }
            if trash_target.exists() {
                fs::remove_dir_all(&trash_target).ok();
            }
            if let Err(error) = fs::rename(&series_dir, &trash_target) {
                return MangaSimpleResponse::error(format!(
                    "Dateien konnten nicht in den Papierkorb verschoben werden: {error}"
                ));
            }
        }
    }

    MangaSimpleResponse::ok()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaTrashResponse {
    subscriptions: Vec<MangaSubscriptionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Lists trashed manga subscriptions.
pub(crate) fn build_trash_response(query: Option<&str>) -> MangaTrashResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let ws = match resolve_workspace(root_override.as_deref()) {
        Ok(ws) => ws,
        Err(message) => {
            return MangaTrashResponse {
                subscriptions: Vec::new(),
                error: Some(message),
            }
        }
    };

    match list_trashed_subscriptions(&ws.store) {
        Ok(subscriptions) => MangaTrashResponse {
            subscriptions: subscriptions
                .iter()
                .map(|subscription| MangaSubscriptionSummary::from_subscription(subscription, &ws))
                .collect(),
            error: None,
        },
        Err(error) => MangaTrashResponse {
            subscriptions: Vec::new(),
            error: Some(error.to_string()),
        },
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdRequest {
    id: String,
    #[serde(default)]
    root: Option<String>,
}

/// Restores a trashed subscription (and its files, if they are in `.trash`).
pub(crate) fn build_restore_response(body: &[u8]) -> MangaSimpleResponse {
    let req: IdRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return MangaSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return MangaSimpleResponse::error(message),
    };

    let subscription = match restore_subscription(&ws.store, &req.id) {
        Ok(subscription) => subscription,
        Err(error) => return MangaSimpleResponse::error(error.to_string()),
    };

    // Bring the files back too, when the delete moved them aside.
    let parent = match current_parent(&ws, subscription_kind(&subscription), &subscription) {
        Ok(parent) => parent,
        Err(error) => return MangaSimpleResponse::error(error.to_string()),
    };
    let trash_target = manga_trash_folder(&parent, &subscription);
    let series_dir = manga_folder(&parent, &subscription);
    if trash_target.exists() && !series_dir.exists() {
        if let Some(parent) = series_dir.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::rename(&trash_target, &series_dir).ok();
    }

    MangaSimpleResponse::ok()
}

/// Permanently deletes a trashed subscription and its trashed files.
pub(crate) fn build_purge_response(body: &[u8]) -> MangaSimpleResponse {
    let req: IdRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return MangaSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return MangaSimpleResponse::error(message),
    };

    // Read the record before purging so its pinned folder name is known; the
    // folder must never be derived from anything else at delete time.
    let trashed = list_trashed_subscriptions(&ws.store)
        .unwrap_or_default()
        .into_iter()
        .find(|subscription| subscription.id == req.id);

    if let Err(error) = purge_trashed_subscription(&ws.store, &req.id) {
        return MangaSimpleResponse::error(error.to_string());
    }
    if let Some(subscription) = trashed {
        let Ok(parent) = current_parent(&ws, subscription_kind(&subscription), &subscription)
        else {
            return MangaSimpleResponse::error(
                "Kein Zielordner festgelegt — die Dateien lassen sich nicht finden.",
            );
        };
        let trash_target = manga_trash_folder(&parent, &subscription);
        if trash_target.exists() {
            fs::remove_dir_all(&trash_target).ok();
        }
    }
    // Active record too, in case the id was never trashed.
    delete_subscription(&ws.store, &req.id).ok();

    MangaSimpleResponse::ok()
}

// ---------------------------------------------------------------------------
// POST /api/manga/update
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequest {
    id: String,
    #[serde(default)]
    root: Option<String>,
    /// New delivery target for this one subscription; clearing needs its own
    /// flag, because a missing field and an explicit null are indistinguishable
    /// and an unrelated update must never wipe the target.
    #[serde(default)]
    target_dir: Option<String>,
    #[serde(default)]
    clear_target_dir: bool,
    /// Page the life-cycle status is read from — an AniList or MyAnimeList
    /// entry for this series.
    ///
    /// Empty string clears it; unlike the target there is no separate flag,
    /// because an empty URL is meaningless and unambiguous.
    #[serde(default)]
    status_source_url: Option<String>,
    /// Life-cycle status set by hand. `"auto"` hands the decision back to the
    /// status source; every other value must be a known status id.
    ///
    /// A string rather than a nullable status plus a clearing flag: `"auto"`
    /// is what the dropdown offers, and it says exactly what it does.
    #[serde(default)]
    status_override: Option<String>,
    /// New total chapter cap; zero lifts the limit.
    #[serde(default)]
    download_limit: Option<u32>,
    /// New category (`manga` or `hmanga`).
    #[serde(default)]
    media_kind: Option<String>,
    #[serde(default)]
    completed: Option<bool>,
    #[serde(default)]
    hiatus: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
}

/// Toggles the completed/hiatus/paused flags of a subscription.
pub(crate) fn build_update_response(body: &[u8]) -> MangaSimpleResponse {
    let req: UpdateRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return MangaSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return MangaSimpleResponse::error(message),
    };

    let mut subscription = match load_subscription(&ws.store, &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return MangaSimpleResponse::error("Abo nicht gefunden."),
        Err(error) => return MangaSimpleResponse::error(error.to_string()),
    };

    if let Some(choice) = req.status_override.as_deref() {
        match choice {
            "auto" | "" => subscription.status_override = None,
            id => match SeriesStatus::from_id(id) {
                Some(status) => subscription.status_override = Some(status),
                None => return MangaSimpleResponse::error(format!("Unbekannter Status: {id}")),
            },
        }
        // Zurueck auf Automatik heisst: beim naechsten Lauf frisch nachsehen,
        // statt eine womoeglich Monate alte Ermittlung wieder hervorzuholen.
        subscription.status_checked_at = None;
    }
    if let Some(completed) = req.completed {
        subscription.completed = completed;
    }
    if let Some(hiatus) = req.hiatus {
        subscription.hiatus = hiatus;
    }
    if let Some(enabled) = req.enabled {
        subscription.enabled = enabled;
    }
    if let Some(url) = req.status_source_url {
        let trimmed = url.trim();
        subscription.status_source_url = (!trimmed.is_empty()).then(|| trimmed.to_string());
        // Eine neue Quelle muss erst gelesen werden, bevor sie etwas bedeutet.
        subscription.status_checked_at = None;
    }
    if req.clear_target_dir {
        subscription.target_dir = None;
    } else if let Some(target) = req.target_dir {
        subscription.target_dir = Some(target);
    }
    if let Some(limit) = req.download_limit {
        subscription.download_limit = (limit > 0).then_some(limit);
    }
    if let Some(kind) = req.media_kind.as_deref().and_then(MediaKind::from_id) {
        if kind.uses_manga_engine() {
            subscription.media_kind = Some(kind);
        }
    }

    match save_subscription(&ws.store, &subscription) {
        Ok(()) => MangaSimpleResponse::ok(),
        Err(error) => MangaSimpleResponse::error(error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// POST /api/manga/check · GET /api/manga/job
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CheckRequest {
    /// Check a single subscription; omitted = all enabled, unfinished ones.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    root: Option<String>,
    /// Per-host request delay in milliseconds (clamped to 500–5000).
    #[serde(default)]
    delay_ms: Option<u64>,
    /// Cap on chapters downloaded per subscription in one run.
    ///
    /// Subscribing to a 3000-chapter series would otherwise start a download
    /// that runs for days; the next check simply picks up where this stopped.
    #[serde(default)]
    max_chapters: Option<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaCheckResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Options carried into the worker thread for one check run.
struct CheckOptions {
    only_id: Option<String>,
    delay_ms: Option<u64>,
    max_chapters: Option<usize>,
}

/// Starts a background check run and returns its job id.
pub(crate) fn build_check_response(body: &[u8]) -> MangaCheckResponse {
    let req: CheckRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return MangaCheckResponse {
                job_id: None,
                error: Some(format!("Invalid request: {error}")),
            }
        }
    };

    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => {
            return MangaCheckResponse {
                job_id: None,
                error: Some(message),
            }
        }
    };

    if MANGA_CHECK_ACTIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return MangaCheckResponse {
            job_id: None,
            error: Some("Eine Manga-Prüfung läuft bereits.".to_string()),
        };
    }

    let job_id = format!(
        "manga-{}-{}",
        unix_now(),
        MANGA_JOB_COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    if let Ok(mut jobs) = MANGA_JOBS.lock() {
        prune_jobs(&mut jobs);
        jobs.insert(job_id.clone(), MangaJobStatus::running());
    }

    let options = CheckOptions {
        only_id: req.id,
        delay_ms: req.delay_ms,
        max_chapters: req.max_chapters,
    };
    spawn_check(ws, options, job_id.clone());

    MangaCheckResponse {
        job_id: Some(job_id),
        error: None,
    }
}

/// Runs a check on a worker thread and writes its outcome into the job.
///
/// Shared by the endpoint and the scheduled run so the two cannot drift apart
/// — the whole reason manga were never fetched automatically is that only one
/// of the two paths existed.
fn spawn_check(ws: Workspace, options: CheckOptions, job_id: String) {
    std::thread::spawn(move || {
        let _active = CheckActiveGuard;
        let outcome = run_check(&ws, &options, &job_id);
        update_job(&job_id, |status| match &outcome {
            Ok(message) => {
                status.state = "done".to_string();
                status.message = Some(message.clone());
            }
            Err(error) => {
                status.state = "failed".to_string();
                status.message = Some(error.to_string());
            }
        });
    });
}

/// Starts a scheduled check of every due subscription.
///
/// The manga counterpart to `webnovel::start_scheduled_check`.
/// Its absence was a silent hole: the tray and the timer only ever started the
/// novel engine, so a manga subscription was never fetched unless someone
/// opened the window and clicked — in a program whose point is running when
/// nobody is looking.
///
/// Returns `false` when a run is already in flight; the timer then skips this
/// tick rather than queueing, because runs longer than the interval would
/// otherwise pile up behind each other.
pub(super) fn start_scheduled_check() -> bool {
    let Ok(ws) = resolve_workspace(None) else {
        return false;
    };
    if MANGA_CHECK_ACTIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }

    let job_id = format!(
        "manga-{}-{}",
        unix_now(),
        MANGA_JOB_COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    if let Ok(mut jobs) = MANGA_JOBS.lock() {
        prune_jobs(&mut jobs);
        jobs.insert(job_id.clone(), MangaJobStatus::running());
    }
    spawn_check(
        ws,
        CheckOptions {
            only_id: None,
            delay_ms: None,
            max_chapters: None,
        },
        job_id,
    );
    true
}

/// Asks a running check to stop.
///
/// Without a `job_id` every running manga job is flagged. Nothing is undone:
/// chapters already written stay written, and the next run picks up where this
/// one left off — stopping is "genug für jetzt", not "mach es rückgängig".
pub(crate) fn build_stop_response(body: &[u8]) -> MangaSimpleResponse {
    let job_id = serde_json::from_slice::<StopRequest>(body)
        .ok()
        .and_then(|req| req.job_id);
    match request_cancel(job_id.as_deref()) {
        0 => MangaSimpleResponse::error("Es läuft gerade keine Manga-Prüfung."),
        _ => MangaSimpleResponse::ok(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StopRequest {
    /// The one job to stop; omitted stops every running one.
    #[serde(default)]
    job_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MangaJobResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<MangaJobStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Returns the progress of a check job; terminal jobs are handed out once.
pub(crate) fn build_job_response(query: Option<&str>) -> MangaJobResponse {
    let Some(job_id) = query.and_then(|q| extract_query_value(q, "job_id")) else {
        return MangaJobResponse {
            status: None,
            error: Some("missing job_id".to_string()),
        };
    };

    let Ok(mut jobs) = MANGA_JOBS.lock() else {
        return MangaJobResponse {
            status: None,
            error: Some("job registry unavailable".to_string()),
        };
    };

    match jobs.get(&job_id).cloned() {
        Some(status) => {
            if status.state != "running" {
                jobs.remove(&job_id);
            }
            MangaJobResponse {
                status: Some(status),
                error: None,
            }
        }
        None => MangaJobResponse {
            status: None,
            error: Some("Job nicht gefunden.".to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// Check runner
// ---------------------------------------------------------------------------

/// Runs one check job over the selected subscriptions.
///
/// Per-subscription failures are recorded in that subscription's `last_error`
/// and the run continues; only store failures abort the whole job.
/// Category of a subscription handled by this engine.
fn subscription_kind(subscription: &Subscription) -> MediaKind {
    subscription
        .media_kind
        .filter(|kind| kind.uses_manga_engine())
        .unwrap_or(MediaKind::Manga)
}

fn run_check(ws: &Workspace, options: &CheckOptions, job_id: &str) -> Result<String> {
    let system_dir = &ws.store;
    let all = list_subscriptions(system_dir)?;
    let selected: Vec<Subscription> = match options.only_id.as_deref() {
        Some(id) => all
            .into_iter()
            .filter(|subscription| subscription.id == id)
            .collect(),
        None => {
            let now = unix_now();
            all.into_iter()
                .filter(|subscription| {
                    status::should_check(
                        subscription.effective_status(),
                        subscription.enabled,
                        subscription.last_check_unix,
                        now,
                    )
                })
                .collect()
        }
    };

    if selected.is_empty() {
        return Ok("Keine passenden Abonnements.".to_string());
    }

    let client = match options.delay_ms {
        Some(delay_ms) => PoliteClient::with_delay_ms(delay_ms)?,
        None => PoliteClient::new()?,
    };

    let mut new_chapters = 0usize;
    let mut failures = 0usize;

    let mut stopped = false;
    for mut subscription in selected {
        if cancel_requested(job_id) {
            stopped = true;
            break;
        }
        update_job(job_id, |status| {
            status.manga_title = subscription.title.clone();
            status.current_chapter = 0;
            status.total_chapters = 0;
            status.current_page = 0;
            status.total_pages = 0;
        });

        match check_one(ws, &client, &mut subscription, options, job_id) {
            Ok(downloaded) => {
                new_chapters += downloaded;
                subscription.last_error = None;
            }
            Err(error) => {
                failures += 1;
                subscription.last_error = Some(error.to_string());
            }
        }
        subscription.last_check_unix = Some(unix_now());
        save_subscription(system_dir, &subscription)?;
    }

    let mut message = if stopped || cancel_requested(job_id) {
        format!("Abgebrochen. {new_chapters} neue Kapitel geladen.")
    } else {
        format!("{new_chapters} neue Kapitel geladen.")
    };
    if failures > 0 {
        message.push_str(&format!(" {failures} Abo(s) mit Fehlern."));
    }
    Ok(message)
}

/// Checks one subscription: refresh the chapter list, download what is missing.
fn check_one(
    ws: &Workspace,
    client: &PoliteClient,
    subscription: &mut Subscription,
    options: &CheckOptions,
    job_id: &str,
) -> Result<usize> {
    if let Some(reason) = blocked_reason(&ws.store, &subscription.url) {
        return Err(FeroError::ExternalApi(reason));
    }
    let source = manga::require_source(&subscription.url)?;
    debug_log(&format!(
        "manga check: '{}' source={}",
        subscription.title,
        source.id()
    ));

    let info = source.fetch_series_info(client, &subscription.url)?;
    apply_series_info(subscription, &info);
    enrich_from_anilist(subscription);

    // Diff by normalized URL. Chapters that vanished upstream are kept —
    // downloaded content is never dropped because of an upstream edit.
    let known: HashSet<String> = subscription
        .known_chapters
        .iter()
        .map(|chapter| normalize_url(&chapter.url))
        .collect();
    let mut next_index = subscription
        .known_chapters
        .iter()
        .map(|chapter| chapter.index)
        .max()
        .unwrap_or(0)
        + 1;
    let mut appeared = 0usize;
    for chapter in &info.chapters {
        if !known.contains(&normalize_url(&chapter.url)) {
            subscription
                .known_chapters
                .push(known_chapter(next_index, chapter));
            next_index += 1;
            appeared += 1;
        }
    }
    // Neue Kapitel sind der Beweis, dass die Serie doch laeuft — und der
    // schlaegt jede Einschaetzung, auch die von Hand gesetzte.
    if appeared > 0 && subscription.reopen() {
        debug_log(&format!(
            "manga: '{}' war als abgeschlossen/pausiert markiert, hat aber \
             {appeared} neue Kapitel — wieder auf laufend gesetzt.",
            subscription.title
        ));
    }

    // Wo die Dateien wirklich liegen, hat Vorrang; siehe Webnovel-Engine.
    let kind = subscription_kind(subscription);
    let parent = match subscription.delivered_to.as_ref() {
        Some(current) => PathBuf::from(current),
        None => {
            let (parent, staged) = ws.delivery_parent_or_staging(kind, subscription)?;
            if staged {
                update_job(job_id, |status| {
                    status.message =
                        Some("Ziel nicht erreichbar — es wird lokal gesammelt.".to_string());
                });
            }
            parent
        }
    };
    subscription.delivered_to = Some(parent.display().to_string());
    let series_dir = manga_folder(&parent, subscription);
    fs::create_dir_all(&series_dir).map_err(FeroError::from)?;
    ensure_cover(client, subscription, &series_dir);

    let mut pending: Vec<usize> = subscription
        .known_chapters
        .iter()
        .enumerate()
        .filter(|(_, chapter)| chapter.needs_fetch())
        .map(|(position, _)| position)
        .collect();
    // Gesamtbestand deckeln (zum Reinschnuppern), zusaetzlich zum Lauf-Limit.
    if let Some(limit) = subscription.download_limit {
        let already = subscription
            .known_chapters
            .iter()
            .filter(|chapter| chapter.downloaded_at_unix.is_some())
            .count();
        let allowed = (limit as usize).saturating_sub(already);
        if pending.len() > allowed {
            pending.truncate(allowed);
        }
    }
    if let Some(limit) = options.max_chapters {
        pending.truncate(limit);
    }
    update_job(job_id, |status| {
        status.total_chapters = pending.len();
    });

    let mut downloaded = 0usize;
    let mut adopted = 0usize;
    let mut skipped = 0usize;
    let mut consecutive_failures = 0usize;
    let mut fetch_error: Option<FeroError> = None;

    for (position, chapter_position) in pending.iter().enumerate() {
        // Zwischen Kapiteln, nicht zwischen Seiten: eine halbe CBZ waere
        // schlimmer als ein Kapitel zu viel, und das Kapitel ist die Einheit,
        // die der naechste Lauf ohnehin wieder aufgreift.
        if cancel_requested(job_id) {
            break;
        }
        update_job(job_id, |status| {
            status.current_chapter = position + 1;
            status.current_page = 0;
            status.total_pages = 0;
        });

        let chapter_ref = {
            let chapter = &subscription.known_chapters[*chapter_position];
            MangaChapterRef {
                title: chapter.title.clone(),
                url: chapter.url.clone(),
                volume: chapter.volume.clone(),
                number: manga::extract_chapter_number(&chapter.title)
                    .or_else(|| manga::extract_chapter_number(&chapter.url)),
            }
        };

        let index = subscription.known_chapters[*chapter_position].index;

        // Vor dem Laden nachsehen, ob die Datei am Ziel schon liegt. Die
        // Abo-Datei ist nicht die einzige Wahrheit: nach einer Uebernahme, nach
        // einem verlorenen Datenordner oder wenn ein zweiter Rechner auf
        // dieselbe Freigabe schreibt, weiss der Werkordner mehr. Ein Kapitel
        // neu zu ziehen, das danebenliegt, kostet Dutzende Bildabrufe fuer
        // nichts.
        if let Some(page_count) =
            cbz_page_count(&series_dir.join(chapter_file_name(subscription, &chapter_ref, index)))
        {
            let chapter = &mut subscription.known_chapters[*chapter_position];
            chapter.downloaded_at_unix = Some(unix_now());
            chapter.placeholder = false;
            chapter.page_count = Some(page_count);
            adopted += 1;
            save_subscription(&ws.store, subscription)?;
            continue;
        }

        match download_chapter(
            &parent,
            client,
            source.as_ref(),
            subscription,
            &chapter_ref,
            index,
            job_id,
        ) {
            Ok(page_count) => {
                consecutive_failures = 0;
                let chapter = &mut subscription.known_chapters[*chapter_position];
                chapter.downloaded_at_unix = Some(unix_now());
                chapter.placeholder = false;
                chapter.page_count = Some(page_count);
                downloaded += 1;
                update_job(job_id, |status| status.downloaded += 1);
                // Persist after every chapter so an aborted run resumes here.
                save_subscription(&ws.store, subscription)?;
            }
            Err(error) => {
                consecutive_failures += 1;
                debug_log(&format!(
                    "manga chapter {}/{}: FEHLER '{}' — {error}",
                    position + 1,
                    pending.len(),
                    chapter_ref.title
                ));
                // Many failures in a row → the site is down or blocking; stop
                // and keep what was fetched so the next run resumes.
                if consecutive_failures >= MAX_CONSECUTIVE_CHAPTER_FAILURES {
                    fetch_error = Some(error);
                    break;
                }
                // A single bad chapter is left undownloaded rather than
                // written as a broken archive; the next check retries it.
                skipped += 1;
            }
        }
    }

    if skipped > 0 {
        debug_log(&format!(
            "manga check: '{}' — {skipped} Kapitel übersprungen (Wiederholung beim nächsten Check)",
            subscription.title
        ));
    }
    if adopted > 0 {
        debug_log(&format!(
            "manga check: '{}' — {adopted} Kapitel lagen bereits im Werkordner \
             und wurden übernommen statt neu geladen.",
            subscription.title
        ));
    }
    // Erst hier, nicht vor dem Herunterladen: „abgeschlossen" verlangt, dass
    // nichts mehr aussteht, und das entscheidet sich in der Schleife darueber.
    refresh_series_status(client, subscription, info.completed_hint);
    refresh_manifest(&series_dir, subscription);

    if let Some(error) = fetch_error {
        return Err(error);
    }
    Ok(downloaded)
}

/// Refreshes the life-cycle status from a comic database, at most weekly.
///
/// Best effort and silent: a status is a nice-to-have, and a database that is
/// slow today must not stop chapters from being fetched. The timestamp is
/// written even on failure, so an unreachable source is retried weekly rather
/// than on every single run.
///
/// `source_completed` is what the scanlation site itself said. It is passed in
/// rather than read here because it comes from the series page that was
/// already fetched for the chapter list — asking twice would be a second
/// request for an answer already in hand.
fn refresh_series_status(
    client: &PoliteClient,
    subscription: &mut Subscription,
    source_completed: Option<bool>,
) {
    // Eine Handentscheidung ist kein Zwischenspeicher: da ist nichts
    // aufzufrischen, und ueberschreiben duerfte sie ohnehin niemand.
    if subscription.status_override.is_some() {
        return;
    }
    if !status::is_due(subscription.status_checked_at, unix_now()) {
        return;
    }
    subscription.status_checked_at = Some(unix_now());

    let found = match subscription.status_source_url.as_deref() {
        Some(url) => manga_status::fetch_by_url(client, url),
        None => manga_status::search(&subscription.title),
    };

    let publication = match found {
        Ok(Some(entry)) => {
            // Den gefundenen Eintrag festnageln: dann steht im Abo sichtbar,
            // worauf sich der Status stuetzt — und wer ihn fuer falsch haelt,
            // kann ihn austauschen, statt gegen eine unsichtbare Suche zu
            // argumentieren.
            if subscription.status_source_url.is_none() {
                if let Some(url) = entry.url.as_deref() {
                    debug_log(&format!(
                        "manga status: '{}' zugeordnet zu '{}' ({url})",
                        subscription.title,
                        entry.title.as_deref().unwrap_or("ohne Titel"),
                    ));
                    subscription.status_source_url = Some(url.to_string());
                }
            }
            if entry.mal_url.is_some() {
                subscription.mal_url = entry.mal_url.clone();
            }
            entry.publication
        }
        Ok(None) => {
            debug_log(&format!(
                "manga status: '{}' — kein sicherer Treffer. Fuer einen \
                 verlaesslichen Status den AniList- oder MyAnimeList-Link im \
                 Abo eintragen.",
                subscription.title
            ));
            OriginalStatus::Unknown
        }
        Err(error) => {
            // Nichts ueberschreiben: ein Netzfehler ist keine Aussage ueber
            // die Serie, und der zuletzt ermittelte Status bleibt richtiger
            // als „unbekannt".
            debug_log(&format!("manga status: '{}' — {error}", subscription.title));
            return;
        }
    };

    let facts = ComicStatusFacts {
        publication,
        source_completed,
    };
    let resolved = status::resolve_comic(&facts, subscription.pending_count());
    if resolved != subscription.series_status {
        debug_log(&format!(
            "manga status: '{}' {:?} -> {:?}",
            subscription.title, subscription.series_status, resolved
        ));
    }
    subscription.series_status = resolved;
}

/// Downloads one chapter's pages and writes the CBZ. Returns the page count.
fn download_chapter(
    delivery_parent: &Path,
    client: &PoliteClient,
    source: &dyn manga::MangaSource,
    subscription: &Subscription,
    chapter: &MangaChapterRef,
    index: u32,
    job_id: &str,
) -> Result<u32> {
    let pages = source.fetch_chapter_pages(client, chapter)?;
    if pages.is_empty() {
        return Err(FeroError::ExternalApi(format!(
            "Kapitel ohne Seiten: {}",
            chapter.url
        )));
    }
    update_job(job_id, |status| status.total_pages = pages.len());

    let mut images = Vec::with_capacity(pages.len());
    for (position, page) in pages.iter().enumerate() {
        update_job(job_id, |status| status.current_page = position + 1);

        let bytes = client.get_image(&page.url, page.referer.as_deref())?;
        if bytes.len() > MAX_PAGE_BYTES {
            return Err(FeroError::ExternalApi(format!(
                "Seite {} ist unerwartet groß ({} MB): {}",
                position + 1,
                bytes.len() / (1024 * 1024),
                page.url
            )));
        }
        // An HTML error page served instead of an image would otherwise end up
        // in the archive as a corrupt "page".
        let Some(media_type) = detect_image_media_type(&bytes) else {
            return Err(FeroError::ExternalApi(format!(
                "Seite {} ist kein gültiges Bild: {}",
                position + 1,
                page.url
            )));
        };
        images.push(CbzPage {
            media_type: media_type.to_string(),
            bytes,
        });
    }

    let series_dir = manga_folder(delivery_parent, subscription);
    let file_name = chapter_file_name(subscription, chapter, index);
    let meta = CbzMeta {
        series: subscription.title.clone(),
        title: Some(chapter.title.clone()),
        number: chapter.number.clone(),
        volume: chapter.volume.clone(),
        summary: subscription.description.clone(),
        writer: subscription.author.clone(),
        penciller: subscription.artist.clone(),
        genres: subscription.genres.clone(),
        web: Some(chapter.url.clone()),
        language: None,
        right_to_left: subscription.right_to_left,
    };
    let page_count = images.len() as u32;
    write_cbz(&series_dir.join(&file_name), &meta, &images)?;
    record_delivery(&series_dir, subscription, &file_name, index)?;
    Ok(page_count)
}

/// Fills metadata gaps from an AniList manga lookup. Best effort.
fn enrich_from_anilist(subscription: &mut Subscription) {
    let has_gaps = subscription.description.is_none()
        || subscription.genres.is_empty()
        || subscription.cover_url.is_none();
    if subscription.anilist_id.is_some() || !has_gaps {
        return;
    }
    let anilist = crate::api::anilist::AniListClient::default();
    let Ok(Some(manga)) = anilist.search_manga(&subscription.title) else {
        return;
    };
    subscription.anilist_id = Some(manga.anilist_id);
    subscription.anilist_url = manga.anilist_url.clone();
    subscription.mal_url = manga.mal_id.map(crate::api::anilist::myanimelist_url);
    if subscription.description.is_none() {
        subscription.description = manga.description.clone();
    }
    if subscription.genres.is_empty() {
        subscription.genres = manga.genres.clone();
    }
    if subscription.tags.is_empty() {
        subscription.tags = manga.tags.clone();
    }
    if subscription.cover_url.is_none() {
        subscription.cover_url = manga.cover_url.clone();
    }
    if subscription.rating_external.is_none() {
        // AniList scores are 0–100; the field holds a 0–5 rating.
        subscription.rating_external = manga.average_score.map(|score| score / 20.0);
    }
}

/// Downloads the series cover into the series folder if not present.
///
/// Non-fatal by design: a missing cover must never block chapter downloads.
fn ensure_cover(client: &PoliteClient, subscription: &Subscription, series_dir: &Path) -> bool {
    let Some(cover_url) = subscription.cover_url.as_deref() else {
        return false;
    };
    if manga_cover_path(series_dir).is_some() {
        return false;
    }
    let Ok(bytes) = client.get_image(cover_url, Some(&subscription.url)) else {
        return false;
    };
    let Some(media_type) = detect_image_media_type(&bytes) else {
        return false;
    };
    let file_name = match media_type {
        "image/png" => "cover.png",
        "image/webp" => "cover.webp",
        "image/gif" => "cover.gif",
        _ => "cover.jpg",
    };
    fs::write(series_dir.join(file_name), bytes).is_ok()
}

// ---------------------------------------------------------------------------
// Vault layout
// ---------------------------------------------------------------------------

/// Returns the directory name for a subscription's files.
///
/// Prefers the name pinned at subscribe time; the result is always a single,
/// safe segment — never empty, never `.`/`..` — so joining it can only descend
/// one level.  Without that guarantee a series whose scraped title sanitizes
/// to nothing would resolve to the parent directory, and a per-series delete
/// would take the whole library with it.
/// Folder name for a series, exported so the reveal route can find it without
/// duplicating the sanitising rules.
pub(crate) fn folder_name(subscription: &Subscription) -> String {
    if let Some(pinned) = subscription.folder_name.as_deref() {
        let sanitized = sanitize_path_segment(pinned);
        if !sanitized.is_empty() {
            return sanitized;
        }
    }
    safe_folder_segment(
        &subscription.title,
        &format!("manga_{}", subscription.id.trim()),
    )
}

/// Picks a folder name that no other subscription already uses.
fn unique_manga_folder_name(ws: &Workspace, subscription: &Subscription) -> String {
    let base = folder_name(subscription);
    let taken = list_subscriptions(&ws.store)
        .unwrap_or_default()
        .iter()
        .filter(|other| other.id != subscription.id)
        .any(|other| folder_name(other) == base);

    if taken {
        format!("{base} ({})", subscription.id)
    } else {
        base
    }
}

/// Vault location of a series' files.
/// Directory holding one series' files. The parent comes from the target
/// chain and is resolved by the caller, so this stays a pure join.
fn manga_folder(delivery_parent: &Path, subscription: &Subscription) -> PathBuf {
    delivery_parent.join(folder_name(subscription))
}

/// Trash location mirroring [`manga_folder`].
/// Where a deleted series' files are parked, next to where they were
/// delivered — with per-work targets there is no single library root any more.
fn manga_trash_folder(delivery_parent: &Path, subscription: &Subscription) -> PathBuf {
    delivery_parent
        .join(".trash")
        .join(folder_name(subscription))
}

/// Returns the existing cover file path inside a series folder, if any.
pub(super) fn manga_cover_path(series_dir: &Path) -> Option<PathBuf> {
    MANGA_COVER_NAMES
        .iter()
        .map(|name| series_dir.join(name))
        .find(|path| path.exists())
}

/// Builds the CBZ file name for a chapter.
///
/// The chapter number is zero-padded so a plain file listing sorts in reading
/// order; decimals (`0010.5`) survive because half-chapters are common.  When
/// the source exposes no number the ToC index stands in.
fn chapter_file_name(subscription: &Subscription, chapter: &MangaChapterRef, index: u32) -> String {
    let label = chapter
        .number
        .as_deref()
        .and_then(pad_chapter_number)
        .unwrap_or_else(|| format!("{index:04}"));
    let safe_title = folder_name(subscription);
    sanitize_path_segment(&format!("{safe_title} - Kapitel {label}.cbz"))
}

/// Zero-pads the integer part of a chapter number to four digits.
fn pad_chapter_number(number: &str) -> Option<String> {
    let (whole, fraction) = match number.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (number, None),
    };
    let whole: u32 = whole.parse().ok()?;
    Some(match fraction {
        Some(fraction) if !fraction.is_empty() => format!("{whole:04}.{fraction}"),
        _ => format!("{whole:04}"),
    })
}

/// Writes the `.fero.yaml` sidecar next to a generated CBZ.
///
/// Only called when a CBZ is newly written: re-writing every sidecar on every
/// check would mean thousands of file writes for a long-running series.
/// Records a delivered chapter in the series folder's `fero.info.json`.
///
/// Replaces the former `.fero.yaml` sidecar; descriptive metadata belongs in
/// the CBZ's `ComicInfo.xml`, where any reader can see it.
/// The manifest bookkeeping shared by every write and by the refresh.
fn manifest_bookkeeping(series_dir: &Path, subscription: &Subscription) -> manifest::WorkManifest {
    let mut record = manifest::load_or_new(
        series_dir,
        &subscription.id,
        subscription_kind(subscription),
        &subscription.url,
        &subscription.title,
    );
    record.title = subscription.title.clone();
    // Ins Manifest gehoert die geltende Einschaetzung; „unbekannt" waere fuer
    // eine andere Instanz weniger wert als die Annahme, dass es weitergeht.
    record.status = match subscription.effective_status() {
        SeriesStatus::Unknown => SeriesStatus::Ongoing,
        known => known,
    };
    record.last_check_unix = Some(unix_now());
    record.latest_release_unix = subscription.latest_release_unix;
    record.chapters = subscription
        .known_chapters
        .iter()
        .filter(|chapter| chapter.downloaded_at_unix.is_some())
        .map(|chapter| manifest::ChapterRecord {
            index: chapter.index,
            title: chapter.title.clone(),
            url: Some(chapter.url.clone()),
            downloaded_at_unix: chapter.downloaded_at_unix.unwrap_or_default(),
        })
        .collect();
    record
}

fn record_delivery(
    series_dir: &Path,
    subscription: &Subscription,
    file_name: &str,
    index: u32,
) -> Result<()> {
    let mut record = manifest_bookkeeping(series_dir, subscription);
    record.record_file(file_name, Some((index, index)), unix_now());
    manifest::save(series_dir, &record)
}

/// Keeps the manifest current even when no file was written; see the webnovel
/// counterpart for why.
fn refresh_manifest(series_dir: &Path, subscription: &Subscription) {
    if !series_dir.is_dir() {
        return;
    }
    let record = manifest_bookkeeping(series_dir, subscription);
    if let Err(error) = manifest::save(series_dir, &record) {
        debug_log(&format!("Manifest nicht aktualisiert: {error}"));
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription(title: &str) -> Subscription {
        Subscription::new("https://example.com/manga/x", "mangatown", title)
    }

    #[test]
    fn chapter_files_sort_in_reading_order() {
        let subscription = subscription("Serie");
        let name = |number: Option<&str>, index: u32| {
            chapter_file_name(
                &subscription,
                &MangaChapterRef {
                    title: "t".to_string(),
                    url: "u".to_string(),
                    volume: None,
                    number: number.map(str::to_string),
                },
                index,
            )
        };

        assert_eq!(name(Some("1"), 1), "Serie - Kapitel 0001.cbz");
        assert_eq!(name(Some("10.5"), 11), "Serie - Kapitel 0010.5.cbz");
        assert_eq!(name(Some("3862"), 9), "Serie - Kapitel 3862.cbz");
        // No number from the source → the ToC index keeps files ordered.
        assert_eq!(name(None, 7), "Serie - Kapitel 0007.cbz");

        let mut names = vec![name(Some("10"), 1), name(Some("2"), 2), name(Some("1"), 3)];
        names.sort();
        assert_eq!(
            names,
            vec![
                "Serie - Kapitel 0001.cbz",
                "Serie - Kapitel 0002.cbz",
                "Serie - Kapitel 0010.cbz",
            ]
        );
    }

    #[test]
    fn folder_name_never_escapes_the_media_directory() {
        // A scraped title is attacker-controlled input for a path segment.
        for title in ["..", ".", "", "../../etc", "/absolute"] {
            let mut record = subscription(title);
            record.folder_name = Some(title.to_string());
            let name = folder_name(&record);
            assert!(
                !name.is_empty(),
                "title {title:?} produced an empty segment"
            );
            assert!(name != "." && name != "..", "title {title:?} escaped");
            assert!(!name.contains('/'), "title {title:?} kept a separator");
        }
    }

    #[test]
    fn trash_folder_mirrors_the_series_folder() {
        let parent = PathBuf::from("/ziel/Manga");
        let record = subscription("Serie");

        let series = manga_folder(&parent, &record);
        let trashed = manga_trash_folder(&parent, &record);
        assert!(series.ends_with("Manga/Serie"));
        assert!(trashed.ends_with("Manga/.trash/Serie"));
    }

    #[test]
    fn pads_only_parseable_numbers() {
        assert_eq!(pad_chapter_number("7").as_deref(), Some("0007"));
        assert_eq!(pad_chapter_number("10.5").as_deref(), Some("0010.5"));
        assert_eq!(pad_chapter_number("extra"), None);
    }

    #[test]
    fn series_info_never_overwrites_existing_metadata() {
        let mut record = subscription("Serie");
        record.author = Some("Vorhanden".to_string());
        record.genres = vec!["Drama".to_string()];

        apply_series_info(
            &mut record,
            &MangaInfo {
                author: Some("Neu".to_string()),
                artist: Some("Zeichner".to_string()),
                genres: vec!["Action".to_string()],
                completed_hint: Some(true),
                ..MangaInfo::default()
            },
        );

        // Existing values win; empty ones get filled.
        assert_eq!(record.author.as_deref(), Some("Vorhanden"));
        assert_eq!(record.genres, vec!["Drama".to_string()]);
        assert_eq!(record.artist.as_deref(), Some("Zeichner"));
        assert!(record.completed);
    }

    /// Der Fall, der den Handschalter noetig gemacht hat: eine Quellseite darf
    /// die Entscheidung des Nutzers nicht bei jedem Prueflauf zurueckdrehen.
    #[test]
    fn a_hand_set_status_survives_what_the_source_page_claims() {
        let mut record = subscription("Serie");
        record.status_override = Some(SeriesStatus::Ongoing);

        apply_series_info(
            &mut record,
            &MangaInfo {
                completed_hint: Some(true),
                ..MangaInfo::default()
            },
        );

        assert!(!record.completed);
        assert_eq!(record.effective_status(), SeriesStatus::Ongoing);
    }
}
