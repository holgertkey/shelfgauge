//! The book card and covers.

use askama::Template;
use axum::extract::{Path, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::{Html, Response};

use super::annotations::{Lists, book_annotations, progress_label};
use super::{date, parse_id, render};
use crate::cache::covers;
use crate::storage::books::{BookStatus, book_by_id};
use crate::web::error::PROTECTED;
use crate::web::layout::{Layout, SidebarMode};
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState};

#[derive(Template)]
#[template(path = "book.html")]
struct BookPage {
    layout: Layout,
    title: String,
    cover: Option<String>,
    authors: Vec<(String, String)>,
    notices: Vec<(&'static str, String)>,
    series: Vec<String>,
    genres: Vec<String>,
    language: Option<String>,
    format: String,
    location: String,
    folder_href: String,
    added: String,
    file_present: bool,
    read_href: Option<String>,
    /// "Continue reading" at the saved position.
    continue_href: Option<String>,
    /// "read 37 %" / "finished".
    progress: Option<String>,
    annotation: Vec<String>,
    /// The bookmarks and marks (`None` for a book that cannot have any).
    lists: Option<Lists>,
}

/// `GET /book/{id}`: the book card.
pub(crate) async fn card(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    let mode = SidebarMode::from_headers(&headers);
    let (book, file_present, sidebar, annotations) = state
        .user_db(move |s, conn| {
            let book = book_by_id(conn, id)?.ok_or(AppError::NotFound)?;
            let file_present = book.status != BookStatus::Removed
                && s.library().root().join(&book.container_path).is_file();
            let focus = if book.status == BookStatus::Removed {
                Focus::None
            } else {
                Focus::book(&book)
            };
            let sidebar = sidebar::build(conn, s, mode, &focus)?;
            // Re-attaching may load the book: only when its file is there.
            let annotations = if file_present || book.status == BookStatus::Removed {
                book_annotations(s, conn, s.userdata(), &book, None)?
            } else {
                None
            };
            Ok((book, file_present, sidebar, annotations))
        })
        .await?;

    let urls = state.urls();
    let mut notices = Vec::new();
    match book.status {
        BookStatus::Removed => notices.push((
            "removed",
            "This book's file is no longer in the library.".to_owned(),
        )),
        BookStatus::Error => notices.push((
            "error",
            format!(
                "The book could not be read: {}",
                book.error_message.as_deref().unwrap_or("unknown error")
            ),
        )),
        BookStatus::Protected => notices.push((
            "protected",
            format!(
                "{} ({}).",
                PROTECTED.trim_end_matches('.'),
                book.drm.as_deref().unwrap_or("unknown DRM")
            ),
        )),
        BookStatus::Ok => {}
    }
    if book.status != BookStatus::Removed && !file_present {
        notices.push((
            "missing",
            "The book's file is not where the index says it is: it may have been moved, \
             renamed or deleted. Rescan the library to update the index."
                .to_owned(),
        ));
    }
    let location = match &book.entry {
        Some(entry) => format!("{} → {}", book.container_path, entry.path),
        None => book.container_path.clone(),
    };
    let cover = book
        .cover_mime
        .as_deref()
        .and_then(covers::known_mime)
        .map(|_| urls.cover(book.id, book.updated_at));
    let readable = book.status == BookStatus::Ok && file_present;
    let position = annotations
        .as_ref()
        .and_then(|a| a.position.as_ref())
        .filter(|_| readable);
    let lists = annotations
        .as_ref()
        .filter(|a| readable || !a.annotations.is_empty())
        .map(|a| {
            Lists::new(
                urls,
                book.id,
                &a.annotations,
                format!("{}#annotations", urls.book(book.id)),
                None,
            )
        });
    render(&BookPage {
        continue_href: position.map(|p| urls.read_at(book.id, p.block)),
        progress: position.map(|p| progress_label(p.progress)),
        lists,
        layout: Layout::new(&state, book.title.clone(), sidebar),
        cover,
        authors: book
            .authors
            .iter()
            .map(|a| (a.display_name.clone(), urls.author(a.id)))
            .collect(),
        notices,
        series: book
            .series
            .iter()
            .map(|s| match &s.number {
                Some(number) => format!("{} #{number}", s.name),
                None => s.name.clone(),
            })
            .collect(),
        genres: book.genres.clone(),
        language: book.language.clone(),
        format: book.format.to_uppercase(),
        folder_href: urls.folder(&sidebar::book_folder(&book), 1),
        location,
        added: date(book.added_at),
        file_present,
        read_href: readable.then(|| urls.read(book.id, 1)),
        annotation: book
            .annotation
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect(),
        title: book.title,
    })
}

/// `GET /cover/{id}?v=<updated_at>`: a cover from the cache.
///
/// Only known image types are served, with their canonical `Content-Type`. A URL with
/// the current version is cached as immutable.
pub(crate) async fn cover(
    State(state): State<AppState>,
    Path(id): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let id = parse_id(&id)?;
    let s = state.clone();
    let (mime, version, data) = state
        .db(move |conn| {
            let book = book_by_id(conn, id)?.ok_or(AppError::NotFound)?;
            let mime = book
                .cover_mime
                .as_deref()
                .and_then(covers::known_mime)
                .ok_or(AppError::NotFound)?;
            let path = covers::path(&s.library().covers_dir(), id, mime);
            let data = std::fs::read(&path).map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    AppError::NotFound
                } else {
                    AppError::Internal(format!("{}: {e}", path.display()))
                }
            })?;
            Ok((mime, book.updated_at, data))
        })
        .await?;

    Ok(super::versioned_bytes(
        mime,
        version,
        query.as_deref(),
        &headers,
        data,
    ))
}
