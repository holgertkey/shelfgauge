//! HTML pages: the library page, the rescan action and the error pages.
//!
//! Handlers return [`AppError`] for failures. Its response only carries the status and
//! an [`ErrorPage`] marker; the [`render_errors`] middleware, which has the state (and so
//! the URL helper), turns it into the HTML error page.

use askama::Template;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};

use crate::scanner::{ProgressSnapshot, ScanPhase};
use crate::storage::StorageError;
use crate::storage::books::count_books;

use super::{AppState, Urls};

/// A failed request; rendered as an error page by [`render_errors`].
#[derive(Debug)]
pub(crate) enum AppError {
    /// No such page or object (404).
    NotFound,
    /// The book is indexed but its file is gone: the library needs a rescan (404).
    #[allow(dead_code, reason = "used by the book routes from Phase 9 on")]
    FileMissing,
    /// An unexpected failure (500); the message is logged, not shown.
    Internal(String),
}

/// Which error page a response should get; set by [`AppError::into_response`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ErrorPage {
    NotFound,
    FileMissing,
    Internal,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, page) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, ErrorPage::NotFound),
            Self::FileMissing => (StatusCode::NOT_FOUND, ErrorPage::FileMissing),
            Self::Internal(message) => {
                tracing::error!("{message}");
                (StatusCode::INTERNAL_SERVER_ERROR, ErrorPage::Internal)
            }
        };
        let mut response = status.into_response();
        response.extensions_mut().insert(page);
        response
    }
}

impl From<StorageError> for AppError {
    fn from(e: StorageError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<r2d2::Error> for AppError {
    fn from(e: r2d2::Error) -> Self {
        StorageError::from(e).into()
    }
}

impl From<askama::Error> for AppError {
    fn from(e: askama::Error) -> Self {
        Self::Internal(format!("template error: {e}"))
    }
}

impl From<tokio::task::JoinError> for AppError {
    fn from(e: tokio::task::JoinError) -> Self {
        Self::Internal(format!("request worker failed: {e}"))
    }
}

/// Renders a template into an HTML response.
fn render(template: &impl Template) -> Result<Html<String>, AppError> {
    Ok(Html(template.render()?))
}

/// Scan progress as shown in the page banner.
pub(crate) struct ScanBanner {
    /// Books indexed so far.
    done: u64,
    /// Books to index; 0 while the scan is still looking for changes.
    total: u64,
}

impl ScanBanner {
    fn of(running: bool, progress: ProgressSnapshot) -> Option<Self> {
        (running && progress.phase != ScanPhase::Done).then_some(Self {
            done: progress.done,
            total: if progress.phase == ScanPhase::Indexing {
                progress.total
            } else {
                0
            },
        })
    }
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexPage<'a> {
    urls: &'a Urls,
    title: &'a str,
    scan: Option<ScanBanner>,
    library: String,
    books: u64,
}

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate<'a> {
    urls: &'a Urls,
    title: &'a str,
    scan: Option<ScanBanner>,
    status: u16,
    message: &'a str,
    rescan: bool,
}

/// `GET /`: the library page.
pub(crate) async fn index(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let pool = state.pool().clone();
    let books = tokio::task::spawn_blocking(move || -> Result<u64, AppError> {
        Ok(count_books(&*pool.get()?)?)
    })
    .await??;
    render(&IndexPage {
        urls: state.urls(),
        title: "Library",
        scan: banner(&state),
        library: state.library().root().display().to_string(),
        books,
    })
}

/// `POST /scan`: starts a rescan (unless one is running) and returns to the library.
pub(crate) async fn rescan(State(state): State<AppState>) -> Redirect {
    if state.scan().start() {
        tracing::info!("rescan requested");
    }
    Redirect::to(&state.urls().home())
}

/// `GET <base_path>` without the trailing slash: redirects to the library page.
pub(crate) async fn to_home(State(state): State<AppState>) -> Redirect {
    Redirect::temporary(&state.urls().home())
}

/// The fallback for unknown paths.
pub(crate) async fn not_found() -> AppError {
    AppError::NotFound
}

fn banner(state: &AppState) -> Option<ScanBanner> {
    let scan = state.scan();
    ScanBanner::of(scan.is_running(), scan.progress())
}

/// Middleware: renders the error page for responses produced by [`AppError`].
pub(crate) async fn render_errors(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let response = next.run(req).await;
    let Some(&page) = response.extensions().get::<ErrorPage>() else {
        return response;
    };
    let status = response.status();
    let (title, message, rescan) = match page {
        ErrorPage::NotFound => ("Not found", "There is no such page in this library.", false),
        ErrorPage::FileMissing => (
            "File not found",
            "The book's file is no longer where the index says it is. It may have been \
             moved, renamed or deleted; rescan the library to update the index.",
            true,
        ),
        ErrorPage::Internal => (
            "Internal error",
            "Something went wrong on the server. The details are in the server log.",
            false,
        ),
    };
    let page = ErrorTemplate {
        urls: state.urls(),
        title,
        scan: banner(&state),
        status: status.as_u16(),
        message,
        rescan,
    };
    match page.render() {
        Ok(html) => {
            let (mut parts, _) = response.into_parts();
            parts.headers.insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/html; charset=utf-8"),
            );
            parts.headers.remove(header::CONTENT_LENGTH);
            Response::from_parts(parts, html.into())
        }
        Err(e) => {
            tracing::error!("cannot render the error page: {e}");
            (status, title).into_response()
        }
    }
}
