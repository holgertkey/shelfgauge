//! Request handlers, grouped by area, and what they share.
//!
//! Handlers never trust their input: ids that do not parse are "not found", page
//! numbers are clamped, and malformed query strings count as empty.

use askama::Template;
use axum::extract::Query;
use axum::extract::rejection::QueryRejection;
use axum::response::Html;
use serde::Deserialize;

use super::AppError;
use super::Urls;
use crate::storage::books::{BookStatus, BookSummary};
use crate::storage::folders::FolderPath;

pub(crate) mod book;
pub(crate) mod library;
pub(crate) mod scan;
pub(crate) mod search;
pub(crate) mod sidebar;

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
        }
    }
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
