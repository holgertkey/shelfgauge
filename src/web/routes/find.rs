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
use crate::annotations::find::{
    self, MAX_HITS, MAX_QUERY_CHARS, MIN_QUERY_CHARS, QueryError, SNIPPET_CONTEXT, Snippet,
};
use crate::annotations::text::BookText;
use crate::format::run_with_parser_stack;
use crate::render::split::{Part, part_of, split};
use crate::web::{AppError, AppState, Urls};

/// A result: a snippet with a link to the hit's part and block.
pub(crate) struct FindHit {
    /// The 1-based number of the result.
    pub(crate) index: usize,
    /// Whether this is the result followed to the page.
    pub(crate) current: bool,
    pub(crate) href: String,
    /// The 1-based part.
    pub(crate) part: usize,
    /// The block id, e.g. `b12`.
    pub(crate) block: String,
    /// Code points into the block's rendered text.
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) snippet: Snippet,
}

/// The results in one section; untitled main-text sections have no title.
pub(crate) struct FindGroup {
    pub(crate) title: String,
    pub(crate) hits: Vec<FindHit>,
}

/// The Search tab's results.
pub(crate) struct FindResults {
    /// Shown instead of results (a query too short or too long).
    pub(crate) message: Option<String>,
    /// The number of matches in the book.
    pub(crate) total: usize,
    /// The query as typed (trimmed).
    pub(crate) query: String,
    /// The library search for the query (titles, authors, series, descriptions).
    pub(crate) library_href: String,
    /// "3 matches", "500 of 1 234 shown — refine the query".
    pub(crate) count: String,
    pub(crate) groups: Vec<FindGroup>,
}

impl FindResults {
    /// The results for the raw `query` in book `id`; `current` is the number of the
    /// result followed to the page.
    pub(crate) fn new(
        urls: &Urls,
        id: i64,
        parts: &[Part],
        text: &BookText,
        query: &str,
        current: Option<usize>,
    ) -> Self {
        let message = |message: String| Self {
            message: Some(message),
            total: 0,
            query: String::new(),
            library_href: String::new(),
            count: String::new(),
            groups: Vec::new(),
        };
        let parsed = match find::Query::parse(query) {
            Ok(parsed) => parsed,
            Err(QueryError::Empty) => {
                return message("Type a word or a phrase to find it in this book.".into());
            }
            Err(QueryError::TooShort) => {
                return message(format!("Type at least {MIN_QUERY_CHARS} characters."));
            }
            Err(QueryError::TooLong) => {
                return message(format!(
                    "The query is too long: at most {MAX_QUERY_CHARS} characters."
                ));
            }
        };
        let found = find::find(text, &parsed, MAX_HITS);
        let query = query.trim();
        // Notes are one group; main-text hits group by their (innermost) section.
        let mut groups: Vec<((bool, Option<usize>), FindGroup)> = Vec::new();
        for (i, hit) in found.hits.iter().enumerate() {
            let Some(part) = part_of(parts, hit.block) else {
                continue;
            };
            let notes = text.in_notes(hit.block);
            let key = (
                notes,
                if notes {
                    None
                } else {
                    text.section_index(hit.block)
                },
            );
            if groups.last().is_none_or(|(last, _)| *last != key) {
                let title = if notes {
                    "Notes".to_owned()
                } else {
                    text.section_title(hit.block).unwrap_or_default().to_owned()
                };
                groups.push((
                    key,
                    FindGroup {
                        title,
                        hits: Vec::new(),
                    },
                ));
            }
            let index = i + 1;
            let block_text = text.text(hit.block).unwrap_or_default();
            groups
                .last_mut()
                .expect("just pushed")
                .1
                .hits
                .push(FindHit {
                    index,
                    current: current == Some(index),
                    href: format!(
                        "{}#{}",
                        urls.read_find(id, part + 1, query, Some(index)),
                        hit.block
                    ),
                    part: part + 1,
                    block: hit.block.to_string(),
                    start: hit.start,
                    end: hit.end,
                    snippet: Snippet::new(block_text, hit.start, hit.end, SNIPPET_CONTEXT),
                });
        }
        let count = match (found.total, found.hits.len()) {
            (0, _) => "No matches.".to_owned(),
            (1, _) => "1 match".to_owned(),
            (total, shown) if shown < total => format!(
                "{} of {} shown — refine the query",
                group_digits(shown),
                group_digits(total)
            ),
            (total, _) => format!("{} matches", group_digits(total)),
        };
        Self {
            message: None,
            total: found.total,
            library_href: urls.search(query, 1),
            query: query.to_owned(),
            count,
            groups: groups.into_iter().map(|(_, group)| group).collect(),
        }
    }
}

/// `n` with its digits in groups of three, separated by a narrow no-break space.
fn group_digits(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push('\u{202f}');
        }
        out.push(c);
    }
    out
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
    let results = tokio::task::spawn_blocking(move || {
        run_with_parser_stack(|| {
            let (content, text) = s.book_text(&book)?;
            let parts = split(&content, s.reader());
            Ok::<_, AppError>(FindResults::new(
                s.urls(),
                id,
                &parts,
                &text,
                &params.q,
                None,
            ))
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

#[cfg(test)]
mod tests {
    use super::group_digits;

    #[test]
    fn digits_are_grouped() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(500), "500");
        assert_eq!(group_digits(1234), "1\u{202f}234");
        assert_eq!(group_digits(1_234_567), "1\u{202f}234\u{202f}567");
    }
}
