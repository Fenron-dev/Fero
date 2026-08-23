//! Dispatch table for the custom-scheme API.
//!
//! A child module may reach into its parent's private items, so the handlers
//! stay private to `desktop` and only the table lives here.

use tauri::http::{Request, Response, StatusCode};

use super::http::response;
use super::*;

/// Routes a single custom-scheme request to its handler and returns the
/// response. Runs on a worker thread (see [`run`]); all blocking work lives
/// here rather than on the webview thread.
pub(super) fn handle_request(request: &Request<Vec<u8>>) -> Response<Vec<u8>> {
    let path = request.uri().path();

    match path {
        "/" | "/index.html" => response(StatusCode::OK, "text/html; charset=utf-8", INDEX_HTML),
        "/app.js" => response(
            StatusCode::OK,
            "application/javascript; charset=utf-8",
            APP_JS,
        ),
        "/styles.css" => response(StatusCode::OK, "text/css; charset=utf-8", STYLES_CSS),
        "/api/targets" => json_response(StatusCode::OK, &build_targets_response()),
        "/api/targets/save" => {
            json_response(StatusCode::OK, &build_save_targets_response(request.body()))
        }
        "/api/anilist-search" => json_response(
            StatusCode::OK,
            &build_anilist_search_response(request.uri().query()),
        ),
        "/api/select-folder" => json_response(StatusCode::OK, &build_select_folder_response()),
        "/api/reveal" => json_response(StatusCode::OK, &build_reveal_response(request.body())),
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
