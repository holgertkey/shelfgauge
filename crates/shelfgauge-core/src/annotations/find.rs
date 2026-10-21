//! Search within one book: a linear pass over the rendered text of its leaf blocks.
//!
//! There is no index. Both the query and every block's text are *normalized* before
//! they are compared: one code point at a time, each is lowercased (`char::to_lowercase`,
//! without the context rules of `str::to_lowercase`), `ё` becomes `е` and `ς` becomes
//! `σ`, the typographic apostrophes `’` and `ʼ` become `'`, soft hyphens and zero-width
//! characters are dropped, and every run of whitespace (NBSP included) becomes one
//! space. A match is a plain substring of the normalized text, found left to right
//! without overlaps; it never spans two blocks.
//!
//! The normalized text keeps a map back to the original code points, so a hit is a
//! range of the block's rendered text (the offsets annotations use, see
//! [`text`](super::text)). `assets/reader.js` repeats the rules to highlight matches in
//! the page; `tests/fixtures/find/normalize.json` (in the repository root) is the table
//! both are tested with.

use super::text::BookText;
use crate::format::model::BlockId;

/// The shortest query, in normalized code points.
pub const MIN_QUERY_CHARS: usize = 2;

/// The longest query, in normalized code points.
pub const MAX_QUERY_CHARS: usize = 100;

/// The most hits a search lists; more are only counted.
pub const MAX_HITS: usize = 500;

/// Raw queries longer than this (in code points) are rejected without normalizing.
const MAX_RAW_QUERY_CHARS: usize = 1000;

/// Code points of context on either side of a hit in a [`Snippet`].
pub const SNIPPET_CONTEXT: usize = 40;

/// Characters that normalization drops: the soft hyphen and zero-width characters.
fn dropped(c: char) -> bool {
    matches!(
        c,
        '\u{ad}' | '\u{200b}'..='\u{200d}' | '\u{2060}' | '\u{feff}'
    )
}

/// Whitespace for normalization. An explicit list, the same as in `reader.js` (the
/// Unicode `White_Space` property without U+0085, which JavaScript's `\s` lacks).
fn is_space(c: char) -> bool {
    matches!(
        c,
        '\t'..='\r'
            | ' '
            | '\u{a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

/// Appends the normalized form of `text` to `out`; with `map`, also the original
/// code point range of every normalized code point, by its byte offset in `out`.
fn normalize_into(text: &str, out: &mut String, mut map: Option<&mut Vec<Span>>) {
    fn push(out: &mut String, map: &mut Option<&mut Vec<Span>>, c: char, i: usize) {
        if let Some(map) = map {
            map.push(Span {
                byte: out.len(),
                start: i,
                end: i + 1,
            });
        }
        out.push(c);
    }
    // Whether the last pushed character is a space from whitespace.
    let mut in_space = false;
    for (i, c) in text.chars().enumerate() {
        if dropped(c) {
            continue;
        }
        if is_space(c) {
            if in_space {
                // The run's space covers this character too.
                if let Some(span) = map.as_deref_mut().and_then(|m| m.last_mut()) {
                    span.end = i + 1;
                }
            } else {
                push(out, &mut map, ' ', i);
                in_space = true;
            }
            continue;
        }
        in_space = false;
        match c {
            '’' | 'ʼ' => push(out, &mut map, '\'', i),
            _ if c.is_ascii() => push(out, &mut map, c.to_ascii_lowercase(), i),
            _ => {
                for lower in c.to_lowercase() {
                    let lower = match lower {
                        'ё' => 'е',
                        'ς' => 'σ',
                        other => other,
                    };
                    push(out, &mut map, lower, i);
                }
            }
        }
    }
}

/// The normalized form of `text` (see the [module docs](self)).
///
/// # Examples
///
/// ```
/// use shelfgauge_core::annotations::find::normalize;
///
/// assert_eq!(normalize("Ёлка\u{a0} и\u{ad}ли"), "елка или");
/// ```
pub fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    normalize_into(text, &mut out, None);
    out
}

/// Where a normalized code point came from.
#[derive(Debug, Clone, Copy)]
struct Span {
    /// Its byte offset in the normalized text.
    byte: usize,
    /// The original code points it stands for.
    start: usize,
    end: usize,
}

/// A normalized text with the map back to the original.
struct Mapped {
    text: String,
    spans: Vec<Span>,
}

impl Mapped {
    fn new(text: &str) -> Self {
        let mut out = String::with_capacity(text.len());
        let mut spans = Vec::new();
        normalize_into(text, &mut out, Some(&mut spans));
        Self { text: out, spans }
    }

    /// The original code point range of the normalized bytes `from..to` (a match).
    fn original(&self, from: usize, to: usize) -> (usize, usize) {
        let first = self.spans.partition_point(|s| s.byte < from);
        let last = self.spans.partition_point(|s| s.byte < to) - 1;
        (self.spans[first].start, self.spans[last].end)
    }

    /// The original code point ranges of every match of `query`.
    fn matches(&self, query: &str) -> impl Iterator<Item = (usize, usize)> {
        self.text
            .match_indices(query)
            .map(|(at, m)| self.original(at, at + m.len()))
    }
}

/// Why a query is not searched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryError {
    /// Nothing to search for.
    Empty,
    /// Shorter than [`MIN_QUERY_CHARS`] after normalization.
    TooShort,
    /// Longer than [`MAX_QUERY_CHARS`] after normalization.
    TooLong,
}

/// A normalized, trimmed query of [`MIN_QUERY_CHARS`]..=[`MAX_QUERY_CHARS`] code points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query(String);

impl Query {
    /// Normalizes and checks `raw`.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge_core::annotations::find::{Query, QueryError};
    ///
    /// assert_eq!(Query::parse("  Ёлка ").unwrap().as_str(), "елка");
    /// assert_eq!(Query::parse(" a "), Err(QueryError::TooShort));
    /// assert_eq!(Query::parse(""), Err(QueryError::Empty));
    /// ```
    pub fn parse(raw: &str) -> Result<Self, QueryError> {
        if raw.chars().nth(MAX_RAW_QUERY_CHARS).is_some() {
            return Err(QueryError::TooLong);
        }
        let normalized = normalize(raw);
        let query = normalized.trim_matches(' ');
        match query.chars().count() {
            0 => Err(QueryError::Empty),
            n if n < MIN_QUERY_CHARS => Err(QueryError::TooShort),
            n if n > MAX_QUERY_CHARS => Err(QueryError::TooLong),
            _ => Ok(Self(query.to_owned())),
        }
    }

    /// The normalized query.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A match in a book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// The leaf block it is in.
    pub block: BlockId,
    /// Its first code point in the block's rendered text.
    pub start: usize,
    /// The code point after its last one.
    pub end: usize,
}

/// The result of a search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Hits {
    /// The number of matches in the book.
    pub total: usize,
    /// The first matches, at most the limit, in book order.
    pub hits: Vec<Hit>,
}

/// The normalized text of a whole book: the normalized texts of its leaf blocks
/// joined with `\n`, which normalization never produces (a match cannot span two
/// blocks). Built on the first search and kept in the [`BookText`].
#[derive(Debug, Clone, Default)]
pub(crate) struct BookIndex {
    text: String,
    /// The byte offset of each leaf's text, in the order of [`BookText::leaves`].
    starts: Vec<usize>,
}

impl BookIndex {
    pub(crate) fn new(text: &BookText) -> Self {
        let mut index = Self::default();
        for &block in text.leaves() {
            if !index.starts.is_empty() {
                index.text.push('\n');
            }
            index.starts.push(index.text.len());
            normalize_into(text.text(block).unwrap_or_default(), &mut index.text, None);
        }
        index
    }
}

/// Every match of `query` in the leaf blocks of `text`, in book order: the first
/// `limit` as [`Hit`]s, all of them counted.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::annotations::find::{Query, find};
/// use shelfgauge_core::annotations::text::BookText;
/// use shelfgauge_core::format::model::{Block, Body, BookContent, Section};
///
/// let section = Section {
///     blocks: vec![Block::paragraph("Ёлка, ёлочка"), Block::paragraph("Ель")],
///     ..Section::default()
/// };
/// let content = BookContent::new(vec![Body::main(section)], Default::default());
/// let hits = find(&BookText::new(&content), &Query::parse("ел").unwrap(), 2);
/// assert_eq!(hits.total, 3);
/// assert_eq!((hits.hits[1].start, hits.hits[1].end), (6, 8));
/// ```
// Never record the query: it is what the user searched for.
#[tracing::instrument(level = "debug", skip_all)]
pub fn find(text: &BookText, query: &Query, limit: usize) -> Hits {
    let index = text.find_index();
    let leaves = text.leaves();
    let mut result = Hits::default();
    // The last leaf with listed hits: its hits are all taken from its own map.
    let mut done: Option<usize> = None;
    for (at, _) in index.text.match_indices(query.as_str()) {
        result.total += 1;
        if result.hits.len() >= limit {
            continue;
        }
        let leaf = index.starts.partition_point(|&start| start <= at) - 1;
        if done == Some(leaf) {
            continue;
        }
        done = Some(leaf);
        let block = leaves[leaf];
        let block_text = text.text(block).unwrap_or_default();
        let mapped = Mapped::new(block_text);
        result.hits.extend(
            mapped
                .matches(query.as_str())
                .map(|(start, end)| Hit { block, start, end })
                .take(limit - result.hits.len()),
        );
    }
    result
}

/// A hit with some of the text around it, for a result list.
///
/// Whitespace runs are shown as one space. The context is cut at word boundaries
/// (unless a single word is longer than the context).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snippet {
    /// Whether text before [`before`](Self::before) was left out.
    pub cut_before: bool,
    /// The text before the match.
    pub before: String,
    /// The match.
    pub matched: String,
    /// The text after the match.
    pub after: String,
    /// Whether text after [`after`](Self::after) was left out.
    pub cut_after: bool,
}

impl Snippet {
    /// The snippet of code points `start..end` of `text` with up to `context` code
    /// points on either side.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge_core::annotations::find::Snippet;
    ///
    /// let s = Snippet::new("one two three four five", 8, 13, 6);
    /// assert_eq!((s.before.as_str(), s.matched.as_str(), s.after.as_str()), ("two ", "three", " four"));
    /// assert!(s.cut_before && s.cut_after);
    /// ```
    pub fn new(text: &str, start: usize, end: usize, context: usize) -> Self {
        let chars: Vec<char> = text.chars().collect();
        let end = end.min(chars.len());
        let start = start.min(end);
        let mut from = start.saturating_sub(context);
        let mut to = (end + context).min(chars.len());
        // Mid-word at a cut: drop the partial word if a word boundary is in reach.
        if from > 0
            && !is_space(chars[from - 1])
            && let Some(space) = chars[from..start].iter().position(|&c| is_space(c))
        {
            from += space + 1;
        }
        if to < chars.len()
            && !is_space(chars[to])
            && let Some(space) = chars[end..to].iter().rposition(|&c| is_space(c))
        {
            to = end + space;
        }
        let piece = |a: usize, b: usize| collapse_spaces(&chars[a..b]);
        let mut before = piece(from, start);
        let mut after = piece(end, to);
        if from > 0 {
            before = before.trim_start().to_owned();
        }
        if to < chars.len() {
            after = after.trim_end().to_owned();
        }
        Self {
            cut_before: from > 0,
            before,
            matched: piece(start, end),
            after,
            cut_after: to < chars.len(),
        }
    }
}

/// `chars` as a string with every whitespace run as one space.
fn collapse_spaces(chars: &[char]) -> String {
    let mut out = String::with_capacity(chars.len());
    let mut space = false;
    for &c in chars {
        if is_space(c) {
            if !space {
                out.push(' ');
            }
            space = true;
        } else {
            out.push(c);
            space = false;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::model::{Block, Body, BookContent, Section};

    #[derive(serde::Deserialize)]
    struct Table {
        normalize: Vec<(String, String)>,
        query: Vec<(String, Option<String>)>,
        #[serde(rename = "match")]
        matches: Vec<MatchCase>,
    }

    #[derive(serde::Deserialize)]
    struct MatchCase {
        text: String,
        query: String,
        hits: Vec<(usize, usize)>,
    }

    // The table is shared with `assets/reader.js`, so it stays in the repository root.
    fn table() -> Table {
        serde_json::from_str(include_str!(
            "../../../../tests/fixtures/find/normalize.json"
        ))
        .unwrap()
    }

    #[test]
    fn shared_table() {
        let table = table();
        for (input, expected) in &table.normalize {
            assert_eq!(&normalize(input), expected, "{input:?}");
        }
        for (raw, expected) in &table.query {
            let parsed = Query::parse(raw).ok().map(|q| q.as_str().to_owned());
            assert_eq!(&parsed, expected, "{raw:?}");
        }
        for case in &table.matches {
            let query = Query::parse(&case.query).unwrap();
            let hits: Vec<_> = Mapped::new(&case.text).matches(query.as_str()).collect();
            assert_eq!(hits, case.hits, "{:?} in {:?}", case.query, case.text);
        }
    }

    #[test]
    fn query_lengths() {
        assert_eq!(Query::parse("   \u{ad} "), Err(QueryError::Empty));
        assert_eq!(Query::parse("ab").unwrap().as_str(), "ab");
        assert!(Query::parse(&"я".repeat(MAX_QUERY_CHARS)).is_ok());
        assert_eq!(
            Query::parse(&"я".repeat(MAX_QUERY_CHARS + 1)),
            Err(QueryError::TooLong)
        );
        // Whitespace runs count as one space.
        let spaced = format!("a{}b", " ".repeat(MAX_QUERY_CHARS * 2));
        assert_eq!(Query::parse(&spaced).unwrap().as_str(), "a b");
        assert_eq!(
            Query::parse(&" ".repeat(MAX_RAW_QUERY_CHARS + 1)),
            Err(QueryError::TooLong)
        );
    }

    #[test]
    fn a_lowercase_that_changes_the_length_maps_back() {
        // "İ" lowercases to two code points; both map to the one original.
        let mapped = Mapped::new("xİy İ");
        assert_eq!(mapped.text, "xi\u{307}y i\u{307}");
        let hits: Vec<_> = mapped.matches("i\u{307}").collect();
        assert_eq!(hits, [(1, 2), (4, 5)]);
        let hits: Vec<_> = mapped.matches("\u{307}y").collect();
        assert_eq!(hits, [(1, 3)], "a match from the middle of an expansion");
    }

    fn b(n: u32) -> BlockId {
        BlockId::new(n).unwrap()
    }

    fn book() -> BookContent {
        let main = Section {
            children: vec![Section {
                title: vec![Block::paragraph("Chapter one")],
                blocks: vec![
                    Block::paragraph("The end of a line"),
                    Block::paragraph("one more: one, one"),
                ],
                ..Section::default()
            }],
            ..Section::default()
        };
        let notes = Section {
            children: vec![Section {
                title: vec![Block::paragraph("1")],
                blocks: vec![Block::paragraph("A note about one")],
                ..Section::default()
            }],
            ..Section::default()
        };
        BookContent::new(
            vec![Body::main(main), Body::notes(notes)],
            Default::default(),
        )
    }

    #[test]
    fn titles_notes_limit_and_total() {
        let text = BookText::new(&book());
        // b1 "Chapter one", b2 "The end of a line", b3 "one more...", b4 "1", b5 note.
        let query = Query::parse("one").unwrap();
        let all = find(&text, &query, MAX_HITS);
        assert_eq!(all.total, 5);
        let blocks: Vec<_> = all.hits.iter().map(|h| (h.block, h.start)).collect();
        assert_eq!(
            blocks,
            [(b(1), 8), (b(3), 0), (b(3), 10), (b(3), 15), (b(5), 13)]
        );
        assert!(text.in_notes(b(5)));
        let limited = find(&text, &query, 2);
        assert_eq!(limited.total, 5, "everything is counted");
        assert_eq!(limited.hits, all.hits[..2]);
        assert_eq!(find(&text, &query, 0).total, 5);
    }

    #[test]
    fn a_hit_never_spans_two_blocks() {
        let text = BookText::new(&book());
        // "line" ends b2 and "one" starts b3; the flat text has "line\none".
        assert!(text.flat().contains("line\none"));
        assert_eq!(find(&text, &Query::parse("line one").unwrap(), 10).total, 0);
        assert_eq!(find(&text, &Query::parse("neon").unwrap(), 10).total, 0);
    }

    #[test]
    fn snippets_cut_at_words_and_chars() {
        let text = "Alpha beta gamma delta epsilon";
        // Context 8 from "gamma": "a beta " → the partial word "a" is dropped.
        let s = Snippet::new(text, 11, 16, 8);
        assert_eq!(s.before, "beta ");
        assert_eq!(s.matched, "gamma");
        assert_eq!(s.after, " delta");
        assert!(s.cut_before && s.cut_after);
        // At the edges nothing is cut.
        let s = Snippet::new(text, 0, 5, 100);
        assert_eq!(
            (s.before.as_str(), s.after.as_str()),
            ("", " beta gamma delta epsilon")
        );
        assert!(!s.cut_before && !s.cut_after);
        // One long word: cut mid-word, on a char boundary.
        let word = "😀".repeat(50);
        let s = Snippet::new(&format!("{word}x{word}"), 50, 51, 3);
        assert_eq!(s.before, "😀😀😀");
        assert_eq!(s.after, "😀😀😀");
        // Whitespace runs collapse; Cyrillic counts by code points.
        let s = Snippet::new("слово \u{a0}\n  другое", 0, 5, 20);
        assert_eq!(s.after, " другое");
        // Ranges past the end are clamped.
        let s = Snippet::new("abc", 2, 9, 1);
        assert_eq!((s.before.as_str(), s.matched.as_str()), ("b", "c"));
    }
}
