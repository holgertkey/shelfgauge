//! The reader app shell: `GET /r/{id}`, `GET /pair` and `GET /` serve the same static
//! page, and `GET /assets/{*path}` its files, embedded from `assets/hub/`.
//!
//! The page is the same for every well-formed publication id and is served without a
//! database lookup, so it says nothing about whether a publication exists. Until the
//! reader app is built it is a placeholder.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use rust_embed::Embed;

use super::{HubError, HubState};
use crate::publication::format::PublicationId;
use crate::web::assets::{content_type, hex_prefix};

#[derive(Embed)]
#[folder = "assets/hub/"]
struct HubAssets;

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

/// The shell page with the base path filled in.
fn shell(state: &HubState) -> Response {
    let file = HubAssets::get("shell.html").expect("the shell page is embedded");
    let text = String::from_utf8_lossy(&file.data).replace("%BASE%", &state.path(""));
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))],
        Html(text),
    )
        .into_response()
}

/// `GET /pair` and `GET /`.
async fn page(State(state): State<HubState>) -> Response {
    shell(&state)
}

/// `GET /r/{id}`: the shell for any well-formed publication id.
async fn reader(
    State(state): State<HubState>,
    Path(id): Path<String>,
) -> Result<Response, HubError> {
    id.parse::<PublicationId>()
        .map_err(|_| HubError::NotFound)?;
    Ok(shell(&state))
}

/// `GET /assets/{*path}`: a file of the shell, revalidated by its `ETag`.
async fn asset(Path(name): Path<String>, headers: HeaderMap) -> Result<Response, HubError> {
    let file = HubAssets::get(&name).ok_or(HubError::NotFound)?;
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
