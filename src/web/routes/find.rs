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

use super::figures::group_digits;
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
    /// Shown instead of results (a query too short or too long, no matches).
    pub(crate) message: Option<String>,
    /// After the results when not all are listed: "The first 500 of 1 234 matches…".
    pub(crate) more: Option<String>,
    /// The number of matches in the book (0 with a message).
    pub(crate) total: usize,
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
            more: None,
            total: 0,
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
        if found.total == 0 {
            return message("No matches.".into());
        }
        let shown = found.hits.len();
        let more = (shown < found.total).then(|| {
            format!(
                "The first {} of {} matches are listed; refine the query to find the rest.",
                group_digits(shown as u64),
                group_digits(found.total as u64)
            )
        });
        Self {
            message: None,
            more,
            total: found.total,
            groups: groups.into_iter().map(|(_, group)| group).collect(),
        }
    }
}

impl FindResults {
    /// The tab's count: "23", "999+" for more than 999, empty without matches.
    pub(crate) fn tab_count(&self) -> String {
        match self.total {
            0 => String::new(),
            n if n > 999 => "999+".to_owned(),
            n => n.to_string(),
        }
    }

    /// The tab's tooltip: "Search results: 23 matches".
    pub(crate) fn tab_title(&self) -> String {
        match self.total {
            0 => "Search results".to_owned(),
            1 => "Search results: 1 match".to_owned(),
            n => format!("Search results: {} matches", group_digits(n as u64)),
        }
    }
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
    use super::FindResults;

    #[test]
    fn tab_counts() {
        let results = |total| FindResults {
            message: None,
            more: None,
            total,
            groups: Vec::new(),
        };
        assert_eq!(results(0).tab_count(), "");
        assert_eq!(results(23).tab_count(), "23");
        assert_eq!(results(999).tab_count(), "999");
        assert_eq!(results(1000).tab_count(), "999+");
        assert_eq!(results(1).tab_title(), "Search results: 1 match");
        assert_eq!(
            results(47_356).tab_title(),
            "Search results: 47\u{202f}356 matches"
        );
    }
}
