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
use super::profile::CurrentProfile;
use super::trace::RequestId;
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
    /// The request is invalid (400); the reason is shown.
    BadRequest(String),
    /// The book is protected by DRM (403); the scheme is shown.
    Protected(String),
    /// The profile may not do this: an admin page for a reader (403).
    Forbidden,
    /// An unexpected failure (500); the message is logged, not shown.
    Internal(String),
}

/// Which error page a response should get; set by [`AppError::into_response`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ErrorPage {
    NotFound,
    FileMissing,
    Unreadable(String),
    BadRequest(String),
    Protected(String),
    Forbidden,
    Internal,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, page) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, ErrorPage::NotFound),
            Self::FileMissing => (StatusCode::NOT_FOUND, ErrorPage::FileMissing),
            Self::Unreadable(reason) => {
                tracing::warn!(reason = ?reason, "cannot read a book");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    ErrorPage::Unreadable(reason),
                )
            }
            Self::BadRequest(reason) => (StatusCode::BAD_REQUEST, ErrorPage::BadRequest(reason)),
            Self::Protected(scheme) => (StatusCode::FORBIDDEN, ErrorPage::Protected(scheme)),
            Self::Forbidden => (StatusCode::FORBIDDEN, ErrorPage::Forbidden),
            Self::Internal(message) => {
                tracing::error!(error = ?message, "internal error");
                (StatusCode::INTERNAL_SERVER_ERROR, ErrorPage::Internal)
            }
        };
        let mut response = status.into_response();
        response.extensions_mut().insert(page);
        response
    }
}

/// The internal error page (500) without logging anything: for a panic, which the
/// panic hook has logged.
pub(crate) fn internal_error() -> Response {
    let mut response = StatusCode::INTERNAL_SERVER_ERROR.into_response();
    response.extensions_mut().insert(ErrorPage::Internal);
    response
}

impl From<StorageError> for AppError {
    fn from(e: StorageError) -> Self {
        Self::Internal(e.to_string())
    }
}

impl From<crate::annotations::attach::AttachError> for AppError {
    fn from(e: crate::annotations::attach::AttachError) -> Self {
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
    /// The request id on the internal error page, to find the request in the log.
    error_id: Option<String>,
    rescan: bool,
}

/// What a DRM-protected book shows instead of its text.
pub(crate) const PROTECTED: &str = "This book is protected by DRM, so shelfgauge cannot \
    open it. Open it in the app of the store where you got it.";

/// The fallback for unknown paths.
pub(crate) async fn not_found() -> AppError {
    AppError::NotFound
}

/// Middleware: renders the error page for responses produced by [`AppError`].
///
/// Error pages have an empty sidebar: they must render even when the index cannot be
/// read. The internal error page shows the request id (see [`RequestId`]).
pub(crate) async fn render_errors(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let id = req.extensions().get::<RequestId>().copied();
    let tokens = super::profile::cookie_tokens(req.headers());
    let response = next.run(req).await;
    let Some(page) = response.extensions().get::<ErrorPage>().cloned() else {
        return response;
    };
    // Routes without the profile middleware (unknown paths, the picker): the profile
    // of the cookie, if any, for the theme and the role.
    let profile = match response.extensions().get::<CurrentProfile>().cloned() {
        Some(profile) => Some(profile),
        None => {
            let s = state.clone();
            super::blocking(move || super::profile::resolve(&s, &tokens))
                .await
                .ok()
                .and_then(Result::ok)
                .flatten()
        }
    };
    let status = response.status();
    let detail = match &page {
        ErrorPage::Unreadable(reason) | ErrorPage::BadRequest(reason) => Some(reason.clone()),
        ErrorPage::Protected(scheme) => Some(format!("Protection: {scheme}.")),
        _ => None,
    };
    let error_id = id
        .filter(|_| page == ErrorPage::Internal)
        .map(|id| id.to_string());
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
        ErrorPage::BadRequest(_) => (
            "Invalid request",
            "The request could not be carried out.",
            false,
        ),
        ErrorPage::Protected(_) => ("Protected by DRM", PROTECTED, false),
        ErrorPage::Forbidden => (
            "Not allowed",
            "This page is for profiles with the admin role.",
            false,
        ),
        ErrorPage::Internal => (
            "Internal error",
            "Something went wrong on the server. The details are in the server log.",
            false,
        ),
    };
    let page = ErrorTemplate {
        layout: Layout::without_sidebar(&state, profile.as_ref(), title),
        status: status.as_u16(),
        message,
        detail,
        error_id,
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
            tracing::error!(error = %e, "cannot render the error page");
            (status, title).into_response()
        }
    }
}
