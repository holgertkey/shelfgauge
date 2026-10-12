//! Full-text search.

use askama::Template;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::Html;

use super::{BookRow, Params, Row, render};
use crate::storage::search::search as search_books;
use crate::web::layout::{Layout, PageLink, Pager, SidebarMode};
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState};

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    layout: Layout,
    rows: Vec<Row>,
    empty: &'static str,
    pager: Option<Pager>,
}

/// `GET /search?q=&page=`: books matching the query, best first.
///
/// The total is not counted: one extra result tells whether there is a next page.
pub(crate) async fn search(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<Params>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let params = Params::from(query);
    let text = params.q.trim().to_owned();
    let page = params.page();
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let (books, has_next, sidebar) = {
        let text = text.clone();
        state
            .db(move |conn| {
                let limit = s.page_size();
                let offset = (page - 1).saturating_mul(u64::from(limit));
                let mut books = if text.is_empty() {
                    Vec::new()
                } else {
                    search_books(conn, &text, limit.saturating_add(1), offset)?
                };
                let has_next = books.len() > limit as usize;
                books.truncate(limit as usize);
                Ok((
                    books,
                    has_next,
                    sidebar::build(conn, &s, mode, &Focus::None)?,
                ))
            })
            .await?
    };
    let urls = state.urls();
    let link = |n: u64| PageLink {
        href: urls.search(&text, n),
        src: String::new(),
    };
    let pager = (page > 1 || has_next).then(|| Pager {
        prev: (page > 1).then(|| link(page - 1)),
        next: has_next.then(|| link(page.saturating_add(1))),
        label: format!("Page {page}"),
    });
    let title = if text.is_empty() {
        "Search".to_owned()
    } else {
        format!("Search: {text}")
    };
    render(&SearchPage {
        layout: Layout::new(&state, title, sidebar).with_query(&text),
        rows: books
            .into_iter()
            .map(|book| Row::Book(BookRow::new(urls, book)))
            .collect(),
        empty: if page > 1 {
            "No more results."
        } else {
            "Nothing found."
        },
        pager,
    })
}
