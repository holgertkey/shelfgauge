//! Library statistics, the book lists behind its figures, and the duplicates.

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Html;
use rusqlite::Connection;

use super::figures::{count, duration, file_size, group_digits, width_class};
use super::{BookRow, Params, Row, date_time, render};
use crate::annotations::store;
use crate::format::fb2::genres::genre_label;
use crate::storage::books::live_books_by_key;
use crate::storage::stats::{
    BookFilter, DuplicateKind, LibraryStats, Share, count_books_where, duplicate_copies,
    duplicate_groups, list_books,
};
use crate::web::layout::{Layout, PageLink, Pager, Paging, SidebarMode};
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState, Urls};

/// The number of latest quotes shown.
const LATEST_QUOTES: usize = 3;

/// A summary card: the figure, what it counts, and a detail line.
struct Card {
    value: String,
    label: &'static str,
    detail: String,
    href: Option<String>,
}

/// A row of a bar chart.
struct Bar {
    label: String,
    href: Option<String>,
    count: String,
    /// The width class (see [`width_class`]).
    width: String,
}

/// A titled bar chart.
struct Chart {
    title: &'static str,
    bars: Vec<Bar>,
}

/// A line of figures, linked to their list if there is one.
struct Fact {
    text: String,
    href: Option<String>,
}

/// A quote in the "Reading and quotes" section.
struct QuoteLine {
    text: String,
    href: String,
    book: String,
    book_href: String,
}

#[derive(Template)]
#[template(path = "stats.html")]
struct StatsPage {
    layout: Layout,
    cards: Vec<Card>,
    charts: Vec<Chart>,
    health: Vec<Fact>,
    reading: Vec<Fact>,
    quotes: Vec<QuoteLine>,
}

/// `GET /stats`: the library statistics.
pub(crate) async fn stats(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let (stats, sidebar) = state
        .db(move |conn| {
            Ok((
                s.library_stats(conn)?,
                sidebar::build(conn, &s, mode, &Focus::None)?,
            ))
        })
        .await?;
    let (reading, quotes) = state.user_db(reading_and_quotes).await?;
    let urls = state.urls();
    render(&StatsPage {
        layout: Layout::new(&state, "Library statistics", sidebar),
        cards: cards(urls, &stats),
        charts: charts(urls, &stats),
        health: health(urls, &stats),
        reading,
        quotes,
    })
}

fn cards(urls: &Urls, stats: &LibraryStats) -> Vec<Card> {
    let mut cards = vec![
        Card {
            value: group_digits(stats.books),
            label: if stats.books == 1 { "book" } else { "books" },
            detail: if stats.readable == stats.books {
                String::new()
            } else {
                format!("{} readable", group_digits(stats.readable))
            },
            href: None,
        },
        Card {
            value: group_digits(stats.authors),
            label: if stats.authors == 1 {
                "author"
            } else {
                "authors"
            },
            detail: String::new(),
            href: Some(urls.authors("", 1)),
        },
        Card {
            value: group_digits(stats.series),
            label: "series",
            detail: String::new(),
            href: None,
        },
        Card {
            value: group_digits(stats.genres),
            label: if stats.genres == 1 { "genre" } else { "genres" },
            detail: String::new(),
            href: None,
        },
        Card {
            value: file_size(stats.size),
            label: "on disk",
            detail: if stats.zip_size > 0 {
                format!("{} in zip archives", file_size(stats.zip_size))
            } else {
                String::new()
            },
            href: None,
        },
    ];
    cards.retain(|card| card.value != "0" || card.label == "books");
    cards
}

/// The share `part` of `whole` in per mille (0 for an empty whole).
fn per_mille(part: u64, whole: u64) -> u64 {
    part.saturating_mul(1000).checked_div(whole).unwrap_or(0)
}

fn charts(urls: &Urls, stats: &LibraryStats) -> Vec<Chart> {
    let readable = stats.readable;
    let shares = |shares: &[Share],
                  label: &dyn Fn(&str) -> String,
                  filter: &dyn Fn(String) -> BookFilter| {
        shares
            .iter()
            .map(|share| Bar {
                label: share
                    .value
                    .as_deref()
                    .map_or_else(|| "Not stated".to_owned(), label),
                href: share
                    .value
                    .clone()
                    .map(|value| urls.books(&filter(value), 1)),
                count: group_digits(share.books),
                width: width_class(per_mille(share.books, readable)),
            })
            .collect::<Vec<_>>()
    };
    let ranked = |list: &[crate::storage::stats::Ranked],
                  href: &dyn Fn(&crate::storage::stats::Ranked) -> String| {
        let top = list.first().map_or(0, |r| r.books);
        list.iter()
            .map(|r| Bar {
                label: r.name.clone(),
                href: Some(href(r)),
                count: group_digits(r.books),
                width: width_class(per_mille(r.books, top)),
            })
            .collect::<Vec<_>>()
    };
    let charts = vec![
        Chart {
            title: "Formats",
            bars: shares(&stats.formats, &|f| f.to_uppercase(), &BookFilter::Format),
        },
        Chart {
            title: "Languages",
            bars: shares(&stats.languages, &str::to_owned, &BookFilter::Language),
        },
        Chart {
            title: "Genres",
            bars: shares(&stats.top_genres, &genre_label, &BookFilter::Genre),
        },
        Chart {
            title: "Top series",
            bars: ranked(&stats.top_series, &|r| urls.search(&r.name, 1)),
        },
        Chart {
            title: "Top authors",
            bars: ranked(&stats.top_authors, &|r| urls.author(r.id)),
        },
    ];
    charts.into_iter().filter(|c| !c.bars.is_empty()).collect()
}

fn health(urls: &Urls, stats: &LibraryStats) -> Vec<Fact> {
    let mut facts = Vec::new();
    if let Some(scan) = &stats.last_scan {
        let what = if scan.cancelled {
            "was cancelled".to_owned()
        } else {
            format!(
                "took {}: {} added, {} updated, {} moved, {} removed, {} errors",
                duration(scan.finished_at - scan.started_at),
                group_digits(scan.added),
                group_digits(scan.updated),
                group_digits(scan.moved),
                group_digits(scan.removed),
                group_digits(scan.errors)
            )
        };
        facts.push(Fact {
            text: format!("The last scan, {}, {what}.", date_time(scan.finished_at)),
            href: None,
        });
    }
    let mut fact = |n: u64, text: String, href: String| {
        if n > 0 {
            facts.push(Fact {
                text,
                href: Some(href),
            });
        }
    };
    fact(
        stats.errors,
        format!("{} could not be read", count(stats.errors, "book", "books")),
        urls.errors(),
    );
    fact(
        stats.protected,
        format!(
            "{} {} protected by DRM",
            count(stats.protected, "book", "books"),
            if stats.protected == 1 { "is" } else { "are" }
        ),
        format!("{}#protected", urls.errors()),
    );
    let d = stats.duplicates;
    fact(
        d.identical_groups,
        format!(
            "{} {} identical copies: {}, {} in extra copies",
            count(d.identical_groups, "book", "books"),
            if d.identical_groups == 1 {
                "has"
            } else {
                "have"
            },
            count(d.identical_files, "file", "files"),
            file_size(d.identical_extra_bytes)
        ),
        urls.duplicates(false, 1),
    );
    fact(
        d.probable_groups,
        format!(
            "{} like the same book in several files",
            match d.probable_groups {
                1 => "1 book looks".to_owned(),
                n => format!("{} books look", group_digits(n)),
            }
        ),
        urls.duplicates(true, 1),
    );
    fact(
        stats.without_cover,
        format!(
            "{} without a cover",
            count(stats.without_cover, "book", "books")
        ),
        urls.books(&BookFilter::WithoutCover, 1),
    );
    fact(
        stats.without_author,
        format!(
            "{} without an author",
            count(stats.without_author, "book", "books")
        ),
        urls.books(&BookFilter::WithoutAuthor, 1),
    );
    facts
}

/// The reading figures and the latest quotes of books in the library.
fn reading_and_quotes(
    state: &AppState,
    index: &Connection,
) -> Result<(Vec<Fact>, Vec<QuoteLine>), AppError> {
    let (progress, counts, latest) = {
        let user = state.userdata().lock();
        (
            store::progress_by_book(&user)?,
            store::counts_by_book(&user)?,
            // More than shown: some may be of books no longer in the library.
            store::latest_marks(&user, 50)?,
        )
    };
    let in_library = |key: &[u8]| -> Result<Option<i64>, AppError> {
        Ok(live_books_by_key(index, key)?.first().copied())
    };
    let (mut reading, mut finished) = (0, 0);
    for (key, progress) in &progress {
        if in_library(key)?.is_some() {
            if *progress >= 1000 {
                finished += 1;
            } else {
                reading += 1;
            }
        }
    }
    let (mut bookmarks, mut quotes, mut quoted_books) = (0, 0, 0);
    for (key, b, q) in &counts {
        if in_library(key)?.is_some() {
            bookmarks += b;
            quotes += q;
            quoted_books += u64::from(*q > 0);
        }
    }
    let urls = state.urls();
    let mut facts = Vec::new();
    let mut fact = |n: u64, text: String| {
        if n > 0 {
            facts.push(Fact { text, href: None });
        }
    };
    fact(
        reading,
        format!("{} in progress", count(reading, "book", "books")),
    );
    fact(
        finished,
        format!("{} finished", count(finished, "book", "books")),
    );
    fact(bookmarks, count(bookmarks, "bookmark", "bookmarks"));
    fact(
        quotes,
        format!(
            "{} in {}",
            count(quotes, "quote", "quotes"),
            count(quoted_books, "book", "books")
        ),
    );
    let mut lines = Vec::new();
    for quote in latest {
        let Some(id) = in_library(&quote.book_key)? else {
            continue;
        };
        lines.push(QuoteLine {
            text: quote.quote.text.clone(),
            href: urls.read_mark(id, quote.anchor.start_block),
            book: quote.book_title.clone(),
            book_href: urls.book(id),
        });
        if lines.len() == LATEST_QUOTES {
            break;
        }
    }
    Ok((facts, lines))
}

#[derive(Template)]
#[template(path = "list.html")]
struct ListPage {
    layout: Layout,
    heading: String,
    total: String,
    rows: Vec<Row>,
    empty: &'static str,
    pager: Option<Pager>,
}

/// A query value, trimmed; `None` if missing or blank.
fn value(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|v| !v.is_empty())
}

/// The list named by the query: `format`, `language`, `genre` or `missing`.
fn filter(params: &Params) -> Option<BookFilter> {
    if let Some(format) = value(&params.format) {
        return Some(BookFilter::Format(format.to_owned()));
    }
    if let Some(language) = value(&params.language) {
        return Some(BookFilter::Language(language.to_owned()));
    }
    if let Some(genre) = value(&params.genre) {
        return Some(BookFilter::Genre(genre.to_owned()));
    }
    match value(&params.missing) {
        Some("cover") => Some(BookFilter::WithoutCover),
        Some("author") => Some(BookFilter::WithoutAuthor),
        _ => None,
    }
}

/// `GET /books?format=|language=|genre=|missing=cover|author&page=`: the readable books
/// behind a figure of the statistics, by title.
pub(crate) async fn books(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let params = Params::from(query);
    let filter = filter(&params).ok_or(AppError::NotFound)?;
    let page = params.page();
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let f = filter.clone();
    let (paging, total, books, sidebar) = state
        .db(move |conn| {
            let total = count_books_where(conn, &f)?;
            let paging = Paging::new(page, total, s.page_size());
            let books = list_books(conn, &f, paging.limit, paging.offset())?;
            Ok((
                paging,
                total,
                books,
                sidebar::build(conn, &s, mode, &Focus::None)?,
            ))
        })
        .await?;
    let urls = state.urls();
    let heading = match &filter {
        BookFilter::Format(format) => format!("Books in {}", format.to_uppercase()),
        BookFilter::Language(language) => format!("Books in the language “{language}”"),
        BookFilter::Genre(code) => format!("Genre: {}", genre_label(code)),
        BookFilter::WithoutCover => "Books without a cover".to_owned(),
        BookFilter::WithoutAuthor => "Books without an author".to_owned(),
    };
    render(&ListPage {
        layout: Layout::new(&state, heading.clone(), sidebar),
        heading,
        total: count(total, "book", "books"),
        rows: books
            .into_iter()
            .map(|book| Row::Book(BookRow::new(urls, book)))
            .collect(),
        empty: "No books.",
        pager: paging.pager(|n| PageLink {
            href: urls.books(&filter, n),
            src: String::new(),
        }),
    })
}

/// A copy in a group on the duplicates page.
struct CopyRow {
    href: String,
    title: String,
    authors: String,
    path: String,
    format: String,
    size: String,
}

#[derive(Template)]
#[template(path = "duplicates.html")]
struct DuplicatesPage {
    layout: Layout,
    probable: bool,
    identical_href: String,
    probable_href: String,
    /// "12 books have identical copies".
    summary: String,
    groups: Vec<Vec<CopyRow>>,
    pager: Option<Pager>,
}

/// `GET /duplicates?kind=probable&page=`: groups of identical copies, or of probable
/// duplicates. Reported only: nothing is merged or hidden.
pub(crate) async fn duplicates(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let params = Params::from(query);
    let probable = params.kind.as_deref() == Some("probable");
    let kind = if probable {
        DuplicateKind::Probable
    } else {
        DuplicateKind::Identical
    };
    let page = params.page();
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let (paging, total, groups, sidebar) = state
        .db(move |conn| {
            let stats = s.library_stats(conn)?;
            let total = match kind {
                DuplicateKind::Identical => stats.duplicates.identical_groups,
                DuplicateKind::Probable => stats.duplicates.probable_groups,
            };
            let paging = Paging::new(page, total, s.page_size());
            let groups = duplicate_groups(conn, kind, paging.limit, paging.offset())?
                .iter()
                .map(|ids| duplicate_copies(conn, ids))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((
                paging,
                total,
                groups,
                sidebar::build(conn, &s, mode, &Focus::None)?,
            ))
        })
        .await?;
    let urls = state.urls();
    let summary = match (probable, total) {
        (false, 0) => "No book has identical copies.".to_owned(),
        (false, n) => format!(
            "{} {} identical copies: the same content in several files.",
            count(n, "book", "books"),
            if n == 1 { "has" } else { "have" }
        ),
        (true, 0) => "No books look like the same book in several files.".to_owned(),
        (true, 1) => "1 book looks like the same book in several files: the same title \
                      and authors, different contents."
            .to_owned(),
        (true, n) => format!(
            "{} books look like the same book in several files: the same title and \
             authors, different contents.",
            group_digits(n)
        ),
    };
    render(&DuplicatesPage {
        layout: Layout::new(&state, "Duplicates", sidebar),
        probable,
        identical_href: urls.duplicates(false, 1),
        probable_href: urls.duplicates(true, 1),
        summary,
        groups: groups
            .into_iter()
            .map(|copies| {
                copies
                    .into_iter()
                    .map(|copy| CopyRow {
                        href: urls.book(copy.id),
                        title: copy.title,
                        authors: copy.authors,
                        path: copy.path,
                        format: copy.format.to_uppercase(),
                        size: file_size(copy.size),
                    })
                    .collect()
            })
            .collect(),
        pager: paging.pager(|n| PageLink {
            href: urls.duplicates(probable, n),
            src: String::new(),
        }),
    })
}
