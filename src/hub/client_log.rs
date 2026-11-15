//! Debug builds only: `POST /debug/log` writes lines sent by the reader app on a
//! device into the hub's own log (target `shelfgauge::client`), for checks of
//! intermediate builds on a phone through the staging hub (`journalctl` there).
//!
//! The reading view loads `assets/hub/debuglog.js` only in a debug build
//! ([`ENABLED`]); a release build has neither the route nor the script, so production
//! never takes these lines.

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::StatusCode;
use axum::routing::post;

use super::HubState;

/// Whether this build takes the app's log lines: debug builds only.
pub(super) const ENABLED: bool = cfg!(debug_assertions);

/// The largest request body.
const MAX_BODY: usize = 64 * 1024;
/// At most this many lines of one request are kept.
const MAX_LINES: usize = 200;
/// A longer line is cut to this many characters.
const MAX_CHARS: usize = 2000;

/// The route (none in a release build).
pub(super) fn routes(state: &HubState) -> Router<HubState> {
    let router = Router::new();
    if !ENABLED {
        return router;
    }
    router
        .route(&state.path("/debug/log"), post(log))
        .layer(DefaultBodyLimit::max(MAX_BODY))
}

/// `POST /debug/log`: one log event per line of the plain text body, control
/// characters dropped.
async fn log(body: String) -> StatusCode {
    for line in body.lines().take(MAX_LINES) {
        let line: String = line
            .chars()
            .filter(|c| !c.is_control())
            .take(MAX_CHARS)
            .collect();
        if !line.is_empty() {
            tracing::info!(target: "shelfgauge::client", "{line}");
        }
    }
    StatusCode::NO_CONTENT
}
