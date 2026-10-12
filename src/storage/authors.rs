//! Authors: de-duplicated by a normalized name key, listed alphabetically.

use rusqlite::{Connection, OptionalExtension, params};

use super::books::{BookSummary, SUMMARY_COLUMNS, SUMMARY_FROM, summary_row};
use super::{StorageError, normalize_key, prefix_upper_bound};
use crate::format::model::Person;

/// An author of a book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorRef {
    /// Author id.
    pub id: i64,
    /// Name for display.
    pub display_name: String,
}

/// An author in the alphabetical list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorSummary {
    /// Author id.
    pub id: i64,
    /// Name for display.
    pub display_name: String,
    /// Number of books that are not removed.
    pub book_count: u64,
}

/// The name shown for an author: `Last First Middle`, or the nickname if there is no
/// name.
///
/// # Examples
///
/// ```
/// use shelfgauge::format::model::Person;
/// use shelfgauge::storage::authors::display_name;
///
/// let person = Person {
///     first_name: Some("Лев".into()),
///     last_name: Some("Толстой".into()),
///     ..Person::default()
/// };
/// assert_eq!(display_name(&person), "Толстой Лев");
/// ```
pub fn display_name(person: &Person) -> String {
    let name = [&person.last_name, &person.first_name, &person.middle_name]
        .into_iter()
        .flatten()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() {
        person
            .nickname
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_owned()
    } else {
        name
    }
}

/// The key identifying an author: the normalized display name (see
/// [`normalize_key`]). Authors with equal keys are the same author.
pub fn sort_key(person: &Person) -> String {
    normalize_key(&display_name(person))
}

/// All name parts, for the search index.
pub(crate) fn search_name(person: &Person) -> String {
    [
        &person.last_name,
        &person.first_name,
        &person.middle_name,
        &person.nickname,
    ]
    .into_iter()
    .flatten()
    .map(String::as_str)
    .collect::<Vec<_>>()
    .join(" ")
}

/// The id of the author, creating it if needed; `None` if the person has no name.
pub(crate) fn upsert_author(
    conn: &Connection,
    person: &Person,
) -> Result<Option<i64>, StorageError> {
    let key = sort_key(person);
    if key.is_empty() {
        return Ok(None);
    }
    let id = conn.query_row(
        "INSERT INTO authors (first_name, middle_name, last_name, nickname, display_name, sort_key)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (sort_key) DO UPDATE SET sort_key = excluded.sort_key
         RETURNING id",
        params![
            person.first_name,
            person.middle_name,
            person.last_name,
            person.nickname,
            display_name(person),
            key
        ],
        |row| row.get(0),
    )?;
    Ok(Some(id))
}

/// The authors of a book in order.
pub(crate) fn book_authors(
    conn: &Connection,
    book_id: i64,
) -> Result<Vec<AuthorRef>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.display_name FROM book_authors ba JOIN authors a ON a.id = ba.author_id
         WHERE ba.book_id = ?1 ORDER BY ba.position",
    )?;
    let rows = stmt.query_map([book_id], |row| {
        Ok(AuthorRef {
            id: row.get(0)?,
            display_name: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The stored name parts of a book's authors in order.
pub(crate) fn book_people(conn: &Connection, book_id: i64) -> Result<Vec<Person>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT a.first_name, a.middle_name, a.last_name, a.nickname
         FROM book_authors ba JOIN authors a ON a.id = ba.author_id
         WHERE ba.book_id = ?1 ORDER BY ba.position",
    )?;
    let rows = stmt.query_map([book_id], |row| {
        Ok(Person {
            first_name: row.get(0)?,
            middle_name: row.get(1)?,
            last_name: row.get(2)?,
            nickname: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Authors with at least one book that is not removed, alphabetically (namesakes by
/// id), whose normalized name starts with `prefix` (empty for all); one page of `limit`
/// entries starting at `offset`.
pub fn list_authors(
    conn: &Connection,
    prefix: &str,
    limit: u32,
    offset: u64,
) -> Result<Vec<AuthorSummary>, StorageError> {
    let prefix = normalize_key(prefix);
    let mut stmt = conn.prepare(
        "SELECT a.id, a.display_name, count(b.id)
           FROM authors a
           JOIN book_authors ba ON ba.author_id = a.id
           JOIN books b ON b.id = ba.book_id AND b.status != 'removed'
          WHERE a.sort_key >= ?1 AND a.sort_key < ?2
          GROUP BY a.id
          ORDER BY a.sort_key, a.id
          LIMIT ?3 OFFSET ?4",
    )?;
    let rows = stmt.query_map(
        params![
            prefix,
            prefix_upper_bound(&prefix),
            limit,
            i64::try_from(offset).unwrap_or(i64::MAX)
        ],
        |row| {
            Ok(AuthorSummary {
                id: row.get(0)?,
                display_name: row.get(1)?,
                book_count: row.get::<_, i64>(2)? as u64,
            })
        },
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The number of authors [`list_authors`] would list for `prefix` across all pages.
pub fn count_authors(conn: &Connection, prefix: &str) -> Result<u64, StorageError> {
    let prefix = normalize_key(prefix);
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM authors a
          WHERE a.sort_key >= ?1 AND a.sort_key < ?2
            AND EXISTS (SELECT 1 FROM book_authors ba JOIN books b ON b.id = ba.book_id
                         WHERE ba.author_id = a.id AND b.status != 'removed')",
        params![prefix, prefix_upper_bound(&prefix)],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// The author `id` with the number of their books that are not removed, or `None` if
/// there is no such author.
pub fn author_by_id(conn: &Connection, id: i64) -> Result<Option<AuthorSummary>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT a.id, a.display_name,
                    (SELECT count(*) FROM book_authors ba JOIN books b ON b.id = ba.book_id
                      WHERE ba.author_id = a.id AND b.status != 'removed')
               FROM authors a WHERE a.id = ?1",
            [id],
            |row| {
                Ok(AuthorSummary {
                    id: row.get(0)?,
                    display_name: row.get(1)?,
                    book_count: row.get::<_, i64>(2)? as u64,
                })
            },
        )
        .optional()?)
}

/// The position of author `id` in the unfiltered [`list_authors`] order, or `None` if
/// the author is not listed (no such author, or all their books are removed).
pub fn author_index(conn: &Connection, id: i64) -> Result<Option<u64>, StorageError> {
    let listed = "EXISTS (SELECT 1 FROM book_authors ba JOIN books b ON b.id = ba.book_id
                          WHERE ba.author_id = a.id AND b.status != 'removed')";
    let sql = format!(
        "SELECT (SELECT count(*) FROM authors a
                  WHERE (a.sort_key < t.sort_key OR (a.sort_key = t.sort_key AND a.id < t.id))
                    AND {listed})
           FROM authors t WHERE t.id = ?1 AND EXISTS (
                SELECT 1 FROM book_authors ba JOIN books b ON b.id = ba.book_id
                 WHERE ba.author_id = t.id AND b.status != 'removed')"
    );
    Ok(conn
        .query_row(&sql, [id], |row| row.get::<_, i64>(0))
        .optional()?
        .map(|n| n as u64))
}

/// The author's books that are not removed: by series (books without a series last),
/// then number in the series, then title.
pub fn books_by_author(
    conn: &Connection,
    author_id: i64,
) -> Result<Vec<BookSummary>, StorageError> {
    let sql = format!(
        "SELECT {SUMMARY_COLUMNS} {SUMMARY_FROM}
         WHERE b.status != 'removed'
           AND b.id IN (SELECT book_id FROM book_authors WHERE author_id = ?1)
         ORDER BY s.sort_key IS NULL, s.sort_key, bs.number_sort IS NULL, bs.number_sort,
                  b.sort_title, b.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([author_id], summary_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}
