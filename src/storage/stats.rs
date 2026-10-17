//! Library statistics: what is in the library, the state of the index, duplicates, and
//! the book lists behind the figures.
//!
//! The composition (formats, languages, genres, series, authors) counts readable books
//! (status `ok`); failed and protected books are counted in the index health. The list
//! of a figure ([`list_books`]) uses the same condition, so its length is the figure.
//!
//! Duplicates are reported, never merged or hidden:
//!
//! - *identical copies* have the same content: the same size and CRC32 of the book
//!   bytes (a plain file's [`crc32`](super::containers::ContainerRecord::crc32), a zip
//!   entry's own), across plain files and archives;
//! - *probable duplicates* are different files of what looks like the same book: the
//!   same normalized title and the same, non-empty set of authors, with at least two
//!   different contents.

use rusqlite::{Connection, params};

use super::StorageError;
use super::books::{BookSummary, SUMMARY_COLUMNS, SUMMARY_FROM, summary_row};
use super::scans::{ScanRecord, last_scan};

/// The condition of the books counted in the composition.
const READABLE: &str = "b.status = 'ok'";

/// The number of entries in the genre, series and author rankings.
const TOP: u32 = 10;

/// The number of genres shown.
const TOP_GENRES: u32 = 12;

/// Everything the statistics page shows that comes from the index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LibraryStats {
    /// Books that are not removed (as on the library page).
    pub books: u64,
    /// Readable books (status `ok`): the base of the composition.
    pub readable: u64,
    /// Authors of books that are not removed.
    pub authors: u64,
    /// Series of readable books.
    pub series: u64,
    /// Genres of readable books.
    pub genres: u64,
    /// The size of the files and archives with books that are not removed.
    pub size: u64,
    /// The part of [`size`](Self::size) in zip archives.
    pub zip_size: u64,
    /// Readable books by format, most first.
    pub formats: Vec<Share>,
    /// Readable books by language (`None`: not stated), most first.
    pub languages: Vec<Share>,
    /// The genres with the most readable books (codes as stored).
    pub top_genres: Vec<Share>,
    /// The series with the most readable books.
    pub top_series: Vec<Ranked>,
    /// The authors with the most readable books.
    pub top_authors: Vec<Ranked>,
    /// Books that could not be read.
    pub errors: u64,
    /// Books protected by DRM.
    pub protected: u64,
    /// Readable books without a cover.
    pub without_cover: u64,
    /// Readable books without an author.
    pub without_author: u64,
    /// The last scan, if one was recorded.
    pub last_scan: Option<ScanRecord>,
    /// Duplicates.
    pub duplicates: DuplicateSummary,
}

/// The number of books with a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Share {
    /// The value (a format, language or genre code); `None` if not stated.
    pub value: Option<String>,
    /// The number of books.
    pub books: u64,
}

/// A series or author in a ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ranked {
    /// Series or author id.
    pub id: i64,
    /// The name.
    pub name: String,
    /// The number of books.
    pub books: u64,
}

/// The counts of duplicates.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DuplicateSummary {
    /// Contents found in more than one file (groups of identical copies).
    pub identical_groups: u64,
    /// The files in those groups.
    pub identical_files: u64,
    /// The size of the copies beyond the first of each group.
    pub identical_extra_bytes: u64,
    /// Groups of probable duplicates.
    pub probable_groups: u64,
}

/// Computes the statistics. Slow on large libraries (a few hundred milliseconds for
/// 100 000 books): cache the result.
pub fn library_stats(conn: &Connection) -> Result<LibraryStats, StorageError> {
    let (size, zip_size): (i64, i64) = conn.query_row(
        "SELECT coalesce(sum(c.size), 0), coalesce(sum(c.size) FILTER (WHERE c.kind = 'zip'), 0)
           FROM containers c
          WHERE c.present = 1
            AND EXISTS (SELECT 1 FROM books b
                         WHERE b.container_id = c.id AND b.status != 'removed')",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (books, readable, without_cover, without_author, errors, protected) = conn.query_row(
        &format!(
            "SELECT count(*),
                        count(*) FILTER (WHERE {READABLE}),
                        count(*) FILTER (WHERE {READABLE} AND b.cover_mime IS NULL),
                        count(*) FILTER (WHERE {READABLE} AND NOT EXISTS
                            (SELECT 1 FROM book_authors ba WHERE ba.book_id = b.id)),
                        count(*) FILTER (WHERE b.status = 'error'),
                        count(*) FILTER (WHERE b.status = 'protected')
                   FROM books b WHERE b.status != 'removed'"
        ),
        [],
        |row| {
            let n = |i: usize| row.get::<_, i64>(i).map(|v| v.max(0) as u64);
            Ok((n(0)?, n(1)?, n(2)?, n(3)?, n(4)?, n(5)?))
        },
    )?;
    // Each grouping also counts its groups (`count(*) OVER ()`), so the series and
    // genres are counted without a second pass.
    let (top_genres, genres) = shares_counted(
        conn,
        &format!(
            "SELECT g.code, t.n, t.total FROM
               (SELECT bg.genre_id, count(*) n, count(*) OVER () total
                  FROM book_genres bg JOIN books b ON b.id = bg.book_id
                 WHERE {READABLE} GROUP BY bg.genre_id) t
               JOIN genres g ON g.id = t.genre_id
              ORDER BY t.n DESC, g.code LIMIT {TOP_GENRES}"
        ),
    )?;
    let (top_series, series) = ranked(
        conn,
        &format!(
            "SELECT s.id, s.name, t.n, t.total FROM
               (SELECT bs.series_id, count(*) n, count(*) OVER () total
                  FROM book_series bs JOIN books b ON b.id = bs.book_id
                 WHERE {READABLE} GROUP BY bs.series_id) t
               JOIN series s ON s.id = t.series_id
              ORDER BY t.n DESC, s.sort_key LIMIT {TOP}"
        ),
    )?;
    let (top_authors, _) = ranked(
        conn,
        &format!(
            "SELECT a.id, a.display_name, t.n, t.total FROM
               (SELECT ba.author_id, count(*) n, count(*) OVER () total
                  FROM book_authors ba JOIN books b ON b.id = ba.book_id
                 WHERE {READABLE} GROUP BY ba.author_id) t
               JOIN authors a ON a.id = t.author_id
              ORDER BY t.n DESC, a.sort_key LIMIT {TOP}"
        ),
    )?;
    Ok(LibraryStats {
        books,
        readable,
        authors: super::authors::count_authors(conn, "")?,
        series,
        genres,
        size: size.max(0) as u64,
        zip_size: zip_size.max(0) as u64,
        formats: shares(
            conn,
            &format!(
                "SELECT b.format, count(*) n FROM books b WHERE {READABLE}
                  GROUP BY b.format ORDER BY n DESC, b.format"
            ),
        )?,
        languages: shares(
            conn,
            &format!(
                "SELECT b.language, count(*) n FROM books b WHERE {READABLE}
                  GROUP BY b.language ORDER BY n DESC, b.language"
            ),
        )?,
        top_genres,
        top_series,
        top_authors,
        errors,
        protected,
        without_cover,
        without_author,
        last_scan: last_scan(conn)?,
        duplicates: duplicate_summary(conn)?,
    })
}

fn shares(conn: &Connection, sql: &str) -> Result<Vec<Share>, StorageError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map([], |row| {
        Ok(Share {
            value: row.get(0)?,
            books: row.get::<_, i64>(1)? as u64,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The rows of a ranking and, from its column 3, the number of entries in all.
fn ranked(conn: &Connection, sql: &str) -> Result<(Vec<Ranked>, u64), StorageError> {
    let mut stmt = conn.prepare(sql)?;
    let mut total = 0;
    let rows = stmt.query_map([], |row| {
        Ok((
            Ranked {
                id: row.get(0)?,
                name: row.get(1)?,
                books: row.get::<_, i64>(2)? as u64,
            },
            row.get::<_, i64>(3)? as u64,
        ))
    })?;
    let mut list = Vec::new();
    for row in rows {
        let (entry, all) = row?;
        total = all;
        list.push(entry);
    }
    Ok((list, total))
}

/// The rows of [`shares`] and, from its column 2, the number of values in all.
fn shares_counted(conn: &Connection, sql: &str) -> Result<(Vec<Share>, u64), StorageError> {
    let mut stmt = conn.prepare(sql)?;
    let mut total = 0;
    let rows = stmt.query_map([], |row| {
        Ok((
            Share {
                value: row.get(0)?,
                books: row.get::<_, i64>(1)? as u64,
            },
            row.get::<_, i64>(2)? as u64,
        ))
    })?;
    let mut list = Vec::new();
    for row in rows {
        let (share, all) = row?;
        total = all;
        list.push(share);
    }
    Ok((list, total))
}

/// Books that are not removed with the size and CRC32 of their bytes (`crc` is NULL
/// for plain files read before the CRC32 was recorded).
const IDENTITY: &str = "
    identity AS (
        SELECT b.id,
               CASE WHEN b.entry_name = x'' THEN c.size ELSE b.entry_size END AS size,
               CASE WHEN b.entry_name = x'' THEN c.crc32 ELSE b.entry_crc32 END AS crc
          FROM books b JOIN containers c ON c.id = b.container_id
         WHERE b.status != 'removed')";

/// Groups of identical copies: `(size, crc)` of more than one book.
const IDENTICAL_GROUPS: &str = "
    identical AS (
        SELECT size, crc, count(*) AS n FROM identity
         WHERE crc IS NOT NULL GROUP BY size, crc HAVING n > 1)";

/// Books with their normalized title, author set and content identity, and the groups
/// of probable duplicates: the same title and author set, different contents.
///
/// Only books whose title occurs more than once are looked at (found through the
/// `sort_title` index): collecting the author set of every book is four times slower.
/// The identity is the size and CRC32, else the plain file's hash (read before the
/// CRC32 was recorded).
const PROBABLE_GROUPS: &str = "
    candidates AS (
        SELECT sort_title FROM books WHERE status != 'removed'
         GROUP BY sort_title HAVING count(*) > 1),
    named AS (
        SELECT b.id, b.sort_title AS title,
               (SELECT group_concat(ba.author_id, ',' ORDER BY ba.author_id)
                  FROM book_authors ba WHERE ba.book_id = b.id) AS authors,
               coalesce(
                   CASE WHEN b.entry_name = x'' THEN c.size || ':' || c.crc32
                        ELSE b.entry_size || ':' || b.entry_crc32 END,
                   hex(b.content_hash), 'b' || b.id) AS content
          FROM books b JOIN containers c ON c.id = b.container_id
         WHERE b.status != 'removed' AND b.sort_title IN (SELECT sort_title FROM candidates)),
    probable AS (
        SELECT title, authors FROM named WHERE authors IS NOT NULL
         GROUP BY title, authors HAVING count(DISTINCT content) > 1)";

/// The counts of identical copies and probable duplicates.
pub fn duplicate_summary(conn: &Connection) -> Result<DuplicateSummary, StorageError> {
    let mut summary = identical_summary(conn)?;
    summary.probable_groups = conn.query_row(
        &format!("WITH {PROBABLE_GROUPS} SELECT count(*) FROM probable"),
        [],
        |row| row.get::<_, i64>(0),
    )? as u64;
    Ok(summary)
}

/// The counts of identical copies only ([`DuplicateSummary::probable_groups`] is 0):
/// the cheaper half of [`duplicate_summary`].
pub fn identical_summary(conn: &Connection) -> Result<DuplicateSummary, StorageError> {
    let (groups, files, extra): (i64, i64, i64) = conn.query_row(
        &format!(
            "WITH {IDENTITY}, {IDENTICAL_GROUPS}
             SELECT count(*), coalesce(sum(n), 0), coalesce(sum((n - 1) * size), 0)
               FROM identical"
        ),
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(DuplicateSummary {
        identical_groups: groups as u64,
        identical_files: files as u64,
        identical_extra_bytes: extra.max(0) as u64,
        probable_groups: 0,
    })
}

/// The kind of duplicates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateKind {
    /// The same content in several files.
    Identical,
    /// Different files that look like the same book.
    Probable,
}

/// The book ids of one page of duplicate groups, each group in id order: identical
/// copies largest first, probable duplicates by title.
pub fn duplicate_groups(
    conn: &Connection,
    kind: DuplicateKind,
    limit: u32,
    offset: u64,
) -> Result<Vec<Vec<i64>>, StorageError> {
    let sql = match kind {
        DuplicateKind::Identical => format!(
            "WITH {IDENTITY}, {IDENTICAL_GROUPS},
                  page AS (SELECT size, crc FROM identical
                            ORDER BY size DESC, crc LIMIT ?1 OFFSET ?2)
             SELECT p.size || ':' || p.crc, i.id
               FROM page p JOIN identity i ON i.size = p.size AND i.crc = p.crc
              ORDER BY p.size DESC, p.crc, i.id"
        ),
        DuplicateKind::Probable => format!(
            "WITH {PROBABLE_GROUPS},
                  page AS (SELECT title, authors FROM probable
                            ORDER BY title, authors LIMIT ?1 OFFSET ?2)
             SELECT p.title || '/' || p.authors, n.id
               FROM page p JOIN named n ON n.title = p.title AND n.authors = p.authors
              ORDER BY p.title, p.authors, n.id"
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(
        params![limit, i64::try_from(offset).unwrap_or(i64::MAX)],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let mut groups: Vec<(String, Vec<i64>)> = Vec::new();
    for row in rows {
        let (key, id) = row?;
        match groups.last_mut() {
            Some((last, ids)) if *last == key => ids.push(id),
            _ => groups.push((key, vec![id])),
        }
    }
    Ok(groups.into_iter().map(|(_, ids)| ids).collect())
}

/// A list of readable books behind a figure of the statistics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookFilter {
    /// Books in a format.
    Format(String),
    /// Books in a language (as stored).
    Language(String),
    /// Books of a genre (code as stored).
    Genre(String),
    /// Books without a cover.
    WithoutCover,
    /// Books without an author.
    WithoutAuthor,
}

impl BookFilter {
    /// The SQL condition on `b` and its parameter.
    fn condition(&self) -> (&'static str, Option<&str>) {
        match self {
            Self::Format(format) => ("b.format = ?1", Some(format)),
            Self::Language(language) => ("b.language = ?1", Some(language)),
            Self::Genre(code) => (
                "EXISTS (SELECT 1 FROM book_genres bg JOIN genres g ON g.id = bg.genre_id
                          WHERE bg.book_id = b.id AND g.code = ?1)",
                Some(code),
            ),
            Self::WithoutCover => ("b.cover_mime IS NULL", None),
            Self::WithoutAuthor => (
                "NOT EXISTS (SELECT 1 FROM book_authors ba WHERE ba.book_id = b.id)",
                None,
            ),
        }
    }
}

/// The number of readable books matching `filter`.
pub fn count_books_where(conn: &Connection, filter: &BookFilter) -> Result<u64, StorageError> {
    let (condition, value) = filter.condition();
    let sql = format!("SELECT count(*) FROM books b WHERE {READABLE} AND {condition}");
    let count: i64 = match value {
        Some(value) => conn.query_row(&sql, [value], |row| row.get(0))?,
        None => conn.query_row(&sql, [], |row| row.get(0))?,
    };
    Ok(count as u64)
}

/// One page of the readable books matching `filter`, by title.
pub fn list_books(
    conn: &Connection,
    filter: &BookFilter,
    limit: u32,
    offset: u64,
) -> Result<Vec<BookSummary>, StorageError> {
    let (condition, value) = filter.condition();
    let offset = i64::try_from(offset).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(&format!(
        "SELECT {SUMMARY_COLUMNS} {SUMMARY_FROM}
          WHERE {READABLE} AND {condition}
          ORDER BY b.sort_title, b.id LIMIT ?2 OFFSET ?3"
    ))?;
    let rows = match value {
        Some(value) => stmt.query_map(params![value, limit, offset], summary_row)?,
        // `?1` is not used: bind it anyway, so the numbering stays the same.
        None => stmt.query_map(params![rusqlite::types::Null, limit, offset], summary_row)?,
    };
    Ok(rows.collect::<Result<_, _>>()?)
}

/// A copy in a group of duplicates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateCopy {
    /// Book id.
    pub id: i64,
    /// Title.
    pub title: String,
    /// Author display names joined with `, `.
    pub authors: String,
    /// The path relative to the library root; for a zip entry, the archive path and
    /// the entry path joined with `/`.
    pub path: String,
    /// Format name.
    pub format: String,
    /// The size of the book's bytes.
    pub size: u64,
}

/// The copies `ids` (of one group from [`duplicate_groups`]), in the order given;
/// unknown ids are left out.
pub fn duplicate_copies(
    conn: &Connection,
    ids: &[i64],
) -> Result<Vec<DuplicateCopy>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.title,
                (SELECT group_concat(a.display_name, ', ' ORDER BY ba.position)
                   FROM book_authors ba JOIN authors a ON a.id = ba.author_id
                  WHERE ba.book_id = b.id),
                c.path, b.entry_path, b.format,
                CASE WHEN b.entry_name = x'' THEN c.size ELSE b.entry_size END
           FROM books b JOIN containers c ON c.id = b.container_id
          WHERE b.id = ?1",
    )?;
    let mut copies = Vec::with_capacity(ids.len());
    for id in ids {
        let mut rows = stmt.query_map([id], |row| {
            let container: String = row.get(3)?;
            let entry: Option<String> = row.get(4)?;
            Ok(DuplicateCopy {
                id: row.get(0)?,
                title: row.get(1)?,
                authors: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                path: match entry {
                    Some(entry) => format!("{container}/{entry}"),
                    None => container,
                },
                format: row.get(5)?,
                size: row.get::<_, Option<i64>>(6)?.unwrap_or(0).max(0) as u64,
            })
        })?;
        if let Some(copy) = rows.next() {
            copies.push(copy?);
        }
    }
    Ok(copies)
}
