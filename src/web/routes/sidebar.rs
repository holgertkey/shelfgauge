//! Sidebar fragments: one level of the tree, fetched by the page's script.

use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::response::Html;

use super::{Params, parse_id};
use crate::storage::authors::author_by_id;
use crate::web::sidebar;
use crate::web::{AppError, AppState};

/// `GET /sidebar/authors?q=&page=`: the author list.
pub(crate) async fn authors(
    State(state): State<AppState>,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let params = Params::from(query);
    let s = state.clone();
    let html = state
        .db(move |conn| sidebar::authors(conn, &s, params.q.trim(), params.page(), None))
        .await?;
    Ok(Html(html))
}

/// `GET /sidebar/author/{id}`: an author's books.
pub(crate) async fn author(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Html<String>, AppError> {
    let id = parse_id(&id)?;
    let s = state.clone();
    let html = state
        .db(move |conn| {
            author_by_id(conn, id)?.ok_or(AppError::NotFound)?;
            sidebar::author_books(conn, &s, id, None)
        })
        .await?;
    Ok(Html(html))
}

/// `GET /sidebar/folder?path=` or `?zip=&in=` (and `page=`): a folder's contents.
pub(crate) async fn folder(
    State(state): State<AppState>,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let params = Params::from(query);
    let s = state.clone();
    let html = state
        .db(move |conn| sidebar::folder(conn, &s, &params.folder(), params.page(), None, None))
        .await?;
    Ok(Html(html))
}
