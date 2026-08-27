//! Desktop shell bootstrap for Fero.
//!
//! Split along the lines the concept lays out: `http` owns the response
//! plumbing, `routes` owns the dispatch table, and this module keeps the
//! bootstrap plus the handlers that have not moved into their own modules yet.

mod browser;
mod http;
mod log;
mod manga;
mod routes;
mod tray;
mod webnovel;

use browser::*;
pub(crate) use http::{extract_query_value, impl_outcome, json_response, Outcome};
pub(crate) use log::debug_log;
use log::debug_log_path;
use log::{build_open_debug_log_response, build_webnovel_debug_log_response};
use routes::handle_request;
use webnovel::*;

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

// Die beiden Fenster-Antworten deklarieren ihr Outcome in browser.rs.
impl_outcome!(
    AniListSearchResponse,
    ScheduleResponse,
    SelectFolderResponse,
    OpenExternalResponse,
    TargetsResponse,
    SimpleResponse,
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
        .setup(|app| {
            // The tray is what remains when the window is closed; without it a
            // closed window would mean no more scheduled checks.
            if let Err(error) = tray::install(app.handle()) {
                debug_log(&format!("Tray konnte nicht angelegt werden: {error}"));
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            let quit_on_close = resolve_workspace(None)
                .ok()
                .map(|ws| load_schedule_settings(&ws.store).quit_on_close)
                .unwrap_or(false);
            if !quit_on_close {
                tray::handle_window_event(window, event);
            }
        })
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
// Workspace, delivery targets and shared response shapes
// ---------------------------------------------------------------------------

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
struct ScheduleResponse {
    interval_hours: u64,
    quit_on_close: bool,
    paused: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_schedule_response() -> ScheduleResponse {
    let settings = resolve_workspace(None)
        .ok()
        .map(|ws| load_schedule_settings(&ws.store))
        .unwrap_or_default();
    ScheduleResponse {
        interval_hours: settings.interval_hours,
        quit_on_close: settings.quit_on_close,
        paused: tray::is_paused(),
        error: None,
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SaveScheduleRequest {
    #[serde(default)]
    interval_hours: Option<u64>,
    #[serde(default)]
    quit_on_close: Option<bool>,
    #[serde(default)]
    paused: Option<bool>,
}

fn build_save_schedule_response(body: &[u8]) -> ScheduleResponse {
    let req: SaveScheduleRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return ScheduleResponse {
                error: Some(format!("Ungültige Anfrage: {error}")),
                ..build_schedule_response()
            }
        }
    };
    let Ok(ws) = resolve_workspace(None) else {
        return ScheduleResponse {
            error: Some("Kein nutzbarer Datenordner eingerichtet.".to_string()),
            ..build_schedule_response()
        };
    };

    let mut settings = load_schedule_settings(&ws.store);
    if let Some(hours) = req.interval_hours {
        // An interval below an hour would hammer the sources; above a week it
        // stops being a schedule.
        settings.interval_hours = hours.clamp(1, 24 * 7);
    }
    if let Some(quit) = req.quit_on_close {
        settings.quit_on_close = quit;
    }
    if let Some(paused) = req.paused {
        tray::set_paused(paused);
    }

    if let Err(error) = save_schedule_settings(&ws.store, &settings) {
        return ScheduleResponse {
            error: Some(error.to_string()),
            ..build_schedule_response()
        };
    }
    build_schedule_response()
}

/// File holding the schedule settings, inside Fero's data directory.
const SCHEDULE_SETTINGS_FILE: &str = "schedule.json";

/// How often Fero checks on its own, and what closing the window means.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScheduleSettings {
    /// Hours between automatic checks.
    pub(crate) interval_hours: u64,
    /// Whether closing the window quits instead of hiding to the tray.
    #[serde(default)]
    pub(crate) quit_on_close: bool,
}

impl Default for ScheduleSettings {
    fn default() -> Self {
        Self {
            interval_hours: 6,
            quit_on_close: false,
        }
    }
}

/// Loads the schedule settings; anything unreadable means "use the defaults".
pub(crate) fn load_schedule_settings(store: &Path) -> ScheduleSettings {
    fs::read_to_string(store.join(SCHEDULE_SETTINGS_FILE))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persists the schedule settings.
///
/// # Errors
/// [`FeroError::Io`] when the file cannot be written.
pub(crate) fn save_schedule_settings(store: &Path, settings: &ScheduleSettings) -> Result<()> {
    let body = serde_json::to_string_pretty(settings)
        .map_err(|error| FeroError::Serialization(error.to_string()))?;
    fs::create_dir_all(store).map_err(FeroError::from)?;
    fs::write(store.join(SCHEDULE_SETTINGS_FILE), body).map_err(FeroError::from)
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

/// Generic ok/error answer, shared by every endpoint that has nothing else to
/// report. Lives here rather than with one media kind because all of them —
/// and the browser window — answer with it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SimpleResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SimpleResponse {
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

/// Serves a subscription's cached cover image.
///
/// Bytes rather than JSON: the frontend points an `<img>` straight at this.
/// Every failure is a plain 404 — a missing cover is not an error worth a
/// message, the image slot simply stays empty.
fn build_cover_response(query: Option<&str>) -> tauri::http::Response<Vec<u8>> {
    use tauri::http::StatusCode;

    let not_found =
        || http::bytes_response(StatusCode::NOT_FOUND, "text/plain", b"not found".to_vec());

    let Some(query) = query else {
        return not_found();
    };
    let Some(id) = extract_query_value(query, "id") else {
        return not_found();
    };
    let Some(kind) = extract_query_value(query, "kind").and_then(|k| MediaKind::from_id(&k)) else {
        return not_found();
    };
    let Ok(ws) = resolve_workspace(None) else {
        return not_found();
    };
    let loaded = match kind {
        MediaKind::Manga => crate::core::manga::load_subscription(&ws.store, &id),
        _ => load_subscription(&ws.store, &id),
    };
    let Ok(Some(subscription)) = loaded else {
        return not_found();
    };
    let Ok(parent) = ws.delivery_parent(kind, &subscription) else {
        return not_found();
    };
    let cover = match kind {
        MediaKind::Manga => {
            manga::manga_cover_path(&parent.join(manga::folder_name(&subscription)))
        }
        _ => webnovel::load_novel_cover_path(&webnovel_folder(&parent, &subscription)),
    };
    let Some(path) = cover else {
        return not_found();
    };
    let Ok(body) = fs::read(&path) else {
        return not_found();
    };

    let content_type = match path.extension().and_then(|e| e.to_str()) {
        Some("png") => "image/png",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        _ => "image/jpeg",
    };
    http::bytes_response(StatusCode::OK, content_type, body)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDataDirRequest {
    directory: String,
}

/// Sets the data directory to a folder the user picked.
///
/// This is the "frei wählbar" half of the portable design: when the folder
/// next to the app is unusable — a translocated app sits on a read-only
/// filesystem — the user chooses where Fero's data lives, and a pointer file
/// in the home directory remembers it.
fn build_set_data_dir_response(body: &[u8]) -> TargetsResponse {
    let req: SetDataDirRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return TargetsResponse {
                error: Some(format!("Ungültige Anfrage: {error}")),
                ..build_targets_response()
            }
        }
    };
    match crate::deliver::targets::set_data_dir(Path::new(&req.directory)) {
        Ok(()) => build_targets_response(),
        Err(error) => TargetsResponse {
            error: Some(error.to_string()),
            ..build_targets_response()
        },
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

/// Only `http`/`https` URLs are accepted — anything else (file paths, custom
/// schemes) is rejected so this endpoint cannot be abused to launch local
/// programs or leak files.
fn build_open_url_response(query: Option<&str>) -> SimpleResponse {
    let url = match query.and_then(|q| extract_query_value(q, "url")) {
        Some(url) => url,
        None => return SimpleResponse::error("missing url"),
    };
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return SimpleResponse::error("Nur http/https-Links erlaubt.");
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
        Ok(_) => SimpleResponse::ok(),
        Err(error) => SimpleResponse::error(format!("Browser-Start fehlgeschlagen: {error}")),
    }
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
    // Jeder Medientyp hat seinen eigenen Store — ein Manga liegt nie im
    // Webnovel-Store, die Suche muss dem Typ folgen.
    let loaded = match kind {
        MediaKind::Manga => crate::core::manga::load_subscription(&ws.store, &req.id),
        _ => load_subscription(&ws.store, &req.id),
    };
    let subscription = match loaded {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return OpenExternalResponse::error("Abo nicht gefunden."),
        Err(error) => return OpenExternalResponse::error(error.to_string()),
    };
    let parent = match ws.delivery_parent(kind, &subscription) {
        Ok(parent) => parent,
        Err(error) => return OpenExternalResponse::error(error.to_string()),
    };
    let folder = match kind {
        MediaKind::Manga => parent.join(manga::folder_name(&subscription)),
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
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

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
}
