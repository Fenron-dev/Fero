//! Webnovel subscriptions: subscribe, check, download, package as EPUB.
//!
//! Mirrors `manga.rs` — one file per media kind, so adding a kind means adding
//! a file rather than growing a shared one.

use super::*;
use crate::api::novel::status as novel_status;
use crate::core::batching;
use crate::core::status::{self, SeriesStatus};

// Das Makro greift auf das private error-Feld zu und muss deshalb dort stehen,
// wo die Strukturen definiert sind.
impl_outcome!(
    RebuildBlocksResponse,
    WebnovelListResponse,
    WebnovelSubscribeResponse,
    WebnovelTrashResponse,
    WebnovelCheckResponse,
    WebnovelJobResponse,
    WebnovelBlocklistResponse,
);

// ---------------------------------------------------------------------------
// Webnovel subscriptions
// ---------------------------------------------------------------------------

/// Directory inside a novel's vault folder that caches downloaded chapters.
const WEBNOVEL_CHAPTER_CACHE_DIR: &str = ".chapters";

/// Moves a chapter cache left in the work folder into Fero's data directory.
///
/// Best effort and silent: the cache is rebuildable by re-scraping, so a failed
/// move must never stop a run. Only the first run after the change finds
/// anything to do.
fn migrate_chapter_cache(novel_dir: &Path, cache_dir: &Path) {
    let old = novel_dir.join(WEBNOVEL_CHAPTER_CACHE_DIR);
    if !old.is_dir() || cache_dir.exists() {
        return;
    }
    if let Some(parent) = cache_dir.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if fs::rename(&old, cache_dir).is_ok() {
        debug_log(&format!(
            "Kapitel-Cache aus {} in den Datenordner verschoben.",
            old.display()
        ));
    }
}

/// Registry of running/finished webnovel check jobs, keyed by job id.
static WEBNOVEL_JOBS: LazyLock<Mutex<HashMap<String, WebnovelJobStatus>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Guards against overlapping check runs (startup, interval, manual).
static WEBNOVEL_CHECK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Monotonic part of generated job ids.
static WEBNOVEL_JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Progress snapshot of one check job, polled by the frontend.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelJobStatus {
    /// `running`, `done`, or `failed`.
    state: String,
    /// Title of the subscription currently being processed.
    novel_title: String,
    /// 1-based position of the chapter currently being fetched.
    current_chapter: usize,
    /// Number of chapters queued for download in the current subscription.
    total_chapters: usize,
    /// Chapters downloaded so far across the whole job.
    downloaded: usize,
    /// Final summary or error message once the job is terminal.
    ///
    /// Visible to the browser module, which reports the fetch state into the
    /// running job so the UI can show what the foreign window is doing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) message: Option<String>,
    /// When the job reached a terminal state — drives cleanup of the registry.
    #[serde(skip)]
    finished_at_unix: Option<u64>,
}

impl WebnovelJobStatus {
    fn running() -> Self {
        Self {
            state: "running".to_string(),
            novel_title: String::new(),
            current_chapter: 0,
            total_chapters: 0,
            downloaded: 0,
            message: None,
            finished_at_unix: None,
        }
    }

    /// Whether the job has reached a terminal state.
    fn is_terminal(&self) -> bool {
        self.state != "running"
    }
}

/// How long a finished job stays pollable before it is dropped.
const WEBNOVEL_JOB_RETENTION_SECS: u64 = 10 * 60;

/// Applies a mutation to a job's status under the registry lock.
///
/// Stamps the finish time when the mutation makes the job terminal, so
/// [`prune_webnovel_jobs`] can expire it later.
pub(super) fn update_webnovel_job(job_id: &str, apply: impl FnOnce(&mut WebnovelJobStatus)) {
    if let Ok(mut jobs) = WEBNOVEL_JOBS.lock() {
        if let Some(status) = jobs.get_mut(job_id) {
            apply(status);
            if status.is_terminal() && status.finished_at_unix.is_none() {
                status.finished_at_unix = Some(unix_now());
            }
        }
    }
}

/// Drops finished jobs the frontend can no longer be waiting for.
///
/// Without this the registry only ever grows — every check run leaves one
/// entry behind for the lifetime of the process.
fn prune_webnovel_jobs(jobs: &mut HashMap<String, WebnovelJobStatus>) {
    let now = unix_now();
    jobs.retain(|_, status| match status.finished_at_unix {
        Some(finished) => now.saturating_sub(finished) < WEBNOVEL_JOB_RETENTION_SECS,
        None => true,
    });
}

/// Clears the "check running" flag when a worker thread ends — even if the
/// worker panics, so a crashed job can never wedge future checks.
struct WebnovelCheckActiveGuard;

impl Drop for WebnovelCheckActiveGuard {
    fn drop(&mut self) {
        WEBNOVEL_CHECK_ACTIVE.store(false, Ordering::SeqCst);
    }
}

/// Cached chapter content stored under `<novel>/.chapters/ch_<index>.json`.
///
/// The cache makes complete-EPUB rebuilds purely local and lets an aborted
/// download resume without re-fetching anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedChapter {
    title: String,
    xhtml: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelSubscriptionSummary {
    id: String,
    url: String,
    source: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    goodreads_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    anilist_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rating_external: Option<f32>,
    /// Delivery target set for this subscription alone, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    target_dir: Option<String>,
    /// Life-cycle status as last read from the status source.
    series_status: SeriesStatus,
    /// Whether that status is one the user should be told about unprompted.
    needs_attention: bool,
    /// UNIX timestamp of the last status check.
    #[serde(skip_serializing_if = "Option::is_none")]
    status_checked_at: Option<u64>,
    /// Whether the translation is finished, when the source says so.
    #[serde(skip_serializing_if = "Option::is_none")]
    translation_done: Option<bool>,
    /// Category this subscription belongs to.
    media_kind: MediaKind,
    /// Total chapter cap, when one is set.
    #[serde(skip_serializing_if = "Option::is_none")]
    download_limit: Option<u32>,
    /// Where the files currently live, when anything was delivered.
    #[serde(skip_serializing_if = "Option::is_none")]
    delivered_to: Option<String>,
    /// True when the files sit somewhere else than the target chain points —
    /// staged locally while the share was offline, or the category changed.
    needs_relocation: bool,
    /// Page the status is read from, when one is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    status_source_url: Option<String>,
    /// Whether a cached cover exists in the work folder.
    ///
    /// Only a flag: with per-work targets there is no library-relative path to
    /// hand the frontend, so the cover is fetched through its own endpoint.
    has_cover: bool,
    completed: bool,
    hiatus: bool,
    enabled: bool,
    known_chapters: usize,
    downloaded_chapters: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_check_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

impl WebnovelSubscriptionSummary {
    /// Builds the summary. `work_dir` is `None` when no target is configured
    /// yet — the subscription is still listed, just without a cover.
    fn from_subscription(subscription: &Subscription, ws: &Workspace) -> Self {
        let kind = subscription_kind(subscription);
        let desired = ws.delivery_parent(kind, subscription).ok();
        // Cover und Dateien liegen dort, wo ausgeliefert wurde — nicht dort,
        // wo die Zielkette gerade hinzeigt.
        let current = subscription
            .delivered_to
            .as_ref()
            .map(PathBuf::from)
            .or_else(|| desired.clone());
        let has_cover = current
            .as_ref()
            .map(|parent| load_novel_cover_path(&webnovel_folder(parent, subscription)).is_some())
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
            description: subscription.description.clone(),
            genres: subscription.genres.clone(),
            tags: subscription.tags.clone(),
            goodreads_url: subscription.goodreads_url.clone(),
            anilist_url: subscription.anilist_url.clone(),
            rating_external: subscription.rating_external,
            target_dir: subscription.target_dir.clone(),
            series_status: subscription.series_status,
            needs_attention: subscription.series_status.needs_attention(),
            status_checked_at: subscription.status_checked_at,
            translation_done: subscription.translation_done,
            media_kind: kind,
            download_limit: subscription.download_limit,
            delivered_to: subscription.delivered_to.clone(),
            needs_relocation,
            status_source_url: subscription.status_source_url.clone(),
            has_cover,
            completed: subscription.completed,
            hiatus: subscription.hiatus,
            enabled: subscription.enabled,
            known_chapters: subscription.known_chapters.len(),
            downloaded_chapters: subscription.downloaded_count(),
            last_check_unix: subscription.last_check_unix,
            last_error: subscription.last_error.clone(),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelListResponse {
    subscriptions: Vec<WebnovelSubscriptionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(super) fn build_webnovel_list_response(query: Option<&str>) -> WebnovelListResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let ws = match resolve_workspace(root_override.as_deref()) {
        Ok(ws) => ws,
        Err(message) => {
            return WebnovelListResponse {
                subscriptions: Vec::new(),
                error: Some(message),
            }
        }
    };

    match list_subscriptions(&ws.store) {
        Ok(subscriptions) => WebnovelListResponse {
            subscriptions: subscriptions
                .iter()
                .map(|subscription| {
                    WebnovelSubscriptionSummary::from_subscription(subscription, &ws)
                })
                .collect(),
            error: None,
        },
        Err(error) => WebnovelListResponse {
            subscriptions: Vec::new(),
            error: Some(error.to_string()),
        },
    }
}

/// Persists the configured targets.
///

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelSubscribeRequest {
    url: String,
    /// Category the new subscription belongs to (`webnovel` or `hwebnovel`).
    #[serde(default)]
    media_kind: Option<String>,
    #[serde(default)]
    root: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelSubscribeResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    subscription: Option<WebnovelSubscriptionSummary>,
    /// True when the URL was already subscribed and no new record was created.
    already_subscribed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl WebnovelSubscribeResponse {
    fn error(message: impl Into<String>) -> Self {
        Self {
            subscription: None,
            already_subscribed: false,
            error: Some(message.into()),
        }
    }
}

pub(super) fn build_webnovel_subscribe_response(body: &[u8]) -> WebnovelSubscribeResponse {
    let req: WebnovelSubscribeRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return WebnovelSubscribeResponse::error(format!("Invalid request: {error}")),
    };

    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return WebnovelSubscribeResponse::error(message),
    };

    let url = normalize_url(&req.url);
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return WebnovelSubscribeResponse::error("Bitte eine vollständige URL angeben.");
    }
    if let Some(reason) = blocked_reason(&ws.store, &url) {
        return WebnovelSubscribeResponse::error(reason);
    }

    // Re-subscribing an existing URL returns the current record unchanged.
    let id = crate::core::webnovel::subscription_id(&url);
    match load_subscription(&ws.store, &id) {
        Ok(Some(existing)) => {
            return WebnovelSubscribeResponse {
                subscription: Some(WebnovelSubscriptionSummary::from_subscription(
                    &existing, &ws,
                )),
                already_subscribed: true,
                error: None,
            }
        }
        Ok(None) => {}
        Err(error) => return WebnovelSubscribeResponse::error(error.to_string()),
    }

    let mut client = match PoliteClient::new() {
        Ok(client) => client,
        Err(error) => return WebnovelSubscribeResponse::error(error.to_string()),
    };
    // Subscribing is always user-initiated → whitelisted (Cloudflare /
    // JS-rendered) hosts are fetched through the visible browser window.
    let uses_window = is_webview_routed(&url);
    if uses_window {
        client = client.with_renderer(std::sync::Arc::new(|url: &str| render_page_via_window(url)));
    }
    let source = detect_source(&url);
    debug_log(&format!(
        "subscribe: {url} routed={uses_window} source={}",
        source.id()
    ));
    let info = match source.fetch_novel_info(&client, &url) {
        Ok(info) => {
            debug_log(&format!("subscribe: OK — {} Kapitel", info.chapters.len()));
            info
        }
        Err(error) => {
            debug_log(&format!("subscribe: FEHLER: {error}"));
            if uses_window {
                close_browser_window();
            }
            return WebnovelSubscribeResponse::error(error.to_string());
        }
    };
    if uses_window {
        close_browser_window();
    }

    let mut subscription = Subscription::new(url, source.id(), info.title.clone());
    // Pin the folder now: the title may change upstream later, and two novels
    // can sanitize to the same segment — both would otherwise end up sharing
    // one directory (and one chapter cache).
    subscription.media_kind = req
        .media_kind
        .as_deref()
        .and_then(MediaKind::from_id)
        .filter(|kind| kind.uses_novel_engine());
    subscription.folder_name = Some(unique_novel_folder_name(&ws, &subscription));
    subscription.author = info.author.clone();
    subscription.cover_url = info.cover_url.clone();
    subscription.description = info.description.clone();
    subscription.genres = info.genres.clone();
    subscription.tags = info.tags.clone();
    subscription.completed = info.completed_hint.unwrap_or(false);
    subscription.known_chapters = info
        .chapters
        .iter()
        .enumerate()
        .map(|(position, chapter)| KnownChapter {
            index: (position + 1) as u32,
            title: chapter.title.clone(),
            url: chapter.url.clone(),
            volume: None,
            page_count: None,
            downloaded_at_unix: None,
            placeholder: false,
        })
        .collect();

    match save_subscription(&ws.store, &subscription) {
        Ok(()) => WebnovelSubscribeResponse {
            subscription: Some(WebnovelSubscriptionSummary::from_subscription(
                &subscription,
                &ws,
            )),
            already_subscribed: false,
            error: None,
        },
        Err(error) => WebnovelSubscribeResponse::error(error.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelUnsubscribeRequest {
    id: String,
    /// When false, the novel's vault folder (EPUBs + chapter cache) is removed.
    #[serde(default = "webnovel_default_true")]
    keep_files: bool,
    #[serde(default)]
    root: Option<String>,
}

fn webnovel_default_true() -> bool {
    true
}

pub(super) fn build_webnovel_unsubscribe_response(body: &[u8]) -> SimpleResponse {
    let req: WebnovelUnsubscribeRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return SimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return SimpleResponse::error(message),
    };

    // Soft delete: the record moves to the in-app trash and can be restored.
    let subscription = match trash_subscription(&ws.store, &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return SimpleResponse::error("Abo nicht gefunden."),
        Err(error) => return SimpleResponse::error(error.to_string()),
    };

    if !req.keep_files {
        // Files move into the vault .trash folder (same convention as
        // delete-files) instead of being removed — reversible via restore.
        let parent = match current_parent(&ws, subscription_kind(&subscription), &subscription) {
            Ok(parent) => parent,
            Err(error) => return SimpleResponse::error(error.to_string()),
        };
        let novel_dir = webnovel_folder(&parent, &subscription);
        if novel_dir.exists() {
            let trash_target = webnovel_trash_folder(&parent, &subscription);
            if let Some(parent) = trash_target.parent() {
                fs::create_dir_all(parent).ok();
            }
            if trash_target.exists() {
                fs::remove_dir_all(&trash_target).ok();
            }
            if let Err(error) = fs::rename(&novel_dir, &trash_target) {
                return SimpleResponse::error(format!(
                    "Dateien konnten nicht in den Papierkorb verschoben werden: {error}"
                ));
            }
        }
    }

    SimpleResponse::ok()
}

/// Vault-trash location of a novel's files.
/// Where a deleted novel's files are parked, next to where they were
/// delivered — with per-work targets there is no single library root any more.
fn webnovel_trash_folder(delivery_parent: &Path, subscription: &Subscription) -> PathBuf {
    delivery_parent
        .join(TRASH_DIR)
        .join(novel_folder_name(subscription))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelTrashEntry {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    trashed_at_unix: Option<u64>,
    /// True when the novel's files also sit in the vault trash.
    files_in_trash: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelTrashResponse {
    entries: Vec<WebnovelTrashEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(super) fn build_webnovel_trash_response(query: Option<&str>) -> WebnovelTrashResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let ws = match resolve_workspace(root_override.as_deref()) {
        Ok(ws) => ws,
        Err(message) => {
            return WebnovelTrashResponse {
                entries: Vec::new(),
                error: Some(message),
            }
        }
    };
    match list_trashed_subscriptions(&ws.store) {
        Ok(trashed) => WebnovelTrashResponse {
            entries: trashed
                .iter()
                .map(|subscription| WebnovelTrashEntry {
                    id: subscription.id.clone(),
                    title: subscription.title.clone(),
                    trashed_at_unix: subscription.trashed_at_unix,
                    files_in_trash: ws
                        .delivery_parent(MediaKind::Webnovel, subscription)
                        .map(|parent| webnovel_trash_folder(&parent, subscription).exists())
                        .unwrap_or(false),
                })
                .collect(),
            error: None,
        },
        Err(error) => WebnovelTrashResponse {
            entries: Vec::new(),
            error: Some(error.to_string()),
        },
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelTrashActionRequest {
    id: String,
    #[serde(default)]
    root: Option<String>,
}

pub(super) fn build_webnovel_restore_response(body: &[u8]) -> SimpleResponse {
    let req: WebnovelTrashActionRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return SimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return SimpleResponse::error(message),
    };
    let subscription = match restore_subscription(&ws.store, &req.id) {
        Ok(subscription) => subscription,
        Err(error) => return SimpleResponse::error(error.to_string()),
    };
    // Bring trashed files back, if any.
    let parent = match current_parent(&ws, subscription_kind(&subscription), &subscription) {
        Ok(parent) => parent,
        Err(error) => return SimpleResponse::error(error.to_string()),
    };
    let trash_source = webnovel_trash_folder(&parent, &subscription);
    if trash_source.exists() {
        let target = webnovel_folder(&parent, &subscription);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).ok();
        }
        if !target.exists() {
            fs::rename(&trash_source, &target).ok();
        }
    }
    SimpleResponse::ok()
}

pub(super) fn build_webnovel_purge_response(body: &[u8]) -> SimpleResponse {
    let req: WebnovelTrashActionRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return SimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return SimpleResponse::error(message),
    };
    // Need the record before purging it to find its trashed folder.
    let trashed = list_trashed_subscriptions(&ws.store)
        .ok()
        .and_then(|entries| {
            entries
                .into_iter()
                .find(|subscription| subscription.id == req.id)
        });
    if let Err(error) = purge_trashed_subscription(&ws.store, &req.id) {
        return SimpleResponse::error(error.to_string());
    }
    if let Some(subscription) = trashed {
        let Ok(parent) = current_parent(&ws, subscription_kind(&subscription), &subscription)
        else {
            return SimpleResponse::error(
                "Kein Zielordner festgelegt — die Dateien lassen sich nicht finden.",
            );
        };
        let folder = webnovel_trash_folder(&parent, &subscription);
        if folder.exists() {
            fs::remove_dir_all(&folder).ok();
        }
    }
    SimpleResponse::ok()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelUpdateRequest {
    id: String,
    /// New delivery target for this one subscription.
    ///
    /// Absent means "leave as is"; clearing needs `clearTargetDir`, because a
    /// missing field and an explicit null are indistinguishable here and an
    /// unrelated update must never wipe the target.
    #[serde(default)]
    target_dir: Option<String>,
    #[serde(default)]
    clear_target_dir: bool,
    /// Page the life-cycle status is read from (a NovelUpdates series page).
    ///
    /// Empty string clears it — unlike the target there is no separate flag,
    /// because an empty URL is meaningless and unambiguous.
    #[serde(default)]
    status_source_url: Option<String>,
    /// New total chapter cap; zero lifts the limit.
    #[serde(default)]
    download_limit: Option<u32>,
    /// New category (`webnovel` or `hwebnovel`).
    #[serde(default)]
    media_kind: Option<String>,
    #[serde(default)]
    completed: Option<bool>,
    #[serde(default)]
    hiatus: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    root: Option<String>,
}

pub(super) fn build_webnovel_update_response(body: &[u8]) -> SimpleResponse {
    let req: WebnovelUpdateRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return SimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return SimpleResponse::error(message),
    };

    let mut subscription = match load_subscription(&ws.store, &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return SimpleResponse::error("Abo nicht gefunden."),
        Err(error) => return SimpleResponse::error(error.to_string()),
    };

    if let Some(completed) = req.completed {
        subscription.completed = completed;
    }
    if let Some(hiatus) = req.hiatus {
        subscription.hiatus = hiatus;
    }
    if let Some(enabled) = req.enabled {
        subscription.enabled = enabled;
    }
    if req.clear_target_dir {
        subscription.target_dir = None;
    } else if let Some(target) = req.target_dir {
        subscription.target_dir = Some(target);
    }
    if let Some(url) = req.status_source_url {
        let trimmed = url.trim();
        subscription.status_source_url = (!trimmed.is_empty()).then(|| trimmed.to_string());
        // A new source has to be read before it means anything.
        subscription.status_checked_at = None;
    }
    if let Some(limit) = req.download_limit {
        subscription.download_limit = (limit > 0).then_some(limit);
    }
    if let Some(kind) = req.media_kind.as_deref().and_then(MediaKind::from_id) {
        if kind.uses_novel_engine() {
            // Die Kategorie bestimmt das Ziel; die Dateien liegen weiter am
            // alten Ort, bis der Nutzer sie ausdruecklich verschiebt.
            subscription.media_kind = Some(kind);
        }
    }

    match save_subscription(&ws.store, &subscription) {
        Ok(()) => SimpleResponse::ok(),
        Err(error) => SimpleResponse::error(error.to_string()),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelCheckRequest {
    /// Check a single subscription; omitted = all enabled, non-completed ones.
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    root: Option<String>,
    #[serde(default = "webnovel_default_true")]
    build_complete: bool,
    #[serde(default = "webnovel_default_true")]
    build_batch: bool,
    /// Per-host request delay in milliseconds (clamped to 500–5000).
    #[serde(default)]
    delay_ms: Option<u64>,
    /// Goodreads metadata mode: `off`, `fill` (default), or `override`.
    #[serde(default)]
    goodreads_mode: Option<String>,
    /// True when the user started the check by hand — required to route
    /// whitelisted hosts through the (visible) browser window.
    #[serde(default)]
    manual: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelCheckResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    job_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Options carried into the worker thread for one check run.
struct WebnovelCheckOptions {
    only_id: Option<String>,
    build_complete: bool,
    build_batch: bool,
    delay_ms: Option<u64>,
    goodreads_mode: GoodreadsMode,
    manual: bool,
}

/// How Goodreads results are merged into a subscription's metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GoodreadsMode {
    /// No Goodreads lookups at all.
    Off,
    /// Fill only fields the source site left empty (default).
    Fill,
    /// Goodreads wins over source-site metadata.
    Override,
}

impl GoodreadsMode {
    fn parse(value: Option<&str>) -> Self {
        match value.unwrap_or("fill") {
            "off" => Self::Off,
            "override" => Self::Override,
            _ => Self::Fill,
        }
    }
}

pub(super) fn build_webnovel_check_response(body: &[u8]) -> WebnovelCheckResponse {
    let req: WebnovelCheckRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return WebnovelCheckResponse {
                job_id: None,
                error: Some(format!("Invalid request: {error}")),
            }
        }
    };

    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => {
            return WebnovelCheckResponse {
                job_id: None,
                error: Some(message),
            }
        }
    };

    // Reject overlapping runs; the flag is released by the worker's guard.
    if WEBNOVEL_CHECK_ACTIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return WebnovelCheckResponse {
            job_id: None,
            error: Some("Eine Prüfung läuft bereits.".to_string()),
        };
    }

    let job_id = format!(
        "job-{}-{}",
        unix_now(),
        WEBNOVEL_JOB_COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    if let Ok(mut jobs) = WEBNOVEL_JOBS.lock() {
        prune_webnovel_jobs(&mut jobs);
        jobs.insert(job_id.clone(), WebnovelJobStatus::running());
    }

    let options = WebnovelCheckOptions {
        only_id: req.id,
        build_complete: req.build_complete,
        build_batch: req.build_batch,
        delay_ms: req.delay_ms,
        goodreads_mode: GoodreadsMode::parse(req.goodreads_mode.as_deref()),
        manual: req.manual,
    };
    spawn_check(ws, options, job_id.clone());

    WebnovelCheckResponse {
        job_id: Some(job_id),
        error: None,
    }
}

/// Starts a check on a worker thread and records its outcome in the job.
///
/// Shared with the scheduler: a timed run must behave exactly like a clicked
/// one, including the guard against overlapping runs.
fn spawn_check(ws: Workspace, options: WebnovelCheckOptions, job_id: String) {
    std::thread::spawn(move || {
        let _active = WebnovelCheckActiveGuard;
        let outcome = run_webnovel_check(&ws, &options, &job_id);
        update_webnovel_job(&job_id, |status| match &outcome {
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
/// Returns `false` when a run is already in flight — the timer then simply
/// skips this tick rather than queueing, because a check that takes longer than
/// the interval would otherwise pile up runs behind each other.
pub(super) fn start_scheduled_check() -> bool {
    let Ok(ws) = resolve_workspace(None) else {
        return false;
    };
    if WEBNOVEL_CHECK_ACTIVE
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }

    let job_id = format!(
        "job-{}-{}",
        unix_now(),
        WEBNOVEL_JOB_COUNTER.fetch_add(1, Ordering::SeqCst)
    );
    if let Ok(mut jobs) = WEBNOVEL_JOBS.lock() {
        prune_webnovel_jobs(&mut jobs);
        jobs.insert(job_id.clone(), WebnovelJobStatus::running());
    }
    spawn_check(
        ws,
        WebnovelCheckOptions {
            only_id: None,
            build_complete: true,
            build_batch: true,
            delay_ms: None,
            goodreads_mode: GoodreadsMode::Fill,
            manual: false,
        },
        job_id,
    );
    true
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelJobResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<WebnovelJobStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(super) fn build_webnovel_job_response(query: Option<&str>) -> WebnovelJobResponse {
    let job_id = query.and_then(|q| extract_query_value(q, "job_id"));
    let Some(job_id) = job_id else {
        return WebnovelJobResponse {
            status: None,
            error: Some("missing job_id".to_string()),
        };
    };

    let Ok(mut jobs) = WEBNOVEL_JOBS.lock() else {
        return WebnovelJobResponse {
            status: None,
            error: Some("job registry unavailable".to_string()),
        };
    };

    match jobs.get(&job_id).cloned() {
        Some(status) => {
            // Terminal jobs are handed out once and then evicted.
            if status.state != "running" {
                jobs.remove(&job_id);
            }
            WebnovelJobResponse {
                status: Some(status),
                error: None,
            }
        }
        None => WebnovelJobResponse {
            status: None,
            error: Some("Job nicht gefunden.".to_string()),
        },
    }
}

/// Runs one check job over the selected subscriptions.
///
/// Per-subscription failures are recorded in that subscription's `last_error`
/// and the run continues; only infrastructure failures (store unwritable)
/// abort the whole job.
fn run_webnovel_check(
    ws: &Workspace,
    options: &WebnovelCheckOptions,
    job_id: &str,
) -> Result<String> {
    let system_dir = &ws.store;
    let all = list_subscriptions(system_dir)?;
    let selected: Vec<Subscription> = match options.only_id.as_deref() {
        Some(id) => all
            .into_iter()
            .filter(|subscription| subscription.id == id)
            .collect(),
        None => all
            .into_iter()
            .filter(|subscription| {
                subscription.enabled && !subscription.completed && !subscription.hiatus
            })
            .collect(),
    };

    if selected.is_empty() {
        return Ok("Keine passenden Abonnements.".to_string());
    }

    let mut client = match options.delay_ms {
        Some(delay_ms) => PoliteClient::with_delay_ms(delay_ms)?,
        None => PoliteClient::new()?,
    };
    // Manual runs may route whitelisted hosts through the browser window.
    let mut uses_window = false;
    if options.manual {
        client = client.with_renderer(std::sync::Arc::new(|url: &str| render_page_via_window(url)));
        set_current_job_id(Some(job_id.to_string()));
    }
    let mut new_chapters = 0usize;
    let mut failures = 0usize;

    for mut subscription in selected {
        // Whitelisted hosts need the visible browser window and the user's
        // presence — never touch them in automatic (startup/interval) runs.
        if !options.manual && is_webview_routed(&subscription.url) {
            subscription.last_error =
                Some("Nur manuell prüfbar (Sicherheitsprüfung / JavaScript-Seite).".to_string());
            save_subscription(system_dir, &subscription)?;
            continue;
        }
        if is_webview_routed(&subscription.url) {
            uses_window = true;
        }

        update_webnovel_job(job_id, |status| {
            status.novel_title = subscription.title.clone();
            status.current_chapter = 0;
            status.total_chapters = 0;
            status.message = None;
        });

        match check_one_subscription(ws, &client, &mut subscription, options, job_id) {
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

    // Close the browser window once the run that opened it is done.
    if uses_window {
        close_browser_window();
    }
    set_current_job_id(None);

    let mut message = format!("{new_chapters} neue Kapitel geladen.");
    if failures > 0 {
        message.push_str(&format!(" {failures} Abo(s) mit Fehlern."));
    }
    Ok(message)
}

/// Checks one subscription: refresh ToC, download pending chapters, build EPUBs.
/// Category of a subscription handled by this engine.
///
/// Stored records from before the categories default to the plain kind.
fn subscription_kind(subscription: &Subscription) -> MediaKind {
    subscription
        .media_kind
        .filter(|kind| kind.uses_novel_engine())
        .unwrap_or(MediaKind::Webnovel)
}

/// Refreshes the life-cycle status when it is stale, at most weekly.
///
/// Best effort and silent: a status is a nice-to-have, and a NovelUpdates that
/// is behind Cloudflare today must not stop the chapters from being fetched.
/// The timestamp is written even on failure, so a permanently unreachable
/// source is retried weekly rather than on every single run.
fn refresh_series_status(client: &PoliteClient, subscription: &mut Subscription) {
    let Some(url) = status_source_for(subscription) else {
        return;
    };
    if !status::is_due(subscription.status_checked_at, unix_now()) {
        return;
    }

    subscription.status_checked_at = Some(unix_now());
    let facts = match novel_status::fetch_status(client, &url) {
        Ok(facts) => facts,
        Err(error) => {
            debug_log(&format!("status: {} — {error}", subscription.title));
            return;
        }
    };

    let local_last = subscription
        .known_chapters
        .iter()
        .filter(|chapter| chapter.downloaded_at_unix.is_some())
        .map(|chapter| chapter.index)
        .max();
    let resolved = status::resolve(&facts, local_last);
    if resolved != subscription.series_status {
        debug_log(&format!(
            "status: '{}' {:?} -> {:?}",
            subscription.title, subscription.series_status, resolved
        ));
    }
    subscription.series_status = resolved;
    subscription.translation_done = facts.fully_translated;
    subscription.status_source_url = Some(url);
}

/// The page a subscription's status is read from.
///
/// A NovelUpdates subscription is its own status source; for everything else
/// the user has to supply the link, because only they know which NU entry
/// belongs to the translation they are following.
fn status_source_for(subscription: &Subscription) -> Option<String> {
    if let Some(url) = subscription.status_source_url.as_deref() {
        return Some(url.to_string());
    }
    host_of(&subscription.url)
        .filter(|host| host.ends_with("novelupdates.com"))
        .map(|_| subscription.url.clone())
}

fn check_one_subscription(
    ws: &Workspace,
    client: &PoliteClient,
    subscription: &mut Subscription,
    options: &WebnovelCheckOptions,
    job_id: &str,
) -> Result<usize> {
    if let Some(reason) = blocked_reason(&ws.store, &subscription.url) {
        return Err(FeroError::ExternalApi(reason));
    }
    refresh_series_status(client, subscription);
    let source = detect_source(&subscription.url);
    debug_log(&format!(
        "check: '{}' host-routed={} source={}",
        subscription.title,
        is_webview_routed(&subscription.url),
        source.id()
    ));
    let info = match source.fetch_novel_info(client, &subscription.url) {
        Ok(info) => {
            debug_log(&format!(
                "check: fetch_novel_info OK — {} Kapitel",
                info.chapters.len()
            ));
            info
        }
        Err(error) => {
            debug_log(&format!("check: fetch_novel_info FEHLER: {error}"));
            return Err(error);
        }
    };

    // Fill metadata gaps and pick up a "finished" flag from the source.
    if subscription.author.is_none() {
        subscription.author = info.author.clone();
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
    if info.completed_hint == Some(true) {
        subscription.completed = true;
    }

    // Metadata enrichment order: source site → Goodreads (per mode) →
    // AniList NOVEL lookup for any gaps that remain.
    enrich_from_goodreads(client, subscription, options.goodreads_mode);
    enrich_from_anilist(subscription);

    // Diff the ToC against known chapters by normalized URL. Known chapters
    // that vanished upstream are kept — local content is never discarded.
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
    for chapter in &info.chapters {
        if !known.contains(&normalize_url(&chapter.url)) {
            subscription.known_chapters.push(KnownChapter {
                index: next_index,
                title: chapter.title.clone(),
                url: chapter.url.clone(),
                volume: None,
                page_count: None,
                downloaded_at_unix: None,
                placeholder: false,
            });
            next_index += 1;
        }
    }

    // Wo die Dateien wirklich liegen, hat Vorrang: waehrend das Ziel offline
    // war, wurde lokal gesammelt, und Nachschub gehoert zu den Geschwistern —
    // nicht auf die gerade wieder erreichbare Freigabe daneben.
    let kind = subscription_kind(subscription);
    let parent = match subscription.delivered_to.as_ref() {
        Some(current) => PathBuf::from(current),
        None => {
            let (parent, staged) = ws.delivery_parent_or_staging(kind, subscription)?;
            if staged {
                update_webnovel_job(job_id, |status| {
                    status.message =
                        Some("Ziel nicht erreichbar — es wird lokal gesammelt.".to_string());
                });
            }
            parent
        }
    };
    subscription.delivered_to = Some(parent.display().to_string());
    let novel_dir = webnovel_folder(&parent, subscription);
    let cache_dir = ws.chapter_cache(&subscription.id)?;
    migrate_chapter_cache(&novel_dir, &cache_dir);
    fs::create_dir_all(&cache_dir).map_err(FeroError::from)?;

    // Fetch the cover once; failures are non-fatal (text matters more).
    //
    // The return value used to trigger a rebuild of the complete EPUB, because
    // the cover is embedded. With immutable blocks that is no longer wanted: a
    // late cover is not worth reshuffling every reading position for.
    ensure_novel_cover(client, subscription, &novel_dir);

    let mut pending: Vec<usize> = subscription
        .known_chapters
        .iter()
        .enumerate()
        // Placeholders are retried here: a chapter that failed once is usually
        // a transient upstream hiccup, and leaving it permanently "done" would
        // bake the error notice into every rebuilt EPUB.
        .filter(|(_, chapter)| chapter.needs_fetch())
        .map(|(position, _)| position)
        .collect();
    // Das Kapitel-Limit deckelt den Gesamtbestand, nicht den einzelnen Lauf:
    // erst reinschnuppern, spaeter hochsetzen, wenn der Titel gefaellt.
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
    update_webnovel_job(job_id, |status| {
        status.total_chapters = pending.len();
    });

    let mut downloaded_indices: Vec<u32> = Vec::new();
    let mut repaired_chapters = 0usize;
    let mut fetch_error: Option<FeroError> = None;
    let mut consecutive_failures = 0usize;
    let mut skipped_chapters = 0usize;
    for (position, chapter_position) in pending.iter().enumerate() {
        update_webnovel_job(job_id, |status| {
            status.current_chapter = position + 1;
        });

        let chapter_ref = {
            let chapter = &subscription.known_chapters[*chapter_position];
            ChapterRef {
                title: chapter.title.clone(),
                url: chapter.url.clone(),
            }
        };
        let was_placeholder = subscription.known_chapters[*chapter_position].placeholder;
        let (content, is_placeholder) = match source.fetch_chapter(client, &chapter_ref) {
            Ok(content) => {
                consecutive_failures = 0;
                debug_log(&format!(
                    "chapter {}/{}: OK '{}' ({} Zeichen) {}",
                    position + 1,
                    pending.len(),
                    chapter_ref.title,
                    content.xhtml.len(),
                    chapter_ref.url
                ));
                (content, false)
            }
            Err(error) => {
                consecutive_failures += 1;
                debug_log(&format!(
                    "chapter {}/{}: FEHLER '{}' — {error} — {}",
                    position + 1,
                    pending.len(),
                    chapter_ref.title,
                    chapter_ref.url
                ));
                // Many failures in a row → likely the site is down or blocking;
                // stop and keep everything fetched so far so the next run
                // resumes exactly here.
                if consecutive_failures >= MAX_CONSECUTIVE_CHAPTER_FAILURES {
                    fetch_error = Some(error);
                    break;
                }
                // A single unparseable chapter (e.g. an author-note "not a
                // chapter" filler) must not block the rest of the novel: store
                // a placeholder so the run continues.  The chapter stays
                // flagged and is retried on the next check.
                skipped_chapters += 1;
                debug_log(&format!(
                    "chapter {}/{}: übersprungen (Platzhalter) '{}'",
                    position + 1,
                    pending.len(),
                    chapter_ref.title
                ));
                (
                    ChapterContent {
                        title: chapter_ref.title.clone(),
                        xhtml: format!(
                            "<p><em>[Dieses Kapitel konnte nicht automatisch geladen \
                             werden. Bitte im Browser öffnen: {}]</em></p>",
                            chapter_ref.url
                        ),
                    },
                    true,
                )
            }
        };

        let chapter = &mut subscription.known_chapters[*chapter_position];
        let cached = CachedChapter {
            title: content.title,
            xhtml: content.xhtml,
        };
        let cache_json = serde_json::to_string(&cached)
            .map_err(|error| FeroError::Serialization(error.to_string()))?;
        fs::write(
            cache_dir.join(chapter_cache_name(chapter.index)),
            cache_json,
        )
        .map_err(FeroError::from)?;
        chapter.downloaded_at_unix = Some(unix_now());
        chapter.placeholder = is_placeholder;
        if was_placeholder && !is_placeholder {
            // A retry that finally succeeded: the cached text changed, so the
            // complete EPUB has to be rebuilt even if nothing else is new.
            repaired_chapters += 1;
        } else if !was_placeholder {
            downloaded_indices.push(chapter.index);
        }

        // Persist after every chapter so an aborted run can resume.
        save_subscription(&ws.store, subscription)?;
        update_webnovel_job(job_id, |status| {
            status.downloaded += 1;
        });
    }

    // Build EPUBs from whatever is cached — also after a partial run.
    if options.build_batch && (!downloaded_indices.is_empty() || repaired_chapters > 0) {
        build_blocks(&novel_dir, &cache_dir, subscription)?;
    }
    // The complete edition is built once, when the serial is actually finished
    // — either because the status source says so or because the user marked it.
    let finished = subscription.series_status.is_settled() || subscription.completed;
    if options.build_complete && finished {
        build_complete_edition(&novel_dir, &cache_dir, subscription)?;
    }

    if skipped_chapters > 0 {
        debug_log(&format!(
            "check: '{}' — {skipped_chapters} Kapitel als Platzhalter übersprungen \
             (werden beim nächsten Check erneut versucht)",
            subscription.title
        ));
    }
    if repaired_chapters > 0 {
        debug_log(&format!(
            "check: '{}' — {repaired_chapters} Platzhalter-Kapitel nachgeladen",
            subscription.title
        ));
    }

    if let Some(error) = fetch_error {
        return Err(error);
    }
    Ok(downloaded_indices.len() + repaired_chapters)
}

/// How many chapters may fail back to back before the run aborts (and resumes
/// on the next check). Below this, a single failing chapter is skipped with a
/// placeholder so one bad entry cannot block an entire novel.
pub(super) const MAX_CONSECUTIVE_CHAPTER_FAILURES: usize = 5;

/// Cover file names probed inside a novel folder, in preference order.
const WEBNOVEL_COVER_NAMES: [&str; 3] = ["cover.jpg", "cover.png", "cover.webp"];

/// Downloads the subscription's cover into the novel folder if not present.
///
/// Non-fatal by design: a missing cover must never block chapter downloads,
/// so all failures are swallowed after basic validation.
/// Returns `true` when a cover file was newly written.
fn ensure_novel_cover(
    client: &PoliteClient,
    subscription: &Subscription,
    novel_dir: &Path,
) -> bool {
    let Some(cover_url) = subscription.cover_url.as_deref() else {
        return false;
    };
    if load_novel_cover_path(novel_dir).is_some() {
        return false;
    }
    let Ok(bytes) = client.get_bytes(cover_url) else {
        return false;
    };
    // Reject anything that is not actually an image (e.g. an error page).
    let Some(media_type) = detect_image_media_type(&bytes) else {
        return false;
    };
    let file_name = match media_type {
        "image/png" => "cover.png",
        "image/webp" => "cover.webp",
        _ => "cover.jpg",
    };
    fs::write(novel_dir.join(file_name), bytes).is_ok()
}

/// Merges Goodreads metadata into the subscription according to `mode`.
///
/// `Fill` touches only empty fields; `Override` lets Goodreads win over the
/// source site (and refreshes on every check so ratings stay current).
/// Best effort — lookup failures are ignored.
fn enrich_from_goodreads(
    client: &PoliteClient,
    subscription: &mut Subscription,
    mode: GoodreadsMode,
) {
    if mode == GoodreadsMode::Off {
        return;
    }
    // In fill mode one successful lookup is enough; override refreshes.
    if mode == GoodreadsMode::Fill && subscription.goodreads_url.is_some() {
        return;
    }
    let Ok(Some(book)) = GoodreadsClient::search_book(client, &subscription.title) else {
        return;
    };

    subscription.goodreads_url = Some(book.url.clone());
    subscription.rating_external = book.average_rating.or(subscription.rating_external);

    let overriding = mode == GoodreadsMode::Override;
    if book.author.is_some() && (overriding || subscription.author.is_none()) {
        subscription.author = book.author.clone();
    }
    if book.description.is_some() && (overriding || subscription.description.is_none()) {
        subscription.description = book.description.clone();
    }
    if !book.genres.is_empty() && (overriding || subscription.genres.is_empty()) {
        subscription.genres = book.genres.clone();
    }
    if book.cover_url.is_some() && (overriding || subscription.cover_url.is_none()) {
        subscription.cover_url = book.cover_url.clone();
    }
}

/// Fills metadata gaps (description, genres, tags, cover, AniList link) from
/// an AniList light-novel lookup. Best effort — errors are ignored.
pub(super) fn enrich_from_anilist(subscription: &mut Subscription) {
    let has_gaps = subscription.description.is_none()
        || subscription.genres.is_empty()
        || subscription.cover_url.is_none();
    if subscription.anilist_id.is_some() || !has_gaps {
        return;
    }
    let anilist = AniListClient::default();
    let Ok(Some(novel)) = anilist.search_novel(&subscription.title) else {
        return;
    };
    subscription.anilist_id = Some(novel.anilist_id);
    subscription.anilist_url = novel.anilist_url.clone();
    if subscription.description.is_none() {
        subscription.description = novel.description.clone();
    }
    if subscription.genres.is_empty() {
        subscription.genres = novel.genres.clone();
    }
    if subscription.tags.is_empty() {
        subscription.tags = novel.tags.clone();
    }
    if subscription.cover_url.is_none() {
        subscription.cover_url = novel.cover_url.clone();
    }
}

/// Returns the existing cover file path inside a novel folder, if any.
pub(super) fn load_novel_cover_path(novel_dir: &Path) -> Option<PathBuf> {
    WEBNOVEL_COVER_NAMES
        .iter()
        .map(|name| novel_dir.join(name))
        .find(|path| path.exists())
}

/// Loads the novel's cover for EPUB embedding, if one is cached.
fn load_novel_cover(novel_dir: &Path) -> Option<EpubCover> {
    let path = load_novel_cover_path(novel_dir)?;
    let bytes = fs::read(&path).ok()?;
    let media_type = detect_image_media_type(&bytes)?;
    Some(EpubCover {
        media_type: media_type.to_string(),
        bytes,
    })
}

/// The novel's folder inside the vault: `Webnovels/<safe title>/`.
/// Returns the directory name for a subscription's files.
///
/// Prefers the name pinned at subscribe time; records written before that
/// field existed fall back to the title. The result is always a single, safe
/// segment — never empty, never `.`/`..` — so joining it can only ever descend
/// one level. Without that guarantee a novel whose scraped title sanitizes to
/// nothing would resolve to the parent directory and a per-novel delete would
/// take the whole library with it.
fn novel_folder_name(subscription: &Subscription) -> String {
    if let Some(pinned) = subscription.folder_name.as_deref() {
        let sanitized = sanitize_path_segment(pinned);
        if !sanitized.is_empty() {
            return sanitized;
        }
    }
    safe_folder_segment(
        &subscription.title,
        &format!("novel_{}", subscription.id.trim()),
    )
}

/// Picks a folder name that no other subscription already uses.
///
/// Distinct novels can sanitize to the same segment ("Re:Zero" / "Re Zero");
/// the loser of that race would otherwise write its EPUBs and chapter cache
/// into the winner's directory. The subscription id disambiguates.
fn unique_novel_folder_name(ws: &Workspace, subscription: &Subscription) -> String {
    let base = novel_folder_name(subscription);
    let taken = list_subscriptions(&ws.store)
        .unwrap_or_default()
        .iter()
        .filter(|other| other.id != subscription.id)
        .any(|other| novel_folder_name(other) == base);

    if taken {
        format!("{base} ({})", subscription.id)
    } else {
        base
    }
}

/// Vault location of a novel's files (EPUBs, cover, chapter cache).
/// Directory holding one novel's files.
///
/// The parent comes from the target chain and is resolved by the caller, so
/// this stays a pure join and the fallible decision happens once per operation
/// instead of once per path.
pub(super) fn webnovel_folder(delivery_parent: &Path, subscription: &Subscription) -> PathBuf {
    delivery_parent.join(novel_folder_name(subscription))
}

/// Cache file name for a chapter index.
fn chapter_cache_name(index: u32) -> String {
    format!("ch_{index:04}.json")
}

/// Loads cached chapters for the given indices, in ascending index order.
fn load_cached_chapters(cache_dir: &Path, indices: &[u32]) -> Result<Vec<EpubChapter>> {
    let mut sorted = indices.to_vec();
    sorted.sort_unstable();

    let mut chapters = Vec::with_capacity(sorted.len());
    for index in sorted {
        let path = cache_dir.join(chapter_cache_name(index));
        let raw = fs::read_to_string(&path).map_err(FeroError::from)?;
        let cached: CachedChapter = serde_json::from_str(&raw)
            .map_err(|error| FeroError::Serialization(error.to_string()))?;
        chapters.push(EpubChapter {
            title: cached.title,
            // Chapters are sanitized at download time already; sanitizing
            // again at build time is defense in depth — a tampered or legacy
            // cache file can still never put scripts into an EPUB.
            xhtml_body: sanitize_to_xhtml(&cached.xhtml),
        });
    }
    Ok(chapters)
}

/// Builds the per-run batch EPUB ("<title> - Kapitel 0045-0063.epub").
///
/// Batch files are never rewritten afterwards, so reading progress in them
/// survives future complete-EPUB rebuilds.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct RebuildBlocksResponse {
    /// Block files written.
    written: usize,
    /// Superseded files from the old naming scheme that were removed.
    removed: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl RebuildBlocksResponse {
    fn error(message: impl Into<String>) -> Self {
        Self {
            written: 0,
            removed: 0,
            error: Some(message.into()),
        }
    }
}

/// Rebuilds a serial's files as immutable blocks, replacing the old scheme.
///
/// Only ever runs on an explicit click. It removes the files delivered under
/// the previous naming — a reading position inside those is lost, and no
/// automatic run should be allowed to decide that for the user.
pub(super) fn build_rebuild_blocks_response(body: &[u8]) -> RebuildBlocksResponse {
    let req: WebnovelTrashActionRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return RebuildBlocksResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return RebuildBlocksResponse::error(message),
    };
    let subscription = match load_subscription(&ws.store, &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return RebuildBlocksResponse::error("Abo nicht gefunden."),
        Err(error) => return RebuildBlocksResponse::error(error.to_string()),
    };
    let parent = match current_parent(&ws, subscription_kind(&subscription), &subscription) {
        Ok(parent) => parent,
        Err(error) => return RebuildBlocksResponse::error(error.to_string()),
    };
    let novel_dir = webnovel_folder(&parent, &subscription);
    let cache_dir = match ws.chapter_cache(&subscription.id) {
        Ok(dir) => dir,
        Err(error) => return RebuildBlocksResponse::error(error.to_string()),
    };
    if !cache_dir.is_dir() {
        return RebuildBlocksResponse::error(
            "Kein Kapitel-Zwischenspeicher vorhanden — bitte zuerst einmal prüfen lassen.",
        );
    }

    let written = match build_blocks(&novel_dir, &cache_dir, &subscription) {
        Ok(written) => written,
        Err(error) => return RebuildBlocksResponse::error(error.to_string()),
    };
    let removed = remove_legacy_files(&novel_dir, &subscription);

    RebuildBlocksResponse {
        written,
        removed,
        error: None,
    }
}

/// Deletes the files delivered under the pre-block naming scheme.
///
/// Matches only what Fero itself wrote for this serial: the single-file edition
/// and the per-run `- Kapitel NNNN-MMMM` files. Anything else in the folder is
/// left alone — Fero does not delete what it did not write.
fn remove_legacy_files(novel_dir: &Path, subscription: &Subscription) -> usize {
    let safe_title = novel_folder_name(subscription);
    let single = format!("{safe_title}.epub");
    let run_prefix = format!("{safe_title} - Kapitel ");

    let Ok(entries) = fs::read_dir(novel_dir) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if name != single && !name.starts_with(&run_prefix) {
            continue;
        }
        if fs::remove_file(entry.path()).is_ok() {
            debug_log(&format!("Blockumbau: {name} entfernt"));
            removed += 1;
        }
    }
    removed
}

/// Writes the block files a serial should have.
///
/// Blocks that already exist are left alone — that is the whole point: a
/// delivered file never changes, so a reading position inside it survives every
/// later run. Only the running `[WIP]` file is rewritten.
fn build_blocks(novel_dir: &Path, cache_dir: &Path, subscription: &Subscription) -> Result<usize> {
    let safe_title = novel_folder_name(subscription);
    let downloaded: Vec<u32> = subscription
        .known_chapters
        .iter()
        .filter(|chapter| chapter.downloaded_at_unix.is_some())
        .map(|chapter| chapter.index)
        .collect();

    let planned = batching::plan(&safe_title, &downloaded, subscription.batch_size());
    let mut written = 0usize;
    let mut stale_wip: Vec<String> = existing_wip_files(novel_dir);

    for file in &planned {
        stale_wip.retain(|name| name != &file.name);
        let path = novel_dir.join(&file.name);
        if path.exists() && !file.is_rewritable() {
            continue;
        }

        let chapters = load_cached_chapters(cache_dir, &file.chapters)?;
        let first = file.chapters.first().copied().unwrap_or(0);
        let last = file.chapters.last().copied().unwrap_or(0);
        let meta = EpubMeta {
            title: format!("{} – Kapitel {first}–{last}", subscription.title),
            author: subscription.author.clone(),
            language: "en".to_string(),
            identifier: format!("{}#block-{first}-{last}", subscription.url),
            description: subscription.description.clone(),
            cover: load_novel_cover(novel_dir),
        };
        write_epub(&path, &meta, &chapters)?;
        record_delivery(novel_dir, subscription, &file.name, Some((first, last)))?;
        written += 1;
    }

    // A WIP file whose window filled up has been replaced by a numbered block;
    // leaving the old one behind would deliver the same chapters twice.
    for name in stale_wip {
        let _ = fs::remove_file(novel_dir.join(&name));
        debug_log(&format!("batch: abgeloeste Datei entfernt — {name}"));
    }

    Ok(written)
}

/// File names in the work folder that are marked as a running edge.
fn existing_wip_files(novel_dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(novel_dir) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.contains("[WIP]") && name.ends_with(".epub"))
        .collect()
}

/// Builds the single-file edition, once, for a serial that is finished.
///
/// Deliberately not rebuilt on every run: a file that changes shifts every
/// reading position inside it. While a serial is still growing, the blocks are
/// the deliverable; the complete edition is what it becomes when it is done.
///
/// The blocks are left in place — someone reading in block three should not
/// find it gone the week the serial finished.
fn build_complete_edition(
    novel_dir: &Path,
    cache_dir: &Path,
    subscription: &Subscription,
) -> Result<bool> {
    let safe_title = novel_folder_name(subscription);
    let file_name = batching::complete_edition_name(&safe_title);
    if novel_dir.join(&file_name).exists() {
        return Ok(false);
    }

    let indices: Vec<u32> = subscription
        .known_chapters
        .iter()
        .filter(|chapter| chapter.downloaded_at_unix.is_some())
        .map(|chapter| chapter.index)
        .collect();
    if indices.is_empty() {
        return Ok(false);
    }

    let chapters = load_cached_chapters(cache_dir, &indices)?;
    let meta = EpubMeta {
        title: subscription.title.clone(),
        author: subscription.author.clone(),
        language: "en".to_string(),
        identifier: subscription.url.clone(),
        description: subscription.description.clone(),
        cover: load_novel_cover(novel_dir),
    };
    write_epub(&novel_dir.join(&file_name), &meta, &chapters)?;
    record_delivery(novel_dir, subscription, &file_name, None)?;
    debug_log(&format!("Gesamtausgabe erzeugt: {file_name}"));
    Ok(true)
}

/// Records a delivered file in the work folder's `fero.info.json`.
///
/// Replaces the former `.fero.yaml` sidecar. Descriptive metadata is not
/// duplicated here — it goes into the EPUB's OPF, where any reader can see it.
pub(super) fn record_delivery(
    work_dir: &Path,
    subscription: &Subscription,
    file_name: &str,
    chapter_range: Option<(u32, u32)>,
) -> Result<()> {
    let mut record = manifest::load_or_new(
        work_dir,
        &subscription.id,
        subscription_kind(subscription),
        &subscription.url,
        &subscription.title,
    );
    record.title = subscription.title.clone();
    // Der ermittelte Status hat Vorrang; die Handschalter greifen nur, solange
    // noch keine Quelle befragt wurde.
    record.status = match subscription.series_status {
        SeriesStatus::Unknown if subscription.completed => SeriesStatus::Completed,
        SeriesStatus::Unknown if subscription.hiatus => SeriesStatus::Hiatus,
        SeriesStatus::Unknown => SeriesStatus::Ongoing,
        known => known,
    };
    record.last_check_unix = Some(unix_now());
    record.record_file(file_name, chapter_range, unix_now());
    record.chapters = subscription
        .known_chapters
        .iter()
        .filter(|chapter| chapter.downloaded_at_unix.is_some())
        .map(|chapter| manifest::ChapterRecord {
            index: chapter.index,
            title: chapter.title.clone(),
            downloaded_at_unix: chapter.downloaded_at_unix.unwrap_or_default(),
        })
        .collect();
    manifest::save(work_dir, &record)
}

/// Opens an external web link in the system browser.
///

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct WebnovelBlocklistResponse {
    entries: Vec<BlocklistEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

pub(super) fn build_webnovel_blocklist_response(query: Option<&str>) -> WebnovelBlocklistResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    match resolve_workspace(root_override.as_deref()) {
        Ok(ws) => WebnovelBlocklistResponse {
            entries: load_blocklist_entries(&ws.store),
            error: None,
        },
        Err(message) => WebnovelBlocklistResponse {
            entries: Vec::new(),
            error: Some(message),
        },
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelBlocklistSaveRequest {
    entries: Vec<BlocklistEntry>,
    #[serde(default)]
    root: Option<String>,
}

pub(super) fn build_webnovel_blocklist_save_response(body: &[u8]) -> SimpleResponse {
    let req: WebnovelBlocklistSaveRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return SimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return SimpleResponse::error(message),
    };
    match save_user_blocklist(&ws.store, &req.entries) {
        Ok(()) => SimpleResponse::ok(),
        Err(error) => SimpleResponse::error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription_with_title(title: &str) -> Subscription {
        Subscription::new("https://example.com/novel", "generic", title)
    }

    /// A novel folder must always sit exactly one level below the media-type
    /// directory — otherwise deleting one novel could delete the library.
    #[test]
    fn novel_folder_never_escapes_media_directory() {
        let base = PathBuf::from("/ziel/Webnovels");

        for title in ["..", ".", "", "///", "../../etc", ".hidden"] {
            let subscription = subscription_with_title(title);

            let folder = webnovel_folder(&base, &subscription);
            assert_eq!(
                folder.parent(),
                Some(base.as_path()),
                "title {title:?} escaped the Webnovel directory"
            );
            assert_ne!(folder, base, "title {title:?} resolved to the parent");

            let trash = webnovel_trash_folder(&base, &subscription);
            let trash_base = base.join(TRASH_DIR);
            assert_eq!(trash.parent(), Some(trash_base.as_path()));
            assert_ne!(trash, trash_base);
        }
    }

    #[test]
    fn novel_folder_prefers_pinned_name() {
        let mut subscription = subscription_with_title("Neuer Titel");
        subscription.folder_name = Some("Alter Titel".to_string());
        assert_eq!(novel_folder_name(&subscription), "Alter Titel");

        // A pinned name is sanitized too — stored records are not trusted.
        subscription.folder_name = Some("..".to_string());
        assert_eq!(novel_folder_name(&subscription), "Neuer Titel");
    }
}
