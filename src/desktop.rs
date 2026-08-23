//! Desktop shell bootstrap for Fero.

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
use crate::core::properties::{legacy_sidecar_path_for, sidecar_path_for};
use crate::core::vault::{RelativePath, Vault};
use crate::core::webnovel::{
    blocked_reason, list_subscriptions, list_trashed_subscriptions, load_blocklist_entries,
    load_subscription, normalize_url, purge_trashed_subscription, restore_subscription,
    save_subscription, save_user_blocklist, trash_subscription, unix_now, BlocklistEntry,
    KnownChapter, Subscription,
};
use crate::deliver::migrate::{migrate_store_from_library, Outcome as MigrationOutcome};
use crate::deliver::manifest;
use crate::deliver::targets::{
    resolve_data_dir, resolve_target, DataDir, MediaKind, TargetResolution, TargetSettings,
};
use crate::error::{Result, VaultError};
use crate::media::{MediaStatus, MediaType};
use serde::{Deserialize, Serialize};
use tauri::http::{header::CONTENT_TYPE, Request, Response, StatusCode};
use tauri::Manager;

const PROTOCOL_SCHEME: &str = "fero";
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

const INDEX_HTML: &str = include_str!("../dist/index.html");
const APP_JS: &str = include_str!("../dist/app.js");
const STYLES_CSS: &str = include_str!("../dist/styles.css");

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
        .map_err(|error| VaultError::AppStartup(error.to_string()))
}

/// Routes a single custom-scheme request to its handler and returns the
/// response. Runs on a worker thread (see [`run`]); all blocking work lives
/// here rather than on the webview thread.
fn handle_request(request: &Request<Vec<u8>>) -> Response<Vec<u8>> {
    let path = request.uri().path();

    match path {
        "/" | "/index.html" => response(StatusCode::OK, "text/html; charset=utf-8", INDEX_HTML),
        "/app.js" => response(
            StatusCode::OK,
            "application/javascript; charset=utf-8",
            APP_JS,
        ),
        "/styles.css" => response(StatusCode::OK, "text/css; charset=utf-8", STYLES_CSS),
        "/api/anilist-search" => json_response(
            StatusCode::OK,
            &build_anilist_search_response(request.uri().query()),
        ),
        "/api/select-folder" => json_response(StatusCode::OK, &build_select_folder_response()),
        "/api/open-external" => json_response(
            StatusCode::OK,
            &build_open_external_response(request.uri().query()),
        ),
        "/api/open-url" => json_response(
            StatusCode::OK,
            &build_open_url_response(request.uri().query()),
        ),
        "/api/webnovel/debug-log" => {
            json_response(StatusCode::OK, &build_webnovel_debug_log_response())
        }
        "/api/webnovel/open-debug-log" => {
            json_response(StatusCode::OK, &build_open_debug_log_response())
        }
        "/api/webnovel/list" => json_response(
            StatusCode::OK,
            &build_webnovel_list_response(request.uri().query()),
        ),
        "/api/webnovel/subscribe" => json_response(
            StatusCode::OK,
            &build_webnovel_subscribe_response(request.body()),
        ),
        "/api/webnovel/unsubscribe" => json_response(
            StatusCode::OK,
            &build_webnovel_unsubscribe_response(request.body()),
        ),
        "/api/webnovel/update" => json_response(
            StatusCode::OK,
            &build_webnovel_update_response(request.body()),
        ),
        "/api/webnovel/check" => json_response(
            StatusCode::OK,
            &build_webnovel_check_response(request.body()),
        ),
        "/api/webnovel/solve" => json_response(
            StatusCode::OK,
            &build_webnovel_solve_response(request.body()),
        ),
        "/api/webnovel/solve-status" => json_response(
            StatusCode::OK,
            &build_webnovel_solve_status_response(request.uri().query()),
        ),
        "/api/webnovel/login" => json_response(
            StatusCode::OK,
            &build_webnovel_login_response(request.body()),
        ),
        "/api/webnovel/login-status" => json_response(
            StatusCode::OK,
            &build_webnovel_login_status_response(request.uri().query()),
        ),
        "/api/webnovel/logout" => json_response(
            StatusCode::OK,
            &build_webnovel_logout_response(request.body()),
        ),
        "/api/webnovel/trash" => json_response(
            StatusCode::OK,
            &build_webnovel_trash_response(request.uri().query()),
        ),
        "/api/webnovel/restore" => json_response(
            StatusCode::OK,
            &build_webnovel_restore_response(request.body()),
        ),
        "/api/webnovel/purge" => json_response(
            StatusCode::OK,
            &build_webnovel_purge_response(request.body()),
        ),
        "/api/webnovel/blocklist" => json_response(
            StatusCode::OK,
            &build_webnovel_blocklist_response(request.uri().query()),
        ),
        "/api/webnovel/blocklist/save" => json_response(
            StatusCode::OK,
            &build_webnovel_blocklist_save_response(request.body()),
        ),
        "/api/webnovel/job" => json_response(
            StatusCode::OK,
            &build_webnovel_job_response(request.uri().query()),
        ),
        // Manga subscriptions mirror the webnovel endpoints one for one; the
        // handlers live in `desktop_manga` to keep this file from growing.
        "/api/manga/list" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_list_response(request.uri().query()),
        ),
        "/api/manga/subscribe" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_subscribe_response(request.body()),
        ),
        "/api/manga/unsubscribe" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_unsubscribe_response(request.body()),
        ),
        "/api/manga/update" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_update_response(request.body()),
        ),
        "/api/manga/check" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_check_response(request.body()),
        ),
        "/api/manga/job" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_job_response(request.uri().query()),
        ),
        "/api/manga/trash" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_trash_response(request.uri().query()),
        ),
        "/api/manga/restore" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_restore_response(request.body()),
        ),
        "/api/manga/purge" => json_response(
            StatusCode::OK,
            &crate::desktop_manga::build_purge_response(request.body()),
        ),
        _ => response(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            "Not Found",
        ),
    }
}

fn response(status: StatusCode, content_type: &str, body: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, content_type)
        .body(body.as_bytes().to_vec())
        .expect("response construction should succeed")
}

pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response<Vec<u8>> {
    match serde_json::to_vec(value) {
        Ok(body) => Response::builder()
            .status(status)
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body)
            .expect("JSON response construction should succeed"),
        Err(error) => response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "text/plain; charset=utf-8",
            &format!("failed to serialize JSON: {error}"),
        ),
    }
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

// ---------------------------------------------------------------------------
// Shared helpers (paths, app state, sidecars, query parsing)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
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
struct SelectFolderResponse {
    path: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct ParsedSidecar {
    media_type: Option<MediaType>,
    title: Option<String>,
    year: Option<u16>,
    series_title: Option<String>,
    season_number: Option<u16>,
    episode_start: Option<u16>,
    episode_end: Option<u16>,
    episode_title: Option<String>,
    episode_count: Option<u16>,
    runtime_minutes: Option<u16>,
    average_score: Option<f32>,
    format: Option<String>,
    airing_season: Option<String>,
    anilist_id: Option<u32>,
    anilist_url: Option<String>,
    status: Option<MediaStatus>,
    description: Option<String>,
    rating_external: Option<f32>,
    author: Option<String>,
    genres: Vec<String>,
    tags: Vec<String>,
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

pub(crate) fn extract_query_value(query: &str, wanted_key: &str) -> Option<String> {
    for pair in query.split('&') {
        let Some((key, value)) = pair.split_once('=') else {
            continue;
        };
        if key == wanted_key {
            return urlencoding::decode(value)
                .ok()
                .map(|value| value.into_owned());
        }
    }

    None
}

pub(crate) fn write_sidecar_preview(
    vault: &Vault,
    media_relative: &RelativePath,
    sidecar_preview: &str,
) -> Result<()> {
    let sidecar_relative = sidecar_path_for(media_relative)?;
    let sidecar_absolute = vault.resolve(sidecar_relative.as_path())?;
    let sidecar_parent = sidecar_absolute.parent().ok_or_else(|| {
        VaultError::InvalidVaultPath(format!(
            "Sidecar-Pfad hat keinen Elternordner: {}",
            sidecar_relative
        ))
    })?;
    fs::create_dir_all(sidecar_parent).map_err(VaultError::from)?;
    fs::write(&sidecar_absolute, sidecar_preview.as_bytes()).map_err(VaultError::from)?;

    // Drop sidecars written under an older naming scheme so a file never has
    // two competing metadata records. Best effort: the new sidecar is already
    // on disk, and failing to remove a stale one must not fail the save.
    for stale in sidecar_candidates(media_relative)?
        .into_iter()
        .skip(1)
        .filter_map(|candidate| vault.resolve(candidate.as_path()).ok())
    {
        if stale != sidecar_absolute && stale.exists() {
            let _ = fs::remove_file(stale);
        }
    }

    Ok(())
}

pub(crate) fn resolve_vault_root(root_override: Option<&str>) -> Result<Option<PathBuf>> {
    if let Some(root) = normalized_override(root_override) {
        let resolved = resolve_existing_root(root)?;
        if !is_authorized_root(&resolved) {
            return Err(VaultError::InvalidVaultPath(format!(
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
        return Err(VaultError::InvalidVaultPath(format!(
            "vault root does not exist: {}",
            root.display()
        )));
    }

    fs::canonicalize(&root).map_err(VaultError::from)
}

fn app_state_path() -> Result<PathBuf> {
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .ok_or_else(|| VaultError::Io("home directory not available".to_string()))?;

    Ok(PathBuf::from(home).join(".fero").join("state.json"))
}

fn load_app_state() -> Result<AppState> {
    let path = app_state_path()?;
    if !path.exists() {
        return Ok(AppState::default());
    }

    let raw = fs::read_to_string(&path).map_err(VaultError::from)?;
    serde_json::from_str(&raw).map_err(|error| VaultError::Serialization(error.to_string()))
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

/// Finds an existing sidecar for a media file, newest naming scheme first.
///
/// Three shapes are recognised, in priority order:
/// 1. `Film.mkv.fero.yaml` — current
/// 2. `Film.fero.yaml` — pre-1.0, replaced the extension
/// 3. `Film.mediashelf.yaml` — pre-rename
///
/// Older files are only read; writing always produces shape 1 (see
/// [`write_sidecar_preview`]).
fn find_sidecar_file(vault: &Vault, media_path: &RelativePath) -> Result<Option<PathBuf>> {
    for candidate in sidecar_candidates(media_path)? {
        let absolute = vault.resolve(candidate.as_path())?;
        if absolute.exists() {
            return Ok(Some(absolute));
        }
    }

    Ok(None)
}

/// All sidecar paths that may exist for a media file, current shape first.
fn sidecar_candidates(media_path: &RelativePath) -> Result<Vec<RelativePath>> {
    let mut candidates = vec![sidecar_path_for(media_path)?];

    if let Ok(legacy) = legacy_sidecar_path_for(media_path) {
        candidates.push(legacy);
    }

    let mut pre_rename = media_path.to_path_buf();
    pre_rename.set_extension("mediashelf.yaml");
    if let Ok(pre_rename) = RelativePath::new(pre_rename) {
        candidates.push(pre_rename);
    }

    Ok(candidates)
}

fn parse_sidecar_metadata(raw: &str) -> Result<ParsedSidecar> {
    let mut sidecar = ParsedSidecar::default();
    let lines = raw.lines();
    // Tracks which list ("genres"/"tags") the following "- item" lines feed.
    let mut active_list: Option<&str> = None;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" {
            active_list = None;
            continue;
        }

        // List entries rendered as `  - "value"` under a "genres:"/"tags:" key.
        if let Some(entry) = trimmed.strip_prefix("- ") {
            let value = unquote_yaml(entry);
            match active_list {
                Some("genres") if !value.is_empty() => sidecar.genres.push(value),
                Some("tags") if !value.is_empty() => sidecar.tags.push(value),
                _ => {}
            }
            continue;
        }

        let Some((key, value)) = trimmed.split_once(':') else {
            active_list = None;
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        active_list = match (key, value.is_empty()) {
            ("genres", true) => Some("genres"),
            ("tags", true) => Some("tags"),
            _ => None,
        };

        match key {
            "media_type" => sidecar.media_type = parse_media_type(unquote_yaml(value)),
            "title" => sidecar.title = Some(unquote_yaml(value)),
            "year" => sidecar.year = value.parse::<u16>().ok(),
            "series_title" => sidecar.series_title = Some(unquote_yaml(value)),
            "season_number" => sidecar.season_number = value.parse::<u16>().ok(),
            "episode_start" => sidecar.episode_start = value.parse::<u16>().ok(),
            "episode_end" => sidecar.episode_end = value.parse::<u16>().ok(),
            "episode_title" => sidecar.episode_title = Some(unquote_yaml(value)),
            "episode_count" => sidecar.episode_count = value.parse::<u16>().ok(),
            "runtime_minutes" => sidecar.runtime_minutes = value.parse::<u16>().ok(),
            "average_score" => sidecar.average_score = value.parse::<f32>().ok(),
            "format" => sidecar.format = Some(unquote_yaml(value)),
            "airing_season" => sidecar.airing_season = Some(unquote_yaml(value)),
            "anilist_id" => sidecar.anilist_id = value.parse::<u32>().ok(),
            "anilist_url" => sidecar.anilist_url = Some(unquote_yaml(value)),
            "status" => sidecar.status = parse_media_status(unquote_yaml(value).as_str()),
            "description" => sidecar.description = Some(unquote_yaml(value)),
            "author" => sidecar.author = Some(unquote_yaml(value)),
            "rating_external" => sidecar.rating_external = value.parse::<f32>().ok(),
            _ => {}
        }
    }

    Ok(sidecar)
}

fn unquote_yaml(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let is_double = trimmed.starts_with('"') && trimmed.ends_with('"');
        let is_single = trimmed.starts_with('\'') && trimmed.ends_with('\'');
        if is_double || is_single {
            return trimmed[1..trimmed.len() - 1]
                .replace("\\n", "\n")
                .replace("\\\"", "\"")
                .replace("\\'", "'")
                .replace("\\\\", "\\");
        }
    }
    trimmed.to_string()
}

fn parse_media_type(value: String) -> Option<MediaType> {
    match value.trim().to_lowercase().as_str() {
        "film" => Some(MediaType::Film),
        "series" => Some(MediaType::Series),
        "anime" => Some(MediaType::Anime),
        "hentai-anime" => Some(MediaType::HentaiAnime),
        "book" => Some(MediaType::Book),
        "ebook" => Some(MediaType::Ebook),
        "webnovel" => Some(MediaType::Webnovel),
        "comic" => Some(MediaType::Comic),
        "manga" => Some(MediaType::Manga),
        "music-album" => Some(MediaType::MusicAlbum),
        "music-track" => Some(MediaType::MusicTrack),
        "podcast" => Some(MediaType::Podcast),
        "audiobook" => Some(MediaType::Audiobook),
        "video-game" => Some(MediaType::VideoGame),
        "document" => Some(MediaType::Document),
        "photo" => Some(MediaType::Photo),
        "video-misc" => Some(MediaType::VideoMisc),
        "archive" => Some(MediaType::Archive),
        "image" => Some(MediaType::Image),
        "software" => Some(MediaType::Software),
        "3d-model" => Some(MediaType::Model3D),
        "unclassified" => Some(MediaType::Unclassified),
        _ => None,
    }
}

fn parse_media_status(value: &str) -> Option<MediaStatus> {
    match value.trim().to_lowercase().as_str() {
        "inbox" => Some(MediaStatus::Inbox),
        "needs-review" => Some(MediaStatus::NeedsReview),
        "in-library" => Some(MediaStatus::InLibrary),
        "wishlist" => Some(MediaStatus::Wishlist),
        "completed" => Some(MediaStatus::Completed),
        "on-hold" => Some(MediaStatus::OnHold),
        "archived" => Some(MediaStatus::Archived),
        "ignored" => Some(MediaStatus::Ignored),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Open-with-system API
// ---------------------------------------------------------------------------

#[derive(Serialize)]
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

/// File types `/api/open-external` may hand to the operating system.
///
/// An allowlist rather than a denylist: the vault holds files that arrived
/// from imports and from web downloads, and handing an arbitrary one to `open`
/// is equivalent to double-clicking it — a `.app`, `.command`, `.pkg` or
/// `.terminal` would execute code.
const OPENABLE_EXTENSIONS: &[&str] = &[
    // Video
    "mp4", "m4v", "mkv", "avi", "mov", "webm", "mpg", "mpeg", "wmv", "flv", "ts", "m2ts",
    // Audio
    "mp3", "m4a", "m4b", "flac", "ogg", "opus", "wav", "aac", "wma", "aiff",
    // Documents / books
    "pdf", "epub", "mobi", "azw3", "cbz", "cbr", "txt", "md", "yaml", "yml", "json", "nfo", "srt",
    "vtt", "ass", // Images
    "jpg", "jpeg", "png", "gif", "webp", "bmp", "tiff", "avif",
];

/// Whether a path carries an extension from [`OPENABLE_EXTENSIONS`].
fn is_openable_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .is_some_and(|extension| OPENABLE_EXTENSIONS.contains(&extension.as_str()))
}

fn build_open_external_response(query: Option<&str>) -> OpenExternalResponse {
    let query = match query {
        Some(q) => q,
        None => return OpenExternalResponse::error("missing query"),
    };

    let path = match extract_query_value(query, "path") {
        Some(p) => p,
        None => return OpenExternalResponse::error("missing path"),
    };

    let root_override = extract_query_value(query, "root");
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => return OpenExternalResponse::error("Kein Vault geöffnet."),
        Err(e) => return OpenExternalResponse::error(e.to_string()),
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => return OpenExternalResponse::error(e.to_string()),
    };

    let relative = match RelativePath::new(&path) {
        Ok(r) => r,
        Err(e) => return OpenExternalResponse::error(e.to_string()),
    };

    let absolute = match vault.resolve_existing(relative.as_path()) {
        Ok(p) => p,
        Err(e) => return OpenExternalResponse::error(e.to_string()),
    };

    if !is_openable_extension(&absolute) {
        return OpenExternalResponse::error(
            "Dieser Dateityp wird aus Sicherheitsgründen nicht geöffnet.".to_string(),
        );
    }

    // `open` on macOS launches the file with the default app; equivalent to
    // double-clicking in Finder. This is fire-and-forget — we only care that
    // the process started, not how it exits.
    match std::process::Command::new("open").arg(&absolute).spawn() {
        Ok(_) => OpenExternalResponse::ok(),
        Err(e) => OpenExternalResponse::error(format!("open failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Webnovel subscriptions
// ---------------------------------------------------------------------------

/// Directory inside a novel's vault folder that caches downloaded chapters.
const WEBNOVEL_CHAPTER_CACHE_DIR: &str = ".chapters";

/// Registry of running/finished webnovel check jobs, keyed by job id.
static WEBNOVEL_JOBS: LazyLock<Mutex<HashMap<String, WebnovelJobStatus>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
/// Guards against overlapping check runs (startup, interval, manual).
static WEBNOVEL_CHECK_ACTIVE: AtomicBool = AtomicBool::new(false);
/// Monotonic part of generated job ids.
static WEBNOVEL_JOB_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Progress snapshot of one check job, polled by the frontend.
#[derive(Debug, Clone, Serialize)]
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
    /// Vault-relative path of the cached cover image, when one exists.
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
                    let work_dir = ws.work_dir(MediaKind::Webnovel, subscription);
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
    /// Legacy library root, still used for path-safety checks.
    pub(crate) vault: Vault,
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
    /// [`VaultError::InvalidVaultPath`] carrying the reason to show the user.
    pub(crate) fn delivery_parent(
        &self,
        kind: MediaKind,
        subscription: &Subscription,
    ) -> Result<PathBuf> {
        let own = subscription.target_dir.as_deref().map(Path::new);
        match resolve_target(own, kind, &self.targets, &self.store) {
            TargetResolution::Resolved { parent, .. } => Ok(parent),
            TargetResolution::NeedsChoice { reason, .. } => Err(VaultError::InvalidVaultPath(reason)),
        }
    }
}

impl Workspace {
    /// Work folder for a subscription, or `None` when no target is configured.
    ///
    /// For read-only views: a subscription without a usable target is still
    /// worth listing, just without the things that live in its folder.
    pub(crate) fn work_dir(&self, kind: MediaKind, subscription: &Subscription) -> Option<PathBuf> {
        self.delivery_parent(kind, subscription)
            .ok()
            .map(|parent| match kind {
                MediaKind::Webnovel => webnovel_folder(&parent, subscription),
                _ => parent.join(novel_folder_name(subscription)),
            })
    }
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
/// [`VaultError::Io`] when the file cannot be written.
pub(crate) fn save_target_settings(store: &Path, settings: &TargetSettings) -> Result<()> {
    let body = serde_json::to_string_pretty(settings)
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    fs::create_dir_all(store).map_err(VaultError::from)?;
    fs::write(store.join(TARGET_SETTINGS_FILE), body).map_err(VaultError::from)
}

/// Resolves both locations, or reports which one is missing.
pub(crate) fn resolve_workspace(
    root_override: Option<&str>,
) -> std::result::Result<Workspace, String> {
    let store = match resolve_data_dir() {
        DataDir::Portable(path) | DataDir::Chosen(path) => path,
        DataDir::NeedsSetup { reason, .. } => return Err(reason),
    };
    let root = match resolve_vault_root(root_override) {
        Ok(Some(root)) => root,
        Ok(None) => return Err("Kein Zielordner festgelegt.".to_string()),
        Err(error) => return Err(error.to_string()),
    };
    let vault = Vault::new(root).map_err(|error| error.to_string())?;
    let targets = load_target_settings(&store);
    Ok(Workspace {
        store,
        vault,
        targets,
    })
}

#[derive(Deserialize)]
struct WebnovelSubscribeRequest {
    url: String,
    #[serde(default)]
    root: Option<String>,
}

#[derive(Serialize)]
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
                    ws.work_dir(MediaKind::Webnovel, &existing).as_deref(),
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
                ws.work_dir(MediaKind::Webnovel, &subscription).as_deref(),
            )),
            already_subscribed: false,
            error: None,
        },
        Err(error) => WebnovelSubscribeResponse::error(error.to_string()),
    }
}

#[derive(Deserialize)]
struct WebnovelUnsubscribeRequest {
    id: String,
    /// When false, the novel's vault folder (EPUBs + chapter cache) is removed.
    #[serde(default = "webnovel_default_true")]
    keep_files: bool,
    #[serde(default)]
    root: Option<String>,
}

#[derive(Serialize)]
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
struct WebnovelTrashEntry {
    id: String,
    title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    trashed_at_unix: Option<u64>,
    /// True when the novel's files also sit in the vault trash.
    files_in_trash: bool,
}

#[derive(Serialize)]
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
struct WebnovelUpdateRequest {
    id: String,
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
        return Err(VaultError::ExternalApi(reason));
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
    let cache_dir = novel_dir.join(WEBNOVEL_CHAPTER_CACHE_DIR);
    fs::create_dir_all(&cache_dir).map_err(VaultError::from)?;

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
    let mut fetch_error: Option<VaultError> = None;
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
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
        fs::write(
            cache_dir.join(chapter_cache_name(chapter.index)),
            cache_json,
        )
        .map_err(VaultError::from)?;
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
        build_batch_epub(&novel_dir, subscription, &downloaded_indices)?;
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
        build_complete_epub(&novel_dir, subscription)?;
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
        let raw = fs::read_to_string(&path).map_err(VaultError::from)?;
        let cached: CachedChapter = serde_json::from_str(&raw)
            .map_err(|error| VaultError::Serialization(error.to_string()))?;
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
fn build_batch_epub(novel_dir: &Path, subscription: &Subscription, indices: &[u32]) -> Result<()> {
    let min = indices.iter().min().copied().unwrap_or(0);
    let max = indices.iter().max().copied().unwrap_or(0);
    let safe_title = novel_folder_name(subscription);
    let file_name = if min == max {
        format!("{safe_title} - Kapitel {min:04}.epub")
    } else {
        format!("{safe_title} - Kapitel {min:04}-{max:04}.epub")
    };

    let cache_dir = novel_dir.join(WEBNOVEL_CHAPTER_CACHE_DIR);
    let chapters = load_cached_chapters(&cache_dir, indices)?;

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
        cover: load_novel_cover(&novel_dir),
    };
    write_epub(&novel_dir.join(&file_name), &meta, &chapters)?;

    record_delivery(novel_dir, subscription, &file_name, Some((min, max)))
}

/// Rebuilds the complete EPUB from every cached chapter.
fn build_complete_epub(novel_dir: &Path, subscription: &Subscription) -> Result<()> {
    let indices: Vec<u32> = subscription
        .known_chapters
        .iter()
        .filter(|chapter| chapter.downloaded_at_unix.is_some())
        .map(|chapter| chapter.index)
        .collect();
    if indices.is_empty() {
        return Ok(());
    }

    let cache_dir = novel_dir.join(WEBNOVEL_CHAPTER_CACHE_DIR);
    let chapters = load_cached_chapters(&cache_dir, &indices)?;

    let safe_title = novel_folder_name(subscription);
    let file_name = format!("{safe_title}.epub");
    let meta = EpubMeta {
        title: subscription.title.clone(),
        author: subscription.author.clone(),
        language: "en".to_string(),
        identifier: subscription.url.clone(),
        description: subscription.description.clone(),
        cover: load_novel_cover(&novel_dir),
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
// Interactive Cloudflare solve window
// ---------------------------------------------------------------------------

/// App handle captured from the URI-scheme context (set on first request).
static APP_HANDLE: OnceLock<tauri::AppHandle> = OnceLock::new();

/// Per-host state of the solve flow: `pending`, `done`, `failed:<msg>`.
static SOLVE_STATES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Window label of the solve window (one at a time).
const SOLVE_WINDOW_LABEL: &str = "cf-solve";
/// Fixed browser user agent for the solve window AND the follow-up requests —
/// Cloudflare binds its clearance cookie to the user agent.
const SOLVE_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) \
    AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.4 Safari/605.1.15";
/// How long the user gets to solve the challenge.
const SOLVE_TIMEOUT_SECS: u64 = 240;
/// Poll interval for the (local, cheap) cookie check.
const SOLVE_POLL_SECS: u64 = 3;
/// Grace period before probing sites that never issue an interactive
/// challenge (auto-pass) — probing too early burns a running challenge.
const SOLVE_GRACE_SECS: u64 = 15;
/// Probe attempts after a clearance cookie appeared. Each failed probe can
/// invalidate the clearance again (TLS binding), so we stop early with a
/// clear message instead of looping the user's challenge forever.
const SOLVE_MAX_PROBES: u32 = 3;

fn set_solve_state(host: &str, state: impl Into<String>) {
    if let Ok(mut states) = SOLVE_STATES.lock() {
        states.insert(host.to_string(), state.into());
    }
}

#[derive(Deserialize)]
struct WebnovelSolveRequest {
    url: String,
}

#[derive(Serialize)]
struct WebnovelSolveResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Opens a visible, isolated browser window on the target URL so the user can
/// solve the Cloudflare/captcha challenge manually. A background thread polls
/// the window's cookies; once `cf_clearance` appears the cookies (plus the
/// matching user agent) are stored for this host and the window closes.
///
/// Security: the window has no IPC capabilities and no access to app
/// internals — it is equivalent to opening the site in a normal browser.
fn build_webnovel_solve_response(body: &[u8]) -> WebnovelSolveResponse {
    let req: WebnovelSolveRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return WebnovelSolveResponse {
                host: None,
                error: Some(format!("Invalid request: {error}")),
            }
        }
    };
    if !req.url.starts_with("http://") && !req.url.starts_with("https://") {
        return WebnovelSolveResponse {
            host: None,
            error: Some("Nur http/https-Links erlaubt.".to_string()),
        };
    }
    let Some(host) = host_of(&req.url) else {
        return WebnovelSolveResponse {
            host: None,
            error: Some("URL ohne gültigen Host.".to_string()),
        };
    };
    let Some(handle) = APP_HANDLE.get().cloned() else {
        return WebnovelSolveResponse {
            host: None,
            error: Some("App-Fenster noch nicht bereit — bitte erneut versuchen.".to_string()),
        };
    };
    let Ok(target_url) = req.url.parse::<tauri::Url>() else {
        return WebnovelSolveResponse {
            host: None,
            error: Some("URL konnte nicht geparst werden.".to_string()),
        };
    };

    set_solve_state(&host, "pending");

    // Window creation must happen on the main thread on macOS.
    let build_handle = handle.clone();
    let build_url = target_url.clone();
    let _ = handle.run_on_main_thread(move || {
        use tauri::{WebviewUrl, WebviewWindowBuilder};
        if let Some(existing) = build_handle.get_webview_window(SOLVE_WINDOW_LABEL) {
            let _ = existing.close();
        }
        let _ = WebviewWindowBuilder::new(
            &build_handle,
            SOLVE_WINDOW_LABEL,
            WebviewUrl::External(build_url),
        )
        .title("Sicherheitsprüfung bestätigen — Fenster schließt sich automatisch")
        .inner_size(1024.0, 820.0)
        .user_agent(SOLVE_USER_AGENT)
        .build();
    });

    // Poll on a worker thread: adopt whatever cookies the window has and
    // verify with a real probe request. Cloudflare sometimes waves the
    // WebView through WITHOUT a visible challenge (no cf_clearance cookie at
    // all) — only the probe tells us whether plain requests now pass.
    let poll_host = host.clone();
    let probe_url = req.url.clone();
    std::thread::spawn(move || {
        let started = std::time::Instant::now();
        let deadline = started + std::time::Duration::from_secs(SOLVE_TIMEOUT_SECS);
        let grace = std::time::Duration::from_secs(SOLVE_GRACE_SECS);
        let mut cycle: u64 = 0;
        let mut probes_after_clearance: u32 = 0;

        loop {
            std::thread::sleep(std::time::Duration::from_secs(SOLVE_POLL_SECS));
            cycle += 1;

            let window = handle.get_webview_window(SOLVE_WINDOW_LABEL);

            // Adopt current cookies (whatever they are) + matching UA.
            let mut has_clearance = false;
            if let Some(active) = window.as_ref() {
                if let Ok(cookies) = active.cookies_for_url(target_url.clone()) {
                    has_clearance = cookies.iter().any(|cookie| cookie.name() == "cf_clearance");
                    if !cookies.is_empty() {
                        let header = cookies
                            .iter()
                            .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
                            .collect::<Vec<_>>()
                            .join("; ");
                        set_browser_session(
                            &poll_host,
                            BrowserSession {
                                cookie_header: header,
                                user_agent: SOLVE_USER_AGENT.to_string(),
                            },
                        );
                    }
                }
            }

            // NEVER probe while an interactive challenge is still running:
            // a probe with freshly issued cookies but a different TLS
            // fingerprint makes Cloudflare revoke the clearance — the user
            // then sees the checkbox loop forever. Probe only once the
            // clearance cookie exists, or (auto-pass sites without any
            // interactive challenge) after a grace period, spaced out.
            let should_probe = if has_clearance {
                probes_after_clearance < SOLVE_MAX_PROBES
            } else {
                started.elapsed() >= grace && cycle.is_multiple_of(4)
            };

            if should_probe {
                if solve_probe_passes(&probe_url) {
                    set_solve_state(&poll_host, "done");
                    if let Some(active) = window {
                        let _ = active.close();
                    }
                    return;
                }
                if has_clearance {
                    probes_after_clearance += 1;
                    if probes_after_clearance >= SOLVE_MAX_PROBES {
                        set_solve_state(
                            &poll_host,
                            "failed:Die Freigabe ist strikt an das Browserfenster gebunden —                              App-Downloads bleiben für diese Seite blockiert.",
                        );
                        if let Some(active) = window {
                            let _ = active.close();
                        }
                        return;
                    }
                }
            }

            if window.is_none() {
                // Window closed by the user: one final probe decides.
                if solve_probe_passes(&probe_url) {
                    set_solve_state(&poll_host, "done");
                } else {
                    set_solve_state(
                        &poll_host,
                        "failed:Fenster geschlossen, Zugriff weiterhin blockiert.",
                    );
                }
                return;
            }
            if std::time::Instant::now() >= deadline {
                set_solve_state(&poll_host, "failed:Zeitüberschreitung.");
                if let Some(active) = handle.get_webview_window(SOLVE_WINDOW_LABEL) {
                    let _ = active.close();
                }
                return;
            }
        }
    });

    WebnovelSolveResponse {
        host: Some(host),
        error: None,
    }
}

/// One plain request against the blocked URL — success means the stored
/// session (or the now-trusted client) passes without a challenge.
fn solve_probe_passes(url: &str) -> bool {
    match PoliteClient::new() {
        Ok(client) => client.get_text(url).is_ok(),
        Err(_) => false,
    }
}

#[derive(Serialize)]
struct WebnovelSolveStatusResponse {
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn build_webnovel_solve_status_response(query: Option<&str>) -> WebnovelSolveStatusResponse {
    let host = query
        .and_then(|q| extract_query_value(q, "host"))
        .unwrap_or_default();
    let raw = SOLVE_STATES
        .lock()
        .ok()
        .and_then(|states| states.get(&host).cloned())
        .unwrap_or_else(|| "unknown".to_string());
    match raw.strip_prefix("failed:") {
        Some(message) => WebnovelSolveStatusResponse {
            state: "failed".to_string(),
            message: Some(message.to_string()),
        },
        None => WebnovelSolveStatusResponse {
            state: raw,
            message: None,
        },
    }
}

// ---------------------------------------------------------------------------
// Site login (NovelUpdates & co. — session captured from a visible window)
// ---------------------------------------------------------------------------
//
// Some sites (NovelUpdates) only expose the real release/chapter links to
// logged-in users. The user signs in through a visible, sandboxed browser
// window; because every app webview shares the same cookie store, the routed
// render window is then logged in too. We additionally capture the login
// cookies into a persisted `BrowserSession` so plain requests carry them and
// the login survives an app restart.

/// Window label of the login window (one at a time).
const LOGIN_WINDOW_LABEL: &str = "mv-login";
/// How long the login window stays watched before giving up.
const LOGIN_TIMEOUT_SECS: u64 = 900;
/// Per-host login state: `pending`, `done`, `failed:<msg>`.
static LOGIN_STATES: LazyLock<Mutex<HashMap<String, String>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn set_login_state(host: &str, state: impl Into<String>) {
    if let Ok(mut states) = LOGIN_STATES.lock() {
        states.insert(host.to_lowercase(), state.into());
    }
}

/// The sign-in page for a host (site root when unknown).
fn login_url_for(host: &str) -> String {
    if host.ends_with("novelupdates.com") {
        "https://www.novelupdates.com/login/".to_string()
    } else {
        format!("https://{host}/")
    }
}

/// Whether a cookie name marks an authenticated session. NovelUpdates runs on
/// WordPress (`wordpress_logged_in_<hash>`); a few common others are covered
/// for generic sites.
fn is_login_cookie(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("logged_in") || lower == "sessionid" || lower.starts_with("wordpress_sec")
}

/// Path of the persisted session store (`~/.fero/webnovel_sessions.json`).
fn webnovel_sessions_path() -> Option<PathBuf> {
    debug_log_path().and_then(|p| p.parent().map(|dir| dir.join("webnovel_sessions.json")))
}

#[derive(Serialize, Deserialize)]
struct StoredSession {
    host: String,
    cookie_header: String,
    user_agent: String,
}

fn load_stored_sessions() -> Vec<StoredSession> {
    webnovel_sessions_path()
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Persists the session store.
///
/// The file holds live login cookies for third-party sites, so it is written
/// with owner-only permissions — the default `0644` would expose every
/// captured session to any other user or process on the machine.
fn write_stored_sessions(sessions: &[StoredSession]) {
    let Some(path) = webnovel_sessions_path() else {
        return;
    };
    if let Ok(json) = serde_json::to_string_pretty(sessions) {
        let _ = write_private_file(&path, &json);
    }
}

/// Persists (and activates) a captured login session for a host.
fn persist_session(host: &str, session: &BrowserSession) {
    let host = host.to_lowercase();
    let mut all = load_stored_sessions();
    all.retain(|entry| entry.host != host);
    all.push(StoredSession {
        host: host.clone(),
        cookie_header: session.cookie_header.clone(),
        user_agent: session.user_agent.clone(),
    });
    write_stored_sessions(&all);
    set_browser_session(&host, session.clone());
}

/// Loads persisted sessions into the RAM store (called once at startup).
/// Moves subscriptions out of the library into Fero's data directory.
///
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

fn restore_webnovel_sessions() {
    // Tighten permissions on stores written by older versions, which created
    // the file with the default (world-readable) mode.
    if let Some(path) = webnovel_sessions_path() {
        if path.exists() {
            restrict_to_owner(&path, false);
        }
        if let Some(parent) = path.parent() {
            restrict_to_owner(parent, true);
        }
    }

    for entry in load_stored_sessions() {
        set_browser_session(
            &entry.host,
            BrowserSession {
                cookie_header: entry.cookie_header,
                user_agent: entry.user_agent,
            },
        );
    }
}

/// Drops a host's persisted + active session (logout).
fn drop_session(host: &str) {
    let host = host.to_lowercase();
    let mut all = load_stored_sessions();
    all.retain(|entry| entry.host != host);
    write_stored_sessions(&all);
    clear_browser_session(&host);
}

#[derive(Deserialize)]
struct WebnovelLoginRequest {
    /// Host to log in to (e.g. "novelupdates.com"); a URL is also accepted.
    host: String,
}

#[derive(Serialize)]
struct WebnovelLoginResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Opens a visible login window for a host and captures the session cookies as
/// soon as the user is signed in.
fn build_webnovel_login_response(body: &[u8]) -> WebnovelLoginResponse {
    let req: WebnovelLoginRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return WebnovelLoginResponse {
                host: None,
                error: Some(format!("Invalid request: {error}")),
            }
        }
    };
    // Accept either a bare host or a full URL.
    let host = host_of(&req.host).unwrap_or_else(|| req.host.trim().to_lowercase());
    if host.is_empty() || host.contains(' ') {
        return WebnovelLoginResponse {
            host: None,
            error: Some("Ungültiger Host.".to_string()),
        };
    }
    let Some(handle) = APP_HANDLE.get().cloned() else {
        return WebnovelLoginResponse {
            host: None,
            error: Some("App-Fenster noch nicht bereit — bitte erneut versuchen.".to_string()),
        };
    };
    let Ok(login_url) = login_url_for(&host).parse::<tauri::Url>() else {
        return WebnovelLoginResponse {
            host: None,
            error: Some("Login-URL konnte nicht gebildet werden.".to_string()),
        };
    };

    set_login_state(&host, "pending");
    debug_log(&format!("login: Fenster geöffnet für {host}"));

    // Window creation must run on the main thread on macOS.
    let build_handle = handle.clone();
    let build_url = login_url.clone();
    let _ = handle.run_on_main_thread(move || {
        use tauri::{WebviewUrl, WebviewWindowBuilder};
        if let Some(existing) = build_handle.get_webview_window(LOGIN_WINDOW_LABEL) {
            let _ = existing.close();
        }
        let _ = WebviewWindowBuilder::new(
            &build_handle,
            LOGIN_WINDOW_LABEL,
            WebviewUrl::External(build_url),
        )
        .title("Anmelden — nach dem Login kannst du dieses Fenster schließen")
        .inner_size(1024.0, 820.0)
        .user_agent(SOLVE_USER_AGENT)
        .build();
    });

    // Poll the window's cookies; capture the session once a login cookie shows
    // up. The window stays open (2FA, verification) until the user closes it.
    let poll_host = host.clone();
    let cookie_url = login_url.clone();
    std::thread::spawn(move || {
        let deadline =
            std::time::Instant::now() + std::time::Duration::from_secs(LOGIN_TIMEOUT_SECS);
        let mut captured = false;
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            let window = handle.get_webview_window(LOGIN_WINDOW_LABEL);

            if let Some(active) = window.as_ref() {
                if let Ok(cookies) = active.cookies_for_url(cookie_url.clone()) {
                    let logged_in = cookies.iter().any(|cookie| is_login_cookie(cookie.name()));
                    if logged_in {
                        let header = cookies
                            .iter()
                            .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
                            .collect::<Vec<_>>()
                            .join("; ");
                        persist_session(
                            &poll_host,
                            &BrowserSession {
                                cookie_header: header,
                                user_agent: SOLVE_USER_AGENT.to_string(),
                            },
                        );
                        if !captured {
                            captured = true;
                            debug_log(&format!("login: {poll_host} — Sitzung erfasst"));
                        }
                        set_login_state(&poll_host, "done");
                    }
                }
            }

            if window.is_none() {
                // Window closed by the user; keep whatever we captured.
                if !captured {
                    set_login_state(
                        &poll_host,
                        "failed:Fenster geschlossen — kein Login erkannt.",
                    );
                    debug_log(&format!(
                        "login: {poll_host} — Fenster ohne Login geschlossen"
                    ));
                }
                return;
            }
            if std::time::Instant::now() >= deadline {
                if !captured {
                    set_login_state(&poll_host, "failed:Zeitüberschreitung.");
                }
                if let Some(active) = handle.get_webview_window(LOGIN_WINDOW_LABEL) {
                    let _ = active.close();
                }
                return;
            }
        }
    });

    WebnovelLoginResponse {
        host: Some(host),
        error: None,
    }
}

#[derive(Serialize)]
struct WebnovelLoginStatusResponse {
    logged_in: bool,
    state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn build_webnovel_login_status_response(query: Option<&str>) -> WebnovelLoginStatusResponse {
    let host = query
        .and_then(|q| extract_query_value(q, "host"))
        .map(|h| host_of(&h).unwrap_or(h).to_lowercase())
        .unwrap_or_default();
    let raw = LOGIN_STATES
        .lock()
        .ok()
        .and_then(|states| states.get(&host).cloned())
        .unwrap_or_else(|| "unknown".to_string());
    // A persisted session for the host means "logged in" across restarts.
    let logged_in = load_stored_sessions()
        .iter()
        .any(|entry| entry.host == host);
    match raw.strip_prefix("failed:") {
        Some(message) => WebnovelLoginStatusResponse {
            logged_in,
            state: "failed".to_string(),
            message: Some(message.to_string()),
        },
        None => WebnovelLoginStatusResponse {
            logged_in,
            state: raw,
            message: None,
        },
    }
}

fn build_webnovel_logout_response(body: &[u8]) -> WebnovelSimpleResponse {
    let req: WebnovelLoginRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return WebnovelSimpleResponse::error(format!("Invalid request: {error}")),
    };
    let host = host_of(&req.host).unwrap_or_else(|| req.host.trim().to_lowercase());
    drop_session(&host);
    set_login_state(&host, "unknown");
    debug_log(&format!("logout: {host} — Sitzung entfernt"));
    WebnovelSimpleResponse::ok()
}

// ---------------------------------------------------------------------------
// Embedded-browser fetch engine (whitelisted, manual-only)
// ---------------------------------------------------------------------------
//
// For hosts in `WEBVIEW_ROUTED_HOSTS` a plain HTTP request either can't pass
// Cloudflare (clearance is TLS-fingerprint-bound) or never sees the content
// (rendered client-side by JavaScript). We drive a visible, sandboxed browser
// window page-by-page and read the fully-rendered HTML back through the window
// TITLE — the only Rust-readable channel that is NOT sent to the server (unlike
// cookies, which overflow request headers). An injected script gzip+hex-encodes
// the rendered `outerHTML` into a JS variable; Rust pulls it in chunks by
// eval-ing "set title to chunk i" and reading the title. The window has no
// IPC / app access whatsoever.

#[derive(Serialize)]
struct WebnovelDebugLogResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    content: String,
}

/// Returns the debug-log path and its (tail) content for the UI.
fn build_webnovel_debug_log_response() -> WebnovelDebugLogResponse {
    let path = debug_log_path();
    let content = path
        .as_ref()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
    // Only the last ~400 lines are useful and keep the payload small.
    let tail: Vec<&str> = content.lines().rev().take(400).collect();
    let content = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    WebnovelDebugLogResponse {
        path: path.map(|p| p.to_string_lossy().to_string()),
        content,
    }
}

/// Opens the debug-log file in the OS default application.
fn build_open_debug_log_response() -> WebnovelSimpleResponse {
    let Some(path) = debug_log_path() else {
        return WebnovelSimpleResponse::error("Log-Pfad nicht ermittelbar.");
    };
    // Make sure the file exists so the OS has something to open.
    if !path.exists() {
        debug_log("open-debug-log: Datei angelegt (war leer)");
    }
    let path_str = path.to_string_lossy().to_string();

    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(&path_str).spawn();
    #[cfg(target_os = "windows")]
    let result = std::process::Command::new("cmd")
        .args(["/C", "start", "", &path_str])
        .spawn();
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let result = std::process::Command::new("xdg-open")
        .arg(&path_str)
        .spawn();

    match result {
        Ok(_) => WebnovelSimpleResponse::ok(),
        Err(error) => WebnovelSimpleResponse::error(format!("Öffnen fehlgeschlagen: {error}")),
    }
}

/// Debug-log path (`~/.fero/webnovel_debug.log`).
fn debug_log_path() -> Option<PathBuf> {
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))?;
    Some(PathBuf::from(home).join(".fero").join("webnovel_debug.log"))
}

/// Size at which the debug log is rotated to `<name>.1`.
const DEBUG_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Appends a timestamped line to the webnovel debug log (best effort).
///
/// The log records every URL the fetcher visits, i.e. a complete reading
/// history — it is therefore owner-readable only and rotated at
/// [`DEBUG_LOG_MAX_BYTES`] so it cannot grow without bound.
pub(crate) fn debug_log(message: &str) {
    let Some(path) = debug_log_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent);
    }

    // Rotate before appending: one previous generation is kept.
    if fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0) >= DEBUG_LOG_MAX_BYTES {
        let rotated = path.with_extension("log.1");
        let _ = fs::remove_file(&rotated);
        let _ = fs::rename(&path, &rotated);
    }

    let line = format!("[{}] {message}\n", unix_now());
    use std::io::Write;
    if let Ok(mut file) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        restrict_to_owner(&path, false);
        let _ = file.write_all(line.as_bytes());
    }
}

/// Label of the persistent fetch/browser window.
const BROWSER_WINDOW_LABEL: &str = "mv-browser";
/// Hard cap for rendering a single page (excluding manual challenge time).
const RENDER_TIMEOUT_SECS: u64 = 45;
/// How long the user may take to solve an in-window challenge per page.
const CHALLENGE_WAIT_SECS: u64 = 180;
/// Characters of hex per title chunk pulled from the window.
const TITLE_CHUNK_LEN: usize = 4000;
/// Largest hex payload accepted from the browser window (≈8 MB of page HTML
/// before compression). The relay script runs inside the foreign page, so the
/// page can replace it and announce any size it likes.
const MAX_RELAY_HEX_LEN: usize = 16 * 1024 * 1024;
/// Cap for the decompressed HTML, so a crafted gzip stream cannot exhaust
/// memory.
const MAX_RENDERED_HTML_BYTES: u64 = 32 * 1024 * 1024;

/// Reflects the current browser-fetch state to the running job's message.
static CURRENT_JOB_ID: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

fn set_current_job_id(job_id: Option<String>) {
    if let Ok(mut guard) = CURRENT_JOB_ID.lock() {
        *guard = job_id;
    }
}

fn note_browser_status(message: &str) {
    let job_id = CURRENT_JOB_ID.lock().ok().and_then(|g| g.clone());
    if let Some(job_id) = job_id {
        let owned = message.to_string();
        update_webnovel_job(&job_id, move |status| status.message = Some(owned));
    }
}

/// Injected once per navigation. Runs in the foreign page context, has no
/// access to the app. Exposes `window.__mvGet(kind, i)` for the Rust puller
/// and builds the encoded payload after the page settles.
const BROWSER_RELAY_SCRIPT: &str = r#"
(function(){
  if (window.__mvInstalled) { return; }
  window.__mvInstalled = true;
  window.__mvData = null;
  window.__mvMode = 'gz';
  window.__mvPath = '';
  function isChallenge(){
    var t=(document.title||'').toLowerCase();
    if(t.indexOf('just a moment')>=0) return true;
    if(t.indexOf('attention required')>=0) return true;
    if(document.querySelector('#challenge-running,#cf-challenge-running,.cf-browser-verification,#challenge-form,#turnstile-wrapper')) return true;
    return false;
  }
  window.__mvGet = function(kind, i){
    try {
      if (isChallenge()) { return 'CH'; }
      if (window.__mvData === null) { return 'WAIT'; }
      if (kind === 'meta') { return 'READY:' + window.__mvData.length + ':' + window.__mvMode + ':' + (window.__mvPath||''); }
      return window.__mvData.substr(i * 4000, 4000);
    } catch(e) { return 'ERR'; }
  };
  async function build(){
    try{
      if (isChallenge()) { done=false; return; }
      var html=document.documentElement.outerHTML;
      html=html.replace(/<script[\s\S]*?<\/script>/gi,'');
      html=html.replace(/<style[\s\S]*?<\/style>/gi,'');
      html=html.replace(/<svg[\s\S]*?<\/svg>/gi,'');
      html=html.replace(/<!--[\s\S]*?-->/g,'');
      var enc=new TextEncoder().encode(html);
      var mode='raw'; var bytes=enc;
      if(typeof CompressionStream!=='undefined'){
        try{
          var cs=new CompressionStream('gzip');
          var w=cs.writable.getWriter(); w.write(enc); w.close();
          var ab=await new Response(cs.readable).arrayBuffer();
          bytes=new Uint8Array(ab); mode='gz';
        }catch(e){ bytes=enc; mode='raw'; }
      }
      var d='0123456789abcdef'; var hex='';
      for(var i=0;i<bytes.length;i++){ hex+=d[(bytes[i]>>>4)&15]+d[bytes[i]&15]; }
      window.__mvData=hex; window.__mvMode=mode; window.__mvPath=location.pathname;
    }catch(e){ window.__mvData=''; window.__mvMode='err'; }
  }
  // Capture once the DOM has quiesced AND carries enough text: SPA chapter
  // bodies (Next.js) arrive via an async fetch that resolves after the initial
  // render settles, so waiting for mere DOM-stillness fires too early on a
  // near-empty skeleton. Gate on visible-text volume; a hard cap still
  // guarantees delivery (short chapters, pages that never fill).
  var done=false, settleTimer=null;
  function textLen(){
    try { return ((document.body && document.body.innerText) || '').replace(/\s+/g,'').length; }
    catch(e){ return 999999; }
  }
  function fire(){
    if(done) return;
    if(isChallenge()){ setTimeout(bump, 1000); return; }
    done=true; build();
  }
  function bump(){
    if(done) return;
    if(settleTimer){ clearTimeout(settleTimer); }
    settleTimer=setTimeout(function(){
      if(done) return;
      if(isChallenge()){ setTimeout(bump, 1000); return; }
      // Enough text → capture; otherwise keep polling for the lazy body.
      if(textLen() >= 1500){ fire(); } else { bump(); }
    }, 900);
  }
  function start(){
    try{
      var obs=new MutationObserver(bump);
      obs.observe(document.documentElement,{childList:true,subtree:true,characterData:true});
    }catch(e){}
    bump();
    // Hard cap: deliver whatever is present so extraction/diagnostics can run.
    setTimeout(function(){ if(!done){ done=true; build(); } }, 15000);
  }
  if(document.readyState==='complete'){ start(); }
  else { window.addEventListener('load', start); }
})();
"#;

/// Decodes a lowercase-hex string into bytes.
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = Vec::with_capacity(hex.len() / 2);
    let nibble = |c: u8| -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    };
    let mut i = 0;
    while i < bytes.len() {
        let hi = nibble(bytes[i])?;
        let lo = nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

/// Runs a closure on the main thread and returns its value (needed because
/// webview title/eval calls must run there).
fn on_main_thread<T, F>(handle: &tauri::AppHandle, f: F) -> Option<T>
where
    F: FnOnce(&tauri::AppHandle) -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    let inner = handle.clone();
    if handle
        .run_on_main_thread(move || {
            let _ = tx.send(f(&inner));
        })
        .is_err()
    {
        return None;
    }
    rx.recv_timeout(std::time::Duration::from_secs(5)).ok()
}

/// Navigates the browser window to `url` (creating it on first use).
fn navigate_browser_window(handle: &tauri::AppHandle, url: &tauri::Url) {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    if let Some(window) = handle.get_webview_window(BROWSER_WINDOW_LABEL) {
        let target = serde_json::to_string(url.as_str()).unwrap_or_else(|_| "\"\"".to_string());
        // Invalidate the current page's payload before navigating so a poll
        // that races the navigation reads WAIT, never the previous page.
        let _ = window.eval(format!(
            "try{{window.__mvData=null;window.__mvPath='';}}catch(e){{}}window.location.href = {target};"
        ));
    } else {
        let _ = WebviewWindowBuilder::new(
            handle,
            BROWSER_WINDOW_LABEL,
            WebviewUrl::External(url.clone()),
        )
        .title("Fero-Browser — lädt Novel-Seiten (bitte geöffnet lassen)")
        .inner_size(1024.0, 820.0)
        .user_agent(SOLVE_USER_AGENT)
        .initialization_script(BROWSER_RELAY_SCRIPT)
        .build();
    }
}

/// Asks the window for `__mvGet(kind, i)` and returns the value.
///
/// The value is written to BOTH the URL hash and the window title, and read
/// back from whichever channel carries our request nonce. The hash survives
/// SPA title management (React/Next.js reset `document.title`) and is never
/// sent to the server; the title is a fallback for engines where the hash
/// isn't reflected by `url()`. A per-request nonce guards against stale reads.
fn pull_from_title(handle: &tauri::AppHandle, kind: &str, index: usize) -> Option<String> {
    let nonce = format!("{}", unix_now_ms());
    let arg = if kind == "meta" {
        "'meta',0".to_string()
    } else {
        format!("'chunk',{index}")
    };
    let script = format!(
        "try{{var v='MV:{nonce}:'+window.__mvGet({arg});location.hash=v;document.title=v;}}catch(e){{location.hash='MV:{nonce}:ERR';}}"
    );
    let _ = on_main_thread(handle, move |h| {
        if let Some(window) = h.get_webview_window(BROWSER_WINDOW_LABEL) {
            let _ = window.eval(&script);
        }
    });
    let prefix = format!("MV:{nonce}:");
    for _ in 0..25 {
        std::thread::sleep(std::time::Duration::from_millis(120));
        let read = on_main_thread(handle, |h| {
            let window = h.get_webview_window(BROWSER_WINDOW_LABEL)?;
            // Prefer the hash; fall back to the title.
            let from_hash = window
                .url()
                .ok()
                .and_then(|url| url.fragment().map(str::to_string));
            let from_title = window.title().ok();
            Some((from_hash, from_title))
        })
        .flatten();
        if let Some((from_hash, from_title)) = read {
            for candidate in [from_hash, from_title].into_iter().flatten() {
                if let Some(rest) = candidate.strip_prefix(&prefix) {
                    return Some(rest.to_string());
                }
            }
        }
    }
    None
}

/// Milliseconds since the UNIX epoch (title nonce).
fn unix_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Closes the browser window at the end of a manual run.
pub(crate) fn close_browser_window() {
    if let Some(handle) = APP_HANDLE.get() {
        let handle = handle.clone();
        let inner = handle.clone();
        let _ = handle.run_on_main_thread(move || {
            if let Some(window) = inner.get_webview_window(BROWSER_WINDOW_LABEL) {
                let _ = window.close();
            }
        });
    }
}

/// Fetches a page's fully-rendered HTML through the browser window.
///
/// Blocks the calling (worker) thread until the injected relay delivers the
/// page via the window title, a challenge times out, or rendering times out.
pub(crate) fn render_page_via_window(url: &str) -> Result<String> {
    let handle = APP_HANDLE
        .get()
        .cloned()
        .ok_or_else(|| VaultError::ExternalApi("App-Fenster noch nicht bereit.".to_string()))?;
    let target = url
        .parse::<tauri::Url>()
        .map_err(|_| VaultError::ExternalApi(format!("URL ungültig: {url}")))?;

    debug_log(&format!("render: navigate → {url}"));
    let nav_url = target.clone();
    let _ = on_main_thread(&handle, move |h| navigate_browser_window(h, &nav_url));

    let start = std::time::Instant::now();
    let mut challenge_seen = false;
    let mut last_meta = String::new();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(800));

        // Give the window a grace period to appear (created async on main).
        if handle.get_webview_window(BROWSER_WINDOW_LABEL).is_none() {
            if start.elapsed() < std::time::Duration::from_secs(8) {
                continue;
            }
            debug_log("render: FAIL Fenster nicht vorhanden");
            return Err(VaultError::ExternalApi(
                "Browserfenster wurde geschlossen.".to_string(),
            ));
        }

        let meta = pull_from_title(&handle, "meta", 0).unwrap_or_else(|| "<none>".to_string());
        if meta != last_meta {
            debug_log(&format!(
                "render: meta='{meta}' (t={}s)",
                start.elapsed().as_secs()
            ));
            last_meta = meta.clone();
        }
        if meta == "CH" {
            challenge_seen = true;
            note_browser_status(
                "Bitte die Sicherheitsprüfung im Browserfenster bestätigen (Fenster offen lassen) …",
            );
            if start.elapsed() >= std::time::Duration::from_secs(CHALLENGE_WAIT_SECS) {
                debug_log("render: FAIL Challenge-Timeout");
                return Err(VaultError::ExternalApi(
                    "Zeitüberschreitung bei der Sicherheitsprüfung im Fenster.".to_string(),
                ));
            }
            continue;
        }
        if let Some(ready) = meta.strip_prefix("READY:") {
            let mut parts = ready.split(':');
            let total: usize = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let mode = parts.next().unwrap_or("gz").to_string();
            let page_path = parts.next().unwrap_or("").to_string();
            if total == 0 {
                continue;
            }
            if total > MAX_RELAY_HEX_LEN {
                debug_log(&format!("render: FAIL Payload zu groß ({total} Zeichen)"));
                return Err(VaultError::ExternalApi(
                    "Die Seite hat eine unerwartet große Antwort geliefert.".to_string(),
                ));
            }
            // Reject a capture that belongs to the previously loaded page: the
            // relay reports its own `location.pathname`, which must match the
            // page we navigated to. Guards against reading stale content while
            // an SPA navigation is still in flight.
            if !page_path.is_empty() {
                let want = target.path().trim_end_matches('/');
                let got = page_path.trim_end_matches('/');
                if want != got {
                    debug_log(&format!(
                        "render: stale Seite path='{page_path}' erwartet='{}' → warte",
                        target.path()
                    ));
                    continue;
                }
            }
            // Content is ready — drop any lingering challenge hint so the
            // normal progress line shows again.
            note_browser_status("");
            // Pull the hex payload chunk by chunk over the title.
            let chunk_count = total.div_ceil(TITLE_CHUNK_LEN);
            debug_log(&format!(
                "render: READY total={total} mode={mode} chunks={chunk_count}"
            ));
            let mut hex = String::with_capacity(total);
            let mut ok = true;
            for i in 0..chunk_count {
                match pull_from_title(&handle, "chunk", i) {
                    Some(chunk) if chunk != "ERR" && chunk != "WAIT" => hex.push_str(&chunk),
                    other => {
                        debug_log(&format!(
                            "render: chunk {i}/{chunk_count} fehlgeschlagen (got={:?})",
                            other.as_deref().unwrap_or("<none>")
                        ));
                        ok = false;
                        break;
                    }
                }
            }
            if !ok || hex.len() != total {
                debug_log(&format!(
                    "render: unvollständig ok={ok} hex.len={} erwartet={total} → retry",
                    hex.len()
                ));
                // Content changed mid-pull (SPA still rendering) — retry.
                if start.elapsed() >= std::time::Duration::from_secs(RENDER_TIMEOUT_SECS) {
                    debug_log("render: FAIL unvollständig nach Timeout");
                    return Err(VaultError::ExternalApi(
                        "Seite konnte nicht vollständig gelesen werden.".to_string(),
                    ));
                }
                continue;
            }
            let bytes = hex_decode(&hex).ok_or_else(|| {
                debug_log("render: FAIL hex-decode");
                VaultError::ExternalApi("Ungültige Daten aus dem Browserfenster.".to_string())
            })?;
            let html = if mode == "gz" {
                use std::io::Read;
                // `take` bounds the *decompressed* size — the compression
                // ratio is chosen by the remote page, not by us.
                let mut decoder =
                    flate2::read::GzDecoder::new(&bytes[..]).take(MAX_RENDERED_HTML_BYTES);
                let mut text = String::new();
                decoder.read_to_string(&mut text).map_err(|e| {
                    debug_log(&format!("render: FAIL gunzip: {e}"));
                    VaultError::ExternalApi(format!("Dekomprimierung fehlgeschlagen: {e}"))
                })?;
                text
            } else {
                String::from_utf8(bytes)
                    .map_err(|e| VaultError::ExternalApi(format!("Ungültiges UTF-8: {e}")))?
            };
            debug_log(&format!("render: OK html.len={}", html.len()));
            return Ok(html);
        }

        // meta == "WAIT"/"ERR"/empty → keep waiting up to the render budget.
        let budget = if challenge_seen {
            CHALLENGE_WAIT_SECS
        } else {
            RENDER_TIMEOUT_SECS
        };
        if start.elapsed() >= std::time::Duration::from_secs(budget) {
            debug_log(&format!("render: FAIL Timeout (letztes meta='{meta}')"));
            return Err(VaultError::ExternalApi(
                "Seite konnte im Browserfenster nicht gerendert werden (Timeout).".to_string(),
            ));
        }
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
