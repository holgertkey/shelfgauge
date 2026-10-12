//! The rescan action and scan progress.
//!
//! Progress is polled (`GET /scan/status`) rather than streamed: an open event stream
//! would keep the graceful shutdown waiting.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};

use crate::web::AppState;
use crate::web::layout::ScanStatus;

/// `POST /scan` (admin): starts a rescan unless one is running.
///
/// A form post is redirected to the library page; a request that accepts JSON (the
/// page's script) gets `202 Accepted` with the scan status.
pub(crate) async fn rescan(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if state.scan().start() {
        tracing::info!("rescan requested");
    }
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    if wants_json {
        (
            StatusCode::ACCEPTED,
            no_store(),
            Json(ScanStatus::of(&state)),
        )
            .into_response()
    } else {
        Redirect::to(&state.urls().home()).into_response()
    }
}

/// `GET /scan/status`: the progress of the running or last scan.
pub(crate) async fn status(State(state): State<AppState>) -> Response {
    (no_store(), Json(ScanStatus::of(&state))).into_response()
}

fn no_store() -> [(header::HeaderName, HeaderValue); 1] {
    [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))]
}
