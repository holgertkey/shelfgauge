//! Reading mode: one part of a book as HTML, and the book's images.

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};

use super::annotations::{Lists, book_annotations};
use super::{Params, parse_id, render};
use crate::annotations::attach;
use crate::annotations::text::BookText;
use crate::cache::covers;
use crate::format::model::BlockId;
use crate::format::run_with_parser_stack;
use crate::render::html::{ReaderLinks, render_part, render_toc};
use crate::render::split::{Part, part_of, split};
use crate::storage::books::{BookDetails, BookStatus, book_by_id};
use crate::web::layout::{Layout, SidebarView, page_number};
use crate::web::{AppError, AppState, Urls};

/// Links of a rendered part.
struct Links<'a> {
    urls: &'a Urls,
    book: i64,
    version: i64,
}

impl ReaderLinks for Links<'_> {
    fn block(&self, part: usize, id: BlockId) -> String {
        format!("{}#{id}", self.urls.read(self.book, part + 1))
    }

    fn image(&self, image_id: &str) -> String {
        self.urls.image(self.book, image_id, self.version)
    }
}

/// The reading progress (per mille of the main text) at the start of a part and at the
/// start of the next one (1000 for the last part of the main text); the reader
/// interpolates between them. `None` for a part that starts in the notes.
struct PartProgress {
    start: u16,
    end: u16,
}

impl PartProgress {
    fn of(text: &BookText, parts: &[Part], index: usize) -> Option<Self> {
        let start = text.progress(parts.get(index)?.start)?;
        let end = parts
            .get(index + 1)
            .and_then(|next| text.progress(next.start))
            .unwrap_or(1000);
        Some(Self {
            start,
            end: end.max(start),
        })
    }

    /// The progress at the start of the part, as shown before the reader updates it.
    fn label(&self) -> String {
        format!("{} %", self.start / 10)
    }
}

/// A neighbouring part.
struct PartLink {
    href: String,
    title: String,
}

#[derive(Template)]
#[template(path = "read.html")]
struct ReadPage {
    layout: Layout,
    book_title: String,
    /// The authors, comma-separated (the focus mode's running head).
    book_authors: String,
    /// The reading progress over this part (the focus mode's status line).
    progress: Option<PartProgress>,
    card_href: String,
    part_title: String,
    label: String,
    html: String,
    toc: String,
    prev: Option<PartLink>,
    next: Option<PartLink>,
    /// The bookmark and mark lists.
    lists: Lists,
    /// URL to create annotations (POST) and to fetch the lists (GET).
    annotations_href: String,
    /// URL to save the reading position; empty when positions are not remembered.
    position_href: String,
    max_text: u32,
    max_note: u32,
}

/// What a read request results in.
enum Outcome {
    /// The part to show.
    Page(Box<ReadPage>),
    /// `?at=`: the URL of the part with the block.
    Redirect(String),
}

/// `GET /book/{id}/read?part=<n>` or `?at=<block id>`.
///
/// `at` wins over `part`: it redirects to the part that contains the block, with the
/// block as the URL fragment. An invalid or unknown block is "not found"; a part number
/// out of range is clamped.
pub(crate) async fn read(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Response, AppError> {
    let id = parse_id(&id)?;
    let params = Params::from(query);
    let book = book(&state, id).await?;
    let s = state.clone();
    let outcome =
        tokio::task::spawn_blocking(move || run_with_parser_stack(|| page(&s, &book, &params)))
            .await??;
    match outcome {
        Outcome::Redirect(url) => Ok(Redirect::to(&url).into_response()),
        Outcome::Page(page) => Ok(render(&*page)?.into_response()),
    }
}

async fn book(state: &AppState, id: i64) -> Result<BookDetails, AppError> {
    let book = state
        .db(move |conn| book_by_id(conn, id)?.ok_or(AppError::NotFound))
        .await?;
    if book.status == BookStatus::Removed {
        return Err(AppError::FileMissing);
    }
    Ok(book)
}

fn page(state: &AppState, book: &BookDetails, params: &Params) -> Result<Outcome, AppError> {
    let content = state.content(book)?;
    let parts = split(&content, state.reader());
    let urls = state.urls();
    let links = Links {
        urls,
        book: book.id,
        version: book.updated_at,
    };
    if let Some(at) = &params.at {
        let block: BlockId = at.parse().map_err(|_| AppError::NotFound)?;
        let part = part_of(&parts, block).ok_or(AppError::NotFound)?;
        // `from=mark` survives the redirect: that page view must not move the position.
        let url = if params.from.as_deref() == Some("mark") {
            format!("{}#{block}", urls.read_from_mark(book.id, part + 1))
        } else {
            links.block(part, block)
        };
        return Ok(Outcome::Redirect(url));
    }
    let count = parts.len().max(1);
    let index = usize::try_from(page_number(params.part.as_deref()))
        .unwrap_or(usize::MAX)
        .min(count)
        - 1;
    let neighbour = |i: usize| {
        parts.get(i).map(|part| PartLink {
            href: urls.read(book.id, i + 1),
            title: part.title.clone(),
        })
    };
    let html = if parts.is_empty() {
        "<p class=\"empty\">This book has no text.</p>".to_owned()
    } else {
        render_part(&content, &parts, index, &links)
    };
    let part_title = parts
        .get(index)
        .map_or_else(String::new, |part| part.title.clone());
    let annotations = {
        let index = state.pool().get()?;
        book_annotations(state, &index, state.userdata(), book, Some(&content))?
            .map_or_else(Vec::new, |a| a.annotations)
    };
    let current = urls.read(book.id, index + 1);
    let title = if count > 1 {
        format!("{} — {}", book.title, part_title)
    } else {
        book.title.clone()
    };
    let progress = PartProgress::of(&BookText::new(&content), &parts, index);
    Ok(Outcome::Page(Box::new(ReadPage {
        layout: Layout::new(state, title, SidebarView::default()),
        book_title: book.title.clone(),
        book_authors: attach::authors(book),
        progress,
        card_href: urls.book(book.id),
        label: format!("Part {} of {count}", index + 1),
        part_title,
        html,
        toc: render_toc(&content, &parts, index, &links),
        prev: index.checked_sub(1).and_then(neighbour),
        next: neighbour(index + 1),
        lists: Lists::new(urls, book.id, &annotations, current),
        annotations_href: urls.annotations(book.id),
        position_href: if state.reader().remember_position {
            urls.position(book.id)
        } else {
            String::new()
        },
        max_text: state.annotations().max_text_length,
        max_note: state.annotations().max_note_length,
    })))
}

/// `GET /book/{id}/image/{image}`: an image of the book's content.
///
/// Only known raster image types are served (an SVG could carry scripts).
pub(crate) async fn image(
    State(state): State<AppState>,
    Path((id, image)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let id = parse_id(&id)?;
    let book = book(&state, id).await?;
    let version = book.updated_at;
    let s = state.clone();
    let (mime, data) = tokio::task::spawn_blocking(move || {
        run_with_parser_stack(|| {
            let content = s.content(&book)?;
            let image = content.images().get(&image).ok_or(AppError::NotFound)?;
            let mime = covers::known_mime(&image.mime).ok_or(AppError::NotFound)?;
            Ok::<_, AppError>((mime, image.data.clone()))
        })
    })
    .await??;
    Ok(super::versioned_bytes(
        mime,
        version,
        query.as_deref(),
        &headers,
        data,
    ))
}
