//! Search within the book: the results list of the reader's Search tab.
//!
//! The results come as a fragment for the live search (`GET /book/{id}/find?q=`) and
//! inside the read page when it has a `find` parameter (the form's target without
//! scripts, and every result link). Queries are never logged.

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Path, Query, State};
use axum::http::header;
use axum::response::{Html, IntoResponse, Response};

use super::{Params, parse_id};
use crate::annotations::text::BookText;
use crate::format::run_with_parser_stack;
use crate::render::split::{Part, split};
use crate::web::{AppError, AppState, Urls};

pub(crate) use crate::annotations::results::FindResults;

/// The results for the raw `query` in book `id`; `current` is the number of the
/// result followed to the page.
pub(crate) fn results(
    urls: &Urls,
    id: i64,
    parts: &[Part],
    text: &BookText,
    query: &str,
    current: Option<usize>,
) -> FindResults {
    FindResults::new(parts, text, query, current, &|part, query, index| {
        urls.read_find(id, part, query, Some(index))
    })
}

/// The live search's answer.
#[derive(Template)]
#[template(path = "partials/find_results.html")]
struct FindFragment {
    results: FindResults,
}

/// `GET /book/{id}/find?q=<query>`: the results as an HTML fragment for the Search
/// tab.
pub(crate) async fn find(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Response, AppError> {
    let id = parse_id(&id)?;
    let params = Params::from(query);
    let book = super::read::book(&state, id).await?;
    let s = state.clone();
    let results = crate::web::blocking(move || {
        run_with_parser_stack(|| {
            let (content, text) = s.book_text(&book)?;
            let parts = split(&content, &s.config().reader);
            Ok::<_, AppError>(results(s.urls(), id, &parts, &text, &params.q, None))
        })
    })
    .await??;
    let fragment = FindFragment { results };
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Html(fragment.render()?),
    )
        .into_response())
}
