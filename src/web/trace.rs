//! Request logging and handler panics.
//!
//! [`trace_requests`] is the outermost layer: every request, even one the `Host` check
//! rejects, runs in a `request{id, method, route}` span and ends with one event that
//! has its status and latency. `route` is the route template (`/book/{id}/read`), `-`
//! when no route matched: never the raw path and never the query string, which may
//! hold a search query. `profile` is the request's profile id, recorded by the profile
//! middleware when it is known (never the name); `remote = true` marks a request from
//! another device (see [`Reach`](super::security::Reach)), and is absent for local
//! ones. The id is a per-process counter, printed in hex; the internal
//! error page shows it so a user can find the matching log lines.
//!
//! [`catch_panics`] is the innermost layer: a panic in a handler is answered with the
//! internal error page instead of a dropped connection. The panic itself is logged by
//! the panic hook ([`crate::logging::log_panic`]), inside the request span.

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::extract::{MatchedPath, Request, State};
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;

use super::error::internal_error;

/// Requests that take longer are logged at `info` ("slow request").
pub(crate) const SLOW_REQUEST: Duration = Duration::from_secs(1);

/// The id of a request, unique within the process; `Display` prints it in hex.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestId(u64);

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:x}", self.0)
    }
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The settings of [`trace_requests`].
#[derive(Debug, Clone)]
pub(crate) struct TraceSettings {
    /// The route template of the static assets, logged at `trace` only.
    pub(crate) assets: Arc<str>,
    /// See [`SLOW_REQUEST`].
    pub(crate) slow: Duration,
}

/// Middleware: runs the request in its span and logs its end.
///
/// The end event is at `warn` for 5xx, at `info` for a slow request, at `debug`
/// otherwise (`trace` for assets). Rejected `Host`/`Origin` requests warn on their own.
pub(crate) async fn trace_requests(
    State(settings): State<TraceSettings>,
    mut req: Request,
    next: Next,
) -> Response {
    let id = RequestId(NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str);
    let asset = route == Some(&*settings.assets);
    let span = tracing::info_span!(
        "request",
        id = %id,
        method = %req.method(),
        route = %route.unwrap_or("-"),
        profile = tracing::field::Empty,
        remote = tracing::field::Empty,
    );
    req.extensions_mut().insert(id);
    let started = Instant::now();
    let response = next.run(req).instrument(span.clone()).await;
    let elapsed = started.elapsed();
    let status = response.status().as_u16();
    let latency_ms = elapsed.as_micros() as f64 / 1000.0;
    let _entered = span.enter();
    if status >= 500 {
        tracing::warn!(status, latency_ms, "request failed");
    } else if elapsed >= settings.slow {
        tracing::info!(status, latency_ms, "slow request");
    } else if asset {
        tracing::trace!(status, latency_ms, "request");
    } else {
        tracing::debug!(status, latency_ms, "request");
    }
    response
}

/// Middleware: answers a panicking handler with the internal error page (500).
pub(crate) async fn catch_panics(req: Request, next: Next) -> Response {
    match CatchUnwind(Box::pin(next.run(req))).await {
        Ok(response) => response,
        Err(_) => internal_error(),
    }
}

/// A future that returns a panic of the inner future as `Err`.
struct CatchUnwind<F>(Pin<Box<F>>);

impl<F: Future> Future for CatchUnwind<F> {
    type Output = std::thread::Result<F::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let inner = self.0.as_mut();
        match catch_unwind(AssertUnwindSafe(|| inner.poll(cx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(value)) => Poll::Ready(Ok(value)),
            Err(panic) => Poll::Ready(Err(panic)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_print_in_hex() {
        assert_eq!(RequestId(0x1a2f).to_string(), "1a2f");
    }

    #[tokio::test]
    async fn catch_unwind_returns_panics() {
        let ok = CatchUnwind(Box::pin(async { 1 })).await;
        assert_eq!(ok.ok(), Some(1));
        let panicked = CatchUnwind(Box::pin(async {
            if true {
                panic!("in a handler");
            }
        }))
        .await;
        assert!(panicked.is_err());
    }
}
