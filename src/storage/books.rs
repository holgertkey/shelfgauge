//! Books: locations, metadata, links to authors/series/genres, and the search index.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::authors::{self, AuthorRef};
use super::containers::parse_kind;
use super::{StorageError, fold_for_search, genres, normalize_key};
use crate::container::ContainerKind;
use crate::format::model::{BookMetadata, Person, Series};

/// The state of a book in the index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BookStatus {
    /// Parsed successfully.
    Ok,
    /// The book could not be parsed; it keeps its location so it is not re-parsed while
    /// unchanged.
    Error,
    /// The book's file disappeared; kept (not deleted) so it can be matched again.
    Removed,
}

impl BookStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
            Self::Removed => "removed",
        }
    }

    pub(crate) fn parse(text: &str) -> Self {
        match text {
            "error" => Self::Error,
            "removed" => Self::Removed,
            _ => Self::Ok,
        }
    }
}

/// The location of a book inside a zip archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRecord {
    /// Raw entry name bytes (the identity of the entry).
    pub name: Vec<u8>,
    /// Decoded, `/`-separated entry path for display and the folder view.
    pub path: String,
    /// Index in the central directory.
    pub index: u64,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// CRC32 of the uncompressed data.
    pub crc32: u32,
}

/// Everything stored about a book, as written by the scanner.
#[derive(Debug, Clone, PartialEq)]
pub struct BookRecord {
    /// The containing file or archive.
    pub container_id: i64,
    /// The archive entry; `None` for a plain file.
    pub entry: Option<EntryRecord>,
    /// Content hash of a plain file (its identity for moved-file matching).
    pub content_hash: Option<Vec<u8>>,
    /// The format name, e.g. `fb2`.
    pub format: String,
    /// The title (the file name if the book has none).
    pub title: String,
    /// Authors in order.
    pub authors: Vec<Person>,
    /// Series in order.
    pub series: Vec<Series>,
    /// Genre codes.
    pub genres: Vec<String>,
    /// The annotation as plain text (paragraphs separated by newlines).
    pub annotation: String,
    /// Language code.
    pub language: Option<String>,
    /// MIME type of the cached cover image, if the book has one.
    pub cover_mime: Option<String>,
    /// Parse status.
    pub status: BookStatus,
    /// Why parsing failed, for [`BookStatus::Error`].
    pub error_message: Option<String>,
}

impl BookRecord {
    /// A successfully parsed book; `fallback_title` is used if the metadata has none.
    pub fn parsed(
        container_id: i64,
        entry: Option<EntryRecord>,
        content_hash: Option<Vec<u8>>,
        format: &str,
        metadata: &BookMetadata,
        fallback_title: &str,
    ) -> Self {
        let title = if metadata.title.trim().is_empty() {
            fallback_title.to_owned()
        } else {
            metadata.title.clone()
        };
        Self {
            container_id,
            entry,
            content_hash,
            format: format.to_owned(),
            title,
            authors: metadata.authors.clone(),
            series: metadata.series.clone(),
            genres: metadata.genres.clone(),
            annotation: metadata.annotation.plain_text(),
            language: metadata.language.clone(),
            cover_mime: metadata.cover.as_ref().map(|c| c.mime.clone()),
            status: BookStatus::Ok,
            error_message: None,
        }
    }

    /// A book that failed to parse.
    pub fn failed(
        container_id: i64,
        entry: Option<EntryRecord>,
        content_hash: Option<Vec<u8>>,
        format: &str,
        title: &str,
        message: &str,
    ) -> Self {
        Self {
            container_id,
            entry,
            content_hash,
            format: format.to_owned(),
            title: title.to_owned(),
            authors: Vec::new(),
            series: Vec::new(),
            genres: Vec::new(),
            annotation: String::new(),
            language: None,
            cover_mime: None,
            status: BookStatus::Error,
            error_message: Some(message.to_owned()),
        }
    }
}

/// A book's location and identity data, for the scanner's change detection and
/// moved-book matching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedBook {
    /// Book id.
    pub id: i64,
    /// Container id.
    pub container_id: i64,
    /// Raw entry name (empty for plain files).
    pub entry_name: Vec<u8>,
    /// Entry size (zip entries only).
    pub entry_size: Option<u64>,
    /// Entry CRC32 (zip entries only).
    pub entry_crc32: Option<u32>,
    /// Content hash (plain files only).
    pub content_hash: Option<Vec<u8>>,
    /// Status.
    pub status: BookStatus,
}

/// A short description of a book for lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookSummary {
    /// Book id.
    pub id: i64,
    /// Title.
    pub title: String,
    /// Author display names joined with `, `.
    pub authors: String,
    /// The first series, if any.
    pub series: Option<SeriesEntry>,
    /// Status.
    pub status: BookStatus,
}

/// A book's membership in a series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesEntry {
    /// Series name.
    pub name: String,
    /// The number in the series, verbatim.
    pub number: Option<String>,
}

/// Everything about a book for the book card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookDetails {
    /// Book id.
    pub id: i64,
    /// Title.
    pub title: String,
    /// Authors in order.
    pub authors: Vec<AuthorRef>,
    /// Series in order.
    pub series: Vec<SeriesEntry>,
    /// Genre codes.
    pub genres: Vec<String>,
    /// Annotation as plain text.
    pub annotation: String,
    /// Language code.
    pub language: Option<String>,
    /// Cover MIME type, if the book has a cover.
    pub cover_mime: Option<String>,
    /// Format name.
    pub format: String,
    /// Status.
    pub status: BookStatus,
    /// Error message for [`BookStatus::Error`].
    pub error_message: Option<String>,
    /// Container path relative to the library root.
    pub container_path: String,
    /// Container kind.
    pub container_kind: ContainerKind,
    /// The archive entry, for books in archives.
    pub entry: Option<EntryRecord>,
    /// When the book was first indexed (Unix seconds).
    pub added_at: i64,
    /// When the record last changed (Unix seconds).
    pub updated_at: i64,
}

/// Inserts the book, or updates the live book at the same location; returns its id.
///
/// Idempotent: writing the same record twice changes nothing but `updated_at`.
pub fn upsert_book(conn: &Connection, record: &BookRecord) -> Result<i64, StorageError> {
    let entry_name = record.entry.as_ref().map_or(&[][..], |e| &e.name[..]);
    match find_live_at(conn, record.container_id, entry_name)? {
        Some(id) => {
            update_book(conn, id, record)?;
            Ok(id)
        }
        None => insert_book(conn, record),
    }
}

/// Inserts a new book and returns its id.
pub fn insert_book(conn: &Connection, record: &BookRecord) -> Result<i64, StorageError> {
    let id = conn.query_row(
        "INSERT INTO books (container_id, format, title, sort_title)
         VALUES (?1, ?2, ?3, ?4) RETURNING id",
        params![
            record.container_id,
            record.format,
            record.title,
            normalize_key(&record.title)
        ],
        |row| row.get(0),
    )?;
    update_book(conn, id, record)?;
    Ok(id)
}

/// Replaces everything stored about book `id` (location included) with `record`.
pub fn update_book(conn: &Connection, id: i64, record: &BookRecord) -> Result<(), StorageError> {
    let entry = record.entry.as_ref();
    conn.execute(
        "UPDATE books SET
             container_id = ?2, entry_name = ?3, entry_path = ?4, entry_index = ?5,
             entry_size = ?6, entry_crc32 = ?7, format = ?8, title = ?9, sort_title = ?10,
             language = ?11, annotation = ?12, cover_mime = ?13, status = ?14,
             error_message = ?15, content_hash = ?16, updated_at = unixepoch()
         WHERE id = ?1",
        params![
            id,
            record.container_id,
            entry.map_or(&[][..], |e| &e.name[..]),
            entry.map(|e| &e.path),
            entry.map(|e| e.index as i64),
            entry.map(|e| e.size as i64),
            entry.map(|e| i64::from(e.crc32)),
            record.format,
            record.title,
            normalize_key(&record.title),
            record.language,
            record.annotation,
            record.cover_mime,
            record.status.as_str(),
            record.error_message,
            record.content_hash,
        ],
    )?;

    conn.execute("DELETE FROM book_authors WHERE book_id = ?1", [id])?;
    let mut position = 0;
    for person in &record.authors {
        if let Some(author_id) = authors::upsert_author(conn, person)? {
            conn.execute(
                "INSERT INTO book_authors (book_id, position, author_id) VALUES (?1, ?2, ?3)",
                params![id, position, author_id],
            )?;
            position += 1;
        }
    }

    conn.execute("DELETE FROM book_series WHERE book_id = ?1", [id])?;
    for (position, series) in record.series.iter().enumerate() {
        let key = normalize_key(&series.name);
        if key.is_empty() {
            continue;
        }
        let series_id: i64 = conn.query_row(
            "INSERT INTO series (name, sort_key) VALUES (?1, ?2)
             ON CONFLICT (sort_key) DO UPDATE SET sort_key = excluded.sort_key
             RETURNING id",
            params![series.name.trim(), key],
            |row| row.get(0),
        )?;
        conn.execute(
            "INSERT INTO book_series (book_id, position, series_id, number, number_sort)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                id,
                position as i64,
                series_id,
                series.index,
                series.numeric_index()
            ],
        )?;
    }

    conn.execute("DELETE FROM book_genres WHERE book_id = ?1", [id])?;
    for code in &record.genres {
        let genre_id = genres::upsert_genre(conn, code)?;
        conn.execute(
            "INSERT OR IGNORE INTO book_genres (book_id, genre_id) VALUES (?1, ?2)",
            params![id, genre_id],
        )?;
    }

    sync_search_index(conn, id, record)?;
    Ok(())
}

/// Marks a book as removed; it leaves the search index and frees its location.
pub fn mark_removed(conn: &Connection, id: i64) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE books SET status = 'removed', updated_at = unixepoch() WHERE id = ?1",
        [id],
    )?;
    conn.execute("DELETE FROM books_fts WHERE rowid = ?1", [id])?;
    Ok(())
}

/// Moves book `id` to a new location (keeping its id and metadata) and marks it `ok`
/// or `error` again, restoring it to the search index.
pub fn relocate(
    conn: &Connection,
    id: i64,
    container_id: i64,
    entry: Option<&EntryRecord>,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE books SET
             container_id = ?2, entry_name = ?3, entry_path = ?4, entry_index = ?5,
             entry_size = ?6, entry_crc32 = ?7,
             status = CASE WHEN error_message IS NULL THEN 'ok' ELSE 'error' END,
             updated_at = unixepoch()
         WHERE id = ?1",
        params![
            id,
            container_id,
            entry.map_or(&[][..], |e| &e.name[..]),
            entry.map(|e| &e.path),
            entry.map(|e| e.index as i64),
            entry.map(|e| e.size as i64),
            entry.map(|e| i64::from(e.crc32)),
        ],
    )?;
    if let Some(book) = book_by_id(conn, id)? {
        let people = authors::book_people(conn, id)?;
        insert_search_row(
            conn,
            id,
            &search_text(&book.title, &people, &book.series, &book.annotation),
        )?;
    }
    Ok(())
}

/// The live (not removed) book at a location.
pub fn find_live_at(
    conn: &Connection,
    container_id: i64,
    entry_name: &[u8],
) -> Result<Option<i64>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT id FROM books
             WHERE container_id = ?1 AND entry_name = ?2 AND status != 'removed'",
            params![container_id, entry_name],
            |row| row.get(0),
        )
        .optional()?)
}

/// All books (any status), ordered by id; used by the scanner to compare the index
/// with the files found.
pub fn all_books(conn: &Connection) -> Result<Vec<LocatedBook>, StorageError> {
    located(conn, "ORDER BY b.id", [])
}

/// The number of books in the library (status `ok` or `error`, not `removed`).
pub fn count_books(conn: &Connection) -> Result<u64, StorageError> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM books WHERE status != 'removed'",
        [],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// Books in a container (any status), ordered by entry index.
pub fn books_in_container(
    conn: &Connection,
    container_id: i64,
) -> Result<Vec<LocatedBook>, StorageError> {
    located(
        conn,
        "WHERE b.container_id = ?1 ORDER BY b.entry_index, b.id",
        params![container_id],
    )
}

/// Plain-file books with `content_hash` (candidates for moved files).
pub fn find_by_content_hash(
    conn: &Connection,
    content_hash: &[u8],
) -> Result<Vec<LocatedBook>, StorageError> {
    located(
        conn,
        "WHERE b.content_hash = ?1 AND b.entry_name = x'' ORDER BY b.id",
        params![content_hash],
    )
}

/// Archive entries with the given size and CRC32 (candidates for moved entries).
pub fn find_by_entry_identity(
    conn: &Connection,
    size: u64,
    crc32: u32,
) -> Result<Vec<LocatedBook>, StorageError> {
    located(
        conn,
        "WHERE b.entry_size = ?1 AND b.entry_crc32 = ?2 ORDER BY b.id",
        params![size as i64, i64::from(crc32)],
    )
}

fn located(
    conn: &Connection,
    tail: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<LocatedBook>, StorageError> {
    let sql = format!(
        "SELECT b.id, b.container_id, b.entry_name, b.entry_size, b.entry_crc32,
                b.content_hash, b.status
         FROM books b {tail}"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params, |row| {
        Ok(LocatedBook {
            id: row.get(0)?,
            container_id: row.get(1)?,
            entry_name: row.get(2)?,
            entry_size: row.get::<_, Option<i64>>(3)?.map(|v| v as u64),
            entry_crc32: row.get::<_, Option<i64>>(4)?.map(|v| v as u32),
            content_hash: row.get(5)?,
            status: BookStatus::parse(&row.get::<_, String>(6)?),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Everything about a book, or `None` if there is no such book.
pub fn book_by_id(conn: &Connection, id: i64) -> Result<Option<BookDetails>, StorageError> {
    let book = conn
        .query_row(
            "SELECT b.id, b.title, b.annotation, b.language, b.cover_mime, b.format,
                    b.status, b.error_message, c.path, c.kind,
                    b.entry_name, b.entry_path, b.entry_index, b.entry_size, b.entry_crc32,
                    b.added_at, b.updated_at
             FROM books b JOIN containers c ON c.id = b.container_id
             WHERE b.id = ?1",
            [id],
            |row| {
                let entry_name: Vec<u8> = row.get(10)?;
                let entry = if entry_name.is_empty() {
                    None
                } else {
                    Some(EntryRecord {
                        name: entry_name,
                        path: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
                        index: row.get::<_, Option<i64>>(12)?.unwrap_or(0) as u64,
                        size: row.get::<_, Option<i64>>(13)?.unwrap_or(0) as u64,
                        crc32: row.get::<_, Option<i64>>(14)?.unwrap_or(0) as u32,
                    })
                };
                Ok(BookDetails {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    authors: Vec::new(),
                    series: Vec::new(),
                    genres: Vec::new(),
                    annotation: row.get(2)?,
                    language: row.get(3)?,
                    cover_mime: row.get(4)?,
                    format: row.get(5)?,
                    status: BookStatus::parse(&row.get::<_, String>(6)?),
                    error_message: row.get(7)?,
                    container_path: row.get(8)?,
                    container_kind: parse_kind(&row.get::<_, String>(9)?),
                    entry,
                    added_at: row.get(15)?,
                    updated_at: row.get(16)?,
                })
            },
        )
        .optional()?;
    let Some(mut book) = book else {
        return Ok(None);
    };
    book.authors = authors::book_authors(conn, id)?;
    book.series = book_series(conn, id)?;
    book.genres = genres::book_genres(conn, id)?;
    Ok(Some(book))
}

fn book_series(conn: &Connection, book_id: i64) -> Result<Vec<SeriesEntry>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT s.name, bs.number FROM book_series bs JOIN series s ON s.id = bs.series_id
         WHERE bs.book_id = ?1 ORDER BY bs.position",
    )?;
    let rows = stmt.query_map([book_id], |row| {
        Ok(SeriesEntry {
            name: row.get(0)?,
            number: row.get(1)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The columns of a [`BookSummary`] row (indexes 0–5, see [`summary_row`]); callers may
/// select extra columns after them. Use with [`SUMMARY_FROM`].
pub(crate) const SUMMARY_COLUMNS: &str = "
    b.id, b.title, b.status,
    (SELECT group_concat(a.display_name, ', ' ORDER BY ba.position)
       FROM book_authors ba JOIN authors a ON a.id = ba.author_id
      WHERE ba.book_id = b.id),
    s.name, bs.number";

/// The joins for [`SUMMARY_COLUMNS`] (`b` is the book, `s` its first series).
pub(crate) const SUMMARY_FROM: &str = "
    FROM books b
    LEFT JOIN book_series bs ON bs.book_id = b.id AND bs.position = 0
    LEFT JOIN series s ON s.id = bs.series_id";

/// Maps columns 0–5 of a [`SUMMARY_COLUMNS`] row.
pub(crate) fn summary_row(row: &Row<'_>) -> rusqlite::Result<BookSummary> {
    let series = match row.get::<_, Option<String>>(4)? {
        Some(name) => Some(SeriesEntry {
            name,
            number: row.get(5)?,
        }),
        None => None,
    };
    Ok(BookSummary {
        id: row.get(0)?,
        title: row.get(1)?,
        status: BookStatus::parse(&row.get::<_, String>(2)?),
        authors: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        series,
    })
}

/// Search text of a book, folded for the FTS index.
struct SearchText {
    title: String,
    authors: String,
    series: String,
    annotation: String,
}

fn sync_search_index(conn: &Connection, id: i64, record: &BookRecord) -> Result<(), StorageError> {
    conn.execute("DELETE FROM books_fts WHERE rowid = ?1", [id])?;
    if record.status == BookStatus::Removed {
        return Ok(());
    }
    let series: Vec<SeriesEntry> = record
        .series
        .iter()
        .map(|s| SeriesEntry {
            name: s.name.clone(),
            number: s.index.clone(),
        })
        .collect();
    let text = search_text(&record.title, &record.authors, &series, &record.annotation);
    insert_search_row(conn, id, &text)
}

fn search_text(
    title: &str,
    people: &[Person],
    series: &[SeriesEntry],
    annotation: &str,
) -> SearchText {
    SearchText {
        title: title.to_owned(),
        authors: people
            .iter()
            .map(authors::search_name)
            .collect::<Vec<_>>()
            .join(" "),
        series: series
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        annotation: annotation.to_owned(),
    }
}

fn insert_search_row(conn: &Connection, id: i64, text: &SearchText) -> Result<(), StorageError> {
    conn.execute("DELETE FROM books_fts WHERE rowid = ?1", [id])?;
    conn.execute(
        "INSERT INTO books_fts (rowid, title, authors, series, annotation)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            id,
            fold_for_search(&text.title),
            fold_for_search(&text.authors),
            fold_for_search(&text.series),
            fold_for_search(&text.annotation)
        ],
    )?;
    Ok(())
}
