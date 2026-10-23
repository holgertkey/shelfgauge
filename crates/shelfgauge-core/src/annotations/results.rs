//! The reader's search results: the hits of a [`find`] grouped by
//! section, with snippets and links.
//!
//! The instance and the hub's reader app render them with the same template
//! (`templates/partials/find_results.html` in the repository).

use super::find::{
    self, MAX_HITS, MAX_QUERY_CHARS, MIN_QUERY_CHARS, QueryError, SNIPPET_CONTEXT, Snippet,
};
use super::text::BookText;
use crate::render::outline::group_digits;
use crate::render::split::{Part, part_of};

/// A result: a snippet with a link to the hit's part and block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindHit {
    /// The 1-based number of the result.
    pub index: usize,
    /// Whether this is the result followed to the page.
    pub current: bool,
    /// The link to the hit.
    pub href: String,
    /// The 1-based part.
    pub part: usize,
    /// The block id, e.g. `b12`.
    pub block: String,
    /// Code points into the block's rendered text: the start of the match.
    pub start: usize,
    /// Code points into the block's rendered text: the end of the match.
    pub end: usize,
    /// The match in its context.
    pub snippet: Snippet,
}

/// The results in one section; untitled main-text sections have no title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindGroup {
    /// The section title ("Notes" for the notes).
    pub title: String,
    /// The hits in the section.
    pub hits: Vec<FindHit>,
}

/// The Search tab's results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindResults {
    /// Shown instead of results (a query too short or too long, no matches).
    pub message: Option<String>,
    /// After the results when not all are listed: "The first 500 of 1 234 matches…".
    pub more: Option<String>,
    /// The number of matches in the book (0 with a message).
    pub total: usize,
    /// The hits by section.
    pub groups: Vec<FindGroup>,
}

impl FindResults {
    /// The results for the raw `query` in a book split into `parts`; `current` is the
    /// number of the result followed to the page. `href(part, query, index)` is the
    /// page of hit `index` (1-based part, trimmed query); the block follows as the
    /// fragment.
    pub fn new(
        parts: &[Part],
        text: &BookText,
        query: &str,
        current: Option<usize>,
        href: &dyn Fn(usize, &str, usize) -> String,
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
                    href: format!("{}#{}", href(part + 1, query, index), hit.block),
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

    /// The tab's count: "23", "999+" for more than 999, empty without matches.
    pub fn tab_count(&self) -> String {
        match self.total {
            0 => String::new(),
            n if n > 999 => "999+".to_owned(),
            n => n.to_string(),
        }
    }

    /// The tab's tooltip: "Search results: 23 matches".
    pub fn tab_title(&self) -> String {
        match self.total {
            0 => "Search results".to_owned(),
            1 => "Search results: 1 match".to_owned(),
            n => format!("Search results: {} matches", group_digits(n as u64)),
        }
    }
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
