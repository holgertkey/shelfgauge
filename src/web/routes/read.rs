//! Reading mode: one part of a book as HTML, and the book's images.

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, RawQuery, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Redirect, Response};

use super::annotations::{Lists, annotation_lists, book_annotations};
use super::find::{FindResults, results};
use super::{Params, parse_id, render};
use crate::annotations::attach;
use crate::cache::covers;
use crate::format::model::BlockId;
use crate::format::run_with_parser_stack;
use crate::render::html::{ReaderLinks, render_part, render_toc};
use crate::render::outline::{
    ChapterTick, PartProgress, SHARE_SCALE, chapter_ticks, html_lang, part_progress, title_before,
};
use crate::render::split::{part_of, split};
use crate::storage::books::{BookDetails, BookStatus, book_by_id};
use crate::web::layout::{Layout, SidebarView, page_number};
use crate::web::profile::CurrentProfile;
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

/// The reading progress over the part, as the template shows it.
struct Progress(PartProgress);

impl std::ops::Deref for Progress {
    type Target = PartProgress;

    fn deref(&self) -> &PartProgress {
        &self.0
    }
}

impl Progress {
    /// The progress at the start of the part, as shown before the reader updates it.
    fn label(&self) -> String {
        format!("{} %", self.start * 100 / SHARE_SCALE)
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
    progress: Option<Progress>,
    /// The chapters on the status line's track.
    chapters: Vec<ChapterTick>,
    /// The book's read URL; the track adds `?progress=` to it.
    read_href: String,
    card_href: String,
    part_title: String,
    /// The running head before the reader updates it: the title in effect where the
    /// part starts, else (`None`) the authors and the book title.
    head_title: Option<String>,
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
    /// The Search tab.
    find: FindPanel,
}

/// The Search tab: its form and, with a `find` parameter, the results.
struct FindPanel {
    /// The form's target: the read page (the part goes in a hidden field).
    action: String,
    /// The live search's URL, without the query.
    href: String,
    /// The current 1-based part.
    part: usize,
    query: String,
    results: Option<FindResults>,
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
    current: CurrentProfile,
    Path(id): Path<String>,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Response, AppError> {
    let id = parse_id(&id)?;
    let params = Params::from(query);
    let book = book(&state, id).await?;
    let s = state.clone();
    let outcome =
        crate::web::blocking(move || run_with_parser_stack(|| page(&s, &current, &book, &params)))
            .await??;
    match outcome {
        Outcome::Redirect(url) => Ok(Redirect::to(&url).into_response()),
        Outcome::Page(page) => Ok(render(&*page)?.into_response()),
    }
}

pub(super) async fn book(state: &AppState, id: i64) -> Result<BookDetails, AppError> {
    let book = state
        .db(move |conn| book_by_id(conn, id)?.ok_or(AppError::NotFound))
        .await?;
    if book.status == BookStatus::Removed {
        return Err(AppError::FileMissing);
    }
    Ok(book)
}

fn page(
    state: &AppState,
    profile: &CurrentProfile,
    book: &BookDetails,
    params: &Params,
) -> Result<Outcome, AppError> {
    let config = state.config();
    let (content, text) = state.book_text(book)?;
    let parts = split(&content, &config.reader);
    let urls = state.urls();
    let links = Links {
        urls,
        book: book.id,
        version: book.updated_at,
    };
    if let Some(progress) = &params.progress {
        // A place on the status line's track: the main-text block at that share.
        let share: u64 = progress
            .parse()
            .ok()
            .filter(|&share| share <= SHARE_SCALE)
            .ok_or(AppError::NotFound)?;
        let block = text
            .block_at_share(share, SHARE_SCALE)
            .ok_or(AppError::NotFound)?;
        let part = part_of(&parts, block).ok_or(AppError::NotFound)?;
        return Ok(Outcome::Redirect(links.block(part, block)));
    }
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
        book_annotations(
            state,
            profile.id(),
            &index,
            state.userdata(),
            book,
            Some(&content),
        )?
        .map_or_else(Vec::new, |a| a.annotations)
    };
    let current = urls.read(book.id, index + 1);
    let title = if count > 1 {
        format!("{} — {}", book.title, part_title)
    } else {
        book.title.clone()
    };
    let progress = part_progress(&text, &parts, index).map(Progress);
    Ok(Outcome::Page(Box::new(ReadPage {
        layout: Layout::new(state, profile, title, SidebarView::default())
            .with_lang(html_lang(book.language.as_deref())),
        book_title: book.title.clone(),
        book_authors: attach::authors(book),
        progress,
        chapters: chapter_ticks(&content, &text),
        read_href: urls.read(book.id, 1),
        card_href: urls.book(book.id),
        label: format!("Part {} of {count}", index + 1),
        part_title,
        head_title: parts
            .get(index)
            .and_then(|part| title_before(&content, part.start)),
        html,
        toc: render_toc(&content, &parts, index, &links),
        prev: index.checked_sub(1).and_then(neighbour),
        next: neighbour(index + 1),
        lists: annotation_lists(urls, book.id, &annotations, current, Some(&text)),
        annotations_href: urls.annotations(book.id),
        position_href: if config.reader.remember_position {
            urls.position(book.id)
        } else {
            String::new()
        },
        max_text: config.annotations.max_text_length,
        max_note: config.annotations.max_note_length,
        find: FindPanel {
            action: urls.read(book.id, 1),
            href: urls.find(book.id, ""),
            part: index + 1,
            results: params
                .find
                .as_deref()
                .filter(|query| !query.trim().is_empty())
                .map(|query| {
                    let hit = params.hit.as_deref().and_then(|hit| hit.parse().ok());
                    results(urls, book.id, &parts, &text, query, hit)
                }),
            query: params.find.clone().unwrap_or_default(),
        },
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
    let (mime, data) = crate::web::blocking(move || {
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
