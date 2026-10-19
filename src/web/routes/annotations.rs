//! Annotations and the reading position: the "user writes" routes, and the lists
//! shown in the reader sidebar and on the book card.
//!
//! The book an annotation belongs to is always the indexed book of the URL (its
//! content key), never something the client says; the quote, its context and the
//! section title are derived from the book on the server. The reader sends JSON and
//! gets JSON; the book card uses plain forms and gets a redirect (303).

use askama::Template;
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Form, FromRequest, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use super::figures::width_class;
use super::parse_id;
use crate::annotations::anchor::{self, Anchor, AnchorError};
use crate::annotations::attach::{self, export_key};
use crate::annotations::store::{self, NewAnnotation, ReadingPosition, UserData};
use crate::annotations::text::{BookText, char_len};
use crate::annotations::{Annotation, BookSnapshot, now};
use crate::cache::covers;
use crate::format::model::{BlockId, BookContent};
use crate::profiles::ProfileId;
use crate::render::split::{part_of, split};
use crate::storage::books::{BookDetails, BookStatus, book_by_id};
use crate::web::profile::CurrentProfile;
use crate::web::{AppError, AppState, Urls};

/// An annotation in a list.
pub(crate) struct ListItem {
    pub(crate) id: String,
    pub(crate) href: String,
    pub(crate) label: String,
    pub(crate) note: String,
    pub(crate) text: String,
    pub(crate) bookmark: bool,
    pub(crate) lost: bool,
    pub(crate) start_block: String,
    pub(crate) start_offset: u32,
    pub(crate) end_block: String,
    pub(crate) end_offset: u32,
    pub(crate) prefix: String,
    pub(crate) suffix: String,
    pub(crate) note_action: String,
    pub(crate) delete_action: String,
    /// Where it starts, in ten-thousandths of the main text (bookmarks in the reader:
    /// the ticks on the status line); `None` when unknown or in the notes.
    pub(crate) progress: Option<u64>,
}

/// A book's bookmarks and marks, in book order.
pub(crate) struct Lists {
    pub(crate) bookmarks: Vec<ListItem>,
    pub(crate) marks: Vec<ListItem>,
    /// Where the forms return to.
    pub(crate) back: String,
}

impl Lists {
    /// The lists; with the book's `text`, items carry their [`ListItem::progress`].
    pub(crate) fn new(
        urls: &Urls,
        book_id: i64,
        annotations: &[Annotation],
        back: String,
        text: Option<&BookText>,
    ) -> Self {
        let item = |a: &Annotation| ListItem {
            id: a.id.clone(),
            href: urls.read_mark(book_id, a.anchor.start_block),
            label: a.label(),
            note: a.note().unwrap_or("").to_owned(),
            text: a.quote.text.clone(),
            bookmark: a.is_bookmark(),
            lost: a.anchor_lost,
            start_block: a.anchor.start_block.to_string(),
            start_offset: a.anchor.start_offset,
            end_block: a.anchor.end_block.to_string(),
            end_offset: a.anchor.end_offset,
            prefix: a.quote.prefix.clone(),
            suffix: a.quote.suffix.clone(),
            note_action: urls.annotation_note(&a.id),
            delete_action: urls.annotation_delete(&a.id),
            progress: text
                .filter(|_| !a.anchor_lost)
                .and_then(|text| text.share_before(a.anchor.start_block, 10_000)),
        };
        Self {
            bookmarks: annotations
                .iter()
                .filter(|a| a.is_bookmark())
                .map(item)
                .collect(),
            marks: annotations
                .iter()
                .filter(|a| !a.is_bookmark())
                .map(item)
                .collect(),
            back,
        }
    }
}

/// The annotations and reading position of a book, for its card or reader page.
pub(crate) struct BookAnnotations {
    pub(crate) annotations: Vec<Annotation>,
    pub(crate) position: Option<ReadingPosition>,
}

/// Loads a book's annotations and position, re-attaching orphan groups first (see
/// [`attach`]). The book is only loaded (if `content` is `None`) when there are
/// candidate orphans. `None` if the book has no content key.
pub(crate) fn book_annotations(
    state: &AppState,
    profile: ProfileId,
    index: &Connection,
    user: &UserData,
    book: &BookDetails,
    content: Option<&BookContent>,
) -> Result<Option<BookAnnotations>, AppError> {
    let Some(snapshot) = attach::snapshot(book) else {
        return Ok(None);
    };
    if book.status == BookStatus::Ok {
        let candidates = attach::orphan_candidates(index, &user.lock(), &snapshot)?;
        if !candidates.is_empty() {
            let loaded;
            let content = match content {
                Some(content) => Some(content),
                None => match state.content(book) {
                    Ok(c) => {
                        loaded = c;
                        Some(&*loaded)
                    }
                    Err(e) => {
                        tracing::debug!(book_id = book.id, error = ?e, "not re-attaching annotations");
                        None
                    }
                },
            };
            // The book is loaded and searched without holding the user-data lock.
            if let Some(content) = content {
                let text = BookText::new(content);
                let mut user = user.lock();
                attach::reattach(
                    index,
                    &mut user,
                    state.exporter(),
                    &snapshot,
                    &text,
                    &candidates,
                )?;
            }
        }
    }
    let user = user.lock();
    Ok(Some(BookAnnotations {
        annotations: store::for_book(&user, profile, &snapshot.key)?,
        position: if state.config().reader.remember_position {
            store::position(&user, profile, &snapshot.key)?
        } else {
            None
        },
    }))
}

/// Rewrites a book's export file; a failure is logged, not returned (the database
/// write it follows has succeeded, and `shelfgauge annotations export` repairs it).
fn export(state: &AppState, index: &Connection, user: &Connection, key: &[u8]) {
    if let Err(e) = export_key(index, user, state.exporter(), key) {
        tracing::error!(error = %e, "cannot update the annotation export");
    }
}

/// A JSON error answer of the write routes.
#[derive(Debug)]
pub(crate) struct ApiError(StatusCode, String);

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: &'a str,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: &self.1 })).into_response()
    }
}

impl From<AppError> for ApiError {
    fn from(e: AppError) -> Self {
        match e {
            AppError::NotFound => Self(StatusCode::NOT_FOUND, "not found".to_owned()),
            AppError::FileMissing => Self(
                StatusCode::NOT_FOUND,
                "the book's file is missing: rescan the library".to_owned(),
            ),
            AppError::BadRequest(message) => Self(StatusCode::UNPROCESSABLE_ENTITY, message),
            AppError::Protected(scheme) => Self(
                StatusCode::FORBIDDEN,
                format!("the book is protected by DRM ({scheme})"),
            ),
            AppError::Forbidden => Self(
                StatusCode::FORBIDDEN,
                "this needs the admin role".to_owned(),
            ),
            AppError::Unreadable(reason) => {
                tracing::warn!(reason = ?reason, "cannot read a book");
                Self(StatusCode::INTERNAL_SERVER_ERROR, reason)
            }
            AppError::Internal(message) => {
                tracing::error!(error = ?message, "internal error");
                Self(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error (see the server log)".to_owned(),
                )
            }
        }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(e: JsonRejection) -> Self {
        Self(e.status(), e.body_text())
    }
}

/// The indexed, readable book `id` with its snapshot.
fn readable_book(index: &Connection, id: i64) -> Result<(BookDetails, BookSnapshot), AppError> {
    let book = book_by_id(index, id)?.ok_or(AppError::NotFound)?;
    if book.status == BookStatus::Removed {
        return Err(AppError::FileMissing);
    }
    let snapshot = attach::snapshot(&book)
        .ok_or_else(|| AppError::BadRequest("the book is not indexed".to_owned()))?;
    Ok((book, snapshot))
}

fn parse_block(text: &str) -> Result<BlockId, AppError> {
    text.parse()
        .map_err(|e: crate::format::model::ParseBlockIdError| AppError::BadRequest(e.to_string()))
}

/// Checks a note against `annotations.max_note_length`.
fn check_note(state: &AppState, note: Option<&str>) -> Result<(), AppError> {
    let max = state.config().annotations.max_note_length as usize;
    match note.map(char_len) {
        Some(len) if len > max => Err(AppError::BadRequest(format!(
            "the note is {len} characters long; the limit is {max}"
        ))),
        _ => Ok(()),
    }
}

/// The body of `POST /book/{id}/annotations`: a selection, or a bookmark on `block`.
#[derive(Debug, Deserialize)]
pub(crate) struct CreateRequest {
    /// A bookmark on this block (instead of a selection).
    block: Option<String>,
    start_block: Option<String>,
    start_offset: Option<u32>,
    end_block: Option<String>,
    end_offset: Option<u32>,
    note: Option<String>,
}

#[derive(Serialize)]
struct Created {
    id: String,
    bookmark: bool,
    label: String,
}

/// `POST /book/{id}/annotations` (JSON): creates a bookmark or a mark; `201` with its
/// id and label.
pub(crate) async fn create(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
    body: Result<Json<CreateRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let id = parse_id(&id)?;
    let Json(request) = body?;
    let profile = current.id();
    let created = state
        .user_db(move |state, index| {
            let (book, snapshot) = readable_book(index, id)?;
            let note = request.note.as_deref().filter(|n| !n.trim().is_empty());
            check_note(state, note)?;
            let (content, text) = state.book_text(&book)?;
            let bad = |e: AnchorError| AppError::BadRequest(e.to_string());
            let (anchor, quote) = match &request.block {
                Some(block) => {
                    let block = parse_block(block)?;
                    (
                        Anchor::at_block(block),
                        anchor::bookmark_quote(&text, block).map_err(bad)?,
                    )
                }
                None => {
                    let field = |name: &str| AppError::BadRequest(format!("`{name}` is missing"));
                    let anchor = Anchor {
                        start_block: parse_block(
                            request
                                .start_block
                                .as_deref()
                                .ok_or_else(|| field("start_block"))?,
                        )?,
                        start_offset: request.start_offset.ok_or_else(|| field("start_offset"))?,
                        end_block: parse_block(
                            request
                                .end_block
                                .as_deref()
                                .ok_or_else(|| field("end_block"))?,
                        )?,
                        end_offset: request.end_offset.ok_or_else(|| field("end_offset"))?,
                    };
                    let quote = anchor::quote(&text, &anchor).map_err(bad)?;
                    let config = state.config();
                    let parts = split(&content, &config.reader);
                    if part_of(&parts, anchor.start_block) != part_of(&parts, anchor.end_block) {
                        return Err(AppError::BadRequest(
                            "a selection must lie within one part of the book".to_owned(),
                        ));
                    }
                    let (len, max) = (
                        char_len(&quote.text),
                        config.annotations.max_text_length as usize,
                    );
                    if len > max {
                        return Err(AppError::BadRequest(format!(
                            "the selection is {len} characters long; the limit is {max}"
                        )));
                    }
                    (anchor, quote)
                }
            };
            let user = state.userdata().lock();
            let annotation = store::insert(
                &user,
                profile,
                &NewAnnotation {
                    section_title: text.section_title(anchor.start_block).map(str::to_owned),
                    book: snapshot,
                    anchor,
                    quote,
                    note: note.map(str::to_owned),
                },
                now(),
            )?;
            export(state, index, &user, &annotation.book_key);
            Ok(Created {
                bookmark: annotation.is_bookmark(),
                label: annotation.label(),
                id: annotation.id,
            })
        })
        .await?;
    Ok((StatusCode::CREATED, Json(created)).into_response())
}

/// The fields of the note and delete forms (and their JSON bodies).
#[derive(Debug, Default, Deserialize)]
pub(crate) struct EditRequest {
    note: Option<String>,
    /// The page to return to after a form post.
    back: Option<String>,
}

/// Whether the request body is JSON (the reader) rather than a form (the book card).
fn is_json(request: &Request) -> bool {
    request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"))
}

/// Reads a JSON or form body.
async fn edit_request(
    state: &AppState,
    request: Request,
) -> Result<(bool, EditRequest), Box<Response>> {
    if is_json(&request) {
        match Json::<EditRequest>::from_request(request, state).await {
            Ok(Json(body)) => Ok((true, body)),
            Err(e) => Err(Box::new(ApiError::from(e).into_response())),
        }
    } else {
        match Form::<EditRequest>::from_request(request, state).await {
            Ok(Form(body)) => Ok((false, body)),
            Err(e) => Err(Box::new(e.into_response())),
        }
    }
}

/// A local page to return to: a path under the base path, never another site.
fn back_url(urls: &Urls, back: Option<&str>, fallback: String) -> String {
    match back {
        Some(back)
            if back.starts_with(&urls.home())
                && !back.starts_with("//")
                && !back.contains(|c: char| c == '\\' || c.is_ascii_control()) =>
        {
            back.to_owned()
        }
        _ => fallback,
    }
}

fn parse_uuid(id: &str) -> Result<String, AppError> {
    uuid::Uuid::parse_str(id)
        .map(|u| u.to_string())
        .map_err(|_| AppError::NotFound)
}

/// What a note or delete request answers.
fn edited(state: &AppState, json: bool, back: Option<&str>, annotation: &Annotation) -> Response {
    if json {
        return Json(Created {
            id: annotation.id.clone(),
            bookmark: annotation.is_bookmark(),
            label: annotation.label(),
        })
        .into_response();
    }
    let urls = state.urls();
    let fallback = format!("{}#annotations", urls.book(annotation.book_id));
    Redirect::to(&back_url(urls, back, fallback)).into_response()
}

/// An error answer: JSON for the reader, the HTML error page for a form.
fn failed(json: bool, e: AppError) -> Response {
    if json {
        ApiError::from(e).into_response()
    } else {
        e.into_response()
    }
}

/// `POST /annotation/{id}/note`: replaces the note (a blank note removes it).
pub(crate) async fn note(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (json, body) = match edit_request(&state, request).await {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let note = body.note.clone();
    let result = async {
        let id = parse_uuid(&id)?;
        state
            .user_db(move |state, index| {
                check_note(state, note.as_deref())?;
                let user = state.userdata().lock();
                let annotation = store::set_note(&user, current.id(), &id, note.as_deref(), now())?
                    .ok_or(AppError::NotFound)?;
                export(state, index, &user, &annotation.book_key);
                Ok(annotation)
            })
            .await
    }
    .await;
    match result {
        Ok(annotation) => edited(&state, json, body.back.as_deref(), &annotation),
        Err(e) => failed(json, e),
    }
}

/// `POST /annotation/{id}/delete`: deletes an annotation (soft).
pub(crate) async fn delete(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (json, body) = match edit_request(&state, request).await {
        Ok(parsed) => parsed,
        Err(response) => return *response,
    };
    let result = async {
        let id = parse_uuid(&id)?;
        state
            .user_db(move |state, index| {
                let user = state.userdata().lock();
                let annotation =
                    store::delete(&user, current.id(), &id, now())?.ok_or(AppError::NotFound)?;
                export(state, index, &user, &annotation.book_key);
                Ok(annotation)
            })
            .await
    }
    .await;
    match result {
        Ok(annotation) => edited(&state, json, body.back.as_deref(), &annotation),
        Err(e) => failed(json, e),
    }
}

/// The body of `POST /book/{id}/position`.
#[derive(Debug, Deserialize)]
pub(crate) struct PositionRequest {
    /// The topmost visible block.
    block: String,
}

/// `POST /book/{id}/position` (JSON): saves the reading position; `204`. Positions in
/// notes are ignored, and nothing is saved with `reader.remember_position = false`.
pub(crate) async fn position(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
    body: Result<Json<PositionRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let id = parse_id(&id)?;
    let Json(request) = body?;
    if !state.config().reader.remember_position {
        return Ok(StatusCode::NO_CONTENT);
    }
    state
        .user_db(move |state, index| {
            let (book, snapshot) = readable_book(index, id)?;
            let block = parse_block(&request.block)?;
            let (_, text) = state.book_text(&book)?;
            let quote = anchor::bookmark_quote(&text, block)
                .map_err(|e| AppError::BadRequest(e.to_string()))?;
            let Some(progress) = text.progress(block) else {
                return Ok(()); // In the notes: not a reading position.
            };
            store::save_position(
                &state.userdata().lock(),
                current.id(),
                &ReadingPosition {
                    book: snapshot,
                    block,
                    suffix: quote.suffix,
                    progress,
                    updated_at: now(),
                },
            )?;
            Ok(())
        })
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /book/{id}/finished` (form): marks the book as finished, which takes it off
/// the "Currently reading" list; back to the library page.
pub(crate) async fn finished(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
) -> Result<Redirect, AppError> {
    let id = parse_id(&id)?;
    state
        .user_db(move |state, index| {
            let (_, snapshot) = readable_book(index, id)?;
            store::mark_finished(&state.userdata().lock(), current.id(), &snapshot.key, now())?;
            Ok(())
        })
        .await?;
    Ok(Redirect::to(&state.urls().home()))
}

/// `POST /reading/clear` (form): empties the "Currently reading" list (the positions
/// are kept); back to the library page (the settings page with `back=settings`).
pub(crate) async fn clear_reading(
    State(state): State<AppState>,
    current: CurrentProfile,
    form: Result<axum::Form<super::Back>, axum::extract::rejection::FormRejection>,
) -> Result<Redirect, AppError> {
    state
        .user_db(move |state, _| {
            store::dismiss_all(&state.userdata().lock(), current.id(), now())?;
            Ok(())
        })
        .await?;
    Ok(Redirect::to(&super::Back::url(form, state.urls())))
}

/// A book on the "Currently reading" list.
pub(crate) struct ReadingItem {
    pub(crate) title: String,
    pub(crate) authors: String,
    pub(crate) card_href: String,
    pub(crate) continue_href: String,
    pub(crate) finished_action: String,
    /// The cover URL, if the book has a cover that can be served.
    pub(crate) cover: Option<String>,
    /// "read 37 %".
    pub(crate) progress: String,
    /// The width class of the progress bar (see [`width_class`]).
    pub(crate) bar: String,
    pub(crate) date: String,
}

/// The "Currently reading" list: up to `reading_list_size` books with a live position
/// in a book that is in the library (by its content key), most recently read first.
pub(crate) fn reading_list(
    state: &AppState,
    profile: ProfileId,
    index: &Connection,
) -> Result<Vec<ReadingItem>, AppError> {
    let config = state.config();
    let limit = config.library.reading_list_size as usize;
    if limit == 0 || !config.reader.remember_position {
        return Ok(Vec::new());
    }
    let positions = store::reading_list(&state.userdata().lock(), profile)?;
    let urls = state.urls();
    let mut items = Vec::new();
    for position in positions {
        let Some(book) = crate::storage::books::live_books_by_key(index, &position.book.key)?
            .into_iter()
            .next()
            .and_then(|id| book_by_id(index, id).transpose())
            .transpose()?
        else {
            continue; // Removed, or replaced and not re-attached yet.
        };
        items.push(ReadingItem {
            title: book.title.clone(),
            authors: attach::authors(&book),
            card_href: urls.book(book.id),
            continue_href: urls.read_at(book.id, position.block),
            finished_action: urls.finished(book.id),
            cover: book
                .cover_mime
                .as_deref()
                .and_then(covers::known_mime)
                .map(|_| urls.cover(book.id, book.updated_at)),
            progress: progress_label(position.progress),
            bar: width_class(u64::from(position.progress)),
            date: super::date(position.updated_at),
        });
        if items.len() == limit {
            break;
        }
    }
    Ok(items)
}

/// The reader's bookmark and mark lists (an HTML fragment, also included in the read
/// page).
#[derive(Template)]
#[template(path = "partials/annotation_lists.html")]
pub(crate) struct ListsFragment {
    pub(crate) lists: Lists,
}

/// `GET /book/{id}/annotations`: the reader's lists, for refreshing after a change.
pub(crate) async fn lists(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let id = parse_id(&id)?;
    let (annotations, text) = state
        .user_db(move |state, index| {
            let (book, snapshot) = readable_book(index, id)?;
            let annotations =
                store::for_book(&state.userdata().lock(), current.id(), &snapshot.key)?;
            // Without the content (e.g. a missing file) the items carry no progress.
            let text = state.book_text(&book).ok().map(|(_, text)| text);
            Ok((annotations, text))
        })
        .await?;
    let urls = state.urls();
    let fragment = ListsFragment {
        lists: Lists::new(urls, id, &annotations, urls.read(id, 1), text.as_deref()),
    };
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Html(fragment.render()?),
    )
        .into_response())
}

/// Who else is reading a book: a profile that shares its activity.
pub(crate) struct FamilyReader {
    pub(crate) name: String,
    pub(crate) initials: String,
    pub(crate) color: u8,
    /// "read 37 %".
    pub(crate) progress: String,
}

/// A book on the "Family is reading" row.
pub(crate) struct FamilyItem {
    pub(crate) reader: FamilyReader,
    pub(crate) title: String,
    pub(crate) authors: String,
    pub(crate) card_href: String,
    pub(crate) cover: Option<String>,
    pub(crate) bar: String,
}

/// The "Currently reading" books of the other profiles that share their activity
/// (live books only, most recently read first; at most `reading_list_size`). Never
/// annotations.
pub(crate) fn family_reading(
    state: &AppState,
    profile: ProfileId,
    index: &Connection,
) -> Result<Vec<FamilyItem>, AppError> {
    let config = state.config();
    let limit = config.library.reading_list_size as usize;
    if limit == 0 || !config.reader.remember_position {
        return Ok(Vec::new());
    }
    let (shared, profiles) = {
        let user = state.userdata().lock();
        (
            store::shared_reading(&user, profile)?,
            crate::profiles::store::list(&user)?,
        )
    };
    let urls = state.urls();
    let mut items = Vec::new();
    for (reader, position) in shared {
        let Some(who) = profiles.iter().find(|p| p.id == reader) else {
            continue;
        };
        let Some(book) = crate::storage::books::live_books_by_key(index, &position.book.key)?
            .into_iter()
            .next()
            .and_then(|id| book_by_id(index, id).transpose())
            .transpose()?
        else {
            continue;
        };
        items.push(FamilyItem {
            reader: FamilyReader {
                name: who.name.clone(),
                initials: who.initials(),
                color: who.color,
                progress: progress_label(position.progress),
            },
            title: book.title.clone(),
            authors: attach::authors(&book),
            card_href: urls.book(book.id),
            cover: book
                .cover_mime
                .as_deref()
                .and_then(covers::known_mime)
                .map(|_| urls.cover(book.id, book.updated_at)),
            bar: width_class(u64::from(position.progress)),
        });
        if items.len() == limit {
            break;
        }
    }
    Ok(items)
}

/// The other sharing profiles reading the book with content key `key`.
pub(crate) fn also_reading(
    state: &AppState,
    profile: ProfileId,
    key: &[u8],
) -> Result<Vec<FamilyReader>, AppError> {
    if !state.config().reader.remember_position {
        return Ok(Vec::new());
    }
    let user = state.userdata().lock();
    let profiles = crate::profiles::store::list(&user)?;
    Ok(store::shared_reading(&user, profile)?
        .into_iter()
        .filter(|(_, position)| position.book.key == key)
        .filter_map(|(reader, position)| {
            let who = profiles.iter().find(|p| p.id == reader)?;
            Some(FamilyReader {
                name: who.name.clone(),
                initials: who.initials(),
                color: who.color,
                progress: progress_label(position.progress),
            })
        })
        .collect())
}

/// "read 37 %" or "finished" for a progress in per mille.
pub(crate) fn progress_label(progress: u16) -> String {
    if progress >= 1000 {
        "finished".to_owned()
    } else {
        format!("read {} %", progress / 10)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn back_urls_stay_local() {
        let urls = Urls::new("/books");
        let fallback = || "/books/book/1".to_owned();
        assert_eq!(
            back_url(&urls, Some("/books/book/1/read?part=2"), fallback()),
            "/books/book/1/read?part=2"
        );
        for bad in [
            "https://evil.example/",
            "//evil.example/books/",
            "/other/",
            "/books\\..",
            "/books/\r\nX: y",
            "/books/\t/evil.example",
        ] {
            assert_eq!(back_url(&urls, Some(bad), fallback()), fallback(), "{bad}");
        }
        assert_eq!(back_url(&urls, None, fallback()), fallback());
        for bad in ["//evil.example", "/\t/evil.example"] {
            assert_eq!(back_url(&Urls::new("/"), Some(bad), fallback()), fallback());
        }
    }

    #[test]
    fn progress_labels() {
        assert_eq!(progress_label(0), "read 0 %");
        assert_eq!(progress_label(379), "read 37 %");
        assert_eq!(progress_label(1000), "finished");
    }
}
