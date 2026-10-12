//! Full-text search over title, authors, series and annotation.

use rusqlite::{Connection, params};

use super::books::{BookSummary, SUMMARY_COLUMNS, SUMMARY_FROM, summary_row};
use super::{StorageError, fold_for_search};

/// Maximum number of words used from a query.
const MAX_TERMS: usize = 16;

/// Builds an FTS5 `MATCH` expression from user input: every word becomes a quoted
/// prefix term (`"word"*`), and all terms must match. `None` if there are no words.
///
/// The input is never interpreted as FTS5 syntax, so any text is safe.
///
/// # Examples
///
/// ```
/// use shelfgauge::storage::search::match_query;
///
/// assert_eq!(match_query("Война мир").as_deref(), Some(r#""Война"* "мир"*"#));
/// assert_eq!(match_query(r#"NOT "x" (y)*"#).as_deref(), Some(r#""NOT"* "x"* "y"*"#));
/// assert_eq!(match_query(" -- "), None);
/// ```
pub fn match_query(input: &str) -> Option<String> {
    let folded = fold_for_search(input);
    let terms: Vec<String> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(MAX_TERMS)
        .map(|word| format!("\"{}\"*", word.replace('"', "\"\"")))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// Books matching `input` (see [`match_query`]), best matches first; removed books are
/// excluded. Returns one page of `limit` results starting at `offset`.
pub fn search(
    conn: &Connection,
    input: &str,
    limit: u32,
    offset: u64,
) -> Result<Vec<BookSummary>, StorageError> {
    let Some(query) = match_query(input) else {
        return Ok(Vec::new());
    };
    let sql = format!(
        "WITH hits AS (SELECT rowid AS id, rank FROM books_fts WHERE books_fts MATCH ?1)
         SELECT {SUMMARY_COLUMNS} {SUMMARY_FROM}
         JOIN hits ON hits.id = b.id
         WHERE b.status != 'removed'
         ORDER BY hits.rank, b.sort_title, b.id
         LIMIT ?2 OFFSET ?3"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![query, limit, i64::try_from(offset).unwrap_or(i64::MAX)],
        summary_row,
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}
