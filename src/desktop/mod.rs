//! Desktop shell bootstrap for Fero.
//!
//! Split along the lines the concept lays out: `http` owns the response
//! plumbing, `routes` owns the dispatch table, and this module keeps the
//! bootstrap plus the handlers that have not moved into their own modules yet.

mod browser;
mod http;
mod log;
mod routes;

pub(crate) use http::{extract_query_value, impl_outcome, json_response, Outcome};
pub(crate) use log::debug_log;
use log::debug_log_path;
use browser::*;
use log::{build_open_debug_log_response, build_webnovel_debug_log_response};
use routes::handle_request;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::api::anilist::{AniListAnimeMetadata, AniListClient};
use crate::api::goodreads::GoodreadsClient;
use crate::api::novel::{
    clear_browser_session, detect_image_media_type, detect_source, host_of, is_webview_routed,
    sanitize_to_xhtml, set_browser_session, BrowserSession, ChapterContent, ChapterRef,
    PoliteClient,
};
use crate::core::epub::{write_epub, EpubChapter, EpubCover, EpubMeta};
use crate::core::subscription::is_valid_subscription_id;
use crate::core::vault::Vault;
use crate::core::webnovel::{
    blocked_reason, list_subscriptions, list_trashed_subscriptions, load_blocklist_entries,
    load_subscription, normalize_url, purge_trashed_subscription, restore_subscription,
    save_subscription, save_user_blocklist, trash_subscription, unix_now, BlocklistEntry,
    KnownChapter, Subscription,
};
use crate::deliver::manifest;
use crate::deliver::migrate::{migrate_store_from_library, Outcome as MigrationOutcome};
use crate::deliver::targets::{
    resolve_data_dir, resolve_target, DataDir, MediaKind, TargetResolution, TargetSettings,
};
use crate::error::{FeroError, Result};
use serde::{Deserialize, Serialize};
use tauri::Manager;

const PROTOCOL_SCHEME: &str = "fero";

// Die beiden Fenster-Antworten deklarieren ihr Outcome in browser.rs: das Makro
// greift auf das private error-Feld zu und muss deshalb dort stehen, wo die
// Struktur definiert ist.
impl_outcome!(
    AniListSearchResponse,
    SelectFolderResponse,
    OpenExternalResponse,
    WebnovelListResponse,
    TargetsResponse,
    WebnovelSubscribeResponse,
    WebnovelSimpleResponse,
    WebnovelTrashResponse,
    WebnovelCheckResponse,
    WebnovelJobResponse,
    WebnovelBlocklistResponse,
);
const LEGACY_SYSTEM_DIR: &str = ".mediashelf";
/// In-vault trash folder; deleted files move here (reversible) preserving
/// their original relative path.
const TRASH_DIR: &str = ".trash";

const PRIVATE_FILE_MODE: u32 = 0o600;
/// Same idea for the app's state directory.
#[cfg(unix)]
const PRIVATE_DIR_MODE: u32 = 0o700;

/// Restricts a path to the owner (no-op on non-Unix platforms).
///
/// Applied to everything under `~/.fero` that is sensitive: captured
/// login sessions and the debug log (which records every visited URL). Best
/// effort — a failure here must never break the operation that wrote the file.
#[cfg(unix)]
fn restrict_to_owner(path: &Path, is_dir: bool) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if is_dir {
        PRIVATE_DIR_MODE
    } else {
        PRIVATE_FILE_MODE
    };
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
}

#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path, _is_dir: bool) {}

/// Creates `~/.fero` (if needed) with owner-only permissions.
fn ensure_private_dir(dir: &Path) {
    let _ = fs::create_dir_all(dir);
    restrict_to_owner(dir, true);
}

/// Writes `contents` to `path` so that only the owner can read it.
///
/// The permissions are applied to a freshly created file **before** the
/// contents are written, so the secret is never briefly world-readable.
fn write_private_file(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        ensure_private_dir(parent);
    }

    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    restrict_to_owner(path, false);
    file.write_all(contents.as_bytes())
}

const INDEX_HTML: &str = include_str!("../../dist/index.html");
const APP_JS: &str = include_str!("../../dist/app.js");
const STYLES_CSS: &str = include_str!("../../dist/styles.css");

/// Starts the Tauri desktop shell.
pub(crate) fn run() -> Result<()> {
    tauri::Builder::default()
        // Handle custom-scheme requests asynchronously: the whole request is
        // processed on a worker thread and answered via the `responder`, so
        // blocking work (AniList HTTP calls, full-file hashing) never runs on
        // the synchronous webview thread and can no longer freeze the UI. (#7)
        .register_asynchronous_uri_scheme_protocol(
            PROTOCOL_SCHEME,
            |context, request, responder| {
                // The Cloudflare-solve window needs an AppHandle from worker
                // threads; capture it once from the protocol context.
                if APP_HANDLE.set(context.app_handle().clone()).is_ok() {
                    // First request wins the OnceLock — restore persisted login
                    // sessions (NovelUpdates etc.) into the RAM store then, and
                    // move the bookkeeping out of the library if it still sits
                    // there from before the split.
                    restore_webnovel_sessions();
                    migrate_store_if_needed();
                }
                std::thread::spawn(move || {
                    responder.respond(handle_request(&request));
                });
            },
        )
        .run(tauri::generate_context!())
        .map_err(|error| FeroError::AppStartup(error.to_string()))
}

fn build_anilist_search_response(query: Option<&str>) -> AniListSearchResponse {
    let Some(query) = query else {
        return AniListSearchResponse::error("missing query".to_string());
    };
    let Some(title) = extract_query_value(query, "title") else {
        return AniListSearchResponse::error("missing title".to_string());
    };

    let adult = extract_query_value(query, "adult")
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    let limit = extract_query_value(query, "limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8);
    let client = AniListClient::default();

    match client.search_anime_candidates(&title, adult, limit) {
        Ok(results) => AniListSearchResponse {
            metadata: results.first().cloned(),
            results,
            error: None,
        },
        Err(error) => AniListSearchResponse::error(error.to_string()),
    }
}

fn build_select_folder_response() -> SelectFolderResponse {
    let selected = rfd::FileDialog::new().pick_folder();
    SelectFolderResponse {
        path: selected.map(|path| path.display().to_string()),
        error: None,
    }
}

/// Only relevant for installations from before the split; the migration itself
/// is idempotent, so this runs unconditionally at startup and reports what it
/// found into the debug log.
fn migrate_store_if_needed() {
    let Some(data_dir) = resolve_data_dir().path().map(Path::to_path_buf) else {
        debug_log("Migration übersprungen: kein nutzbarer Datenordner.");
        return;
    };
    let Ok(Some(root)) = resolve_vault_root(None) else {
        return;
    };
    let Ok(vault) = Vault::new(root) else {
        return;
    };

    match migrate_store_from_library(&data_dir, &vault.system_dir()) {
        MigrationOutcome::NothingToDo => {}
        MigrationOutcome::Migrated {
            subscriptions,
            from,
        } => debug_log(&format!(
            "{subscriptions} Abos aus {from} in den Datenordner kopiert. \
             Die Originale bleiben vorerst liegen und können nach einer \
             Kontrolle von Hand gelöscht werden."
        )),
        MigrationOutcome::Failed(reason) => debug_log(&format!(
            "Migration der Abos fehlgeschlagen: {reason}. Es wird weiter der \
             alte Ort in der Bibliothek verwendet."
        )),
    }
}

// ---------------------------------------------------------------------------
// Shared helpers (paths, app state, sidecars, query parsing)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct AniListSearchResponse {
    metadata: Option<AniListAnimeMetadata>,
    results: Vec<AniListAnimeMetadata>,
    error: Option<String>,
}

impl AniListSearchResponse {
    fn error(error: String) -> Self {
        Self {
            metadata: None,
            results: Vec::new(),
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SelectFolderResponse {
    path: Option<String>,
    error: Option<String>,
}

/// Longest path segment we generate. Well below the 255-byte limit of APFS,
/// ext4 and NTFS even after multi-byte characters are counted.
const MAX_PATH_SEGMENT_CHARS: usize = 120;

/// Names Windows refuses to use for a file or directory, with or without an
/// extension. Checked case-insensitively so vaults stay portable.
const RESERVED_SEGMENT_NAMES: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Turns arbitrary text into a single, safe path segment.
///
/// Besides replacing separators and control characters this guarantees the
/// result can never act as a path operator: leading dots are stripped (so
/// neither `.`/`..` nor hidden folders can be produced) and the length is
/// capped. An input that carries no usable characters yields an **empty**
/// string — callers that build real paths must treat that as "no name" and
/// substitute their own fallback (see [`safe_folder_segment`]).
pub(crate) fn sanitize_path_segment(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());

    for character in value.chars() {
        let replacement = match character {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => ' ',
            control if control.is_control() => ' ',
            other => other,
        };
        sanitized.push(replacement);
    }

    // Separators became spaces above, so a path like "../../etc" now reads
    // ".. .. etc" — drop the dot-only remnants instead of carrying them into
    // the name.
    let collapsed = sanitized
        .split_whitespace()
        .filter(|part| !part.is_empty() && !part.chars().all(|character| character == '.'))
        .collect::<Vec<_>>()
        .join(" ");

    // Leading dots are what turn a title into a path operator (`..`) or into a
    // hidden entry; trailing dots/spaces are rejected by Windows.
    let trimmed = collapsed
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim();

    // Truncate on a character boundary, then re-trim in case the cut exposed
    // trailing dots or spaces.
    let capped: String = trimmed.chars().take(MAX_PATH_SEGMENT_CHARS).collect();
    let capped = capped.trim_end_matches(['.', ' ']).to_string();

    if RESERVED_SEGMENT_NAMES
        .iter()
        .any(|reserved| capped.eq_ignore_ascii_case(reserved))
    {
        return format!("{capped}_");
    }

    capped
}

/// Returns a safe folder segment for `value`, falling back to `fallback` when
/// the sanitized name would be empty.
///
/// Used wherever a name derived from **remote** data (a scraped novel title)
/// becomes a real directory: an empty segment would silently resolve to the
/// parent directory, which turns a per-novel delete into a delete of the whole
/// library.
pub(crate) fn safe_folder_segment(value: &str, fallback: &str) -> String {
    let sanitized = sanitize_path_segment(value);
    if sanitized.is_empty() {
        return fallback.to_string();
    }
    sanitized
}

pub(crate) fn resolve_vault_root(root_override: Option<&str>) -> Result<Option<PathBuf>> {
    if let Some(root) = normalized_override(root_override) {
        let resolved = resolve_existing_root(root)?;
        if !is_authorized_root(&resolved) {
            return Err(FeroError::InvalidTarget(format!(
                "vault root is not authorized: {}",
                resolved.display()
            )));
        }
        return Ok(Some(resolved));
    }

    if let Ok(root) = env::var("FERO_VAULT_ROOT") {
        if let Some(root) = normalized_override(Some(root.as_str())) {
            return Ok(Some(resolve_existing_root(root)?));
        }
    }

    if let Ok(Some(root)) = load_saved_vault_root() {
        return Ok(Some(resolve_existing_root(root)?));
    }

    for candidate in auto_detect_vault_roots() {
        if looks_like_vault_root(&candidate) {
            return Ok(Some(resolve_existing_root(candidate)?));
        }
    }

    Ok(None)
}

fn normalized_override(root_override: Option<&str>) -> Option<PathBuf> {
    let root = root_override?.trim();
    if root.is_empty() {
        return None;
    }

    Some(PathBuf::from(root))
}

fn load_saved_vault_root() -> Result<Option<PathBuf>> {
    Ok(load_app_state()?.vault_root.map(PathBuf::from))
}

fn resolve_existing_root(root: PathBuf) -> Result<PathBuf> {
    if !root.exists() {
        return Err(FeroError::InvalidTarget(format!(
            "vault root does not exist: {}",
            root.display()
        )));
    }

    fs::canonicalize(&root).map_err(FeroError::from)
}

fn app_state_path() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or_else(|| FeroError::Io("home directory not available".to_string()))?;

    Ok(PathBuf::from(home).join(".fero").join("state.json"))
}

fn load_app_state() -> Result<AppState> {
    let path = app_state_path()?;
    if !path.exists() {
        return Ok(AppState::default());
    }

    let raw = fs::read_to_string(&path).map_err(FeroError::from)?;
    serde_json::from_str(&raw).map_err(|error| FeroError::Serialization(error.to_string()))
}

fn auto_detect_vault_roots() -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(current_exe) = env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            candidates.push(parent.join("Vault"));
            candidates.push(parent.to_path_buf());
        }
    }

    if let Ok(current_dir) = env::current_dir() {
        candidates.push(current_dir.join("Vault"));
        candidates.push(current_dir);
    }

    candidates
}

fn looks_like_vault_root(path: &Path) -> bool {
    path.join("Inbox").is_dir()
        || path.join(".fero").is_dir()
        || path.join(LEGACY_SYSTEM_DIR).is_dir()
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
struct AppState {
    vault_root: Option<String>,
    /// Vault roots the user has explicitly opened or created.
    ///
    /// Requests may carry a `root=` override; it is honoured only for entries
    /// in this list, so a stray or manipulated parameter cannot point the file
    /// endpoints at arbitrary directories.
    #[serde(default)]
    known_roots: Vec<String>,
}

/// Whether `root` may be used as a `root=` override.
///
/// Authorized are: the currently saved vault, every root the user opened or
/// created before, and the `FERO_VAULT_ROOT` environment override. When
/// no state file can be read the check cannot be made and the root is allowed,
/// so a missing home directory degrades to the previous behavior instead of
/// locking the user out of their library.
fn is_authorized_root(root: &Path) -> bool {
    let Ok(state) = load_app_state() else {
        return true;
    };

    let matches = |candidate: &str| {
        resolve_existing_root(PathBuf::from(candidate))
            .map(|resolved| resolved == root)
            .unwrap_or(false)
    };

    if state.vault_root.as_deref().is_some_and(matches) {
        return true;
    }
    if state.known_roots.iter().any(|known| matches(known)) {
        return true;
    }
    env::var("FERO_VAULT_ROOT")
        .ok()
        .is_some_and(|configured| matches(&configured))
}

// ---------------------------------------------------------------------------
// Open-with-system API
// ---------------------------------------------------------------------------

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OpenExternalResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl OpenExternalResponse {
    fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }
    fn error(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevealRequest {
    /// Subscription whose folder should be shown.
    id: String,
    /// Media kind id, see `MediaKind::id`.
    kind: String,
}

/// Shows a subscription's work folder in the system file manager.
///
/// Takes a subscription id rather than a path: with a delivery target per work
/// there is no shared root to make a path relative to, and deriving the folder
/// here means no caller can ask for a directory Fero does not own.
fn build_reveal_response(body: &[u8]) -> OpenExternalResponse {
    let req: RevealRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return OpenExternalResponse::error(format!("Invalid request: {error}")),
    };
    let Some(kind) = MediaKind::from_id(&req.kind) else {
        return OpenExternalResponse::error(format!("Unbekannter Medientyp: {}", req.kind));
    };
    let ws = match resolve_workspace(None) {
        Ok(ws) => ws,
        Err(message) => return OpenExternalResponse::error(message),
    };
    let subscription = match load_subscription(&ws.store, &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return OpenExternalResponse::error("Abo nicht gefunden."),
        Err(error) => return OpenExternalResponse::error(error.to_string()),
    };
    let parent = match ws.delivery_parent(kind, &subscription) {
        Ok(parent) => parent,
        Err(error) => return OpenExternalResponse::error(error.to_string()),
    };
    let folder = match kind {
        MediaKind::Manga => parent.join(crate::desktop_manga::folder_name(&subscription)),
        _ => webnovel_folder(&parent, &subscription),
    };
    if !folder.is_dir() {
        return OpenExternalResponse::error(
            "Für dieses Abo wurde noch nichts heruntergeladen.".to_string(),
        );
    }

    // `open -R` reveals the target in Finder instead of launching it. Fero
    // shows where files are; opening them is the library's job.
    match std::process::Command::new("open")
        .arg("-R")
        .arg(&folder)
        .spawn()
    {
        Ok(_) => OpenExternalResponse::ok(),
        Err(error) => OpenExternalResponse::error(format!("open failed: {error}")),
    }
}

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
struct WebnovelJobStatus {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
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
fn update_webnovel_job(job_id: &str, apply: impl FnOnce(&mut WebnovelJobStatus)) {
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
    fn from_subscription(subscription: &Subscription, work_dir: Option<&Path>) -> Self {
        let has_cover = work_dir
            .map(|dir| load_novel_cover_path(dir).is_some())
            .unwrap_or(false);
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
struct WebnovelListResponse {
    subscriptions: Vec<WebnovelSubscriptionSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_webnovel_list_response(query: Option<&str>) -> WebnovelListResponse {
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
                    let work_dir = ws
                        .delivery_parent_opt(MediaKind::Webnovel, subscription)
                        .map(|parent| webnovel_folder(&parent, subscription));
                    WebnovelSubscriptionSummary::from_subscription(
                        subscription,
                        work_dir.as_deref(),
                    )
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

/// Resolves the active vault or produces a user-facing German error message.
/// Fero's own storage plus the library it delivers into.
///
/// The two are deliberately separate. Subscriptions, blocklists and caches are
/// Fero's bookkeeping and live in Fero's data directory; only finished works go
/// into the library. Writing bookkeeping into the library would put Fero's
/// files inside someone else's collection.
pub(crate) struct Workspace {
    /// Fero's data directory — subscriptions, blocklist, caches.
    pub(crate) store: PathBuf,
    /// Configured download targets per media kind, plus the shared fallback.
    pub(crate) targets: TargetSettings,
}

impl Workspace {
    /// Directory the work folder for `subscription` belongs in.
    ///
    /// Walks the target chain: the subscription's own target, then the media
    /// kind's default, then the confirmed fallback. Fallible on purpose — if
    /// nothing is configured, or a configured target is offline, the work must
    /// not silently land somewhere else.
    ///
    /// # Errors
    /// [`FeroError::InvalidTarget`] carrying the reason to show the user.
    pub(crate) fn delivery_parent(
        &self,
        kind: MediaKind,
        subscription: &Subscription,
    ) -> Result<PathBuf> {
        let own = subscription.target_dir.as_deref().map(Path::new);
        match resolve_target(own, kind, &self.targets, &self.store) {
            TargetResolution::Resolved { parent, .. } => Ok(parent),
            TargetResolution::NeedsChoice { reason, .. } => Err(FeroError::InvalidTarget(reason)),
        }
    }
}

impl Workspace {
    /// Chapter cache for one subscription, inside Fero's data directory.
    ///
    /// Used to live in the delivered work folder. That put Fero's scratch data
    /// into the library and made every rebuild depend on the target being
    /// online — the cache belongs to Fero, not to the collection.
    ///
    /// # Errors
    /// [`FeroError::InvalidProperty`] if the id is not a well-formed
    /// subscription id; it becomes a directory name.
    pub(crate) fn chapter_cache(&self, subscription_id: &str) -> Result<PathBuf> {
        if !is_valid_subscription_id(subscription_id) {
            return Err(FeroError::InvalidProperty(format!(
                "invalid subscription id: {subscription_id}"
            )));
        }
        Ok(self.store.join("cache").join(subscription_id))
    }

    /// Delivery parent, or `None` when no usable target is configured.
    ///
    /// For read-only views: a subscription without a target is still worth
    /// listing, just without the things that live in its folder. Deliberately
    /// returns the *parent* — how a work folder is named is the business of the
    /// media kind's module, not of the workspace.
    pub(crate) fn delivery_parent_opt(
        &self,
        kind: MediaKind,
        subscription: &Subscription,
    ) -> Option<PathBuf> {
        self.delivery_parent(kind, subscription).ok()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetKindView {
    id: &'static str,
    label: &'static str,
    folder_segment: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
}

/// What the settings page needs to render the target configuration.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TargetsResponse {
    /// Fero's data directory, or `None` while setup is pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    data_dir: Option<String>,
    /// Why the data directory is unusable, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    data_dir_problem: Option<String>,
    /// Whether the data directory sits next to the application.
    portable: bool,
    kinds: Vec<TargetKindView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fallback: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_targets_response() -> TargetsResponse {
    let data = resolve_data_dir();
    let (data_dir, data_dir_problem, portable) = match &data {
        DataDir::Portable(path) => (Some(path.display().to_string()), None, true),
        DataDir::Chosen(path) => (Some(path.display().to_string()), None, false),
        DataDir::NeedsSetup { reason, .. } => (None, Some(reason.clone()), false),
    };
    let settings = data.path().map(load_target_settings).unwrap_or_default();

    TargetsResponse {
        data_dir,
        data_dir_problem,
        portable,
        kinds: MediaKind::ALL
            .iter()
            .map(|kind| TargetKindView {
                id: kind.id(),
                label: kind.label(),
                folder_segment: kind.folder_segment(),
                directory: settings
                    .default_for(*kind)
                    .map(|dir| dir.display().to_string()),
            })
            .collect(),
        fallback: settings.fallback.clone(),
        error: None,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveTargetsRequest {
    /// Media kind id, or `null` to address the shared fallback.
    #[serde(default)]
    kind: Option<String>,
    /// New directory, or `null` to clear the entry.
    #[serde(default)]
    directory: Option<String>,
}

fn build_save_targets_response(body: &[u8]) -> TargetsResponse {
    let req: SaveTargetsRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return TargetsResponse {
                error: Some(format!("Ungültige Anfrage: {error}")),
                ..build_targets_response()
            }
        }
    };
    let Some(store) = resolve_data_dir().path().map(Path::to_path_buf) else {
        return TargetsResponse {
            error: Some("Kein nutzbarer Datenordner eingerichtet.".to_string()),
            ..build_targets_response()
        };
    };

    let mut settings = load_target_settings(&store);
    match req.kind.as_deref() {
        None => settings.fallback = req.directory.clone(),
        Some(id) => match MediaKind::from_id(id) {
            Some(kind) => settings.set_default(kind, req.directory.as_deref().map(Path::new)),
            None => {
                return TargetsResponse {
                    error: Some(format!("Unbekannter Medientyp: {id}")),
                    ..build_targets_response()
                }
            }
        },
    }

    if let Err(error) = save_target_settings(&store, &settings) {
        return TargetsResponse {
            error: Some(error.to_string()),
            ..build_targets_response()
        };
    }
    build_targets_response()
}

/// File holding the configured download targets, inside Fero's data directory.
const TARGET_SETTINGS_FILE: &str = "targets.json";

/// Loads the configured targets; missing or unreadable settings mean "nothing
/// configured yet", which the target chain reports as a choice for the user.
pub(crate) fn load_target_settings(store: &Path) -> TargetSettings {
    fs::read_to_string(store.join(TARGET_SETTINGS_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persists the configured targets.
///
/// # Errors
/// [`FeroError::Io`] when the file cannot be written.
pub(crate) fn save_target_settings(store: &Path, settings: &TargetSettings) -> Result<()> {
    let body = serde_json::to_string_pretty(settings)
        .map_err(|error| FeroError::Serialization(error.to_string()))?;
    fs::create_dir_all(store).map_err(FeroError::from)?;
    fs::write(store.join(TARGET_SETTINGS_FILE), body).map_err(FeroError::from)
}

/// Resolves both locations, or reports which one is missing.
pub(crate) fn resolve_workspace(
    root_override: Option<&str>,
) -> std::result::Result<Workspace, String> {
    // The library root is deliberately not part of this any more: with a target
    // per media kind (and per subscription) there is no single root to resolve.
    // Where a work goes is decided per work, by the target chain.
    let _ = root_override;
    let store = match resolve_data_dir() {
        DataDir::Portable(path) | DataDir::Chosen(path) => path,
        DataDir::NeedsSetup { reason, .. } => return Err(reason),
    };
    let targets = load_target_settings(&store);
    Ok(Workspace { store, targets })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelSubscribeRequest {
    url: String,
    #[serde(default)]
    root: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelSubscribeResponse {
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

fn build_webnovel_subscribe_response(body: &[u8]) -> WebnovelSubscribeResponse {
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
                    &existing,
                    ws.delivery_parent_opt(MediaKind::Webnovel, &existing)
                        .map(|parent| webnovel_folder(&parent, &existing))
                        .as_deref(),
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
                ws.delivery_parent_opt(MediaKind::Webnovel, &subscription)
                    .map(|parent| webnovel_folder(&parent, &subscription))
                    .as_deref(),
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelSimpleResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl WebnovelSimpleResponse {
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

fn webnovel_default_true() -> bool {
    true
}

fn build_webnovel_unsubscribe_response(body: &[u8]) -> WebnovelSimpleResponse {
    let req: WebnovelUnsubscribeRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return WebnovelSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };

    // Soft delete: the record moves to the in-app trash and can be restored.
    let subscription = match trash_subscription(&ws.store, &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return WebnovelSimpleResponse::error("Abo nicht gefunden."),
        Err(error) => return WebnovelSimpleResponse::error(error.to_string()),
    };

    if !req.keep_files {
        // Files move into the vault .trash folder (same convention as
        // delete-files) instead of being removed — reversible via restore.
        let parent = match ws.delivery_parent(MediaKind::Webnovel, &subscription) {
            Ok(parent) => parent,
            Err(error) => return WebnovelSimpleResponse::error(error.to_string()),
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
                return WebnovelSimpleResponse::error(format!(
                    "Dateien konnten nicht in den Papierkorb verschoben werden: {error}"
                ));
            }
        }
    }

    WebnovelSimpleResponse::ok()
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
struct WebnovelTrashResponse {
    entries: Vec<WebnovelTrashEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_webnovel_trash_response(query: Option<&str>) -> WebnovelTrashResponse {
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

fn build_webnovel_restore_response(body: &[u8]) -> WebnovelSimpleResponse {
    let req: WebnovelTrashActionRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return WebnovelSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };
    let subscription = match restore_subscription(&ws.store, &req.id) {
        Ok(subscription) => subscription,
        Err(error) => return WebnovelSimpleResponse::error(error.to_string()),
    };
    // Bring trashed files back, if any.
    let parent = match ws.delivery_parent(MediaKind::Webnovel, &subscription) {
        Ok(parent) => parent,
        Err(error) => return WebnovelSimpleResponse::error(error.to_string()),
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
    WebnovelSimpleResponse::ok()
}

fn build_webnovel_purge_response(body: &[u8]) -> WebnovelSimpleResponse {
    let req: WebnovelTrashActionRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return WebnovelSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return WebnovelSimpleResponse::error(message),
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
        return WebnovelSimpleResponse::error(error.to_string());
    }
    if let Some(subscription) = trashed {
        let Ok(parent) = ws.delivery_parent(MediaKind::Webnovel, &subscription) else {
            return WebnovelSimpleResponse::error(
                "Kein Zielordner festgelegt — die Dateien lassen sich nicht finden.",
            );
        };
        let folder = webnovel_trash_folder(&parent, &subscription);
        if folder.exists() {
            fs::remove_dir_all(&folder).ok();
        }
    }
    WebnovelSimpleResponse::ok()
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
    #[serde(default)]
    completed: Option<bool>,
    #[serde(default)]
    hiatus: Option<bool>,
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    root: Option<String>,
}

fn build_webnovel_update_response(body: &[u8]) -> WebnovelSimpleResponse {
    let req: WebnovelUpdateRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return WebnovelSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };

    let mut subscription = match load_subscription(&ws.store, &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return WebnovelSimpleResponse::error("Abo nicht gefunden."),
        Err(error) => return WebnovelSimpleResponse::error(error.to_string()),
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

    match save_subscription(&ws.store, &subscription) {
        Ok(()) => WebnovelSimpleResponse::ok(),
        Err(error) => WebnovelSimpleResponse::error(error.to_string()),
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
struct WebnovelCheckResponse {
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

fn build_webnovel_check_response(body: &[u8]) -> WebnovelCheckResponse {
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
    let thread_job_id = job_id.clone();
    std::thread::spawn(move || {
        let _active = WebnovelCheckActiveGuard;
        let outcome = run_webnovel_check(&ws, &options, &thread_job_id);
        update_webnovel_job(&thread_job_id, |status| match &outcome {
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

    WebnovelCheckResponse {
        job_id: Some(job_id),
        error: None,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelJobResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<WebnovelJobStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_webnovel_job_response(query: Option<&str>) -> WebnovelJobResponse {
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

    let parent = ws.delivery_parent(MediaKind::Webnovel, subscription)?;
    let novel_dir = webnovel_folder(&parent, subscription);
    let cache_dir = ws.chapter_cache(&subscription.id)?;
    migrate_chapter_cache(&novel_dir, &cache_dir);
    fs::create_dir_all(&cache_dir).map_err(FeroError::from)?;

    // Fetch the cover once; failures are non-fatal (text matters more).
    let cover_added = ensure_novel_cover(client, subscription, &novel_dir);

    let pending: Vec<usize> = subscription
        .known_chapters
        .iter()
        .enumerate()
        // Placeholders are retried here: a chapter that failed once is usually
        // a transient upstream hiccup, and leaving it permanently "done" would
        // bake the error notice into every rebuilt EPUB.
        .filter(|(_, chapter)| chapter.needs_fetch())
        .map(|(position, _)| position)
        .collect();
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
    if !downloaded_indices.is_empty() && options.build_batch && !subscription.completed {
        build_batch_epub(&novel_dir, &cache_dir, subscription, &downloaded_indices)?;
    }
    // The complete EPUB is also rebuilt when it is missing entirely or when a
    // cover arrived after the last build (covers embed into the EPUB itself).
    let safe_title = novel_folder_name(subscription);
    let complete_file = format!("{safe_title}.epub");
    let complete_missing = !novel_dir.join(&complete_file).exists();
    if options.build_complete
        && (!downloaded_indices.is_empty()
            || repaired_chapters > 0
            || cover_added
            || complete_missing)
    {
        build_complete_epub(&novel_dir, &cache_dir, subscription)?;
    } else if !complete_missing {
        // No rebuild needed, but enrichment may have added metadata — keep the
        // manifest in step with the subscription.
        record_delivery(&novel_dir, subscription, &complete_file, None)?;
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
const MAX_CONSECUTIVE_CHAPTER_FAILURES: usize = 5;

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
fn enrich_from_anilist(subscription: &mut Subscription) {
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
fn load_novel_cover_path(novel_dir: &Path) -> Option<PathBuf> {
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
fn webnovel_folder(delivery_parent: &Path, subscription: &Subscription) -> PathBuf {
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
fn build_batch_epub(
    novel_dir: &Path,
    cache_dir: &Path,
    subscription: &Subscription,
    indices: &[u32],
) -> Result<()> {
    let min = indices.iter().min().copied().unwrap_or(0);
    let max = indices.iter().max().copied().unwrap_or(0);
    let safe_title = novel_folder_name(subscription);
    let file_name = if min == max {
        format!("{safe_title} - Kapitel {min:04}.epub")
    } else {
        format!("{safe_title} - Kapitel {min:04}-{max:04}.epub")
    };

    let chapters = load_cached_chapters(cache_dir, indices)?;

    let meta = EpubMeta {
        title: if min == max {
            format!("{} – Kapitel {min}", subscription.title)
        } else {
            format!("{} – Kapitel {min}–{max}", subscription.title)
        },
        author: subscription.author.clone(),
        language: "en".to_string(),
        identifier: format!("{}#batch-{min}-{max}", subscription.url),
        description: subscription.description.clone(),
        cover: load_novel_cover(novel_dir),
    };
    write_epub(&novel_dir.join(&file_name), &meta, &chapters)?;

    record_delivery(novel_dir, subscription, &file_name, Some((min, max)))
}

/// Rebuilds the complete EPUB from every cached chapter.
fn build_complete_epub(
    novel_dir: &Path,
    cache_dir: &Path,
    subscription: &Subscription,
) -> Result<()> {
    let indices: Vec<u32> = subscription
        .known_chapters
        .iter()
        .filter(|chapter| chapter.downloaded_at_unix.is_some())
        .map(|chapter| chapter.index)
        .collect();
    if indices.is_empty() {
        return Ok(());
    }

    let chapters = load_cached_chapters(cache_dir, &indices)?;

    let safe_title = novel_folder_name(subscription);
    let file_name = format!("{safe_title}.epub");
    let meta = EpubMeta {
        title: subscription.title.clone(),
        author: subscription.author.clone(),
        language: "en".to_string(),
        identifier: subscription.url.clone(),
        description: subscription.description.clone(),
        cover: load_novel_cover(novel_dir),
    };
    write_epub(&novel_dir.join(&file_name), &meta, &chapters)?;

    record_delivery(novel_dir, subscription, &file_name, None)
}

/// Records a delivered file in the work folder's `fero.info.json`.
///
/// Replaces the former `.fero.yaml` sidecar. Descriptive metadata is not
/// duplicated here — it goes into the EPUB's OPF, where any reader can see it.
fn record_delivery(
    work_dir: &Path,
    subscription: &Subscription,
    file_name: &str,
    chapter_range: Option<(u32, u32)>,
) -> Result<()> {
    let mut record = manifest::load_or_new(
        work_dir,
        &subscription.id,
        MediaKind::Webnovel,
        &subscription.url,
        &subscription.title,
    );
    record.title = subscription.title.clone();
    record.status = if subscription.completed {
        manifest::SeriesStatus::Completed
    } else if subscription.hiatus {
        manifest::SeriesStatus::Hiatus
    } else {
        manifest::SeriesStatus::Ongoing
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
/// Only `http`/`https` URLs are accepted — anything else (file paths, custom
/// schemes) is rejected so this endpoint cannot be abused to launch local
/// programs or leak files.
fn build_open_url_response(query: Option<&str>) -> WebnovelSimpleResponse {
    let url = match query.and_then(|q| extract_query_value(q, "url")) {
        Some(url) => url,
        None => return WebnovelSimpleResponse::error("missing url"),
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return WebnovelSimpleResponse::error("Nur http/https-Links erlaubt.");
    }

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&url).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", &url])
        .spawn();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let result = std::process::Command::new("xdg-open").arg(&url).spawn();

    match result {
        Ok(_) => WebnovelSimpleResponse::ok(),
        Err(error) => {
            WebnovelSimpleResponse::error(format!("Browser-Start fehlgeschlagen: {error}"))
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebnovelBlocklistResponse {
    entries: Vec<BlocklistEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_webnovel_blocklist_response(query: Option<&str>) -> WebnovelBlocklistResponse {
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

fn build_webnovel_blocklist_save_response(body: &[u8]) -> WebnovelSimpleResponse {
    let req: WebnovelBlocklistSaveRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return WebnovelSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let ws = match resolve_workspace(req.root.as_deref()) {
        Ok(ws) => ws,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };
    match save_user_blocklist(&ws.store, &req.entries) {
        Ok(()) => WebnovelSimpleResponse::ok(),
        Err(error) => WebnovelSimpleResponse::error(error.to_string()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription_with_title(title: &str) -> Subscription {
        Subscription::new("https://example.com/novel", "generic", title)
    }

    #[test]
    fn sanitize_strips_path_operators() {
        assert_eq!(sanitize_path_segment(".."), "");
        assert_eq!(sanitize_path_segment("."), "");
        assert_eq!(sanitize_path_segment("../../etc"), "etc");
        assert_eq!(sanitize_path_segment("///"), "");
        assert_eq!(sanitize_path_segment(".hidden"), "hidden");
        assert_eq!(sanitize_path_segment("Normaler Titel"), "Normaler Titel");
    }

    #[test]
    fn sanitize_caps_length_and_reserved_names() {
        let long = "a".repeat(400);
        assert_eq!(
            sanitize_path_segment(&long).chars().count(),
            MAX_PATH_SEGMENT_CHARS
        );
        assert_eq!(sanitize_path_segment("CON"), "CON_");
        assert_eq!(sanitize_path_segment("trailing."), "trailing");
    }

    #[test]
    fn safe_folder_segment_uses_fallback_when_unusable() {
        assert_eq!(safe_folder_segment("..", "Fallback"), "Fallback");
        assert_eq!(safe_folder_segment("", "Fallback"), "Fallback");
        assert_eq!(safe_folder_segment("Titel", "Fallback"), "Titel");
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
