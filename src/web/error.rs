//! Failed requests and the error pages.
//!
//! Handlers return [`AppError`]. Its response only carries the status and an
//! [`ErrorPage`] marker; the [`render_errors`] middleware, which has the state (and so
//! the URL helper), turns it into the HTML error page.

use askama::Template;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use super::AppState;
use super::layout::Layout;
use crate::storage::StorageError;

/// A failed request; rendered as an error page by [`render_errors`].
#[derive(Debug)]
pub(crate) enum AppError {
    /// No such page or object (404).
    NotFound,
    /// The book is indexed but its file is gone: the library needs a rescan (404).
    FileMissing,
    /// The book cannot be read or parsed (500); the reason is shown.
    Unreadable(String),
    /// An unexpected failure (500); the message is logged, not shown.
    Internal(String),
}

/// Which error page a response should get; set by [`AppError::into_response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ErrorPage {
    NotFound,
    FileMissing,
    Unreadable(String),
    Internal,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, page) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, ErrorPage::NotFound),
            Self::FileMissing => (StatusCode::NOT_FOUND, ErrorPage::FileMissing),
            Self::Unreadable(reason) => {
                tracing::warn!("cannot read a book: {reason}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorPage::Unreadable(reason),
                )
            }
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

#[derive(Template)]
#[template(path = "error.html")]
struct ErrorTemplate<'a> {
    layout: Layout,
    status: u16,
    message: &'a str,
    detail: Option<String>,
    rescan: bool,
}

/// The fallback for unknown paths.
pub(crate) async fn not_found() -> AppError {
    AppError::NotFound
}

/// Middleware: renders the error page for responses produced by [`AppError`].
///
/// Error pages have an empty sidebar: they must render even when the index cannot be
/// read.
pub(crate) async fn render_errors(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let response = next.run(req).await;
    let Some(page) = response.extensions().get::<ErrorPage>().cloned() else {
        return response;
    };
    let status = response.status();
    let detail = match &page {
        ErrorPage::Unreadable(reason) => Some(reason.clone()),
        _ => None,
    };
    let (title, message, rescan) = match page {
        ErrorPage::NotFound => ("Not found", "There is no such page in this library.", false),
        ErrorPage::FileMissing => (
            "File not found",
            "The book's file is no longer where the index says it is. It may have been \
             moved, renamed or deleted; rescan the library to update the index.",
            true,
        ),
        ErrorPage::Unreadable(_) => (
            "Cannot read the book",
            "The book's file could not be read or parsed.",
            false,
        ),
        ErrorPage::Internal => (
            "Internal error",
            "Something went wrong on the server. The details are in the server log.",
            false,
        ),
    };
    let page = ErrorTemplate {
        layout: Layout::without_sidebar(&state, title),
        status: status.as_u16(),
        message,
        detail,
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
