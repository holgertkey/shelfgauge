//! Request handlers, grouped by area, and what they share.
//!
//! Handlers never trust their input: ids that do not parse are "not found", page
//! numbers are clamped, and malformed query strings count as empty.

use askama::Template;
use axum::extract::Query;
use axum::extract::rejection::QueryRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::AppError;
use super::Urls;
use crate::storage::books::{BookStatus, BookSummary};
use crate::storage::folders::FolderPath;

pub(crate) mod annotations;
pub(crate) mod book;
pub(crate) mod figures;
pub(crate) mod find;
pub(crate) mod hub;
pub(crate) mod library;
pub(crate) mod login;
pub(crate) mod profiles;
pub(crate) mod read;
pub(crate) mod scan;
pub(crate) mod search;
pub(crate) mod settings;
pub(crate) mod shelf;
pub(crate) mod sidebar;
pub(crate) mod stats;

/// The query parameters any page may take.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Params {
    /// Search text or author name prefix.
    #[serde(default)]
    q: String,
    /// 1-based page number.
    page: Option<String>,
    /// A directory (folder pages).
    path: Option<String>,
    /// An archive (folder pages).
    zip: Option<String>,
    /// A directory inside `zip`.
    #[serde(rename = "in")]
    inner: Option<String>,
    /// 1-based part (reading mode).
    part: Option<String>,
    /// A block id to open (reading mode), e.g. `b12`.
    at: Option<String>,
    /// A place to open (reading mode): a share of the main text in ten-thousandths.
    progress: Option<String>,
    /// `mark` when the page is opened from a bookmark or mark (reading mode): the
    /// reading position is not saved for this page view.
    from: Option<String>,
    /// A query to search the book for (reading mode).
    find: Option<String>,
    /// The 1-based number of the search result followed (reading mode).
    hit: Option<String>,
    /// A format (book lists).
    format: Option<String>,
    /// A language (book lists).
    language: Option<String>,
    /// A genre code (book lists).
    genre: Option<String>,
    /// `cover` or `author`: books without one (book lists).
    missing: Option<String>,
    /// `probable` for probable duplicates (duplicates page).
    kind: Option<String>,
}

impl Params {
    /// The parameters, or none if the query string is malformed.
    fn from(query: Result<Query<Params>, QueryRejection>) -> Self {
        query.map(|Query(params)| params).unwrap_or_default()
    }

    /// The `page` value as a page number (1 if missing or invalid).
    fn page(&self) -> u64 {
        super::layout::page_number(self.page.as_deref())
    }

    /// The folder named by `path`, or by `zip` and `in`.
    fn folder(&self) -> FolderPath {
        let clean = |s: &Option<String>| s.as_deref().unwrap_or("").trim_matches('/').to_owned();
        match &self.zip {
            Some(zip) if !zip.trim_matches('/').is_empty() => FolderPath::Archive {
                archive: zip.trim_matches('/').to_owned(),
                inner: clean(&self.inner),
            },
            _ => FolderPath::Dir(clean(&self.path)),
        }
    }
}

/// The `back` field of an action's form: where to go after it.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Back {
    #[serde(default)]
    back: String,
}

impl Back {
    /// The page to redirect to: the settings page for `back=settings`, otherwise the
    /// library page. A request without a form body counts as no `back`.
    fn url<T>(form: Result<axum::Form<Back>, T>, urls: &Urls) -> String {
        match form {
            Ok(axum::Form(Back { back })) if back == "settings" => urls.settings(),
            _ => urls.home(),
        }
    }
}

/// An id from a URL path; anything else is "not found".
fn parse_id(text: &str) -> Result<i64, AppError> {
    text.parse::<i64>()
        .ok()
        .filter(|id| *id > 0)
        .ok_or(AppError::NotFound)
}

/// Renders a template into an HTML response.
fn render(template: &impl Template) -> Result<Html<String>, AppError> {
    Ok(Html(template.render()?))
}

/// A response with binary data that belongs to a book version (covers, images).
///
/// The `ETag` is the version; a URL whose query carries `v=<version>` is cached as
/// immutable, other URLs are revalidated. `mime` must come from an allowlist.
fn versioned_bytes(
    mime: &'static str,
    version: i64,
    query: Option<&str>,
    headers: &HeaderMap,
    data: Vec<u8>,
) -> Response {
    let etag = format!("\"{version}\"");
    let current = query.is_some_and(|q| q.split('&').any(|pair| pair == format!("v={version}")));
    let cache_control = if current {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    let response_headers = [
        (
            header::ETAG,
            HeaderValue::from_str(&etag).expect("digits are a valid header"),
        ),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        ),
    ];
    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|tag| tag.trim() == etag));
    if not_modified {
        return (StatusCode::NOT_MODIFIED, response_headers).into_response();
    }
    (response_headers, [(header::CONTENT_TYPE, mime)], data).into_response()
}

/// An entry of a book list in the main area.
pub(crate) enum Row {
    /// A series heading.
    Heading(String),
    /// A book.
    Book(BookRow),
}

/// A book in a list in the main area.
pub(crate) struct BookRow {
    href: String,
    title: String,
    authors: String,
    series: String,
    error: bool,
    protected: bool,
}

impl BookRow {
    fn new(urls: &Urls, book: BookSummary) -> Self {
        let series = book.series.map_or_else(String::new, |s| match s.number {
            Some(number) => format!("{} #{number}", s.name),
            None => s.name,
        });
        Self {
            href: urls.book(book.id),
            title: book.title,
            authors: book.authors,
            series,
            error: book.status == BookStatus::Error,
            protected: book.status == BookStatus::Protected,
        }
    }
}

/// `YYYY-MM-DD HH:MM UTC` of a Unix time in seconds.
fn date_time(unix: i64) -> String {
    let seconds = unix.rem_euclid(86_400);
    format!(
        "{} {:02}:{:02} UTC",
        date(unix),
        seconds / 3600,
        seconds % 3600 / 60
    )
}

/// `YYYY-MM-DD` (UTC) of a Unix time in seconds.
fn date(unix: i64) -> String {
    // Howard Hinnant's days-to-civil algorithm.
    let days = unix.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert_eq!(parse_id("12").unwrap(), 12);
        for bad in ["", "0", "-1", "x", "1.5", "99999999999999999999"] {
            assert!(matches!(parse_id(bad), Err(AppError::NotFound)), "{bad}");
        }
    }

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400), "2000-02-29");
        assert_eq!(date(1_791_763_200), "2026-10-12");
        assert_eq!(date(-86_400), "1969-12-31");
        assert_eq!(
            date_time(1_791_763_200 + 3_600 * 6 + 60 * 35 + 59),
            "2026-10-12 06:35 UTC"
        );
        assert_eq!(date_time(-60), "1969-12-31 23:59 UTC");
    }

    #[test]
    fn folder_params() {
        let params = |path: Option<&str>, zip: Option<&str>, inner: Option<&str>| Params {
            path: path.map(str::to_owned),
            zip: zip.map(str::to_owned),
            inner: inner.map(str::to_owned),
            ..Params::default()
        };
        assert_eq!(
            params(None, None, None).folder(),
            FolderPath::Dir(String::new())
        );
        assert_eq!(
            params(Some("/a/b/"), None, None).folder(),
            FolderPath::Dir("a/b".to_owned())
        );
        assert_eq!(
            params(None, Some("x.zip"), Some("d/")).folder(),
            FolderPath::Archive {
                archive: "x.zip".to_owned(),
                inner: "d".to_owned()
            }
        );
        assert_eq!(
            params(Some("a"), Some(""), None).folder(),
            FolderPath::Dir("a".to_owned())
        );
    }
}
