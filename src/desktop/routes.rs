//! Dispatch table for the custom-scheme API.
//!
//! A child module may reach into its parent's private items, so the handlers
//! stay private to `desktop` and only the table lives here.

use tauri::http::{Request, Response, StatusCode};

use super::http::{json_outcome, response};
use super::*;

/// Routes a single custom-scheme request to its handler and returns the
/// response. Runs on a worker thread (see [`run`]); all blocking work lives
/// here rather than on the webview thread.
/// Which status a route answers with when its handler reports a failure.
///
/// The code belongs here rather than in the handler: the same response struct
/// means "you asked for something that does not exist" on one endpoint and "we
/// could not reach the source" on another. `/api/webnovel/debug-log` has no
/// failure state at all — an unreadable log reads as empty.
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
        "/api/targets" => {
            json_outcome(&build_targets_response(), StatusCode::INTERNAL_SERVER_ERROR)
        }
        "/api/targets/save" => json_outcome(
            &build_save_targets_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/schedule" => json_outcome(&build_schedule_response(), StatusCode::INTERNAL_SERVER_ERROR),
        "/api/schedule/save" => json_outcome(
            &build_save_schedule_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/anilist-search" => json_outcome(
            &build_anilist_search_response(request.uri().query()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/select-folder" => json_outcome(
            &build_select_folder_response(),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/reveal" => json_outcome(
            &build_reveal_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/open-url" => json_outcome(
            &build_open_url_response(request.uri().query()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/webnovel/debug-log" => {
            json_response(StatusCode::OK, &build_webnovel_debug_log_response())
        }
        "/api/webnovel/open-debug-log" => json_outcome(
            &build_open_debug_log_response(),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/webnovel/list" => json_outcome(
            &build_webnovel_list_response(request.uri().query()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/webnovel/subscribe" => json_outcome(
            &build_webnovel_subscribe_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/webnovel/unsubscribe" => json_outcome(
            &build_webnovel_unsubscribe_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/webnovel/update" => json_outcome(
            &build_webnovel_update_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/webnovel/check" => json_outcome(
            &build_webnovel_check_response(request.body()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/webnovel/solve" => json_outcome(
            &build_webnovel_solve_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/webnovel/solve-status" => json_response(
            StatusCode::OK,
            &build_webnovel_solve_status_response(request.uri().query()),
        ),
        "/api/webnovel/login" => json_outcome(
            &build_webnovel_login_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/webnovel/login-status" => json_response(
            StatusCode::OK,
            &build_webnovel_login_status_response(request.uri().query()),
        ),
        "/api/webnovel/logout" => json_outcome(
            &build_webnovel_logout_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/webnovel/trash" => json_outcome(
            &build_webnovel_trash_response(request.uri().query()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/webnovel/restore" => json_outcome(
            &build_webnovel_restore_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/webnovel/purge" => json_outcome(
            &build_webnovel_purge_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/webnovel/rebuild-blocks" => json_outcome(
            &webnovel::build_rebuild_blocks_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/webnovel/blocklist" => json_outcome(
            &build_webnovel_blocklist_response(request.uri().query()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/webnovel/blocklist/save" => json_outcome(
            &build_webnovel_blocklist_save_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/webnovel/job" => json_outcome(
            &build_webnovel_job_response(request.uri().query()),
            StatusCode::NOT_FOUND,
        ),
        // Manga subscriptions mirror the webnovel endpoints one for one; the
        // handlers live in `manga` to keep this file from growing.
        "/api/manga/list" => json_outcome(
            &manga::build_list_response(request.uri().query()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/manga/subscribe" => json_outcome(
            &manga::build_subscribe_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/manga/unsubscribe" => json_outcome(
            &manga::build_unsubscribe_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/manga/update" => json_outcome(
            &manga::build_update_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/manga/check" => json_outcome(
            &manga::build_check_response(request.body()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/manga/job" => json_outcome(
            &manga::build_job_response(request.uri().query()),
            StatusCode::NOT_FOUND,
        ),
        "/api/manga/trash" => json_outcome(
            &manga::build_trash_response(request.uri().query()),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/manga/restore" => json_outcome(
            &manga::build_restore_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        "/api/manga/purge" => json_outcome(
            &manga::build_purge_response(request.body()),
            StatusCode::NOT_FOUND,
        ),
        _ => response(
            StatusCode::NOT_FOUND,
            "text/plain; charset=utf-8",
            "Not Found",
        ),
    }
}
