//! Response plumbing for the custom-scheme API.

use serde::Serialize;
use tauri::http::{header::CONTENT_TYPE, Response, StatusCode};

pub(super) fn response(status: StatusCode, content_type: &str, body: &str) -> Response<Vec<u8>> {
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

/// A response that can report a failure.
///
/// Every handler answers with a struct carrying an optional `error`. The trait
/// makes that readable from the routing table, so the status code is decided
/// there — where the semantics of the route are known — instead of by parsing
/// the message.
pub(crate) trait Outcome {
    /// The failure message, when the operation did not succeed.
    fn failure(&self) -> Option<&str>;
}

/// Implements [`Outcome`] for response structs with an `error: Option<String>`.
macro_rules! impl_outcome {
    ($($type:ty),+ $(,)?) => {
        $(impl $crate::desktop::Outcome for $type {
            fn failure(&self) -> Option<&str> {
                self.error.as_deref()
            }
        })+
    };
}
pub(crate) use impl_outcome;

/// Serializes `value`, answering `200` on success and `on_failure` otherwise.
///
/// The failure status belongs to the route: the same struct means "you asked
/// for something that does not exist" on one endpoint and "we could not reach
/// the source" on another.
pub(super) fn json_outcome<T: Serialize + Outcome>(
    value: &T,
    on_failure: StatusCode,
) -> Response<Vec<u8>> {
    let status = if value.failure().is_some() {
        on_failure
    } else {
        StatusCode::OK
    };
    json_response(status, value)
}
