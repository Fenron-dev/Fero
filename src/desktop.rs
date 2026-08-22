//! Desktop shell bootstrap for Fero.

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::api::anilist::{AniListAnimeMetadata, AniListClient};
use crate::api::audible::{AudibleClient, AudibleSearchResponse};
use crate::api::audiobookshelf::AbsClient;
use crate::api::goodreads::GoodreadsClient;
use crate::api::novel::{
    clear_browser_session, detect_image_media_type, detect_source, host_of, is_webview_routed,
    sanitize_to_xhtml, set_browser_session, BrowserSession, ChapterContent, ChapterRef,
    PoliteClient,
};
use crate::core::duplicate::compute_fingerprint;
use crate::core::duplicate::compute_fingerprint_for_file;
use crate::core::epub::{write_epub, EpubChapter, EpubCover, EpubMeta};
use crate::core::import::{
    ClassificationSource, DuplicatePolicy, FileClassification, ImportConfig, ImportPlan,
    ImportPlanItem, ImportPlanner, IncomingFile, PlannedImportStep, ResolvedMetadata, UserPrompt,
};
use crate::core::playlist::{
    delete_playlist, list_playlists, load_cursor, load_playlist, save_cursor, save_playlist,
    Playlist, PlaylistCursor,
};
use crate::core::progress::{
    delete_progress, list_in_progress, load_progress, save_progress, MediaProgress, ProgressRecord,
};
use crate::core::properties::{
    legacy_sidecar_path_for, render_sidecar_yaml, sidecar_path_for, SIDECAR_SUFFIX,
};
use crate::core::vault::{RelativePath, Vault};
use crate::core::webnovel::{
    blocked_reason, list_subscriptions, list_trashed_subscriptions, load_blocklist_entries,
    load_subscription, normalize_url, purge_trashed_subscription, restore_subscription,
    save_subscription, save_user_blocklist, trash_subscription, unix_now, BlocklistEntry,
    KnownChapter, Subscription,
};
use crate::error::{Result, VaultError};
use crate::media::{MediaEntry, MediaStatus, MediaType, PropertySource};
use serde::{Deserialize, Serialize};
use tauri::http::{header::CONTENT_TYPE, Request, Response, StatusCode};
use tauri::Manager;

const PROTOCOL_SCHEME: &str = "fero";
const LEGACY_SYSTEM_DIR: &str = ".mediashelf";
/// In-vault trash folder; deleted files move here (reversible) preserving
/// their original relative path.
const TRASH_DIR: &str = ".trash";
const LEGACY_SIDECAR_SUFFIX: &str = ".mediashelf.yaml";
const ANILIST_CACHE_FILE: &str = "anilist_cache.json";

const INBOX_SUBFOLDERS: &[&str] = &[
    "Unsortiert",
    "Anime/TV",
    "Anime/Filme",
    "Serien",
    "Filme",
    "Musik",
    "Bücher",
    "Hörbücher",
    "Manga",
    "Comics",
    "TTRPG",
    "Games",
];

type AniListCacheMap = HashMap<String, AniListAnimeMetadata>;

/// Permission bits for files that hold credentials or browsing history:
/// readable and writable by the owner only.
#[cfg(unix)]
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

fn anilist_cache_path() -> Option<PathBuf> {
    let home = env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))?;
    Some(
        PathBuf::from(home)
            .join(".fero")
            .join(ANILIST_CACHE_FILE),
    )
}

fn load_anilist_cache() -> AniListCacheMap {
    let path = match anilist_cache_path() {
        Some(p) => p,
        None => return HashMap::new(),
    };
    let raw = match fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => return HashMap::new(),
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

fn save_anilist_cache(cache: &AniListCacheMap) {
    let Some(path) = anilist_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent);
    }
    if let Ok(body) = serde_json::to_string(cache) {
        let _ = fs::write(path, body);
    }
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
                    // sessions (NovelUpdates etc.) into the RAM store then.
                    restore_webnovel_sessions();
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
        "/api/vault-plan" => {
            json_response(StatusCode::OK, &build_plan_response(request.uri().query()))
        }
        "/api/anilist-search" => json_response(
            StatusCode::OK,
            &build_anilist_search_response(request.uri().query()),
        ),
        "/api/audible-search" => json_response(
            StatusCode::OK,
            &build_audible_search_response(request.uri().query()),
        ),
        "/api/select-folder" => json_response(StatusCode::OK, &build_select_folder_response()),
        "/api/create-vault" => json_response(
            StatusCode::OK,
            &build_create_vault_response(request.uri().query()),
        ),
        "/api/vault-root" => json_response(
            StatusCode::OK,
            &build_vault_root_response(request.uri().query()),
        ),
        "/api/media-file" => {
            let range = request
                .headers()
                .get("range")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            build_media_file_response(request.uri().query(), range.as_deref())
        }
        "/api/apply-import" => {
            json_response(StatusCode::OK, &build_apply_import_response(request.body()))
        }
        "/api/save-sidecars" => json_response(
            StatusCode::OK,
            &build_save_sidecars_response(request.body()),
        ),
        "/api/delete-files" => {
            json_response(StatusCode::OK, &build_delete_files_response(request.body()))
        }
        "/api/cleanup-vault" => json_response(
            StatusCode::OK,
            &build_cleanup_vault_response(request.uri().query()),
        ),
        "/api/progress/save" => json_response(
            StatusCode::OK,
            &build_save_progress_response(request.body()),
        ),
        "/api/progress/load" => json_response(
            StatusCode::OK,
            &build_load_progress_response(request.uri().query()),
        ),
        "/api/progress/delete" => json_response(
            StatusCode::OK,
            &build_delete_progress_response(request.body()),
        ),
        "/api/progress/list" => json_response(
            StatusCode::OK,
            &build_list_progress_response(request.uri().query()),
        ),
        "/api/open-external" => json_response(
            StatusCode::OK,
            &build_open_external_response(request.uri().query()),
        ),
        "/api/open-vault-root" => json_response(
            StatusCode::OK,
            &build_open_vault_root_response(request.uri().query()),
        ),
        "/api/playlist/list" => json_response(
            StatusCode::OK,
            &build_list_playlists_response(request.uri().query()),
        ),
        "/api/playlist/get" => json_response(
            StatusCode::OK,
            &build_get_playlist_response(request.uri().query()),
        ),
        "/api/playlist/save" => json_response(
            StatusCode::OK,
            &build_save_playlist_response(request.body()),
        ),
        "/api/playlist/delete" => json_response(
            StatusCode::OK,
            &build_delete_playlist_response(request.body()),
        ),
        "/api/playlist/cursor/save" => {
            json_response(StatusCode::OK, &build_save_cursor_response(request.body()))
        }
        "/api/playlist/cursor/load" => json_response(
            StatusCode::OK,
            &build_load_cursor_response(request.uri().query()),
        ),
        "/api/abs/test" => json_response(StatusCode::OK, &build_abs_test_response(request.body())),
        "/api/abs/libraries" => json_response(
            StatusCode::OK,
            &build_abs_libraries_response(request.body()),
        ),
        "/api/abs/library-items" => json_response(
            StatusCode::OK,
            &build_abs_library_items_response(request.body()),
        ),
        "/api/abs/sync-progress" => json_response(
            StatusCode::OK,
            &build_abs_sync_progress_response(request.body()),
        ),
        "/api/recent-items" => json_response(
            StatusCode::OK,
            &build_recent_items_response(request.uri().query()),
        ),
        "/api/in-progress" => json_response(
            StatusCode::OK,
            &build_in_progress_response(request.uri().query()),
        ),
        "/api/list-subtitles" => json_response(
            StatusCode::OK,
            &build_list_subtitles_response(request.uri().query()),
        ),
        "/api/open-url" => json_response(
            StatusCode::OK,
            &build_open_url_response(request.uri().query()),
        ),
        "/api/trash/list" => json_response(
            StatusCode::OK,
            &build_trash_list_response(request.uri().query()),
        ),
        "/api/trash/restore" => json_response(
            StatusCode::OK,
            &build_trash_restore_response(request.body()),
        ),
        "/api/trash/purge" => {
            json_response(StatusCode::OK, &build_trash_purge_response(request.body()))
        }
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

fn build_plan_response(query: Option<&str>) -> DemoPlanResponse {
    let root_override = query.and_then(|query| extract_query_value(query, "root"));
    let refresh = query
        .and_then(|query| extract_query_value(query, "refresh"))
        .map(|value| value.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    if let Some(root_override) = root_override {
        match build_vault_plan(Some(&root_override), refresh) {
            Ok(plan) => return plan,
            Err(error) => {
                return build_error_plan(
                    format!("Vault-Pfad konnte nicht geladen werden: {error}"),
                    Some(root_override),
                );
            }
        }
    }

    match resolve_vault_root(None) {
        Ok(Some(vault_root)) => match build_vault_plan_with_root(vault_root, refresh) {
            Ok(plan) => plan,
            Err(error) => {
                build_error_plan(format!("Vault konnte nicht gescannt werden: {error}"), None)
            }
        },
        Ok(None) => build_demo_plan(Some(
            "Kein Vault gefunden, daher Demo-Daten angezeigt.".to_string(),
        )),
        Err(error) => build_error_plan(format!("Vault-Erkennung fehlgeschlagen: {error}"), None),
    }
}

fn build_audible_search_response(query: Option<&str>) -> AudibleSearchResponse {
    let Some(query) = query else {
        return AudibleSearchResponse::error("missing query".to_string());
    };
    let Some(title) = extract_query_value(query, "title") else {
        return AudibleSearchResponse::error("missing title".to_string());
    };

    let author = extract_query_value(query, "author");
    let limit = extract_query_value(query, "limit")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8);

    let client = match AudibleClient::new() {
        Ok(c) => c,
        Err(e) => return AudibleSearchResponse::error(e.to_string()),
    };

    match client.search(&title, author.as_deref(), limit) {
        Ok(results) => AudibleSearchResponse {
            metadata: results.first().cloned(),
            results,
            error: None,
        },
        Err(error) => AudibleSearchResponse::error(error.to_string()),
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

fn build_create_vault_response(query: Option<&str>) -> CreateVaultResponse {
    let Some(query) = query else {
        return CreateVaultResponse::error("missing query".to_string());
    };

    let Some(parent) = extract_query_value(query, "parent") else {
        return CreateVaultResponse::error("missing parent".to_string());
    };
    let Some(name) = extract_query_value(query, "name") else {
        return CreateVaultResponse::error("missing name".to_string());
    };

    match create_vault_at(&parent, &name) {
        Ok(path) => {
            // Creating a vault authorizes it, just like opening one does.
            let mut state = load_app_state().unwrap_or_default();
            let resolved = resolve_existing_root(path.clone()).unwrap_or_else(|_| path.clone());
            register_known_root(&mut state, &resolved);
            let _ = save_app_state(&state);

            CreateVaultResponse {
                path: Some(path.display().to_string()),
                created: true,
                error: None,
            }
        }
        Err(error) => CreateVaultResponse::error(error.to_string()),
    }
}

fn build_vault_root_response(query: Option<&str>) -> VaultRootResponse {
    let mut state = load_app_state().unwrap_or_default();

    if let Some(query) = query {
        if let Some(root) = extract_query_value(query, "root") {
            let normalized = root.trim();
            if normalized.is_empty() {
                state.vault_root = None;
            } else {
                match resolve_existing_root(PathBuf::from(normalized)) {
                    Ok(resolved) => {
                        // Opening a vault is the explicit user action that
                        // authorizes it for later `root=` overrides.
                        register_known_root(&mut state, &resolved);
                        state.vault_root = Some(resolved.display().to_string());
                    }
                    Err(error) => {
                        return VaultRootResponse::error(error.to_string());
                    }
                }
            }

            if let Err(error) = save_app_state(&state) {
                return VaultRootResponse::error(error.to_string());
            }
        }
    }

    VaultRootResponse {
        root: state.vault_root,
        error: None,
    }
}

fn build_select_folder_response() -> SelectFolderResponse {
    let selected = rfd::FileDialog::new().pick_folder();
    SelectFolderResponse {
        path: selected.map(|path| path.display().to_string()),
        error: None,
    }
}

fn build_media_file_response(query: Option<&str>, range: Option<&str>) -> Response<Vec<u8>> {
    let Some(query) = query else {
        return response(
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            "missing query",
        );
    };

    let Some(path) = extract_query_value(query, "path") else {
        return response(
            StatusCode::BAD_REQUEST,
            "text/plain; charset=utf-8",
            "missing path",
        );
    };

    let root_override = extract_query_value(query, "root");
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(root)) => root,
        Ok(None) => {
            return response(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                "no vault open",
            );
        }
        Err(error) => {
            return response(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                &error.to_string(),
            );
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(vault) => vault,
        Err(error) => {
            return response(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                &error.to_string(),
            );
        }
    };

    let relative = match RelativePath::new(path) {
        Ok(path) => path,
        Err(error) => {
            return response(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                &error.to_string(),
            );
        }
    };

    // `resolve_existing` also follows symlinks and rejects anything that ends
    // up outside the vault.
    let absolute = match vault.resolve_existing(relative.as_path()) {
        Ok(path) => path,
        Err(error) => {
            return response(
                StatusCode::BAD_REQUEST,
                "text/plain; charset=utf-8",
                &error.to_string(),
            );
        }
    };

    let file_size = match fs::metadata(&absolute) {
        Ok(m) => m.len(),
        Err(error) => {
            return response(
                StatusCode::NOT_FOUND,
                "text/plain; charset=utf-8",
                &error.to_string(),
            )
        }
    };

    let content_type = media_content_type(&absolute);

    // Serve a partial range when the browser requests one (required for video seeking).
    if let Some(range_str) = range.and_then(|r| r.strip_prefix("bytes=")) {
        let (start, end) = parse_byte_range(range_str, file_size);

        if start >= file_size || end >= file_size || start > end {
            return Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("Content-Range", format!("bytes */{file_size}"))
                .body(Vec::new())
                .expect("range error response should build");
        }

        // Serve at most one window per request; the browser keeps asking for
        // the next one, so a `bytes=0-` on a 40 GB file stays cheap.
        let end = end.min(start.saturating_add(MAX_RANGE_LENGTH_BYTES - 1));
        let length = end - start + 1;

        let body = match read_file_range(&absolute, start, length) {
            Ok(b) => b,
            Err(error) => {
                return response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "text/plain; charset=utf-8",
                    &error.to_string(),
                )
            }
        };

        return Response::builder()
            .status(StatusCode::PARTIAL_CONTENT)
            .header(CONTENT_TYPE, content_type)
            .header("Content-Range", format!("bytes {start}-{end}/{file_size}"))
            .header("Content-Length", length.to_string())
            .header("Accept-Ranges", "bytes")
            .body(body)
            .expect("partial content response should build");
    }

    // No Range header. Small files go out whole; for anything larger only the
    // first window is sent as `206 Partial Content` — the browser then asks
    // for the rest by range instead of us holding a multi-GB file in memory.
    if file_size > FULL_RESPONSE_LIMIT_BYTES {
        let end = FULL_RESPONSE_LIMIT_BYTES.min(file_size) - 1;
        let length = end + 1;

        return match read_file_range(&absolute, 0, length) {
            Ok(body) => Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(CONTENT_TYPE, content_type)
                .header("Content-Range", format!("bytes 0-{end}/{file_size}"))
                .header("Content-Length", length.to_string())
                .header("Accept-Ranges", "bytes")
                .body(body)
                .expect("partial content response should build"),
            Err(error) => response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "text/plain; charset=utf-8",
                &error.to_string(),
            ),
        };
    }

    match fs::read(&absolute) {
        Ok(body) => Response::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, content_type)
            .header("Content-Length", file_size.to_string())
            .header("Accept-Ranges", "bytes")
            .body(body)
            .expect("media response should build"),
        Err(error) => response(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            &error.to_string(),
        ),
    }
}

/// Largest file served in one piece when the client sends no `Range` header.
/// Anything bigger is answered with a first window plus `Accept-Ranges`.
const FULL_RESPONSE_LIMIT_BYTES: u64 = 8 * 1024 * 1024;

/// Largest single range served, so a `bytes=0-` request on a huge file cannot
/// force a multi-GB allocation.
const MAX_RANGE_LENGTH_BYTES: u64 = 16 * 1024 * 1024;

/// Parses a `bytes=start-end` range spec and clamps both ends to `[0, file_size - 1]`.
fn parse_byte_range(range_str: &str, file_size: u64) -> (u64, u64) {
    let last = file_size.saturating_sub(1);

    // Suffix form: "-500" means the last 500 bytes.
    if let Some(suffix) = range_str.strip_prefix('-') {
        if let Ok(n) = suffix.parse::<u64>() {
            return (file_size.saturating_sub(n), last);
        }
        return (0, last);
    }

    let mut parts = range_str.splitn(2, '-');
    let start = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let end = parts
        .next()
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(last)
        .min(last);

    (start, end)
}

/// Reads exactly `length` bytes from `path` starting at byte offset `start`.
fn read_file_range(path: &Path, start: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf)?;
    Ok(buf)
}

fn build_apply_import_response(body: &[u8]) -> ApplyImportResponse {
    let request: ApplyImportRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => {
            return ApplyImportResponse::error(format!(
                "Import-Anfrage konnte nicht gelesen werden: {error}"
            ));
        }
    };

    let vault_root = match resolve_vault_root(request.vault_root.as_deref()) {
        Ok(Some(root)) => root,
        Ok(None) => {
            return ApplyImportResponse::error("Kein Vault geöffnet.".to_string());
        }
        Err(error) => {
            return ApplyImportResponse::error(format!(
                "Vault konnte nicht aufgelöst werden: {error}"
            ));
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(vault) => vault,
        Err(error) => {
            return ApplyImportResponse::error(format!(
                "Vault konnte nicht initialisiert werden: {error}"
            ));
        }
    };

    // Dry-run: report conflicts (existing targets) without moving anything.
    if request.dry_run {
        let mut conflicts = Vec::new();
        let mut skipped = Vec::new();
        for item in &request.items {
            match detect_import_conflict(&vault, item) {
                Ok(Some(conflict)) => conflicts.push(conflict),
                Ok(None) => {}
                Err(error) => skipped.push(ApplyImportSkipped {
                    source_path: item.source_path.clone(),
                    reason: error.to_string(),
                }),
            }
        }
        return ApplyImportResponse {
            applied: Vec::new(),
            skipped,
            conflicts,
            error: None,
        };
    }

    let mut applied = Vec::new();
    let mut skipped = Vec::new();

    for item in request.items {
        match apply_import_item(&vault, &item) {
            Ok(()) => {
                applied.push(item.source_path.clone());
            }
            Err(error) => skipped.push(ApplyImportSkipped {
                source_path: item.source_path.clone(),
                reason: error.to_string(),
            }),
        }
    }

    ApplyImportResponse {
        applied,
        skipped,
        conflicts: Vec::new(),
        error: None,
    }
}

/// Checks whether applying `item` would overwrite an existing target file.
///
/// Returns `Ok(Some(conflict))` when the resolved target exists and differs from
/// the source, `Ok(None)` when there is no conflict, and `Err` when the source
/// is missing or paths are invalid.
fn detect_import_conflict(
    vault: &Vault,
    item: &ApplyImportItem,
) -> Result<Option<ApplyImportConflict>> {
    let source_relative = RelativePath::new(&item.source_path)?;
    let target_relative = RelativePath::new(&item.target_path)?;
    let source_absolute = vault.resolve(source_relative.as_path())?;
    let target_absolute = vault.resolve(target_relative.as_path())?;

    if !source_absolute.exists() {
        return Err(VaultError::InvalidVaultPath(format!(
            "Quelldatei nicht gefunden: {}",
            source_relative
        )));
    }

    if source_absolute == target_absolute || !target_absolute.exists() {
        return Ok(None);
    }

    let source_size = fs::metadata(&source_absolute).map(|m| m.len()).unwrap_or(0);
    let target_size = fs::metadata(&target_absolute).map(|m| m.len()).unwrap_or(0);

    Ok(Some(ApplyImportConflict {
        source_path: item.source_path.clone(),
        target_path: item.target_path.clone(),
        target_size,
        source_size,
    }))
}

fn build_save_sidecars_response(body: &[u8]) -> SaveSidecarsResponse {
    let request: SaveSidecarsRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return SaveSidecarsResponse::error(format!("ungültiger Request: {error}")),
    };

    let vault_root = match resolve_vault_root(request.vault_root.as_deref()) {
        Ok(Some(root)) => root,
        Ok(None) => return SaveSidecarsResponse::error("kein Vault ausgewählt".to_string()),
        Err(error) => return SaveSidecarsResponse::error(error.to_string()),
    };

    let vault = match Vault::new(&vault_root) {
        Ok(vault) => vault,
        Err(error) => return SaveSidecarsResponse::error(error.to_string()),
    };

    let mut saved = Vec::new();
    let mut skipped = Vec::new();

    for item in request.items {
        match save_sidecar_item(&vault, &item) {
            Ok(()) => saved.push(item.media_path),
            Err(error) => skipped.push(ApplyImportSkipped {
                source_path: item.media_path,
                reason: error.to_string(),
            }),
        }
    }

    SaveSidecarsResponse {
        saved,
        skipped,
        error: None,
    }
}

fn build_delete_files_response(body: &[u8]) -> DeleteFilesResponse {
    let request: DeleteFilesRequest = match serde_json::from_slice(body) {
        Ok(request) => request,
        Err(error) => return DeleteFilesResponse::error(format!("ungültiger Request: {error}")),
    };

    let vault_root = match resolve_vault_root(request.vault_root.as_deref()) {
        Ok(Some(root)) => root,
        Ok(None) => return DeleteFilesResponse::error("kein Vault ausgewählt".to_string()),
        Err(error) => return DeleteFilesResponse::error(error.to_string()),
    };

    let vault = match Vault::new(&vault_root) {
        Ok(vault) => vault,
        Err(error) => return DeleteFilesResponse::error(error.to_string()),
    };

    let mut deleted = Vec::new();
    let mut skipped = Vec::new();

    for path in request.paths {
        match delete_vault_file(&vault, &path, request.permanent) {
            Ok(()) => deleted.push(path),
            Err(error) => skipped.push(ApplyImportSkipped {
                source_path: path,
                reason: error.to_string(),
            }),
        }
    }

    DeleteFilesResponse {
        deleted,
        skipped,
        error: None,
    }
}

/// Deletes a single vault file plus its sidecar.
///
/// When `permanent` is false the file is moved into the vault `.trash` folder
/// (reversible). When true it is removed from disk. The accompanying
/// `.fero.yaml` sidecar, if present, is removed alongside.
fn delete_vault_file(vault: &Vault, path: &str, permanent: bool) -> Result<()> {
    let relative = RelativePath::new(path)?;

    if !vault.resolve(relative.as_path())?.exists() {
        return Err(VaultError::InvalidVaultPath(format!(
            "Datei nicht gefunden: {relative}"
        )));
    }

    // Deleting is destructive, so the symlink-aware form decides what is
    // actually touched — a link inside the vault must never delete its target
    // outside of it.
    let absolute = vault.resolve_existing(relative.as_path())?;

    // Every naming scheme is considered: a vault can still hold sidecars
    // written by an older version.
    let sidecars: Vec<(RelativePath, PathBuf)> = sidecar_candidates(&relative)?
        .into_iter()
        .filter_map(|rel| {
            let absolute = vault.resolve(rel.as_path()).ok()?;
            absolute.exists().then_some((rel, absolute))
        })
        .collect();

    if permanent {
        fs::remove_file(&absolute).map_err(VaultError::from)?;
        // Remove the sidecars too (best effort — never fail the delete).
        for (_, sidecar) in &sidecars {
            let _ = fs::remove_file(sidecar);
        }
    } else {
        // Move to .trash/<original relative path> so structure is preserved and
        // name collisions across folders are avoided.
        let trash_target = vault.root().join(TRASH_DIR).join(relative.as_path());
        if let Some(parent) = trash_target.parent() {
            fs::create_dir_all(parent).map_err(VaultError::from)?;
        }
        move_file_with_fallback(&absolute, &trash_target)?;
        // Move the sidecars ALONGSIDE the file (not delete them) so a restore
        // brings the metadata back.
        for (sidecar_rel, sidecar_abs) in &sidecars {
            let sidecar_trash = vault.root().join(TRASH_DIR).join(sidecar_rel.as_path());
            if let Some(parent) = sidecar_trash.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = move_file_with_fallback(sidecar_abs, &sidecar_trash);
        }
    }

    prune_empty_inbox_dirs(vault, absolute.parent());
    Ok(())
}

// ---------------------------------------------------------------------------
// Vault trash browser (list / restore / purge)
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct TrashEntry {
    /// Path relative to the vault root (== original location).
    relative_path: String,
    /// File name for display.
    name: String,
    size_bytes: u64,
    /// Modification time in the trash (UNIX seconds) — proxy for deletion time.
    trashed_at: u64,
}

#[derive(Serialize)]
struct TrashListResponse {
    entries: Vec<TrashEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Returns whether a file name is a sidecar (skipped in the trash listing —
/// sidecars ride along with their media file).
fn is_sidecar_name(name: &str) -> bool {
    name.ends_with(SIDECAR_SUFFIX) || name.ends_with(LEGACY_SIDECAR_SUFFIX)
}

/// Recursively collects trashed media files (skips sidecars).
fn collect_trash_entries(base: &Path, dir: &Path, entries: &mut Vec<TrashEntry>) {
    let Ok(read) = fs::read_dir(dir) else {
        return;
    };
    for entry in read.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_trash_entries(base, &path, entries);
            continue;
        }
        let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if is_sidecar_name(name) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(base) else {
            continue;
        };
        let metadata = path.metadata().ok();
        let size_bytes = metadata.as_ref().map(|m| m.len()).unwrap_or(0);
        let trashed_at = metadata
            .and_then(|m| m.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        entries.push(TrashEntry {
            relative_path: relative.to_string_lossy().replace('\\', "/"),
            name: name.to_string(),
            size_bytes,
            trashed_at,
        });
    }
}

fn build_trash_list_response(query: Option<&str>) -> TrashListResponse {
    let root_override = query.and_then(|query| extract_query_value(query, "root"));
    let vault = match resolve_webnovel_vault(root_override.as_deref()) {
        Ok(vault) => vault,
        Err(message) => {
            return TrashListResponse {
                entries: Vec::new(),
                error: Some(message),
            }
        }
    };
    let trash_root = vault.root().join(TRASH_DIR);
    let mut entries = Vec::new();
    if trash_root.exists() {
        collect_trash_entries(&trash_root, &trash_root, &mut entries);
    }
    entries.sort_by_key(|entry| std::cmp::Reverse(entry.trashed_at));
    TrashListResponse {
        entries,
        error: None,
    }
}

#[derive(Deserialize)]
struct TrashActionRequest {
    #[serde(default)]
    paths: Vec<String>,
    #[serde(default)]
    all: bool,
    vault_root: Option<String>,
}

#[derive(Serialize)]
struct TrashActionResponse {
    processed: Vec<String>,
    skipped: Vec<ApplyImportSkipped>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl TrashActionResponse {
    fn error(message: impl Into<String>) -> Self {
        Self {
            processed: Vec::new(),
            skipped: Vec::new(),
            error: Some(message.into()),
        }
    }
}

fn build_trash_restore_response(body: &[u8]) -> TrashActionResponse {
    let req: TrashActionRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return TrashActionResponse::error(format!("Invalid request: {error}")),
    };
    let vault = match resolve_webnovel_vault(req.vault_root.as_deref()) {
        Ok(vault) => vault,
        Err(message) => return TrashActionResponse::error(message),
    };

    let mut processed = Vec::new();
    let mut skipped = Vec::new();
    for rel in &req.paths {
        match restore_trashed_file(&vault, rel) {
            Ok(()) => processed.push(rel.clone()),
            Err(error) => skipped.push(ApplyImportSkipped {
                source_path: rel.clone(),
                reason: error.to_string(),
            }),
        }
    }
    TrashActionResponse {
        processed,
        skipped,
        error: None,
    }
}

/// Moves a trashed file (and its sidecar) back to its original location.
fn restore_trashed_file(vault: &Vault, relative_path: &str) -> Result<()> {
    let relative = RelativePath::new(relative_path)?;
    let trash_source = vault.root().join(TRASH_DIR).join(relative.as_path());
    if !trash_source.exists() {
        return Err(VaultError::InvalidVaultPath(format!(
            "Im Papierkorb nicht gefunden: {relative}"
        )));
    }
    let target = vault.resolve(relative.as_path())?;
    if target.exists() {
        return Err(VaultError::InvalidVaultPath(
            "Zielpfad existiert bereits.".to_string(),
        ));
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).map_err(VaultError::from)?;
    }
    move_file_with_fallback(&trash_source, &target)?;

    // Bring the sidecar back too, if it was trashed alongside. Files deleted
    // by an older version carry an older sidecar name, so every known shape is
    // considered.
    for sidecar_rel in sidecar_candidates(&relative)? {
        let sidecar_source = vault.root().join(TRASH_DIR).join(sidecar_rel.as_path());
        if !sidecar_source.exists() {
            continue;
        }
        if let Ok(sidecar_target) = vault.resolve(sidecar_rel.as_path()) {
            if let Some(parent) = sidecar_target.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let _ = move_file_with_fallback(&sidecar_source, &sidecar_target);
        }
    }
    Ok(())
}

fn build_trash_purge_response(body: &[u8]) -> TrashActionResponse {
    let req: TrashActionRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return TrashActionResponse::error(format!("Invalid request: {error}")),
    };
    let vault = match resolve_webnovel_vault(req.vault_root.as_deref()) {
        Ok(vault) => vault,
        Err(message) => return TrashActionResponse::error(message),
    };
    let trash_root = vault.root().join(TRASH_DIR);

    if req.all {
        if trash_root.exists() {
            if let Err(error) = fs::remove_dir_all(&trash_root) {
                return TrashActionResponse::error(format!(
                    "Papierkorb konnte nicht geleert werden: {error}"
                ));
            }
        }
        return TrashActionResponse {
            processed: vec!["*".to_string()],
            skipped: Vec::new(),
            error: None,
        };
    }

    let mut processed = Vec::new();
    let mut skipped = Vec::new();
    for rel in &req.paths {
        match purge_trashed_file(&vault, rel) {
            Ok(()) => processed.push(rel.clone()),
            Err(error) => skipped.push(ApplyImportSkipped {
                source_path: rel.clone(),
                reason: error.to_string(),
            }),
        }
    }
    TrashActionResponse {
        processed,
        skipped,
        error: None,
    }
}

/// Permanently deletes a trashed file and its sidecar.
fn purge_trashed_file(vault: &Vault, relative_path: &str) -> Result<()> {
    let relative = RelativePath::new(relative_path)?;
    let trash_source = vault.root().join(TRASH_DIR).join(relative.as_path());
    if trash_source.exists() {
        fs::remove_file(&trash_source).map_err(VaultError::from)?;
    }
    for sidecar_rel in sidecar_candidates(&relative)? {
        let sidecar_source = vault.root().join(TRASH_DIR).join(sidecar_rel.as_path());
        if sidecar_source.exists() {
            let _ = fs::remove_file(&sidecar_source);
        }
    }
    Ok(())
}

fn build_cleanup_vault_response(query: Option<&str>) -> CleanupVaultResponse {
    let root_override = query.and_then(|query| extract_query_value(query, "root"));
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(root)) => root,
        Ok(None) => {
            return CleanupVaultResponse::error("Kein Vault geöffnet.".to_string());
        }
        Err(error) => return CleanupVaultResponse::error(error.to_string()),
    };
    let vault = match Vault::new(&vault_root) {
        Ok(vault) => vault,
        Err(error) => return CleanupVaultResponse::error(error.to_string()),
    };
    match run_vault_cleanup(&vault) {
        Ok(response) => response,
        Err(error) => CleanupVaultResponse::error(error.to_string()),
    }
}

/// Scans the vault for maintenance issues: orphaned sidecars, empty folders, and
/// thumbnail/asset entries that no longer have a corresponding media file.
fn run_vault_cleanup(vault: &Vault) -> Result<CleanupVaultResponse> {
    let mut issues: Vec<CleanupIssue> = Vec::new();

    // --- Orphaned .fero.yaml sidecars ---
    find_orphaned_sidecars(vault, vault.root(), &mut issues)?;

    // --- Empty directories (excluding system and inbox) ---
    find_empty_directories(vault, vault.root(), &mut issues)?;

    // --- Orphaned thumbnail cache files ---
    if vault.thumbnails_dir().exists() {
        for entry in fs::read_dir(vault.thumbnails_dir()).map_err(VaultError::from)? {
            let entry = entry.map_err(VaultError::from)?;
            let path = entry.path();
            if path.extension().map(|e| e == "jpg").unwrap_or(false) {
                // Thumbnails are named by content hash; we can't re-link them here without
                // the DB.  Flag them as potentially stale for manual review.
                issues.push(CleanupIssue {
                    kind: "stale_thumbnail".to_string(),
                    path: path.display().to_string(),
                    description: "Thumbnail ohne zugehörigen Eintrag (DB nicht verfügbar)"
                        .to_string(),
                });
            }
        }
    }

    let summary = CleanupSummary {
        orphaned_sidecars: issues
            .iter()
            .filter(|i| i.kind == "orphaned_sidecar")
            .count(),
        empty_directories: issues
            .iter()
            .filter(|i| i.kind == "empty_directory")
            .count(),
        stale_thumbnails: issues
            .iter()
            .filter(|i| i.kind == "stale_thumbnail")
            .count(),
    };

    Ok(CleanupVaultResponse {
        issues,
        summary,
        error: None,
    })
}

/// Whether `directory` holds a non-sidecar file whose stem is `stem`.
///
/// Used to tell a genuinely orphaned legacy sidecar apart from one that simply
/// predates the current naming scheme.
fn has_sibling_with_stem(directory: &Path, stem: &str) -> bool {
    let Ok(entries) = fs::read_dir(directory) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            return false;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            return false;
        };
        if is_sidecar_name(name) {
            return false;
        }
        path.file_stem()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value == stem)
    })
}

fn find_orphaned_sidecars(
    vault: &Vault,
    directory: &Path,
    issues: &mut Vec<CleanupIssue>,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    for entry in entries {
        let entry = entry.map_err(VaultError::from)?;
        let path = entry.path();
        if path.is_dir() {
            if should_skip_scanned_directory(vault, &path) {
                continue;
            }
            find_orphaned_sidecars(vault, &path, issues)?;
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if name.ends_with(SIDECAR_SUFFIX) || name.ends_with(LEGACY_SIDECAR_SUFFIX) {
            // Derive the expected media filename by stripping the sidecar suffix.
            let stripped = if name.ends_with(SIDECAR_SUFFIX) {
                name.trim_end_matches(SIDECAR_SUFFIX)
            } else {
                name.trim_end_matches(LEGACY_SIDECAR_SUFFIX)
            };
            let media_path = path.with_file_name(stripped);
            // Sidecars written before the suffix was appended carry the media
            // file's *stem* only (`Film.fero.yaml` for `Film.mkv`), so a
            // missing exact match does not yet mean the sidecar is orphaned.
            if !media_path.exists() && !has_sibling_with_stem(directory, stripped) {
                issues.push(CleanupIssue {
                    kind: "orphaned_sidecar".to_string(),
                    path: path.display().to_string(),
                    description: format!("Sidecar ohne Mediendatei: {}", media_path.display()),
                });
            }
        }
    }
    Ok(())
}

fn find_empty_directories(
    vault: &Vault,
    directory: &Path,
    issues: &mut Vec<CleanupIssue>,
) -> Result<()> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Ok(());
    };
    let mut has_content = false;

    for entry in entries {
        let entry = entry.map_err(VaultError::from)?;
        let path = entry.path();

        if path.is_dir() {
            if should_skip_scanned_directory(vault, &path)
                || path == vault.inbox_dir()
                || path.starts_with(vault.inbox_dir())
            {
                has_content = true; // inbox subfolders are intentionally empty initially
                continue;
            }
            find_empty_directories(vault, &path, issues)?;
            has_content = true;
        } else if path.is_file() {
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            if !is_hidden_system_entry(name) {
                has_content = true;
            }
        }
    }

    if !has_content && directory != vault.root() {
        issues.push(CleanupIssue {
            kind: "empty_directory".to_string(),
            path: directory.display().to_string(),
            description: "Leerer Ordner".to_string(),
        });
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
struct CleanupIssue {
    kind: String,
    path: String,
    description: String,
}

#[derive(Debug, Clone, Serialize)]
struct CleanupSummary {
    orphaned_sidecars: usize,
    empty_directories: usize,
    stale_thumbnails: usize,
}

#[derive(Debug, Clone, Serialize)]
struct CleanupVaultResponse {
    issues: Vec<CleanupIssue>,
    summary: CleanupSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl CleanupVaultResponse {
    fn error(message: String) -> Self {
        Self {
            issues: Vec::new(),
            summary: CleanupSummary {
                orphaned_sidecars: 0,
                empty_directories: 0,
                stale_thumbnails: 0,
            },
            error: Some(message),
        }
    }
}

fn create_vault_at(parent: &str, name: &str) -> Result<PathBuf> {
    let parent = PathBuf::from(parent.trim());
    if parent.as_os_str().is_empty() {
        return Err(VaultError::InvalidVaultPath(
            "parent path is empty".to_string(),
        ));
    }
    if !parent.exists() {
        return Err(VaultError::InvalidVaultPath(format!(
            "parent path does not exist: {}",
            parent.display()
        )));
    }
    if !parent.is_dir() {
        return Err(VaultError::InvalidVaultPath(format!(
            "parent path is not a directory: {}",
            parent.display()
        )));
    }

    let vault_name = sanitize_path_segment(name.trim());
    if vault_name.is_empty() {
        return Err(VaultError::InvalidVaultPath(
            "vault name is empty".to_string(),
        ));
    }

    let vault_root = parent.join(vault_name);
    fs::create_dir_all(&vault_root).map_err(VaultError::from)?;

    let vault = Vault::new(vault_root.clone())?;
    fs::create_dir_all(vault.inbox_dir()).map_err(VaultError::from)?;
    fs::create_dir_all(vault.review_queue_dir()).map_err(VaultError::from)?;
    fs::create_dir_all(vault.covers_dir()).map_err(VaultError::from)?;
    fs::create_dir_all(vault.system_dir()).map_err(VaultError::from)?;

    // Pre-create sorted inbox subfolders so users can drop files in the right place immediately.
    let inbox = vault.inbox_dir();
    for subfolder in INBOX_SUBFOLDERS {
        fs::create_dir_all(inbox.join(subfolder)).map_err(VaultError::from)?;
    }

    // Create app-internal cache directories up front so cover downloads and progress
    // tracking never need to check-and-mkdir at call time.
    fs::create_dir_all(vault.thumbnails_dir()).map_err(VaultError::from)?;
    fs::create_dir_all(vault.assets_dir()).map_err(VaultError::from)?;
    fs::create_dir_all(vault.progress_dir()).map_err(VaultError::from)?;

    fs::canonicalize(&vault_root).map_err(VaultError::from)
}

fn build_vault_plan(root_override: Option<&str>, refresh: bool) -> Result<DemoPlanResponse> {
    let vault_root = resolve_vault_root(root_override)?
        .ok_or_else(|| VaultError::InvalidVaultPath("no vault root found".to_string()))?;
    build_vault_plan_with_root(vault_root, refresh)
}

fn build_vault_plan_with_root(vault_root: PathBuf, refresh: bool) -> Result<DemoPlanResponse> {
    let vault = Vault::new(vault_root.clone())?;
    let files = scan_vault_files(&vault)?;
    let planner = ImportPlanner::new(ImportConfig {
        duplicate_policy: DuplicatePolicy::AskUser,
        ..ImportConfig::default()
    });
    let anilist_client = AniListClient::default();
    let mut anilist_cache = if refresh {
        HashMap::new()
    } else {
        load_anilist_cache()
    };

    let mut items = Vec::with_capacity(files.len());
    let mut anilist_results = Vec::with_capacity(files.len());
    let mut seen_fingerprints = HashSet::new();

    for file in &files {
        let mut item = planner.plan_file(&file.incoming)?;
        anilist_results.push(resolve_anilist_metadata_cached(
            &anilist_client,
            &file.incoming,
            &item,
            &mut anilist_cache,
        ));
        if is_in_inbox(&file.incoming.source_path) {
            if let Some(fingerprint) = item.fingerprint.as_ref() {
                let is_duplicate = !seen_fingerprints.insert(fingerprint.hash.clone());
                if is_duplicate {
                    item.duplicate_of = Some(fingerprint.hash.clone());
                    item.manual_review = true;
                    item.target_path = None;
                    item.steps.retain(|step| {
                        !matches!(
                            step,
                            PlannedImportStep::MoveFile { .. }
                                | PlannedImportStep::WriteSidecar { .. }
                        )
                    });
                    item.steps.push(PlannedImportStep::AskUser {
                        prompt: UserPrompt {
                            field_name: "duplicate".to_string(),
                            message: "Duplikat gefunden".to_string(),
                            options: vec![
                                "Behalten".to_string(),
                                "Überspringen".to_string(),
                                "Zusammenführen".to_string(),
                            ],
                        },
                    });
                }
            }
        }

        items.push(item);
    }

    save_anilist_cache(&anilist_cache);

    let plan = ImportPlan {
        dry_run: true,
        summary: Default::default(),
        items: items.clone(),
    };
    let summary = summarize_demo_plan(&plan);

    let mut plan_items: Vec<DemoPlanItem> = files
        .iter()
        .zip(items.iter())
        .zip(anilist_results.iter())
        .map(|((file, item), anilist_metadata)| {
            DemoPlanItem::from_scanned(
                &file.incoming,
                item,
                anilist_metadata.as_ref(),
                file.sidecar.as_ref(),
                file.sidecar_preview.as_deref(),
            )
        })
        .collect();

    group_audiobook_folders(&mut plan_items, Some(vault_root.as_path()));

    // Attach cover art (cover.jpg/poster.png next to the file) so collection
    // cards and the inspector can show it without extra requests.
    for plan_item in &mut plan_items {
        if plan_item.cover_url.is_none() {
            plan_item.cover_url = find_cover_url(&vault, &plan_item.source_path);
        }
        plan_item.modified_at = fs::metadata(vault.root().join(&plan_item.source_path))
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs());
    }

    // Append parts list to the sidecar YAML preview for audiobook group representatives.
    for item in &mut plan_items {
        if let Some(parts) = item.audiobook_parts.as_deref() {
            let parts_yaml = parts
                .iter()
                .enumerate()
                .map(|(i, p)| format!("  - path: \"{}\"\n    part: {}", p, i + 1))
                .collect::<Vec<_>>()
                .join("\n");
            item.sidecar_preview = format!("{}parts:\n{}\n", item.sidecar_preview, parts_yaml);
        }
    }

    Ok(DemoPlanResponse {
        title: "Vault-Dry-Run".to_string(),
        source: "vault".to_string(),
        vault_root: Some(vault_root.display().to_string()),
        note: if files.is_empty() {
            Some("Vault ist leer.".to_string())
        } else {
            None
        },
        summary,
        items: plan_items,
    })
}

fn build_demo_plan(note: Option<String>) -> DemoPlanResponse {
    let planner = ImportPlanner::new(ImportConfig {
        duplicate_policy: DuplicatePolicy::AskUser,
        ..ImportConfig::default()
    });
    let anilist_client = AniListClient::default();

    let files = vec![
        IncomingFile {
            source_path: RelativePath::new("Inbox/Violet Evergarden.mkv")
                .expect("demo relative path should be valid"),
            size_bytes: 1_843_200_000,
            fingerprint: None,
            classification: Some(FileClassification {
                media_type: MediaType::Anime,
                confidence: 0.97,
                source: ClassificationSource::Api,
            }),
            metadata: Some(ResolvedMetadata {
                title: Some("Violet Evergarden".to_string()),
                year: Some(2018),
            }),
        },
        IncomingFile {
            source_path: RelativePath::new("Inbox/unknown_scan.zip")
                .expect("demo relative path should be valid"),
            size_bytes: 42_000_000,
            fingerprint: None,
            classification: None,
            metadata: None,
        },
        IncomingFile {
            source_path: RelativePath::new("Inbox/Demo Track 01.flac")
                .expect("demo relative path should be valid"),
            size_bytes: 38_200_000,
            fingerprint: Some(compute_fingerprint(b"shared-demo-fingerprint")),
            classification: Some(FileClassification {
                media_type: MediaType::MusicTrack,
                confidence: 0.82,
                source: ClassificationSource::Filename,
            }),
            metadata: Some(ResolvedMetadata {
                title: Some("Demo Track 01".to_string()),
                year: None,
            }),
        },
        IncomingFile {
            source_path: RelativePath::new("Inbox/Demo Track 01 copy.flac")
                .expect("demo relative path should be valid"),
            size_bytes: 38_200_000,
            fingerprint: Some(compute_fingerprint(b"shared-demo-fingerprint")),
            classification: Some(FileClassification {
                media_type: MediaType::MusicTrack,
                confidence: 0.82,
                source: ClassificationSource::Filename,
            }),
            metadata: Some(ResolvedMetadata {
                title: Some("Demo Track 01".to_string()),
                year: None,
            }),
        },
    ];

    let mut items = Vec::with_capacity(files.len());
    let mut anilist_results = Vec::with_capacity(files.len());
    let mut seen_fingerprints = HashSet::new();
    let mut demo_cache: AniListCacheMap = HashMap::new();

    for file in &files {
        let mut item = planner
            .plan_file(file)
            .expect("demo planning should succeed");
        anilist_results.push(resolve_anilist_metadata_cached(
            &anilist_client,
            file,
            &item,
            &mut demo_cache,
        ));

        if let Some(fingerprint) = item.fingerprint.as_ref() {
            let is_duplicate = !seen_fingerprints.insert(fingerprint.hash.clone());
            if is_duplicate {
                item.duplicate_of = Some(fingerprint.hash.clone());
                item.manual_review = true;
                item.target_path = None;
                item.steps.retain(|step| {
                    !matches!(
                        step,
                        PlannedImportStep::MoveFile { .. } | PlannedImportStep::WriteSidecar { .. }
                    )
                });
                item.steps.push(PlannedImportStep::AskUser {
                    prompt: UserPrompt {
                        field_name: "duplicate".to_string(),
                        message: "Duplikat gefunden".to_string(),
                        options: vec![
                            "Behalten".to_string(),
                            "Überspringen".to_string(),
                            "Zusammenführen".to_string(),
                        ],
                    },
                });
            }
        }

        items.push(item);
    }

    let plan = ImportPlan {
        dry_run: true,
        summary: Default::default(),
        items: items.clone(),
    };
    let summary = summarize_demo_plan(&plan);

    DemoPlanResponse {
        title: "Demo-Dry-Run".to_string(),
        source: "demo".to_string(),
        vault_root: None,
        note,
        summary,
        items: files
            .iter()
            .zip(items.iter())
            .zip(anilist_results.iter())
            .map(|((file, item), anilist_metadata)| {
                DemoPlanItem::from_scanned(file, item, anilist_metadata.as_ref(), None, None)
            })
            .collect(),
    }
}

fn build_error_plan(note: String, vault_root: Option<String>) -> DemoPlanResponse {
    DemoPlanResponse {
        title: "Vault-Dry-Run".to_string(),
        source: "error".to_string(),
        vault_root,
        note: Some(note),
        summary: DemoSummary::default(),
        items: Vec::new(),
    }
}

fn summarize_demo_plan(plan: &ImportPlan) -> DemoSummary {
    let total_files = plan.items.len();
    let items_needing_review = plan
        .items
        .iter()
        .filter(|item| requires_review(item))
        .count();
    let duplicates = plan
        .items
        .iter()
        .filter(|item| item.duplicate_of.is_some())
        .count();
    let planned_moves = plan
        .items
        .iter()
        .filter(|item| {
            item.steps
                .iter()
                .any(|step| matches!(step, PlannedImportStep::MoveFile { .. }))
        })
        .count();
    let planned_sidecars = plan
        .items
        .iter()
        .filter(|item| {
            item.steps
                .iter()
                .any(|step| matches!(step, PlannedImportStep::WriteSidecar { .. }))
        })
        .count();
    let planned_api_fetches = plan
        .items
        .iter()
        .map(|item| {
            item.steps
                .iter()
                .filter(|step| matches!(step, PlannedImportStep::FetchMetadata { .. }))
                .count()
        })
        .sum();

    DemoSummary {
        total_files,
        items_needing_review,
        duplicates,
        planned_moves,
        planned_sidecars,
        planned_api_fetches,
        // Placeholder until smart collections are computed from the index.
        smart_collections: DEFAULT_SMART_COLLECTIONS,
    }
}

/// Number of built-in smart collections reported by the plan summary.
const DEFAULT_SMART_COLLECTIONS: usize = 3;

#[derive(Debug, Clone, Serialize)]
struct DemoPlanResponse {
    title: String,
    source: String,
    vault_root: Option<String>,
    note: Option<String>,
    summary: DemoSummary,
    items: Vec<DemoPlanItem>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct DemoSummary {
    total_files: usize,
    items_needing_review: usize,
    duplicates: usize,
    planned_moves: usize,
    planned_sidecars: usize,
    planned_api_fetches: usize,
    smart_collections: usize,
}

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
struct CreateVaultResponse {
    path: Option<String>,
    created: bool,
    error: Option<String>,
}

impl CreateVaultResponse {
    fn error(error: String) -> Self {
        Self {
            path: None,
            created: false,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SelectFolderResponse {
    path: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Clone)]
struct ScannedVaultFile {
    incoming: IncomingFile,
    sidecar: Option<ParsedSidecar>,
    sidecar_preview: Option<String>,
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

#[derive(Debug, Clone, Deserialize)]
struct ApplyImportRequest {
    vault_root: Option<String>,
    items: Vec<ApplyImportItem>,
    /// When true, no files are moved — the response only reports which targets
    /// already exist (conflicts) so the UI can ask the user how to proceed.
    #[serde(default)]
    dry_run: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct ApplyImportItem {
    source_path: String,
    target_path: String,
    sidecar_preview: String,
    /// When true, an existing target file is deleted before the move so the
    /// inbox copy can replace it (resolves the "file in both places" case).
    #[serde(default)]
    overwrite: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct SaveSidecarsRequest {
    vault_root: Option<String>,
    items: Vec<SaveSidecarItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct SaveSidecarItem {
    media_path: String,
    sidecar_preview: String,
}

#[derive(Debug, Clone, Serialize)]
struct ApplyImportSkipped {
    source_path: String,
    reason: String,
}

/// A target path that already exists on disk — reported during a dry-run so the
/// user can decide whether to overwrite or skip.
#[derive(Debug, Clone, Serialize)]
struct ApplyImportConflict {
    source_path: String,
    target_path: String,
    /// Size in bytes of the existing target file (helps the user judge whether
    /// it is the same file).
    target_size: u64,
    /// Size in bytes of the inbox source file.
    source_size: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ApplyImportResponse {
    applied: Vec<String>,
    skipped: Vec<ApplyImportSkipped>,
    #[serde(default)]
    conflicts: Vec<ApplyImportConflict>,
    error: Option<String>,
}

impl ApplyImportResponse {
    fn error(error: String) -> Self {
        Self {
            applied: Vec::new(),
            skipped: Vec::new(),
            conflicts: Vec::new(),
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct SaveSidecarsResponse {
    saved: Vec<String>,
    skipped: Vec<ApplyImportSkipped>,
    error: Option<String>,
}

impl SaveSidecarsResponse {
    fn error(error: String) -> Self {
        Self {
            saved: Vec::new(),
            skipped: Vec::new(),
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct DeleteFilesRequest {
    vault_root: Option<String>,
    /// Vault-relative paths of files to delete.
    paths: Vec<String>,
    /// When false (default), files are moved to the vault `.trash` folder so the
    /// action is reversible. When true, files are removed permanently.
    #[serde(default)]
    permanent: bool,
}

#[derive(Debug, Clone, Serialize)]
struct DeleteFilesResponse {
    deleted: Vec<String>,
    skipped: Vec<ApplyImportSkipped>,
    error: Option<String>,
}

impl DeleteFilesResponse {
    fn error(error: String) -> Self {
        Self {
            deleted: Vec::new(),
            skipped: Vec::new(),
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct DemoPlanItem {
    source_path: String,
    target_path: Option<String>,
    manual_review: bool,
    needs_review: bool,
    duplicate_of: Option<String>,
    media_type: String,
    classification_source: Option<String>,
    confidence: Option<f32>,
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
    /// Cover image URL (vault cover file next to the media), when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    rating_external: Option<f32>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    genres: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    /// File modification time (UNIX seconds), for date sorting in the UI.
    #[serde(skip_serializing_if = "Option::is_none")]
    modified_at: Option<u64>,
    /// Lifecycle status from the sidecar (completed, in-library, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<String>,
    collection_path: String,
    size_bytes: u64,
    folder_segment: String,
    sidecar_path: Option<String>,
    sidecar_preview: String,
    steps: Vec<String>,
    /// When this item is the group representative of a multi-file audiobook,
    /// contains the vault-relative paths of all sibling audio parts (sorted).
    #[serde(skip_serializing_if = "Option::is_none")]
    audiobook_parts: Option<Vec<String>>,
    /// True for audio files that are part of a multi-file audiobook group but
    /// are not the group representative.  The UI can collapse these.
    #[serde(default)]
    is_audiobook_part: bool,
    /// Author name extracted from embedded audio tags (ID3/MP4) or from
    /// Audible metadata.  Populated before the first Audible sync so the
    /// collection view already has an author even for unsynced books.
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<String>,
}

impl DemoPlanItem {
    fn from_scanned(
        file: &IncomingFile,
        item: &ImportPlanItem,
        anilist: Option<&AniListAnimeMetadata>,
        sidecar: Option<&ParsedSidecar>,
        sidecar_preview: Option<&str>,
    ) -> Self {
        let classification = item
            .classification
            .as_ref()
            .or(file.classification.as_ref());
        let media_type = classification
            .map(|classification| classification.media_type)
            .or(sidecar.and_then(|value| value.media_type))
            .unwrap_or(MediaType::Unclassified);
        let needs_review = requires_review(item);
        let title = build_display_title(file, anilist, media_type);
        let anime_context = derive_anime_context(file, item, anilist, title.as_deref());
        let effective_series_title = sidecar
            .and_then(|value| value.series_title.clone())
            .or_else(|| {
                anime_context
                    .as_ref()
                    .and_then(|context| context.series_title.clone())
            });
        let effective_season_number = sidecar.and_then(|value| value.season_number).or_else(|| {
            anime_context
                .as_ref()
                .and_then(|context| context.season_number)
        });
        let effective_episode_start = sidecar.and_then(|value| value.episode_start).or_else(|| {
            anime_context
                .as_ref()
                .and_then(|context| context.episode_start)
        });
        let effective_episode_end = sidecar.and_then(|value| value.episode_end).or_else(|| {
            anime_context
                .as_ref()
                .and_then(|context| context.episode_end)
        });
        let effective_episode_title = sidecar
            .and_then(|value| value.episode_title.clone())
            .or_else(|| {
                anime_context
                    .as_ref()
                    .and_then(|context| context.episode_title.clone())
            });
        let effective_year = file
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.year)
            .or_else(|| sidecar.and_then(|value| value.year));
        let collection_path = build_collection_path(
            media_type,
            title.as_deref(),
            effective_year,
            anime_context.as_ref(),
            anilist,
        );
        let target_path = build_target_path_preview(
            file,
            item,
            media_type,
            title.as_deref(),
            anime_context.as_ref(),
            anilist,
        );
        let preview_path = target_path
            .as_ref()
            .and_then(|path| RelativePath::new(path).ok())
            .or_else(|| item.target_path.clone())
            .unwrap_or_else(|| file.source_path.clone());

        Self {
            source_path: item.source_path.to_string(),
            target_path,
            manual_review: item.manual_review,
            needs_review,
            duplicate_of: item.duplicate_of.clone(),
            media_type: media_type.to_string(),
            classification_source: classification
                .map(|classification| classification_source_label(&classification.source)),
            confidence: classification.map(|classification| classification.confidence),
            title,
            year: file
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.year)
                .or_else(|| sidecar.and_then(|value| value.year)),
            series_title: effective_series_title,
            season_number: effective_season_number,
            episode_start: effective_episode_start,
            episode_end: effective_episode_end,
            episode_title: effective_episode_title,
            episode_count: sidecar
                .and_then(|value| value.episode_count)
                .or_else(|| anilist.and_then(|metadata| metadata.episodes)),
            runtime_minutes: sidecar
                .and_then(|value| value.runtime_minutes)
                .or_else(|| anilist.and_then(|metadata| metadata.duration)),
            average_score: sidecar
                .and_then(|value| value.average_score)
                .or_else(|| anilist.and_then(|metadata| metadata.average_score)),
            format: sidecar
                .and_then(|value| value.format.clone())
                .or_else(|| anilist.and_then(|metadata| metadata.format.clone())),
            airing_season: sidecar
                .and_then(|value| value.airing_season.clone())
                .or_else(|| anilist.and_then(|metadata| metadata.season.clone())),
            anilist_id: sidecar
                .and_then(|value| value.anilist_id)
                .or_else(|| anilist.map(|metadata| metadata.anilist_id)),
            anilist_url: sidecar
                .and_then(|value| value.anilist_url.clone())
                .or_else(|| anilist.and_then(|metadata| metadata.anilist_url.clone())),
            cover_url: None,
            description: sidecar.and_then(|value| value.description.clone()),
            rating_external: sidecar.and_then(|value| value.rating_external),
            genres: sidecar
                .map(|value| value.genres.clone())
                .unwrap_or_default(),
            tags: sidecar.map(|value| value.tags.clone()).unwrap_or_default(),
            modified_at: None,
            status: sidecar
                .and_then(|value| value.status)
                .map(|status| status.to_string()),
            collection_path,
            size_bytes: file.size_bytes,
            folder_segment: media_type.folder_segment().to_string(),
            sidecar_path: sidecar_path_for(&preview_path)
                .ok()
                .map(|path| path.to_string()),
            sidecar_preview: sidecar_preview.map(ToString::to_string).unwrap_or_else(|| {
                render_sidecar_preview(file, item, media_type, anilist, anime_context.as_ref())
            }),
            steps: item.steps.iter().cloned().map(format_plan_step).collect(),
            audiobook_parts: None,
            is_audiobook_part: false,
            author: sidecar.and_then(|value| value.author.clone()),
        }
    }
}

fn render_sidecar_preview(
    file: &IncomingFile,
    item: &ImportPlanItem,
    media_type: MediaType,
    anilist: Option<&AniListAnimeMetadata>,
    anime_context: Option<&AnimeEpisodeContext>,
) -> String {
    let relative_path = item
        .target_path
        .clone()
        .unwrap_or_else(|| file.source_path.clone());
    let mut entry = MediaEntry::new(
        format!("preview-{}", file.source_path),
        media_type,
        relative_path,
        file.source_path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| file.source_path.to_string()),
    );

    entry.source = match item
        .classification
        .as_ref()
        .map(|classification| classification.source)
    {
        Some(ClassificationSource::Api) => PropertySource::Api,
        Some(ClassificationSource::Ai) => PropertySource::Ai,
        Some(ClassificationSource::User) => PropertySource::User,
        Some(ClassificationSource::Folder)
        | Some(ClassificationSource::Filename)
        | Some(ClassificationSource::Extension)
        | Some(ClassificationSource::Unknown)
        | None => PropertySource::System,
    };
    entry.properties.status = Some(if item.manual_review || item.duplicate_of.is_some() {
        MediaStatus::NeedsReview
    } else {
        MediaStatus::Inbox
    });
    entry.properties.title = build_display_title(file, anilist, media_type);
    entry.properties.title_original = anilist.and_then(|metadata| metadata.title_native.clone());
    entry.properties.description = anilist.and_then(|metadata| metadata.description.clone());
    entry.properties.year = anilist
        .and_then(|metadata| metadata.season_year)
        .or(file.metadata.as_ref().and_then(|metadata| metadata.year));
    entry.properties.anilist_id = anilist.map(|metadata| metadata.anilist_id);
    entry.properties.anilist_url = anilist.and_then(|metadata| metadata.anilist_url.clone());
    entry.properties.series_title = anime_context
        .and_then(|context| context.series_title.clone())
        .or_else(|| {
            anilist.and_then(|metadata| metadata.display_title().map(|value| value.to_string()))
        });
    entry.properties.season_number = anime_context.and_then(|context| context.season_number);
    entry.properties.episode_start = anime_context.and_then(|context| context.episode_start);
    entry.properties.episode_end = anime_context.and_then(|context| context.episode_end);
    entry.properties.episode_title =
        anime_context.and_then(|context| context.episode_title.clone());
    entry.properties.episode_count = anilist.and_then(|metadata| metadata.episodes);
    entry.properties.runtime_minutes = anilist.and_then(|metadata| metadata.duration);
    entry.properties.average_score = anilist.and_then(|metadata| metadata.average_score);
    entry.properties.format = anilist.and_then(|metadata| metadata.format.clone());
    entry.properties.airing_season = anilist.and_then(|metadata| metadata.season.clone());
    entry.properties.rating_external = anilist.and_then(|metadata| metadata.average_score);
    entry.properties.genres = anilist
        .map(|metadata| metadata.genres.clone())
        .unwrap_or_default();
    entry.properties.categories = anilist
        .and_then(|metadata| metadata.format.clone())
        .map(|format| vec![format])
        .unwrap_or_default();
    entry.properties.notes = Some(if item.manual_review {
        "Manuelle Prüfung erforderlich".to_string()
    } else if item.duplicate_of.is_some() {
        "Als Duplikat markiert".to_string()
    } else {
        "Automatisch erzeugte Vorschau".to_string()
    });
    render_sidecar_yaml(&entry).unwrap_or_else(|error| format!("---\nerror: {error}\n---\n"))
}

#[derive(Debug, Clone, Default)]
struct AnimeEpisodeContext {
    series_title: Option<String>,
    season_number: Option<u16>,
    episode_start: Option<u16>,
    episode_end: Option<u16>,
    episode_title: Option<String>,
}

fn build_display_title(
    file: &IncomingFile,
    anilist: Option<&AniListAnimeMetadata>,
    media_type: MediaType,
) -> Option<String> {
    if matches!(media_type, MediaType::Anime | MediaType::HentaiAnime) {
        if let Some(anilist_title) = anilist.and_then(|metadata| metadata.display_title()) {
            return Some(anilist_title.to_string());
        }
    }

    file.metadata
        .as_ref()
        .and_then(|metadata| metadata.title.clone())
        .or_else(|| {
            file.source_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
        })
        .map(|value| normalize_title_candidate(&value))
        .filter(|value| !value.is_empty())
}

fn derive_anime_context(
    file: &IncomingFile,
    _item: &ImportPlanItem,
    anilist: Option<&AniListAnimeMetadata>,
    series_title_hint: Option<&str>,
) -> Option<AnimeEpisodeContext> {
    let media_type = file.classification.as_ref()?.media_type;
    if !matches!(
        media_type,
        MediaType::Anime | MediaType::HentaiAnime | MediaType::Series
    ) {
        return None;
    }

    let file_name = file
        .source_path
        .file_stem()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| file.source_path.to_string());

    // For Anime: AniList > caller hint > folder extraction > file stem.
    // For Series: folder extraction takes priority because the caller hint
    // is derived from the episode filename (e.g. "S01E02 - Title"), not the
    // series name.
    let series_title = if matches!(media_type, MediaType::Anime | MediaType::HentaiAnime) {
        anilist
            .and_then(|metadata| metadata.display_title().map(|value| value.to_string()))
            .or_else(|| series_title_hint.map(|value| value.to_string()))
            .or_else(|| extract_series_hint_from_path(&file.source_path))
            .or_else(|| extract_anime_series_hint(&file.source_path))
            .or_else(|| Some(normalize_title_candidate(&file_name)))
    } else {
        extract_series_hint_from_path(&file.source_path)
            .or_else(|| extract_anime_series_hint(&file.source_path))
            .or_else(|| Some(normalize_title_candidate(&file_name)))
    };

    let season_number = extract_season_number(&file.source_path).or(Some(1));
    let (episode_start, episode_end) = parse_episode_range(&file_name);
    let episode_title = parse_episode_title(&file_name, series_title.as_deref());

    Some(AnimeEpisodeContext {
        series_title,
        season_number,
        episode_start,
        episode_end,
        episode_title,
    })
}

fn build_collection_path(
    media_type: MediaType,
    title: Option<&str>,
    year: Option<u16>,
    anime_context: Option<&AnimeEpisodeContext>,
    anilist: Option<&AniListAnimeMetadata>,
) -> String {
    // Append "(year)" suffix when a year is known — helps distinguish remakes and re-releases.
    let year_suffix =
        |y: Option<u16>| -> String { y.map(|y| format!(" ({y})")).unwrap_or_default() };

    match media_type {
        MediaType::Anime | MediaType::HentaiAnime => {
            if is_anilist_movie(anilist) {
                let t = safe_folder_segment(title.unwrap_or_default(), "Unbenannt");
                let y =
                    year_suffix(year.or_else(|| anilist.and_then(|a| a.start_date.as_ref()?.year)));
                return format!("Anime/Filme/{t}{y}");
            }

            let series = anime_context
                .and_then(|context| context.series_title.as_deref())
                .or(title)
                .unwrap_or("Unbenannt");
            let season_number = anime_context
                .and_then(|context| context.season_number)
                .unwrap_or(1);
            format!(
                "Anime/Serien/{}/Staffel {}",
                safe_folder_segment(series, "Unbenannt"),
                season_number
            )
        }
        MediaType::Series => {
            let series = anime_context
                .and_then(|context| context.series_title.as_deref())
                .or(title)
                .unwrap_or("Unbekannte Serie");
            let season_number = anime_context
                .and_then(|context| context.season_number)
                .unwrap_or(1);
            format!(
                "Serien/{}/Staffel {}",
                safe_folder_segment(series, "Unbekannte Serie"),
                season_number
            )
        }
        MediaType::Film => {
            let t = safe_folder_segment(title.unwrap_or_default(), "Unbenannt");
            let y = year_suffix(year);
            format!("Filme/{t}{y}")
        }
        // Books and Ebooks: Bücher/<Title (Year)>/  — Author subfolder added once OpenLibrary
        // metadata is available.
        MediaType::Book | MediaType::Ebook => {
            let t = safe_folder_segment(title.unwrap_or_default(), "Unbenannt");
            let y = year_suffix(year);
            format!("Bücher/{t}{y}")
        }
        MediaType::Audiobook => {
            let t = safe_folder_segment(title.unwrap_or_default(), "Unbenannt");
            let y = year_suffix(year);
            format!("Hörbücher/{t}{y}")
        }
        // Music: Musik/<Title (Year)>/  — Artist subfolder added once MusicBrainz metadata is
        // available.
        MediaType::MusicAlbum | MediaType::MusicTrack => {
            let t = safe_folder_segment(title.unwrap_or_default(), "Unbenannt");
            let y = year_suffix(year);
            format!("Musik/{t}{y}")
        }
        _ => {
            let folder = media_type.folder_segment();
            let t = safe_folder_segment(title.unwrap_or_default(), "Unbenannt");
            let y = year_suffix(year);
            format!("{folder}/{t}{y}")
        }
    }
}

fn build_target_path_preview(
    file: &IncomingFile,
    item: &ImportPlanItem,
    media_type: MediaType,
    title: Option<&str>,
    anime_context: Option<&AnimeEpisodeContext>,
    anilist: Option<&AniListAnimeMetadata>,
) -> Option<String> {
    let is_anime = matches!(media_type, MediaType::Anime | MediaType::HentaiAnime);
    let is_series_with_context = media_type == MediaType::Series && anime_context.is_some();

    if !is_anime && !is_series_with_context {
        return item.target_path.as_ref().map(|path| path.to_string());
    }

    if is_anime && is_anilist_movie(anilist) {
        let movie_title = safe_folder_segment(title.unwrap_or_default(), "Unbenannt");
        let year = file
            .metadata
            .as_ref()
            .and_then(|m| m.year)
            .or_else(|| anilist.and_then(|a| a.start_date.as_ref()?.year));
        let year_suffix = year.map(|y| format!(" ({y})")).unwrap_or_default();
        let folder_name = format!("{movie_title}{year_suffix}");
        let extension = file
            .source_path
            .extension()
            .map(|ext| ext.to_string_lossy().to_string());
        let mut path = PathBuf::from("Anime");
        path.push("Filme");
        path.push(&folder_name);
        let file_name = match extension {
            Some(extension) if !extension.is_empty() => format!("{folder_name}.{extension}"),
            _ => folder_name,
        };
        path.push(file_name.as_str());
        return Some(path.display().to_string());
    }

    let series_title = anime_context
        .and_then(|context| context.series_title.as_deref())
        .or(title)
        .unwrap_or("Unbenannt");
    let season_number = anime_context
        .and_then(|context| context.season_number)
        .unwrap_or(1);
    let episode_label = anime_context
        .and_then(format_episode_label)
        .unwrap_or_else(|| {
            file.source_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
                .unwrap_or_else(|| "episode".to_string())
        });
    let extension = file
        .source_path
        .extension()
        .map(|ext| ext.to_string_lossy().to_string());

    // Anime → "Anime/Serien/…",  TV Series → "Serien/…"
    let mut path = if is_anime {
        let mut p = PathBuf::from("Anime");
        p.push("Serien");
        p
    } else {
        PathBuf::from("Serien")
    };
    path.push(safe_folder_segment(series_title, "Unbenannt"));
    path.push(format!("Staffel {season_number}"));
    let safe_label = safe_folder_segment(&episode_label, "Unbenannt");
    let file_name = match extension {
        Some(extension) if !extension.is_empty() => format!("{safe_label}.{extension}"),
        _ => safe_label,
    };
    path.push(file_name);
    Some(path.display().to_string())
}

fn is_anilist_movie(anilist: Option<&AniListAnimeMetadata>) -> bool {
    anilist
        .and_then(|metadata| metadata.format.as_deref())
        .map(|format| format.eq_ignore_ascii_case("MOVIE"))
        .unwrap_or(false)
}

fn format_episode_label(context: &AnimeEpisodeContext) -> Option<String> {
    let season = context.season_number.unwrap_or(1);
    match (context.episode_start, context.episode_end) {
        (Some(start), Some(end)) if start != end => {
            let label = format!("S{season:02}E{start:02}-E{end:02}");
            Some(match context.episode_title.as_deref() {
                Some(title) if !title.trim().is_empty() => {
                    format!("{label} - {}", sanitize_path_segment(title))
                }
                _ => label,
            })
        }
        (Some(start), _) => {
            let label = format!("S{season:02}E{start:02}");
            Some(match context.episode_title.as_deref() {
                Some(title) if !title.trim().is_empty() => {
                    format!("{label} - {}", sanitize_path_segment(title))
                }
                _ => label,
            })
        }
        _ => context
            .episode_title
            .as_deref()
            .map(sanitize_path_segment)
            .filter(|title| !title.is_empty()),
    }
}

fn resolve_anilist_metadata_cached(
    client: &AniListClient,
    file: &IncomingFile,
    item: &ImportPlanItem,
    cache: &mut AniListCacheMap,
) -> Option<AniListAnimeMetadata> {
    let classification = item
        .classification
        .as_ref()
        .or(file.classification.as_ref())?;
    if !should_attempt_anilist(classification.media_type, &file.source_path) {
        return None;
    }

    let search_title = build_anime_search_title(file)?;
    let cache_key = search_title.to_lowercase();

    if let Some(cached) = cache.get(&cache_key) {
        return Some(cached.clone());
    }

    let result = client
        .search_anime(
            &search_title,
            AniListClient::adult_flag_for(classification.media_type),
        )
        .ok()
        .flatten();

    if let Some(ref metadata) = result {
        cache.insert(cache_key, metadata.clone());
    }

    result
}

fn should_attempt_anilist(media_type: MediaType, source_path: &RelativePath) -> bool {
    if matches!(media_type, MediaType::Anime | MediaType::HentaiAnime) {
        return true;
    }

    source_path.to_string().to_lowercase().contains("anime")
}

fn build_anime_search_title(file: &IncomingFile) -> Option<String> {
    let raw = file
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.title.clone())
        .or_else(|| extract_series_hint_from_path(&file.source_path))
        .or_else(|| extract_anime_series_hint(&file.source_path))
        .or_else(|| {
            file.source_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().to_string())
        })?;

    let cleaned = normalize_title_candidate(&raw);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

fn normalize_title_candidate(value: &str) -> String {
    let mut normalized = value.replace(['_', '.'], " ");
    normalized = strip_bracketed_sections(&normalized);
    normalized = normalized.replace("  ", " ");

    if let Some((head, tail)) = normalized.rsplit_once(" - ") {
        if looks_like_episode_fragment(tail) {
            normalized = head.to_string();
        }
    }

    normalized
        .split_whitespace()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn strip_bracketed_sections(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    let mut square_depth = 0usize;
    let mut round_depth = 0usize;

    for character in value.chars() {
        match character {
            '[' => square_depth += 1,
            ']' => square_depth = square_depth.saturating_sub(1),
            '(' => round_depth += 1,
            ')' => round_depth = round_depth.saturating_sub(1),
            _ if square_depth == 0 && round_depth == 0 => result.push(character),
            _ => {}
        }
    }

    result
}

fn looks_like_episode_fragment(value: &str) -> bool {
    let lower = value.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }

    lower.chars().all(|character| {
        character.is_ascii_digit()
            || matches!(character, '+' | '-' | 'e' | 'p' | 's' | 'x' | ' ' | '.')
    }) || lower.contains("episode")
        || lower.contains("ep")
}

fn parse_episode_range(value: &str) -> (Option<u16>, Option<u16>) {
    let lower = value.to_lowercase();
    if let Some((start, end)) = lower.split_once('+') {
        return (parse_leading_number(start), parse_leading_number(end));
    }

    if let Some((start, end)) = lower.split_once('-') {
        let start_number = parse_leading_number(start);
        let end_number = parse_leading_number(end);
        if start_number.is_some() && end_number.is_some() {
            return (start_number, end_number);
        }
    }

    if let Some(number) = extract_number_after_marker(&lower, "episode") {
        return (Some(number), Some(number));
    }

    if let Some(number) = extract_number_after_marker(&lower, "ep") {
        return (Some(number), Some(number));
    }

    if let Some(number) = extract_short_marker_number(&lower, 'e') {
        return (Some(number), Some(number));
    }

    if let Some(number) = trailing_number(&lower) {
        return (Some(number), Some(number));
    }

    (None, None)
}

fn parse_episode_title(value: &str, series_title: Option<&str>) -> Option<String> {
    let cleaned = normalize_title_candidate(value);
    if cleaned.is_empty() {
        return None;
    }

    if let Some(series_title) = series_title {
        let lower_cleaned = cleaned.to_lowercase();
        let lower_series = series_title.to_lowercase();
        if lower_cleaned == lower_series {
            return None;
        }
        if lower_cleaned.starts_with(&lower_series) {
            if let Some(rest) = cleaned.get(series_title.len()..) {
                let rest = rest.trim_start_matches(['-', ':', ' ']).trim();
                if !rest.is_empty() {
                    return Some(rest.to_string());
                }
            }
        }
    }

    Some(cleaned)
}

fn extract_anime_series_hint(relative_path: &RelativePath) -> Option<String> {
    let mut components = relative_path.as_path().components();
    while let Some(component) = components.next() {
        let text = component.as_os_str().to_string_lossy();
        if text.eq_ignore_ascii_case("anime") {
            let mut series_candidate = None;
            for next in components {
                let candidate = next.as_os_str().to_string_lossy().to_string();
                if is_season_folder(&candidate) {
                    continue;
                }
                series_candidate = Some(candidate);
                break;
            }
            return series_candidate.map(|value| normalize_title_candidate(&value));
        }
    }

    None
}

fn extract_series_hint_from_path(relative_path: &RelativePath) -> Option<String> {
    let mut previous_meaningful: Option<String> = None;

    for component in relative_path.as_path().components() {
        let text = component.as_os_str().to_string_lossy();
        let value = text.trim();
        if value.is_empty() || is_hidden_system_entry(value) {
            continue;
        }

        if is_season_folder(value) {
            if let Some(previous) = previous_meaningful.as_ref() {
                let candidate = normalize_title_candidate(previous);
                if !candidate.is_empty() {
                    return Some(candidate);
                }
            }
            continue;
        }

        previous_meaningful = Some(value.to_string());
    }

    None
}

fn is_hidden_system_entry(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.starts_with("._")
        || trimmed.starts_with('.')
        || trimmed.eq_ignore_ascii_case(".ds_store")
        || trimmed.eq_ignore_ascii_case("thumbs.db")
        || trimmed.eq_ignore_ascii_case("desktop.ini")
}

fn extract_season_number(relative_path: &RelativePath) -> Option<u16> {
    let path = relative_path.to_string().to_lowercase();
    if let Some(number) = extract_number_after_marker(&path, "season ") {
        return Some(number);
    }
    if let Some(number) = extract_number_after_marker(&path, "staffel ") {
        return Some(number);
    }
    if let Some(number) = extract_number_after_marker(&path, "season") {
        return Some(number);
    }
    if let Some(number) = extract_number_after_marker(&path, "staffel") {
        return Some(number);
    }
    if let Some(number) = extract_short_marker_number(&path, 's') {
        return Some(number);
    }

    None
}

fn is_season_folder(value: &str) -> bool {
    let lower = value.trim().to_lowercase();
    lower.starts_with("season ")
        || lower.starts_with("season_")
        || lower.starts_with("season-")
        || lower.starts_with("season")
        || lower.starts_with("staffel ")
        || lower.starts_with("staffel_")
        || lower.starts_with("staffel-")
        || lower.starts_with("staffel")
        || extract_short_marker_number(&lower, 's').is_some()
}

fn extract_number_after_marker(value: &str, marker: &str) -> Option<u16> {
    let index = value.find(marker)?;
    let tail = &value[index + marker.len()..];
    parse_leading_number(tail)
}

fn extract_short_marker_number(value: &str, marker: char) -> Option<u16> {
    for (index, character) in value.char_indices() {
        if character != marker {
            continue;
        }

        let before_is_boundary = value[..index]
            .chars()
            .next_back()
            .map(|previous| !previous.is_ascii_alphanumeric())
            .unwrap_or(true);
        let after_is_digit = value[index + character.len_utf8()..]
            .chars()
            .next()
            .map(|next| next.is_ascii_digit())
            .unwrap_or(false);

        if before_is_boundary && after_is_digit {
            return parse_leading_number(&value[index + character.len_utf8()..]);
        }
    }

    None
}

fn parse_leading_number(value: &str) -> Option<u16> {
    let digits: String = value
        .chars()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(|character| character.is_ascii_digit())
        .collect();

    if digits.is_empty() {
        return None;
    }

    digits.parse().ok()
}

fn trailing_number(value: &str) -> Option<u16> {
    let digits: String = value
        .chars()
        .rev()
        .skip_while(|character| !character.is_ascii_digit())
        .take_while(|character| character.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();

    if digits.is_empty() {
        return None;
    }

    digits.parse().ok()
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

fn classification_source_label(source: &ClassificationSource) -> String {
    match source {
        ClassificationSource::User => "user".to_string(),
        ClassificationSource::Folder => "folder".to_string(),
        ClassificationSource::Filename => "filename".to_string(),
        ClassificationSource::Extension => "extension".to_string(),
        ClassificationSource::Api => "api".to_string(),
        ClassificationSource::Ai => "ai".to_string(),
        ClassificationSource::Unknown => "unknown".to_string(),
    }
}

fn requires_review(item: &ImportPlanItem) -> bool {
    item.manual_review
        || item.duplicate_of.is_some()
        || item.steps.iter().any(|step| {
            matches!(
                step,
                PlannedImportStep::QueueReview { .. } | PlannedImportStep::AskUser { .. }
            )
        })
}

fn format_plan_step(step: PlannedImportStep) -> String {
    match step {
        PlannedImportStep::DetectType => "Typ erkennen".to_string(),
        PlannedImportStep::FetchMetadata { provider } => format!("Metadaten von {provider} holen"),
        PlannedImportStep::AskUser { prompt } => {
            format!("Benutzer fragen: {}", prompt.message)
        }
        PlannedImportStep::MoveFile { target } => format!("Datei verschieben nach {target}"),
        PlannedImportStep::WriteSidecar { target } => format!("Sidecar schreiben nach {target}"),
        PlannedImportStep::RecordAudit => "Audit-Eintrag schreiben".to_string(),
        PlannedImportStep::RegisterDuplicate { fingerprint } => {
            format!("Fingerprint registrieren {fingerprint}")
        }
        PlannedImportStep::QueueReview { reason } => format!("Zur Prüfung: {reason}"),
        PlannedImportStep::Skip { reason } => format!("Überspringen: {reason}"),
    }
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

fn media_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        // Images
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("bmp") => "image/bmp",
        Some("svg") => "image/svg+xml",
        Some("avif") => "image/avif",
        Some("tif" | "tiff") => "image/tiff",
        // Video (natively supported by macOS WKWebView)
        Some("mp4" | "m4v") => "video/mp4",
        Some("mov") => "video/quicktime",
        Some("webm") => "video/webm",
        Some("ogv") => "video/ogg",
        Some("avi") => "video/x-msvideo",
        Some("mkv") => "video/x-matroska",
        // Audio
        Some("mp3") => "audio/mpeg",
        Some("m4a" | "m4b") => "audio/mp4",
        Some("aac") => "audio/aac",
        Some("ogg" | "oga") => "audio/ogg",
        Some("opus") => "audio/opus",
        Some("flac") => "audio/flac",
        Some("wav") => "audio/wav",
        Some("weba") => "audio/webm",
        // Documents
        Some("pdf") => "application/pdf",
        Some("epub") => "application/epub+zip",
        _ => "application/octet-stream",
    }
}

fn apply_import_item(vault: &Vault, item: &ApplyImportItem) -> Result<()> {
    let source_relative = RelativePath::new(&item.source_path)?;
    let target_relative = RelativePath::new(&item.target_path)?;

    let source_absolute = vault.resolve(source_relative.as_path())?;
    let target_absolute = vault.resolve(target_relative.as_path())?;

    if !source_absolute.exists() {
        return Err(VaultError::InvalidVaultPath(format!(
            "Quelldatei nicht gefunden: {}",
            source_relative
        )));
    }

    let target_parent = target_absolute.parent().ok_or_else(|| {
        VaultError::InvalidVaultPath(format!(
            "Zielpfad hat keinen Elternordner: {}",
            target_relative
        ))
    })?;
    fs::create_dir_all(target_parent).map_err(VaultError::from)?;

    if source_absolute != target_absolute {
        if target_absolute.exists() {
            if item.overwrite {
                // User confirmed overwrite: remove the existing target so the
                // inbox copy replaces it and no duplicate remains.
                fs::remove_file(&target_absolute).map_err(VaultError::from)?;
            } else {
                return Err(VaultError::InvalidVaultPath(format!(
                    "Zieldatei existiert bereits: {}",
                    target_relative
                )));
            }
        }
        move_file_with_fallback(&source_absolute, &target_absolute)?;
    }

    write_sidecar_preview(vault, &target_relative, &item.sidecar_preview)?;

    prune_empty_inbox_dirs(vault, source_absolute.parent());
    Ok(())
}

fn save_sidecar_item(vault: &Vault, item: &SaveSidecarItem) -> Result<()> {
    let media_relative = RelativePath::new(&item.media_path)?;
    let media_absolute = vault.resolve(media_relative.as_path())?;

    if !media_absolute.exists() {
        return Err(VaultError::InvalidVaultPath(format!(
            "Datei nicht gefunden: {}",
            media_relative
        )));
    }

    write_sidecar_preview(vault, &media_relative, &item.sidecar_preview)
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

/// Moves a file, falling back to copy+delete across filesystem boundaries.
///
/// The fallback copies to a `.part` sibling first, flushes it to disk and only
/// then renames it into place. A crash or a full disk therefore leaves either
/// the untouched source or a complete target — never a truncated file under
/// the final name.
fn move_file_with_fallback(source: &Path, target: &Path) -> Result<()> {
    match fs::rename(source, target) {
        Ok(()) => Ok(()),
        Err(error) if is_cross_device_error(&error) => {
            let staging = partial_copy_path(target);
            // A leftover from an earlier aborted run must not be reused.
            let _ = fs::remove_file(&staging);

            fs::copy(source, &staging).map_err(|error| {
                let _ = fs::remove_file(&staging);
                VaultError::from(error)
            })?;

            // Without the flush the rename can be visible before the contents
            // are, so a power loss would leave an empty file in the library.
            let flushed = fs::File::open(&staging).and_then(|file| file.sync_all());
            if let Err(error) = flushed {
                let _ = fs::remove_file(&staging);
                return Err(VaultError::from(error));
            }

            if let Err(error) = fs::rename(&staging, target) {
                let _ = fs::remove_file(&staging);
                return Err(VaultError::from(error));
            }

            fs::remove_file(source).map_err(VaultError::from)?;
            Ok(())
        }
        Err(error) => Err(VaultError::from(error)),
    }
}

/// Extension appended to the temporary file used while copying across devices.
const PARTIAL_COPY_EXTENSION: &str = "fero-part";

/// Returns the staging path for `target` (`<target>.fero-part`).
///
/// The suffix is appended rather than replacing the extension so that
/// `Film.mkv` and `Film.mp4` cannot stage onto the same temporary file.
fn partial_copy_path(target: &Path) -> PathBuf {
    let mut staging = target.as_os_str().to_owned();
    staging.push(".");
    staging.push(PARTIAL_COPY_EXTENSION);
    PathBuf::from(staging)
}

/// `EXDEV` — source and target live on different filesystems.
#[cfg(unix)]
const CROSS_DEVICE_OS_ERROR: i32 = 18;
/// `ERROR_NOT_SAME_DEVICE`.
#[cfg(windows)]
const CROSS_DEVICE_OS_ERROR: i32 = 17;

/// Whether an I/O error means "source and target are on different filesystems".
#[cfg(any(unix, windows))]
fn is_cross_device_error(error: &std::io::Error) -> bool {
    error.raw_os_error() == Some(CROSS_DEVICE_OS_ERROR)
}

#[cfg(not(any(unix, windows)))]
fn is_cross_device_error(_error: &std::io::Error) -> bool {
    false
}

/// Removes now-empty directories left behind after a file moved out.
///
/// Walks upwards and stops at the inbox root, the vault root, or as soon as a
/// directory is non-empty. The boundaries are compared on canonical paths:
/// callers pass paths that went through `resolve_existing`, and on macOS a
/// vault below a symlinked prefix (`/tmp` → `/private/tmp`) would otherwise
/// never match the boundary and the walk would continue *above* the vault.
fn prune_empty_inbox_dirs(vault: &Vault, start: Option<&Path>) {
    let canonical = |path: &Path| fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let inbox_root = canonical(&vault.inbox_dir());
    let vault_root = canonical(vault.root());
    let mut current = start.map(canonical);

    while let Some(path) = current {
        if path == inbox_root || path == vault_root {
            break;
        }
        // Never touch anything outside the vault, whatever the caller passed.
        if !path.starts_with(&vault_root) {
            break;
        }

        let can_remove = match fs::read_dir(&path) {
            Ok(mut entries) => entries.next().is_none(),
            Err(_) => false,
        };

        if !can_remove {
            break;
        }

        let parent = path.parent().map(Path::to_path_buf);
        if fs::remove_dir(&path).is_err() {
            break;
        }
        current = parent;
    }
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

fn save_app_state(state: &AppState) -> Result<()> {
    let path = app_state_path()?;
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent);
    }

    let body = serde_json::to_string_pretty(state)
        .map_err(|error| VaultError::Serialization(error.to_string()))?;
    fs::write(path, body).map_err(VaultError::from)
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

/// How many previously opened vault roots stay authorized.
const MAX_KNOWN_ROOTS: usize = 16;

/// Records a vault root as user-approved.
fn register_known_root(state: &mut AppState, root: &Path) {
    let value = root.display().to_string();
    if state.known_roots.iter().any(|known| known == &value) {
        return;
    }
    state.known_roots.push(value);
    if state.known_roots.len() > MAX_KNOWN_ROOTS {
        let excess = state.known_roots.len() - MAX_KNOWN_ROOTS;
        state.known_roots.drain(0..excess);
    }
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

#[derive(Debug, Clone, Serialize)]
struct VaultRootResponse {
    root: Option<String>,
    error: Option<String>,
}

impl VaultRootResponse {
    fn error(error: String) -> Self {
        Self {
            root: None,
            error: Some(error),
        }
    }
}

fn scan_vault_files(vault: &Vault) -> Result<Vec<ScannedVaultFile>> {
    let mut files = Vec::new();
    scan_directory(vault, vault.root(), &mut files)?;
    Ok(files)
}

fn scan_directory(
    vault: &Vault,
    directory: &Path,
    files: &mut Vec<ScannedVaultFile>,
) -> Result<()> {
    for entry in fs::read_dir(directory).map_err(VaultError::from)? {
        let entry = entry.map_err(VaultError::from)?;
        let path = entry.path();

        let metadata = entry.metadata().map_err(VaultError::from)?;

        if metadata.is_dir() {
            if should_skip_scanned_directory(vault, &path) {
                continue;
            }
            scan_directory(vault, &path, files)?;
            continue;
        }

        if !metadata.is_file() {
            continue;
        }

        if should_skip_scanned_file(&path) {
            continue;
        }

        let relative_path = vault.relative_from_absolute(&path)?;
        let sidecar_file = find_sidecar_file(vault, &relative_path)?;
        let sidecar_preview = sidecar_file
            .as_ref()
            .map(|path| fs::read_to_string(path).map_err(VaultError::from))
            .transpose()?;
        let parsed_sidecar = sidecar_preview
            .as_deref()
            .map(parse_sidecar_metadata)
            .transpose()?;
        let parsed_nfo = find_nfo_companion(&path)
            .map(|nfo_path| {
                fs::read_to_string(&nfo_path)
                    .map(|content| parse_nfo_metadata(&content))
                    .map_err(VaultError::from)
            })
            .transpose()?;
        // NFO data fills any gap not already covered by the .fero.yaml sidecar.
        let effective_sidecar = merge_nfo_into_sidecar(parsed_sidecar, parsed_nfo.as_ref());
        let fingerprint = if is_in_inbox(&relative_path) {
            Some(compute_fingerprint_for_file(&path)?)
        } else {
            None
        };
        let classification = effective_sidecar
            .as_ref()
            .and_then(classification_from_sidecar)
            .or_else(|| detect_classification(&relative_path));
        let resolved_title = effective_sidecar
            .as_ref()
            .and_then(|sidecar| {
                sidecar
                    .series_title
                    .clone()
                    .or_else(|| sidecar.title.clone())
            })
            .or_else(|| {
                relative_path
                    .file_stem()
                    .map(|stem| stem.to_string_lossy().to_string())
            });
        let resolved_metadata = Some(ResolvedMetadata {
            title: resolved_title,
            year: effective_sidecar.as_ref().and_then(|sidecar| sidecar.year),
        });

        files.push(ScannedVaultFile {
            incoming: IncomingFile {
                source_path: relative_path,
                size_bytes: metadata.len(),
                fingerprint,
                classification,
                metadata: resolved_metadata,
            },
            sidecar: effective_sidecar,
            sidecar_preview,
        });
    }

    Ok(())
}

fn should_skip_scanned_directory(vault: &Vault, path: &Path) -> bool {
    if path == vault.system_dir() || path == vault.root().join(LEGACY_SYSTEM_DIR) {
        return true;
    }

    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };

    name.eq_ignore_ascii_case("_review_queue") || is_hidden_system_entry(name)
}

fn should_skip_scanned_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|value| value.to_str()) else {
        return false;
    };

    let lower = name.to_lowercase();
    is_hidden_system_entry(name)
        || name.ends_with(SIDECAR_SUFFIX)
        || name.ends_with(LEGACY_SIDECAR_SUFFIX)
        // NFO files are metadata companions, not independent media entries.
        || lower.ends_with(".nfo")
        // Cover art files (cover.jpg, poster.png, …) are companions of the
        // media next to them — indexing them as standalone images put every
        // downloaded webnovel cover into the "Bilder" collection.
        || COVER_FILENAMES.contains(&lower.as_str())
}

/// Audio file extensions that qualify a file to be counted as an audiobook part.
const AUDIOBOOK_AUDIO_EXTS: &[&str] = &[
    "mp3", "m4a", "m4b", "aac", "ogg", "opus", "flac", "wav", "weba",
];

/// Numeric-aware string comparison so that "Part9" sorts before "Part21".
///
/// Splits each string into runs of digits and non-digits and compares runs
/// of digits numerically rather than lexicographically.
fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let mut ai = a.chars().peekable();
    let mut bi = b.chars().peekable();
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return std::cmp::Ordering::Equal,
            (None, _) => return std::cmp::Ordering::Less,
            (_, None) => return std::cmp::Ordering::Greater,
            (Some(ac), Some(bc)) => {
                if ac.is_ascii_digit() && bc.is_ascii_digit() {
                    let mut an = String::new();
                    while ai.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        an.push(ai.next().unwrap());
                    }
                    let mut bn = String::new();
                    while bi.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        bn.push(bi.next().unwrap());
                    }
                    let av: u64 = an.parse().unwrap_or(0);
                    let bv: u64 = bn.parse().unwrap_or(0);
                    match av.cmp(&bv) {
                        std::cmp::Ordering::Equal => {}
                        other => return other,
                    }
                } else {
                    ai.next();
                    bi.next();
                    let ac_lo = ac.to_lowercase().next().unwrap_or(ac);
                    let bc_lo = bc.to_lowercase().next().unwrap_or(bc);
                    match ac_lo.cmp(&bc_lo) {
                        std::cmp::Ordering::Equal => {}
                        other => return other,
                    }
                }
            }
        }
    }
}

/// Post-processes a flat list of plan items to detect multi-file audiobook groups.
///
/// Rule: if every audio file in a directory is classified as `Audiobook`, they
/// are treated as parts of a single audiobook.  The lexicographically first
/// item becomes the group representative and receives `audiobook_parts`; the
/// others are flagged as `is_audiobook_part = true`.
///
/// Items that don't belong to a multi-file group are left unchanged.
/// Converts a hyphen-separated folder name to a human-readable display title.
///
/// Example: `"Listening-to-Bone-Lord--Scribd"` → `"Listening to Bone Lord Scribd"`
fn prettify_folder_title(raw: &str) -> String {
    raw.replace("--", " ")
        .replace('-', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn group_audiobook_folders(items: &mut [DemoPlanItem], vault_root: Option<&Path>) {
    // Group item indices by parent directory, counting only Audiobook items.
    let mut dir_groups: HashMap<String, Vec<usize>> = HashMap::new();

    for (idx, item) in items.iter().enumerate() {
        if item.media_type != MediaType::Audiobook.to_string() {
            continue;
        }
        let ext = item
            .source_path
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if !AUDIOBOOK_AUDIO_EXTS.contains(&ext.as_str()) {
            continue;
        }
        // Parent directory = everything before the last '/'
        let parent = item
            .source_path
            .rfind('/')
            .map(|idx| item.source_path[..idx].to_string())
            .unwrap_or_default();
        dir_groups.entry(parent).or_default().push(idx);
    }

    for (_dir, mut group_indices) in dir_groups {
        if group_indices.len() < 2 {
            continue; // Single file — not a multi-part audiobook
        }

        // Sort indices by source_path using numeric-aware comparison so that
        // "Part9" sorts before "Part21" even without zero-padding.
        group_indices.sort_by(|&a, &b| natural_cmp(&items[a].source_path, &items[b].source_path));

        // Try to read embedded audio tags from the first file in the group.
        // Tags often contain a cleaner title and author than the folder name.
        let tags = vault_root.and_then(|root| {
            let rel = &items[group_indices[0]].source_path;
            let abs = root.join(rel);
            read_audio_tags(&abs)
        });

        // Derive the audiobook folder name.  Priority:
        //   1. Album tag from embedded metadata (clean title)
        //   2. Parent directory name (prettified hyphens → spaces)
        let (path_segment, display_title) = {
            if let Some(album) = tags.as_ref().and_then(|t| t.album.as_deref()) {
                (sanitize_path_segment(album), album.to_string())
            } else {
                let src = &items[group_indices[0]].source_path;
                let raw = src
                    .rfind('/')
                    .and_then(|end| {
                        let parent = &src[..end];
                        parent
                            .rfind('/')
                            .map(|start| parent[start + 1..].to_string())
                    })
                    .unwrap_or_else(|| "Unbenannt".to_string());
                (sanitize_path_segment(&raw), prettify_folder_title(&raw))
            }
        };

        // Author from tags, if available.
        let tag_author = tags.and_then(|t| t.artist);

        // Update representative: title and (if found) author.
        items[group_indices[0]].title = Some(display_title);
        items[group_indices[0]].author = tag_author;

        // Set correct target_path for all members so they land in the same folder.
        for &idx in &group_indices {
            let src = items[idx].source_path.clone();
            let file_name = match src.rfind('/') {
                Some(pos) => src[pos + 1..].to_string(),
                None => src,
            };
            items[idx].target_path = Some(format!("Hörbücher/{}/{}", path_segment, file_name));
        }

        let part_paths: Vec<String> = group_indices
            .iter()
            .map(|&idx| items[idx].source_path.clone())
            .collect();

        let representative = group_indices[0];
        items[representative].audiobook_parts = Some(part_paths);

        for &part_idx in &group_indices[1..] {
            items[part_idx].is_audiobook_part = true;
        }
    }
}

// ---------------------------------------------------------------------------
// Audio tag extraction (ID3v2 + MP4) — no external crate required.
// ---------------------------------------------------------------------------

/// Album title and artist extracted from embedded audio metadata.
struct AudioTagData {
    /// Album title — typically the audiobook or book title.
    album: Option<String>,
    /// Album artist or lead artist — typically the author.
    artist: Option<String>,
}

/// Reads the first `max_bytes` of a file into a buffer.
fn read_file_prefix(path: &Path, max_bytes: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max_bytes];
    let n = file.read(&mut buf).ok()?;
    buf.truncate(n);
    if n < 10 {
        None
    } else {
        Some(buf)
    }
}

/// Attempts to read audio metadata tags from the given file.
///
/// Recognises ID3v2.3/v2.4 headers (MP3) and MPEG-4 atoms (M4B/M4A/AAC).
/// Returns `None` if the format is unrecognised or tags are absent.
fn read_audio_tags(path: &Path) -> Option<AudioTagData> {
    // 512 KB is enough to cover the ID3 header for most files; for MP4 the
    // moov metadata box is usually written first (fast-start).
    let data = read_file_prefix(path, 512 * 1024)?;

    if data.len() >= 10 && &data[0..3] == b"ID3" {
        return parse_id3v2_tags(&data);
    }

    // MP4/M4B/M4A: the 4 bytes at offset 4 are the box type; ftyp or moov are
    // both valid indicators.
    if data.len() >= 8 && (&data[4..8] == b"ftyp" || &data[4..8] == b"moov") {
        return parse_mp4_tags(&data);
    }

    None
}

// --- ID3v2 ---------------------------------------------------------------

fn parse_id3v2_tags(data: &[u8]) -> Option<AudioTagData> {
    if data.len() < 10 {
        return None;
    }
    let version = data[3];
    // Only handle v2.3 and v2.4; v2.2 uses 3-byte frame IDs (rare today).
    if !(3..=4).contains(&version) {
        return None;
    }
    let flags = data[5];

    // Tag size is encoded as a 4-byte synchsafe integer.
    let tag_size = ((data[6] as usize & 0x7F) << 21)
        | ((data[7] as usize & 0x7F) << 14)
        | ((data[8] as usize & 0x7F) << 7)
        | (data[9] as usize & 0x7F);
    let tag_end = (10 + tag_size).min(data.len());

    // Skip optional extended header (flag bit 6).
    let mut pos = 10;
    if (flags & 0x40) != 0 && pos + 4 <= tag_end {
        let ext_size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += ext_size;
    }

    let mut album: Option<String> = None;
    let mut album_artist: Option<String> = None;
    let mut lead_artist: Option<String> = None;

    while pos + 10 < tag_end {
        // Null padding marks end of frames.
        if data[pos] == 0 {
            break;
        }
        let frame_id = &data[pos..pos + 4];

        let frame_size = if version == 4 {
            // ID3v2.4: frame size is also synchsafe.
            ((data[pos + 4] as usize & 0x7F) << 21)
                | ((data[pos + 5] as usize & 0x7F) << 14)
                | ((data[pos + 6] as usize & 0x7F) << 7)
                | (data[pos + 7] as usize & 0x7F)
        } else {
            u32::from_be_bytes([data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]])
                as usize
        };

        if frame_size == 0 {
            pos += 10;
            continue;
        }
        if pos + 10 + frame_size > tag_end {
            break;
        }

        let content = &data[pos + 10..pos + 10 + frame_size];
        if frame_id == b"TALB" {
            album = decode_id3_text(content);
        } else if frame_id == b"TPE2" {
            album_artist = decode_id3_text(content);
        } else if frame_id == b"TPE1" {
            lead_artist = decode_id3_text(content);
        }

        pos += 10 + frame_size;
    }

    Some(AudioTagData {
        album,
        artist: album_artist.or(lead_artist),
    })
}

/// Decodes an ID3 text frame.  The first byte is the encoding flag:
/// 0 = ISO-8859-1, 1/2 = UTF-16 (with/without BOM), 3 = UTF-8.
fn decode_id3_text(data: &[u8]) -> Option<String> {
    if data.is_empty() {
        return None;
    }
    let encoding = data[0];
    let text = &data[1..];
    let result: String = match encoding {
        // ISO-8859-1: map bytes directly to char (valid for ASCII range)
        0 => text
            .iter()
            .map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' })
            .collect(),
        // UTF-16 with or without BOM
        1 | 2 => {
            if text.len() < 2 {
                return None;
            }
            let units: Vec<u16> = if text[0] == 0xFF && text[1] == 0xFE {
                text[2..]
                    .chunks(2)
                    .filter(|c| c.len() == 2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect()
            } else if text[0] == 0xFE && text[1] == 0xFF {
                text[2..]
                    .chunks(2)
                    .filter(|c| c.len() == 2)
                    .map(|c| u16::from_be_bytes([c[0], c[1]]))
                    .collect()
            } else {
                // No BOM — assume little-endian (most common on Windows/iTunes)
                text.chunks(2)
                    .filter(|c| c.len() == 2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect()
            };
            String::from_utf16_lossy(&units)
        }
        // UTF-8
        3 => String::from_utf8_lossy(text).into_owned(),
        _ => return None,
    };
    let cleaned = result.trim_matches('\0').trim().to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

// --- MPEG-4 atoms (M4B / M4A / AAC) -------------------------------------

/// Returns `(content_start, content_end)` for the first atom named `target`
/// found by scanning `data[from..to]`.
fn find_mp4_atom(data: &[u8], target: &[u8], from: usize, to: usize) -> Option<(usize, usize)> {
    let mut pos = from;
    let limit = to.min(data.len());
    while pos + 8 <= limit {
        let size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        if size < 8 {
            break;
        }
        let end = pos + size;
        if end > data.len() {
            break;
        }
        if &data[pos + 4..pos + 8] == target {
            return Some((pos + 8, end));
        }
        pos = end;
    }
    None
}

fn parse_mp4_tags(data: &[u8]) -> Option<AudioTagData> {
    let (moov_s, moov_e) = find_mp4_atom(data, b"moov", 0, data.len())?;

    // Locate ilst: try moov→udta→meta→ilst, then moov→meta→ilst.
    let ilst = locate_mp4_ilst(data, moov_s, moov_e).or_else(|| {
        // Fallback: meta directly inside moov (some encoders skip udta)
        let (meta_s, meta_e) = find_mp4_atom(data, b"meta", moov_s, moov_e)?;
        // meta is a FullBox: skip 4-byte version/flags
        let adj = (meta_s + 4).min(meta_e);
        let (ilst_s, ilst_e) = find_mp4_atom(data, b"ilst", adj, meta_e)?;
        Some((ilst_s, ilst_e))
    })?;

    let (ilst_s, ilst_e) = ilst;

    let album = read_mp4_string_item(data, b"\xa9alb", ilst_s, ilst_e);
    let album_artist = read_mp4_string_item(data, b"aART", ilst_s, ilst_e);
    let artist = read_mp4_string_item(data, b"\xa9ART", ilst_s, ilst_e);

    Some(AudioTagData {
        album,
        artist: album_artist.or(artist),
    })
}

fn locate_mp4_ilst(data: &[u8], moov_s: usize, moov_e: usize) -> Option<(usize, usize)> {
    let (udta_s, udta_e) = find_mp4_atom(data, b"udta", moov_s, moov_e)?;
    let (meta_s, meta_e) = find_mp4_atom(data, b"meta", udta_s, udta_e)?;
    // meta FullBox: skip 4-byte version/flags
    let adj = (meta_s + 4).min(meta_e);
    let (ilst_s, ilst_e) = find_mp4_atom(data, b"ilst", adj, meta_e)?;
    Some((ilst_s, ilst_e))
}

/// Reads the string value of an iTunes-style item atom (e.g. `©alb`) from ilst.
fn read_mp4_string_item(
    data: &[u8],
    item_name: &[u8],
    ilst_s: usize,
    ilst_e: usize,
) -> Option<String> {
    let (item_s, item_e) = find_mp4_atom(data, item_name, ilst_s, ilst_e)?;
    // Inside the item atom is a 'data' atom: header(8) + type(4) + locale(4) + string
    let (data_s, data_e) = find_mp4_atom(data, b"data", item_s, item_e)?;
    let payload_start = data_s + 8; // skip type(4) + locale(4)
    if payload_start >= data_e {
        return None;
    }
    let text = String::from_utf8_lossy(&data[payload_start..data_e]).into_owned();
    let cleaned = text.trim_matches('\0').trim().to_string();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

/// Looks for a Kodi/XBMC-style `.nfo` companion file alongside a media file.
///
/// Checks in priority order:
/// 1. Same filename with `.nfo` extension (e.g. `Movie.mkv` → `Movie.nfo`)
/// 2. `movie.nfo` in the same directory (common Kodi convention)
fn find_nfo_companion(media_path: &Path) -> Option<PathBuf> {
    let mut nfo_path = media_path.to_path_buf();
    nfo_path.set_extension("nfo");
    if nfo_path.exists() {
        return Some(nfo_path);
    }
    let movie_nfo = media_path.parent()?.join("movie.nfo");
    if movie_nfo.exists() {
        Some(movie_nfo)
    } else {
        None
    }
}

/// Metadata extracted from a Kodi/XBMC NFO companion file.
#[derive(Debug, Default)]
struct ParsedNfoMetadata {
    title: Option<String>,
    /// For episode NFOs: the show title (`<showtitle>`).
    series_title: Option<String>,
    year: Option<u16>,
    season_number: Option<u16>,
    episode_start: Option<u16>,
}

/// Parses a subset of the Kodi NFO XML format without a full XML parser.
///
/// Only extracts fields that map to existing `ParsedSidecar` slots; everything
/// else is ignored.  The NFO format is well-defined enough that tag-based
/// substring search is reliable here.
fn parse_nfo_metadata(content: &str) -> ParsedNfoMetadata {
    ParsedNfoMetadata {
        title: extract_xml_tag(content, "title"),
        series_title: extract_xml_tag(content, "showtitle"),
        year: extract_xml_tag(content, "year").and_then(|s| s.parse::<u16>().ok()),
        season_number: extract_xml_tag(content, "season").and_then(|s| s.parse::<u16>().ok()),
        episode_start: extract_xml_tag(content, "episode").and_then(|s| s.parse::<u16>().ok()),
    }
}

/// Extracts the text content of the first matching XML tag.
fn extract_xml_tag(content: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = content.find(&open)? + open.len();
    let end = content[start..].find(&close).map(|i| start + i)?;
    let value = content[start..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Merges NFO companion data into a sidecar, with NFO only filling absent fields.
///
/// The `.fero.yaml` sidecar always wins; NFO data only fills gaps.
fn merge_nfo_into_sidecar(
    sidecar: Option<ParsedSidecar>,
    nfo: Option<&ParsedNfoMetadata>,
) -> Option<ParsedSidecar> {
    let Some(nfo) = nfo else {
        return sidecar;
    };
    let mut result = sidecar.unwrap_or_default();
    if result.title.is_none() {
        result.title = nfo.title.clone();
    }
    if result.series_title.is_none() {
        result.series_title = nfo.series_title.clone();
    }
    if result.year.is_none() {
        result.year = nfo.year;
    }
    if result.season_number.is_none() {
        result.season_number = nfo.season_number;
    }
    if result.episode_start.is_none() {
        result.episode_start = nfo.episode_start;
    }
    Some(result)
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

fn classification_from_sidecar(sidecar: &ParsedSidecar) -> Option<FileClassification> {
    sidecar.media_type.map(|media_type| FileClassification {
        media_type,
        confidence: 0.99,
        source: ClassificationSource::User,
    })
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

fn classify_from_inbox_folder(path: &str) -> Option<FileClassification> {
    let make = |media_type: MediaType| FileClassification {
        media_type,
        confidence: 0.96,
        source: ClassificationSource::Folder,
    };
    // These paths reflect the pre-created INBOX_SUBFOLDERS — the user intentionally
    // placed the file there, so treat the folder as a strong signal.
    if path.starts_with("inbox/anime/tv/") || path.starts_with("inbox/anime/serien/") {
        return Some(make(MediaType::Anime));
    }
    if path.starts_with("inbox/anime/") {
        return Some(make(MediaType::Anime));
    }
    if path.starts_with("inbox/serien/") {
        return Some(make(MediaType::Series));
    }
    if path.starts_with("inbox/filme/") {
        return Some(make(MediaType::Film));
    }
    if path.starts_with("inbox/musik/") {
        return Some(make(MediaType::MusicAlbum));
    }
    if path.starts_with("inbox/bücher/") {
        return Some(make(MediaType::Book));
    }
    if path.starts_with("inbox/hörbücher/") {
        return Some(make(MediaType::Audiobook));
    }
    if path.starts_with("inbox/manga/") {
        return Some(make(MediaType::Manga));
    }
    if path.starts_with("inbox/comics/") {
        return Some(make(MediaType::Comic));
    }
    if path.starts_with("inbox/ttrpg/") {
        return Some(make(MediaType::RPG));
    }
    if path.starts_with("inbox/games/") {
        return Some(make(MediaType::VideoGame));
    }
    if path.starts_with("inbox/unsortiert/") {
        return Some(FileClassification {
            media_type: MediaType::Unclassified,
            confidence: 0.50,
            source: ClassificationSource::Folder,
        });
    }
    None
}

/// Image extensions that should never be classified as Audiobook even when
/// placed inside `Inbox/Hörbücher/`.  These are cover-art files that happen
/// to live next to the audio tracks.
const IMAGE_EXTS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tiff", "avif"];

fn detect_classification(relative_path: &RelativePath) -> Option<FileClassification> {
    let path = relative_path.to_string().to_lowercase();

    let extension = relative_path
        .extension()
        .map(|ext| ext.to_string_lossy().to_lowercase());

    // Inbox subfolder takes highest priority — the user explicitly sorted it there.
    // Exception: image files in audiobook folders are artwork, not audio items.
    if let Some(cls) = classify_from_inbox_folder(&path) {
        let is_image = IMAGE_EXTS.contains(&extension.as_deref().unwrap_or(""));
        if is_image && cls.media_type == MediaType::Audiobook {
            return None; // leave unclassified; will stay in Inbox
        }
        return Some(cls);
    }

    match extension.as_deref() {
        Some("mkv" | "mp4" | "avi" | "mov" | "webm") => {
            if path.contains("anime") {
                Some(FileClassification {
                    media_type: MediaType::Anime,
                    confidence: 0.94,
                    source: ClassificationSource::Folder,
                })
            } else if has_season_episode_marker(&path) {
                Some(FileClassification {
                    media_type: MediaType::Series,
                    confidence: 0.91,
                    source: ClassificationSource::Filename,
                })
            } else if path.contains("series") {
                Some(FileClassification {
                    media_type: MediaType::Series,
                    confidence: 0.88,
                    source: ClassificationSource::Folder,
                })
            } else {
                Some(FileClassification {
                    media_type: MediaType::Film,
                    confidence: 0.68,
                    source: ClassificationSource::Extension,
                })
            }
        }
        Some("flac" | "mp3" | "m4a" | "aac" | "ogg" | "wav") => Some(FileClassification {
            media_type: MediaType::MusicTrack,
            confidence: 0.87,
            source: ClassificationSource::Extension,
        }),
        Some("pdf" | "epub" | "mobi" | "azw3") => Some(FileClassification {
            media_type: MediaType::Ebook,
            confidence: 0.84,
            source: ClassificationSource::Extension,
        }),
        Some("cbz" | "cbr") => Some(FileClassification {
            media_type: MediaType::Manga,
            confidence: 0.82,
            source: ClassificationSource::Extension,
        }),
        Some("zip" | "rar" | "7z" | "tar" | "gz") => Some(FileClassification {
            media_type: MediaType::Archive,
            confidence: 0.78,
            source: ClassificationSource::Extension,
        }),
        Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "bmp" | "tif" | "tiff") => {
            if contains_any(&path, &PHOTO_HINTS) {
                Some(FileClassification {
                    media_type: MediaType::Photo,
                    confidence: 0.92,
                    source: if contains_any(&path, &CAMERA_HINTS) {
                        ClassificationSource::Folder
                    } else {
                        ClassificationSource::Filename
                    },
                })
            } else if contains_any(&path, &IMAGE_HINTS) {
                Some(FileClassification {
                    media_type: MediaType::Image,
                    confidence: 0.91,
                    source: ClassificationSource::Filename,
                })
            } else {
                Some(FileClassification {
                    media_type: MediaType::Image,
                    confidence: 0.81,
                    source: ClassificationSource::Extension,
                })
            }
        }
        _ => None,
    }
}

const PHOTO_HINTS: [&str; 8] = [
    "dcim", "camera", "photo", "photos", "picture", "pictures", "img_", "dsc",
];

const CAMERA_HINTS: [&str; 4] = ["dcim", "camera", "dsc", "img_"];

const IMAGE_HINTS: [&str; 7] = [
    "screenshot",
    "screen shot",
    "wallpaper",
    "scan",
    "diagram",
    "logo",
    "icon",
];

fn contains_any(value: &str, hints: &[&str]) -> bool {
    hints.iter().any(|hint| value.contains(hint))
}

fn is_in_inbox(relative_path: &RelativePath) -> bool {
    relative_path
        .as_path()
        .components()
        .next()
        .map(|component| component.as_os_str() == "Inbox")
        .unwrap_or(false)
}

fn has_season_episode_marker(value: &str) -> bool {
    let lower = value.to_lowercase();
    let bytes = lower.as_bytes();

    bytes.windows(6).any(|window| {
        window[0] == b's'
            && window[1].is_ascii_digit()
            && window[2].is_ascii_digit()
            && window[3] == b'e'
            && window[4].is_ascii_digit()
            && window[5].is_ascii_digit()
    })
}

// ---------------------------------------------------------------------------
// Progress API
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SaveProgressRequest {
    vault_root: Option<String>,
    vault_path: String,
    progress: MediaProgress,
    #[serde(default)]
    completed: bool,
}

#[derive(Serialize)]
struct SaveProgressResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SaveProgressResponse {
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

#[derive(Serialize)]
struct LoadProgressResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    record: Option<ProgressRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize)]
struct DeleteProgressRequest {
    vault_root: Option<String>,
    vault_path: String,
}

#[derive(Serialize)]
struct DeleteProgressResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl DeleteProgressResponse {
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

#[derive(Serialize)]
struct ListProgressResponse {
    records: Vec<ProgressRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_save_progress_response(body: &[u8]) -> SaveProgressResponse {
    let req: SaveProgressRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return SaveProgressResponse::error(format!("Invalid request: {e}")),
    };

    let vault_root = match resolve_vault_root(req.vault_root.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => return SaveProgressResponse::error("Kein Vault geöffnet."),
        Err(e) => return SaveProgressResponse::error(e.to_string()),
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => return SaveProgressResponse::error(e.to_string()),
    };

    match save_progress(
        &vault.progress_dir(),
        &req.vault_path,
        req.progress,
        req.completed,
    ) {
        Ok(()) => SaveProgressResponse::ok(),
        Err(e) => SaveProgressResponse::error(e.to_string()),
    }
}

fn build_load_progress_response(query: Option<&str>) -> LoadProgressResponse {
    let query = match query {
        Some(q) => q,
        None => {
            return LoadProgressResponse {
                record: None,
                error: Some("missing query".into()),
            }
        }
    };

    let vault_path = match extract_query_value(query, "path") {
        Some(p) => p,
        None => {
            return LoadProgressResponse {
                record: None,
                error: Some("missing path".into()),
            }
        }
    };

    let root_override = extract_query_value(query, "root");
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return LoadProgressResponse {
                record: None,
                error: Some("Kein Vault geöffnet.".into()),
            }
        }
        Err(e) => {
            return LoadProgressResponse {
                record: None,
                error: Some(e.to_string()),
            }
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => {
            return LoadProgressResponse {
                record: None,
                error: Some(e.to_string()),
            }
        }
    };

    match load_progress(&vault.progress_dir(), &vault_path) {
        Ok(record) => LoadProgressResponse {
            record,
            error: None,
        },
        Err(e) => LoadProgressResponse {
            record: None,
            error: Some(e.to_string()),
        },
    }
}

fn build_delete_progress_response(body: &[u8]) -> DeleteProgressResponse {
    let req: DeleteProgressRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return DeleteProgressResponse::error(format!("Invalid request: {e}")),
    };

    let vault_root = match resolve_vault_root(req.vault_root.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => return DeleteProgressResponse::error("Kein Vault geöffnet."),
        Err(e) => return DeleteProgressResponse::error(e.to_string()),
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => return DeleteProgressResponse::error(e.to_string()),
    };

    match delete_progress(&vault.progress_dir(), &req.vault_path) {
        Ok(()) => DeleteProgressResponse::ok(),
        Err(e) => DeleteProgressResponse::error(e.to_string()),
    }
}

fn build_list_progress_response(query: Option<&str>) -> ListProgressResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return ListProgressResponse {
                records: vec![],
                error: Some("Kein Vault geöffnet.".into()),
            }
        }
        Err(e) => {
            return ListProgressResponse {
                records: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => {
            return ListProgressResponse {
                records: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    match list_in_progress(&vault.progress_dir()) {
        Ok(records) => ListProgressResponse {
            records,
            error: None,
        },
        Err(e) => ListProgressResponse {
            records: vec![],
            error: Some(e.to_string()),
        },
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

fn build_open_vault_root_response(query: Option<&str>) -> OpenExternalResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => return OpenExternalResponse::error("Kein Vault geöffnet."),
        Err(e) => return OpenExternalResponse::error(e.to_string()),
    };

    let program = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };

    match std::process::Command::new(program)
        .arg(vault_root.as_path())
        .spawn()
    {
        Ok(_) => OpenExternalResponse::ok(),
        Err(e) => OpenExternalResponse::error(format!("open failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// Dashboard APIs
// ---------------------------------------------------------------------------

/// A lightweight item descriptor for dashboard cards.
#[derive(Serialize)]
struct DashboardItem {
    vault_path: String,
    title: String,
    media_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    year: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cover_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress_fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    position_seconds: Option<f64>,
    /// File modification time as UNIX seconds.
    modified_at: u64,
}

#[derive(Serialize)]
struct RecentItemsResponse {
    items: Vec<DashboardItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct InProgressResponse {
    items: Vec<DashboardItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

const DASHBOARD_LIMIT: usize = 24;

/// Standard filenames to look for when searching for a cover image.
const COVER_FILENAMES: &[&str] = &[
    "cover.jpg",
    "cover.jpeg",
    "cover.png",
    "cover.webp",
    "folder.jpg",
    "folder.jpeg",
    "folder.png",
    "poster.jpg",
    "poster.jpeg",
    "poster.png",
];

/// Searches the media file's directory (and its parent) for a cover image.
///
/// Returns a `/api/media-file?path=...&root=...` URL suitable for the frontend
/// to use directly as an `<img src>`.
fn find_cover_url(vault: &Vault, relative_path: &str) -> Option<String> {
    use std::path::Path;

    let file_path = Path::new(relative_path);
    let dir = file_path.parent()?;
    let vault_root = vault.root().to_string_lossy();
    let vault_root_enc = urlencoding::encode(&vault_root);

    let parent_dir = dir.parent();
    let mut dirs_to_check: Vec<&Path> = vec![dir];
    if let Some(p) = parent_dir {
        // Only check parent if it is not the vault root itself (empty string = root)
        if p != Path::new("") {
            dirs_to_check.push(p);
        }
    }

    for check_dir in dirs_to_check {
        for name in COVER_FILENAMES {
            let candidate_rel = check_dir.join(name);
            let candidate_abs = vault.root().join(&candidate_rel);
            if candidate_abs.exists() {
                let rel_str = candidate_rel.to_string_lossy().replace('\\', "/");
                return Some(format!(
                    "/api/media-file?path={}&root={}",
                    urlencoding::encode(&rel_str),
                    vault_root_enc,
                ));
            }
        }
    }

    None
}

fn build_recent_items_response(query: Option<&str>) -> RecentItemsResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return RecentItemsResponse {
                items: vec![],
                error: Some("Kein Vault geöffnet.".into()),
            }
        }
        Err(e) => {
            return RecentItemsResponse {
                items: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let vault = match Vault::new(&vault_root) {
        Ok(v) => v,
        Err(e) => {
            return RecentItemsResponse {
                items: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let scanned = match scan_vault_files(&vault) {
        Ok(files) => files,
        Err(e) => {
            return RecentItemsResponse {
                items: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    // Sort by file system modification time, newest first.
    let mut with_mtime: Vec<(u64, ScannedVaultFile)> = scanned
        .into_iter()
        .filter_map(|file| {
            let abs = vault.resolve(file.incoming.source_path.as_path()).ok()?;
            let mtime = fs::metadata(&abs)
                .ok()?
                .modified()
                .ok()?
                .duration_since(std::time::UNIX_EPOCH)
                .ok()?
                .as_secs();
            Some((mtime, file))
        })
        .collect();

    with_mtime.sort_by_key(|entry| std::cmp::Reverse(entry.0));

    let items = with_mtime
        .into_iter()
        .take(DASHBOARD_LIMIT)
        .map(|(mtime, file)| {
            let cls = file.incoming.classification.as_ref();
            let media_type = cls
                .map(|c| c.media_type.to_string())
                .unwrap_or_else(|| "unclassified".to_string());
            let title = file
                .sidecar
                .as_ref()
                .and_then(|s| s.title.clone().or_else(|| s.series_title.clone()))
                .or_else(|| {
                    file.incoming
                        .source_path
                        .file_stem()
                        .map(|s| s.to_string_lossy().to_string())
                })
                .unwrap_or_else(|| file.incoming.source_path.to_string());
            let year = file
                .incoming
                .metadata
                .as_ref()
                .and_then(|m| m.year)
                .or_else(|| file.sidecar.as_ref().and_then(|s| s.year));
            let cover_url = find_cover_url(&vault, &file.incoming.source_path.to_string());
            DashboardItem {
                vault_path: file.incoming.source_path.to_string(),
                title,
                media_type,
                year,
                cover_url,
                progress_fraction: None,
                position_seconds: None,
                modified_at: mtime,
            }
        })
        .collect();

    RecentItemsResponse { items, error: None }
}

fn build_in_progress_response(query: Option<&str>) -> InProgressResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return InProgressResponse {
                items: vec![],
                error: Some("Kein Vault geöffnet.".into()),
            }
        }
        Err(e) => {
            return InProgressResponse {
                items: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => {
            return InProgressResponse {
                items: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let records = match list_in_progress(&vault.progress_dir()) {
        Ok(r) => r,
        Err(e) => {
            return InProgressResponse {
                items: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let items = records
        .into_iter()
        .take(DASHBOARD_LIMIT)
        .map(|record| {
            let title = record
                .vault_path
                .rsplit('/')
                .next()
                .and_then(|name| name.rsplit('.').nth(1).map(|_| name.to_string()))
                .unwrap_or_else(|| record.vault_path.clone());
            let position_seconds = match &record.progress {
                MediaProgress::Video {
                    position_seconds, ..
                }
                | MediaProgress::Audio {
                    position_seconds, ..
                } => Some(*position_seconds),
                MediaProgress::Audiobook {
                    position_seconds, ..
                } => Some(*position_seconds),
                _ => None,
            };
            let modified_at = record.last_accessed;
            let cover_url = find_cover_url(&vault, &record.vault_path);
            DashboardItem {
                vault_path: record.vault_path.clone(),
                title,
                media_type: "unknown".to_string(),
                year: None,
                cover_url,
                progress_fraction: record.fraction(),
                position_seconds,
                modified_at,
            }
        })
        .collect();

    InProgressResponse { items, error: None }
}

// ---------------------------------------------------------------------------
// Subtitle discovery
// ---------------------------------------------------------------------------

/// Subtitle file extensions the in-app player can parse.
const SUBTITLE_EXTS: &[&str] = &["srt", "vtt", "ass", "ssa"];

#[derive(Debug, Clone, Serialize)]
struct SubtitleTrack {
    /// Vault-relative path to the subtitle file.
    path: String,
    /// Human-readable label (filename, with any language hint).
    label: String,
}

#[derive(Debug, Serialize)]
struct ListSubtitlesResponse {
    subtitles: Vec<SubtitleTrack>,
    error: Option<String>,
}

/// Lists subtitle files near a video so the player can offer track selection.
///
/// Searches the video's own directory and common `Subs` / `Subtitles`
/// subfolders. Files whose stem starts with the video stem are listed first so
/// the best match is auto-selected by the frontend.
fn build_list_subtitles_response(query: Option<&str>) -> ListSubtitlesResponse {
    let video_path = match query.and_then(|q| extract_query_value(q, "path")) {
        Some(p) => p,
        None => {
            return ListSubtitlesResponse {
                subtitles: vec![],
                error: Some("Kein Pfad angegeben.".into()),
            }
        }
    };
    let root_override = query.and_then(|q| extract_query_value(q, "root"));

    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return ListSubtitlesResponse {
                subtitles: vec![],
                error: Some("Kein Vault geöffnet.".into()),
            }
        }
        Err(e) => {
            return ListSubtitlesResponse {
                subtitles: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => {
            return ListSubtitlesResponse {
                subtitles: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let relative = match RelativePath::new(&video_path) {
        Ok(r) => r,
        Err(e) => {
            return ListSubtitlesResponse {
                subtitles: vec![],
                error: Some(e.to_string()),
            }
        }
    };
    let absolute = match vault.resolve(relative.as_path()) {
        Ok(a) => a,
        Err(e) => {
            return ListSubtitlesResponse {
                subtitles: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let video_dir = match absolute.parent() {
        Some(d) => d.to_path_buf(),
        None => {
            return ListSubtitlesResponse {
                subtitles: vec![],
                error: None,
            }
        }
    };
    let video_stem = absolute
        .file_stem()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    // Directories to scan: the video's folder plus optional subtitle subfolders.
    let mut dirs = vec![video_dir.clone()];
    for sub in ["Subs", "Subtitles", "subs", "subtitles"] {
        let candidate = video_dir.join(sub);
        if candidate.is_dir() {
            dirs.push(candidate);
        }
    }

    let mut matching = Vec::new();
    let mut others = Vec::new();
    for dir in dirs {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            let ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default();
            if !SUBTITLE_EXTS.contains(&ext.as_str()) {
                continue;
            }
            let Ok(rel) = vault.relative_from_absolute(&path) else {
                continue;
            };
            let file_stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let track = SubtitleTrack {
                path: rel.to_string(),
                label: file_stem.clone(),
            };
            // Files whose stem starts with the video stem are the best match.
            if !video_stem.is_empty() && file_stem.to_lowercase().starts_with(&video_stem) {
                matching.push(track);
            } else {
                others.push(track);
            }
        }
    }

    matching.sort_by(|a, b| a.label.cmp(&b.label));
    others.sort_by(|a, b| a.label.cmp(&b.label));
    matching.extend(others);

    ListSubtitlesResponse {
        subtitles: matching,
        error: None,
    }
}

// ---------------------------------------------------------------------------
// Playlist APIs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct ListPlaylistsResponse {
    playlists: Vec<Playlist>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct GetPlaylistResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    playlist: Option<Playlist>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize)]
struct SavePlaylistRequest {
    vault_root: Option<String>,
    playlist: Playlist,
}

#[derive(Serialize)]
struct SavePlaylistResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SavePlaylistResponse {
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
struct DeletePlaylistRequest {
    vault_root: Option<String>,
    id: String,
}

#[derive(Serialize)]
struct DeletePlaylistResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl DeletePlaylistResponse {
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
struct SaveCursorRequest {
    vault_root: Option<String>,
    cursor: PlaylistCursor,
}

#[derive(Serialize)]
struct SaveCursorResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl SaveCursorResponse {
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

#[derive(Serialize)]
struct LoadCursorResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<PlaylistCursor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

fn build_list_playlists_response(query: Option<&str>) -> ListPlaylistsResponse {
    let root_override = query.and_then(|q| extract_query_value(q, "root"));
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return ListPlaylistsResponse {
                playlists: vec![],
                error: Some("Kein Vault geöffnet.".into()),
            }
        }
        Err(e) => {
            return ListPlaylistsResponse {
                playlists: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => {
            return ListPlaylistsResponse {
                playlists: vec![],
                error: Some(e.to_string()),
            }
        }
    };

    match list_playlists(&vault.system_dir()) {
        Ok(playlists) => ListPlaylistsResponse {
            playlists,
            error: None,
        },
        Err(e) => ListPlaylistsResponse {
            playlists: vec![],
            error: Some(e.to_string()),
        },
    }
}

fn build_get_playlist_response(query: Option<&str>) -> GetPlaylistResponse {
    let query = match query {
        Some(q) => q,
        None => {
            return GetPlaylistResponse {
                playlist: None,
                error: Some("missing query".into()),
            }
        }
    };

    let id = match extract_query_value(query, "id") {
        Some(id) => id,
        None => {
            return GetPlaylistResponse {
                playlist: None,
                error: Some("missing id".into()),
            }
        }
    };

    let root_override = extract_query_value(query, "root");
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return GetPlaylistResponse {
                playlist: None,
                error: Some("Kein Vault geöffnet.".into()),
            }
        }
        Err(e) => {
            return GetPlaylistResponse {
                playlist: None,
                error: Some(e.to_string()),
            }
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => {
            return GetPlaylistResponse {
                playlist: None,
                error: Some(e.to_string()),
            }
        }
    };

    match load_playlist(&vault.system_dir(), &id) {
        Ok(playlist) => GetPlaylistResponse {
            playlist,
            error: None,
        },
        Err(e) => GetPlaylistResponse {
            playlist: None,
            error: Some(e.to_string()),
        },
    }
}

fn build_save_playlist_response(body: &[u8]) -> SavePlaylistResponse {
    let req: SavePlaylistRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return SavePlaylistResponse::error(format!("Invalid request: {e}")),
    };

    let vault_root = match resolve_vault_root(req.vault_root.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => return SavePlaylistResponse::error("Kein Vault geöffnet."),
        Err(e) => return SavePlaylistResponse::error(e.to_string()),
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => return SavePlaylistResponse::error(e.to_string()),
    };

    let mut playlist = req.playlist;
    match save_playlist(&vault.system_dir(), &mut playlist) {
        Ok(()) => SavePlaylistResponse::ok(),
        Err(e) => SavePlaylistResponse::error(e.to_string()),
    }
}

fn build_delete_playlist_response(body: &[u8]) -> DeletePlaylistResponse {
    let req: DeletePlaylistRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return DeletePlaylistResponse::error(format!("Invalid request: {e}")),
    };

    let vault_root = match resolve_vault_root(req.vault_root.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => return DeletePlaylistResponse::error("Kein Vault geöffnet."),
        Err(e) => return DeletePlaylistResponse::error(e.to_string()),
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => return DeletePlaylistResponse::error(e.to_string()),
    };

    match delete_playlist(&vault.system_dir(), &req.id) {
        Ok(()) => DeletePlaylistResponse::ok(),
        Err(e) => DeletePlaylistResponse::error(e.to_string()),
    }
}

fn build_save_cursor_response(body: &[u8]) -> SaveCursorResponse {
    let req: SaveCursorRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return SaveCursorResponse::error(format!("Invalid request: {e}")),
    };

    let vault_root = match resolve_vault_root(req.vault_root.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => return SaveCursorResponse::error("Kein Vault geöffnet."),
        Err(e) => return SaveCursorResponse::error(e.to_string()),
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => return SaveCursorResponse::error(e.to_string()),
    };

    let mut cursor = req.cursor;
    match save_cursor(&vault.progress_dir(), &mut cursor) {
        Ok(()) => SaveCursorResponse::ok(),
        Err(e) => SaveCursorResponse::error(e.to_string()),
    }
}

fn build_load_cursor_response(query: Option<&str>) -> LoadCursorResponse {
    let query = match query {
        Some(q) => q,
        None => {
            return LoadCursorResponse {
                cursor: None,
                error: Some("missing query".into()),
            }
        }
    };

    let id = match extract_query_value(query, "id") {
        Some(id) => id,
        None => {
            return LoadCursorResponse {
                cursor: None,
                error: Some("missing id".into()),
            }
        }
    };

    let root_override = extract_query_value(query, "root");
    let vault_root = match resolve_vault_root(root_override.as_deref()) {
        Ok(Some(r)) => r,
        Ok(None) => {
            return LoadCursorResponse {
                cursor: None,
                error: Some("Kein Vault geöffnet.".into()),
            }
        }
        Err(e) => {
            return LoadCursorResponse {
                cursor: None,
                error: Some(e.to_string()),
            }
        }
    };

    let vault = match Vault::new(vault_root) {
        Ok(v) => v,
        Err(e) => {
            return LoadCursorResponse {
                cursor: None,
                error: Some(e.to_string()),
            }
        }
    };

    match load_cursor(&vault.progress_dir(), &id) {
        Ok(cursor) => LoadCursorResponse {
            cursor,
            error: None,
        },
        Err(e) => LoadCursorResponse {
            cursor: None,
            error: Some(e.to_string()),
        },
    }
}

// ---------------------------------------------------------------------------
// Audiobookshelf (ABS) sync APIs
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct AbsTestResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl AbsTestResponse {
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

#[derive(Serialize)]
struct AbsLibrariesResponse {
    libraries: Vec<crate::api::audiobookshelf::AbsLibrary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct AbsLibraryItemsResponse {
    items: Vec<crate::api::audiobookshelf::AbsLibraryItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Deserialize)]
struct AbsSyncProgressRequest {
    abs_url: String,
    api_key: String,
    item_id: String,
    current_time: f64,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    is_finished: bool,
}

#[derive(Serialize)]
struct AbsSyncProgressResponse {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl AbsSyncProgressResponse {
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

/// Credentials and target for an Audiobookshelf call.
///
/// Sent in the request **body**, never as query parameters: the API key would
/// otherwise sit in a URL and end up in logs and crash reports.
#[derive(Deserialize)]
struct AbsRequest {
    /// Server root URL, e.g. `http://localhost:13378`.
    url: String,
    /// ABS API key.
    #[serde(default)]
    key: String,
    /// Library id — only used by `/api/abs/library-items`.
    #[serde(default)]
    library: String,
}

fn build_abs_test_response(body: &[u8]) -> AbsTestResponse {
    let req: AbsRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => return AbsTestResponse::error(format!("Invalid request: {error}")),
    };
    match AbsClient::new(req.url, req.key) {
        Ok(client) => match client.test_connection() {
            Ok(()) => AbsTestResponse::ok(),
            Err(e) => AbsTestResponse::error(e.to_string()),
        },
        Err(e) => AbsTestResponse::error(e.to_string()),
    }
}

fn build_abs_libraries_response(body: &[u8]) -> AbsLibrariesResponse {
    let req: AbsRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return AbsLibrariesResponse {
                libraries: vec![],
                error: Some(format!("Invalid request: {error}")),
            }
        }
    };
    match AbsClient::new(req.url, req.key) {
        Ok(client) => match client.list_libraries() {
            Ok(libraries) => AbsLibrariesResponse {
                libraries,
                error: None,
            },
            Err(e) => AbsLibrariesResponse {
                libraries: vec![],
                error: Some(e.to_string()),
            },
        },
        Err(e) => AbsLibrariesResponse {
            libraries: vec![],
            error: Some(e.to_string()),
        },
    }
}

fn build_abs_library_items_response(body: &[u8]) -> AbsLibraryItemsResponse {
    let req: AbsRequest = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(error) => {
            return AbsLibraryItemsResponse {
                items: vec![],
                error: Some(format!("Invalid request: {error}")),
            }
        }
    };
    if req.library.trim().is_empty() {
        return AbsLibraryItemsResponse {
            items: vec![],
            error: Some("missing library".into()),
        };
    }
    let library_id = req.library.clone();
    match AbsClient::new(req.url, req.key) {
        Ok(client) => match client.list_library_items(&library_id) {
            Ok(items) => AbsLibraryItemsResponse { items, error: None },
            Err(e) => AbsLibraryItemsResponse {
                items: vec![],
                error: Some(e.to_string()),
            },
        },
        Err(e) => AbsLibraryItemsResponse {
            items: vec![],
            error: Some(e.to_string()),
        },
    }
}

fn build_abs_sync_progress_response(body: &[u8]) -> AbsSyncProgressResponse {
    let req: AbsSyncProgressRequest = match serde_json::from_slice(body) {
        Ok(r) => r,
        Err(e) => return AbsSyncProgressResponse::error(format!("Invalid request: {e}")),
    };
    match AbsClient::new(&req.abs_url, &req.api_key) {
        Ok(client) => {
            let result = client.set_progress(
                &req.item_id,
                req.current_time,
                req.duration,
                req.is_finished,
            );
            match result {
                Ok(()) => AbsSyncProgressResponse::ok(),
                Err(e) => AbsSyncProgressResponse::error(e.to_string()),
            }
        }
        Err(e) => AbsSyncProgressResponse::error(e.to_string()),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    cover_path: Option<String>,
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
    fn from_subscription(subscription: &Subscription, vault: &Vault) -> Self {
        // Detail views want the cover; resolve the cached file (if any) to a
        // vault-relative path the frontend can feed into /api/media-file.
        let cover_path =
            load_novel_cover_path(&webnovel_folder(vault, subscription)).and_then(|absolute| {
                vault
                    .relative_from_absolute(&absolute)
                    .ok()
                    .map(|relative| relative.to_string())
            });
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
            cover_path,
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
    let vault = match resolve_webnovel_vault(root_override.as_deref()) {
        Ok(vault) => vault,
        Err(message) => {
            return WebnovelListResponse {
                subscriptions: Vec::new(),
                error: Some(message),
            }
        }
    };

    match list_subscriptions(&vault.system_dir()) {
        Ok(subscriptions) => WebnovelListResponse {
            subscriptions: subscriptions
                .iter()
                .map(|subscription| {
                    WebnovelSubscriptionSummary::from_subscription(subscription, &vault)
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
fn resolve_webnovel_vault(root_override: Option<&str>) -> std::result::Result<Vault, String> {
    let root = match resolve_vault_root(root_override) {
        Ok(Some(root)) => root,
        Ok(None) => return Err("Kein Vault geöffnet.".to_string()),
        Err(error) => return Err(error.to_string()),
    };
    Vault::new(root).map_err(|error| error.to_string())
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

    let vault = match resolve_webnovel_vault(req.root.as_deref()) {
        Ok(vault) => vault,
        Err(message) => return WebnovelSubscribeResponse::error(message),
    };

    let url = normalize_url(&req.url);
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return WebnovelSubscribeResponse::error("Bitte eine vollständige URL angeben.");
    }
    if let Some(reason) = blocked_reason(&vault.system_dir(), &url) {
        return WebnovelSubscribeResponse::error(reason);
    }

    // Re-subscribing an existing URL returns the current record unchanged.
    let id = crate::core::webnovel::subscription_id(&url);
    match load_subscription(&vault.system_dir(), &id) {
        Ok(Some(existing)) => {
            return WebnovelSubscribeResponse {
                subscription: Some(WebnovelSubscriptionSummary::from_subscription(
                    &existing, &vault,
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
    subscription.folder_name = Some(unique_novel_folder_name(&vault, &subscription));
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

    match save_subscription(&vault.system_dir(), &subscription) {
        Ok(()) => WebnovelSubscribeResponse {
            subscription: Some(WebnovelSubscriptionSummary::from_subscription(
                &subscription,
                &vault,
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
    let vault = match resolve_webnovel_vault(req.root.as_deref()) {
        Ok(vault) => vault,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };

    // Soft delete: the record moves to the in-app trash and can be restored.
    let subscription = match trash_subscription(&vault.system_dir(), &req.id) {
        Ok(Some(subscription)) => subscription,
        Ok(None) => return WebnovelSimpleResponse::error("Abo nicht gefunden."),
        Err(error) => return WebnovelSimpleResponse::error(error.to_string()),
    };

    if !req.keep_files {
        // Files move into the vault .trash folder (same convention as
        // delete-files) instead of being removed — reversible via restore.
        let novel_dir = webnovel_folder(&vault, &subscription);
        if novel_dir.exists() {
            let trash_target = webnovel_trash_folder(&vault, &subscription);
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
fn webnovel_trash_folder(vault: &Vault, subscription: &Subscription) -> PathBuf {
    vault
        .root()
        .join(TRASH_DIR)
        .join(MediaType::Webnovel.folder_segment())
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
    let vault = match resolve_webnovel_vault(root_override.as_deref()) {
        Ok(vault) => vault,
        Err(message) => {
            return WebnovelTrashResponse {
                entries: Vec::new(),
                error: Some(message),
            }
        }
    };
    match list_trashed_subscriptions(&vault.system_dir()) {
        Ok(trashed) => WebnovelTrashResponse {
            entries: trashed
                .iter()
                .map(|subscription| WebnovelTrashEntry {
                    id: subscription.id.clone(),
                    title: subscription.title.clone(),
                    trashed_at_unix: subscription.trashed_at_unix,
                    files_in_trash: webnovel_trash_folder(&vault, subscription).exists(),
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
    let vault = match resolve_webnovel_vault(req.root.as_deref()) {
        Ok(vault) => vault,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };
    let subscription = match restore_subscription(&vault.system_dir(), &req.id) {
        Ok(subscription) => subscription,
        Err(error) => return WebnovelSimpleResponse::error(error.to_string()),
    };
    // Bring trashed files back, if any.
    let trash_source = webnovel_trash_folder(&vault, &subscription);
    if trash_source.exists() {
        let target = webnovel_folder(&vault, &subscription);
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
    let vault = match resolve_webnovel_vault(req.root.as_deref()) {
        Ok(vault) => vault,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };
    // Need the record before purging it to find its trashed folder.
    let trashed = list_trashed_subscriptions(&vault.system_dir())
        .ok()
        .and_then(|entries| {
            entries
                .into_iter()
                .find(|subscription| subscription.id == req.id)
        });
    if let Err(error) = purge_trashed_subscription(&vault.system_dir(), &req.id) {
        return WebnovelSimpleResponse::error(error.to_string());
    }
    if let Some(subscription) = trashed {
        let folder = webnovel_trash_folder(&vault, &subscription);
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
    let vault = match resolve_webnovel_vault(req.root.as_deref()) {
        Ok(vault) => vault,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };

    let mut subscription = match load_subscription(&vault.system_dir(), &req.id) {
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

    match save_subscription(&vault.system_dir(), &subscription) {
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

    let vault = match resolve_webnovel_vault(req.root.as_deref()) {
        Ok(vault) => vault,
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
        let outcome = run_webnovel_check(&vault, &options, &thread_job_id);
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
    vault: &Vault,
    options: &WebnovelCheckOptions,
    job_id: &str,
) -> Result<String> {
    let system_dir = vault.system_dir();
    let all = list_subscriptions(&system_dir)?;
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
            save_subscription(&system_dir, &subscription)?;
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

        match check_one_subscription(vault, &client, &mut subscription, options, job_id) {
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
        save_subscription(&system_dir, &subscription)?;
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
    vault: &Vault,
    client: &PoliteClient,
    subscription: &mut Subscription,
    options: &WebnovelCheckOptions,
    job_id: &str,
) -> Result<usize> {
    if let Some(reason) = blocked_reason(&vault.system_dir(), &subscription.url) {
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

    let novel_dir = webnovel_folder(vault, subscription);
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
        save_subscription(&vault.system_dir(), subscription)?;
        update_webnovel_job(job_id, |status| {
            status.downloaded += 1;
        });
    }

    // Build EPUBs from whatever is cached — also after a partial run.
    if !downloaded_indices.is_empty() && options.build_batch && !subscription.completed {
        build_batch_epub(vault, subscription, &downloaded_indices)?;
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
        build_complete_epub(vault, subscription)?;
    } else if !complete_missing {
        // No rebuild needed, but Goodreads/AniList enrichment may have added
        // metadata — keep the sidecar (and thus the collections view) in sync.
        write_webnovel_sidecar(vault, subscription, &complete_file, &subscription.id, None)?;
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
fn unique_novel_folder_name(vault: &Vault, subscription: &Subscription) -> String {
    let base = novel_folder_name(subscription);
    let taken = list_subscriptions(&vault.system_dir())
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
fn webnovel_folder(vault: &Vault, subscription: &Subscription) -> PathBuf {
    vault
        .root()
        .join(MediaType::Webnovel.folder_segment())
        .join(novel_folder_name(subscription))
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
fn build_batch_epub(vault: &Vault, subscription: &Subscription, indices: &[u32]) -> Result<()> {
    let min = indices.iter().min().copied().unwrap_or(0);
    let max = indices.iter().max().copied().unwrap_or(0);
    let safe_title = novel_folder_name(subscription);
    let file_name = if min == max {
        format!("{safe_title} - Kapitel {min:04}.epub")
    } else {
        format!("{safe_title} - Kapitel {min:04}-{max:04}.epub")
    };

    let novel_dir = webnovel_folder(vault, subscription);
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

    let entry_id = format!("{}-batch-{min:04}-{max:04}", subscription.id);
    write_webnovel_sidecar(vault, subscription, &file_name, &entry_id, Some((min, max)))
}

/// Rebuilds the complete EPUB from every cached chapter.
fn build_complete_epub(vault: &Vault, subscription: &Subscription) -> Result<()> {
    let indices: Vec<u32> = subscription
        .known_chapters
        .iter()
        .filter(|chapter| chapter.downloaded_at_unix.is_some())
        .map(|chapter| chapter.index)
        .collect();
    if indices.is_empty() {
        return Ok(());
    }

    let novel_dir = webnovel_folder(vault, subscription);
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

    // The rebuild shifts EPUB CFIs, so a stored reading position would point
    // at the wrong place — drop it. Batch EPUBs keep in-progress reads.
    let relative = format!(
        "{}/{}/{}",
        MediaType::Webnovel.folder_segment(),
        safe_title,
        file_name
    );
    delete_progress(&vault.progress_dir(), &relative).ok();

    write_webnovel_sidecar(vault, subscription, &file_name, &subscription.id, None)
}

/// Writes the `.fero.yaml` sidecar next to a generated EPUB.
/// Unions the tags already present in a novel's sidecar with the incoming
/// ones (subscription-derived), preserving user-added tags across re-checks.
/// Case-insensitive de-duplication; incoming tags win the casing.
fn merge_webnovel_tags(vault: &Vault, relative: &RelativePath, incoming: &[String]) -> Vec<String> {
    let mut result: Vec<String> = incoming.to_vec();
    let mut seen: HashSet<String> = result.iter().map(|tag| tag.to_lowercase()).collect();

    if let Ok(Some(sidecar_path)) = find_sidecar_file(vault, relative) {
        if let Ok(raw) = fs::read_to_string(&sidecar_path) {
            if let Ok(existing) = parse_sidecar_metadata(&raw) {
                for tag in existing.tags {
                    if seen.insert(tag.to_lowercase()) {
                        result.push(tag);
                    }
                }
            }
        }
    }
    result
}

fn write_webnovel_sidecar(
    vault: &Vault,
    subscription: &Subscription,
    file_name: &str,
    entry_id: &str,
    chapter_range: Option<(u32, u32)>,
) -> Result<()> {
    let safe_title = novel_folder_name(subscription);
    let relative = RelativePath::new(
        PathBuf::from(MediaType::Webnovel.folder_segment())
            .join(&safe_title)
            .join(file_name),
    )?;

    let mut entry = MediaEntry::new(entry_id, MediaType::Webnovel, relative.clone(), file_name);
    entry.source = PropertySource::Api;
    entry.created_at_unix = unix_now();
    entry.updated_at_unix = entry.created_at_unix;
    entry.properties.title = Some(subscription.title.clone());
    entry.properties.author = subscription.author.clone();
    entry.properties.description = subscription.description.clone();
    entry.properties.series_title = Some(subscription.title.clone());
    entry.properties.genres = subscription.genres.clone();
    // Preserve tags a user added by hand in the inspector: union the existing
    // sidecar tags with the subscription's, so a re-check never wipes them.
    entry.properties.tags = merge_webnovel_tags(vault, &relative, &subscription.tags);
    entry.properties.anilist_id = subscription.anilist_id;
    entry.properties.anilist_url = subscription.anilist_url.clone();
    entry.properties.rating_external = subscription.rating_external;
    // Keep the subscription and Goodreads URLs discoverable from the sidecar.
    entry.properties.notes = Some(match subscription.goodreads_url.as_deref() {
        Some(goodreads) => format!("Quelle: {} · Goodreads: {goodreads}", subscription.url),
        None => format!("Quelle: {}", subscription.url),
    });
    entry.properties.status = Some(if subscription.completed {
        MediaStatus::Completed
    } else if subscription.hiatus {
        // OnHold doubles as "hiatus": abandoned upstream, never finished.
        MediaStatus::OnHold
    } else {
        MediaStatus::InLibrary
    });
    if let Some((start, end)) = chapter_range {
        entry.properties.episode_start = Some(start.min(u32::from(u16::MAX)) as u16);
        entry.properties.episode_end = Some(end.min(u32::from(u16::MAX)) as u16);
    }

    // Point the sidecar at the cached cover so library views can show it.
    let novel_dir = webnovel_folder(vault, subscription);
    if let Some(cover_path) = load_novel_cover_path(&novel_dir) {
        if let Some(cover_name) = cover_path.file_name().and_then(|name| name.to_str()) {
            entry.properties.cover_path = RelativePath::new(
                PathBuf::from(MediaType::Webnovel.folder_segment())
                    .join(&safe_title)
                    .join(cover_name),
            )
            .ok();
        }
    }

    let yaml = render_sidecar_yaml(&entry)?;
    // Shared writer: creates the parent directory and clears sidecars left
    // behind by older naming schemes.
    write_sidecar_preview(vault, &relative, &yaml)
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
    match resolve_webnovel_vault(root_override.as_deref()) {
        Ok(vault) => WebnovelBlocklistResponse {
            entries: load_blocklist_entries(&vault.system_dir()),
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
    let vault = match resolve_webnovel_vault(req.root.as_deref()) {
        Ok(vault) => vault,
        Err(message) => return WebnovelSimpleResponse::error(message),
    };
    match save_user_blocklist(&vault.system_dir(), &req.entries) {
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
    Some(
        PathBuf::from(home)
            .join(".fero")
            .join("webnovel_debug.log"),
    )
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
        let vault = Vault::new("/vault").expect("vault root should be valid");
        let base = vault.root().join(MediaType::Webnovel.folder_segment());

        for title in ["..", ".", "", "///", "../../etc", ".hidden"] {
            let subscription = subscription_with_title(title);

            let folder = webnovel_folder(&vault, &subscription);
            assert_eq!(
                folder.parent(),
                Some(base.as_path()),
                "title {title:?} escaped the Webnovel directory"
            );
            assert_ne!(folder, base, "title {title:?} resolved to the parent");

            let trash = webnovel_trash_folder(&vault, &subscription);
            let trash_base = vault
                .root()
                .join(TRASH_DIR)
                .join(MediaType::Webnovel.folder_segment());
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
