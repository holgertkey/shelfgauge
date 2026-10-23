//! The reader app shell: `GET /` (the shelf) and `GET /pair` serve the start page,
//! `GET /r/{id}` the reading view, and `GET /assets/{*path}` their files: the app's own
//! from `assets/hub/` and the library's reading files (`style.css`, `reader.css`,
//! `theme.js`, `app.js`, `reader.js`, `icon.svg`) from `assets/`.
//!
//! The pages are static (`templates/hub/`): the same for every well-formed
//! publication id and served without a database lookup, so they say nothing about
//! whether a publication exists. The app (`assets/hub/hub.js`) fills them in.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use rust_embed::Embed;

use super::{HubError, HubState};
use crate::annotations::lists::Lists;
use crate::annotations::results::FindResults;
use crate::publication::format::PublicationId;
use crate::web::assets::{content_type, hex_prefix};

#[derive(Embed)]
#[folder = "assets/hub/"]
struct HubAssets;

/// The library's files the reading view shares.
#[derive(Embed)]
#[folder = "assets/"]
#[include = "style.css"]
#[include = "reader.css"]
#[include = "theme.js"]
#[include = "app.js"]
#[include = "reader.js"]
#[include = "icon.svg"]
#[exclude = "hub/*"]
struct SharedAssets;

/// `/` and `/pair`.
#[derive(Template)]
#[template(path = "hub/home.html")]
struct HomePage {
    base: String,
}

/// `/r/{id}`.
#[derive(Template)]
#[template(path = "hub/read.html")]
struct ReadPage {
    base: String,
    /// The limits of a mark's text and note: the library's defaults.
    max_text: u32,
    max_note: u32,
    /// Empty lists (the app fills them in).
    lists: Lists,
    /// No search yet (the app shows results).
    find: FindPanel,
}

/// The Search panel of the reading view, empty.
struct FindPanel {
    results: Option<FindResults>,
}

/// The shell routes.
pub(super) fn routes(state: &HubState) -> Router<HubState> {
    let root = match state.path("") {
        base if base.is_empty() => "/".to_owned(),
        base => base,
    };
    Router::new()
        .route(&root, get(page))
        .route(&state.path("/r/{id}"), get(reader))
        .route(&state.path("/pair"), get(page))
        .route(&state.path("/assets/{*path}"), get(asset))
}

/// A page of the shell.
fn shell(page: &impl Template) -> Response {
    let text = page.render().expect("the shell pages render");
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))],
        Html(text),
    )
        .into_response()
}

/// `GET /pair` and `GET /`.
async fn page(State(state): State<HubState>) -> Response {
    shell(&HomePage {
        base: state.path(""),
    })
}

/// `GET /r/{id}`: the reading view for any well-formed publication id.
async fn reader(
    State(state): State<HubState>,
    Path(id): Path<String>,
) -> Result<Response, HubError> {
    id.parse::<PublicationId>()
        .map_err(|_| HubError::NotFound)?;
    let limits = crate::config::AnnotationsConfig::default();
    Ok(shell(&ReadPage {
        base: state.path(""),
        max_text: limits.max_text_length,
        max_note: limits.max_note_length,
        lists: Lists::default(),
        find: FindPanel { results: None },
    }))
}

/// `GET /assets/{*path}`: a file of the shell, revalidated by its `ETag`.
async fn asset(Path(name): Path<String>, headers: HeaderMap) -> Result<Response, HubError> {
    let file = HubAssets::get(&name)
        .or_else(|| SharedAssets::get(&name))
        .ok_or(HubError::NotFound)?;
    let etag = format!("\"{}\"", hex_prefix(&file.metadata.sha256_hash()));
    let etag_value = HeaderValue::from_str(&etag).expect("hex is a valid header");
    let cache = (header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|tag| tag.trim() == etag));
    if matches {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag_value), cache],
        )
            .into_response());
    }
    Ok((
        [
            (header::ETAG, etag_value),
            cache,
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(&name)),
            ),
        ],
        file.data,
    )
        .into_response())
}
