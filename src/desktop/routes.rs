//! Dispatch table for the custom-scheme API.
//!
//! A child module may reach into its parent's private items, so the handlers
//! stay private to `desktop` and only the table lives here.

use tauri::http::{header::CONTENT_TYPE, Method, Request, Response, StatusCode};

use super::http::{json_outcome, response};
use super::*;

/// Routes that may change state or open native UI.
///
/// Keeping this explicit is security-relevant: a foreign page shown in the
/// login/Cloudflare webview must not be able to trigger an import, deletion or
/// folder picker by navigating to a `fero://` URL.
const POST_ROUTES: &[&str] = &[
    "/api/import",
    "/api/relocate",
    "/api/data-dir/save",
    "/api/targets/save",
    "/api/schedule/save",
    "/api/select-folder",
    "/api/reveal",
    "/api/open-url",
    "/api/webnovel/open-debug-log",
    "/api/webnovel/subscribe",
    "/api/webnovel/unsubscribe",
    "/api/webnovel/update",
    "/api/webnovel/check",
    "/api/webnovel/solve",
    "/api/webnovel/login",
    "/api/webnovel/logout",
    "/api/webnovel/restore",
    "/api/webnovel/purge",
    "/api/webnovel/rebuild-blocks",
    "/api/webnovel/blocklist/save",
    "/api/webnovel/stop",
    "/api/manga/subscribe",
    "/api/manga/unsubscribe",
    "/api/manga/update",
    "/api/manga/check",
    "/api/manga/stop",
    "/api/manga/restore",
    "/api/manga/purge",
];

const GET_ROUTES: &[&str] = &[
    "/",
    "/index.html",
    "/app.js",
    "/styles.css",
    "/api/cover",
    "/api/targets",
    "/api/schedule",
    "/api/platform-search",
    "/api/anilist-search",
    "/api/webnovel/debug-log",
    "/api/webnovel/list",
    "/api/webnovel/solve-status",
    "/api/webnovel/login-status",
    "/api/webnovel/trash",
    "/api/webnovel/blocklist",
    "/api/webnovel/job",
    "/api/manga/list",
    "/api/manga/job",
    "/api/manga/trash",
];

fn reject_invalid_request(request: &Request<Vec<u8>>, path: &str) -> Option<Response<Vec<u8>>> {
    let expected = if POST_ROUTES.contains(&path) {
        Method::POST
    } else if GET_ROUTES.contains(&path) {
        Method::GET
    } else {
        return None;
    };

    if request.method() != expected {
        return Some(response(
            StatusCode::METHOD_NOT_ALLOWED,
            "text/plain; charset=utf-8",
            "Method Not Allowed",
        ));
    }

    // `application/json` cannot be emitted by a cross-origin HTML form without
    // a CORS preflight. The app frontend sets it on every POST. This closes the
    // remaining CSRF path even if a foreign webview learns the custom scheme.
    if expected == Method::POST
        && !request
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("application/json"))
    {
        return Some(response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "text/plain; charset=utf-8",
            "Content-Type must be application/json",
        ));
    }

    None
}

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
    if let Some(rejection) = reject_invalid_request(request, path) {
        return rejection;
    }

    match path {
        "/" | "/index.html" => response(StatusCode::OK, "text/html; charset=utf-8", INDEX_HTML),
        "/app.js" => response(
            StatusCode::OK,
            "application/javascript; charset=utf-8",
            APP_JS,
        ),
        "/styles.css" => response(StatusCode::OK, "text/css; charset=utf-8", STYLES_CSS),
        "/api/import" => json_outcome(&build_import_response(), StatusCode::BAD_REQUEST),
        "/api/relocate" => json_outcome(
            &build_relocate_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/cover" => build_cover_response(request.uri().query()),
        "/api/data-dir/save" => json_outcome(
            &build_set_data_dir_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/targets" => {
            json_outcome(&build_targets_response(), StatusCode::INTERNAL_SERVER_ERROR)
        }
        "/api/targets/save" => json_outcome(
            &build_save_targets_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/schedule" => json_outcome(
            &build_schedule_response(),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
        "/api/schedule/save" => json_outcome(
            &build_save_schedule_response(request.body()),
            StatusCode::BAD_REQUEST,
        ),
        "/api/platform-search" => json_outcome(
            &build_platform_search_response(request.uri().query()),
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
            &build_open_url_response(request.body()),
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
        "/api/webnovel/stop" => json_outcome(
            &build_webnovel_stop_response(request.body()),
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
        "/api/manga/stop" => json_outcome(
            &manga::build_stop_response(request.body()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: Method, path: &str, content_type: Option<&str>) -> Request<Vec<u8>> {
        let mut builder = Request::builder().method(method).uri(path);
        if let Some(content_type) = content_type {
            builder = builder.header(CONTENT_TYPE, content_type);
        }
        builder.body(Vec::new()).expect("request should build")
    }

    #[test]
    fn state_changing_routes_reject_get_before_running_the_handler() {
        let response = handle_request(&request(Method::GET, "/api/import", None));
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn posts_require_json_to_block_cross_origin_html_forms() {
        let response = handle_request(&request(Method::POST, "/api/import", Some("text/plain")));
        assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[test]
    fn posts_accept_json_with_a_charset_parameter() {
        let request = request(
            Method::POST,
            "/api/import",
            Some("application/json; charset=utf-8"),
        );

        assert_ne!(
            handle_request(&request).status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[test]
    fn read_routes_reject_post() {
        let response = handle_request(&request(
            Method::POST,
            "/api/targets",
            Some("application/json"),
        ));
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn app_responses_disable_content_sniffing_and_cross_origin_embedding() {
        let response = handle_request(&request(Method::GET, "/index.html", None));
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            response.headers()["cross-origin-resource-policy"],
            "same-origin"
        );
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
    }
}
