//! The user-data database, `.shelfgauge/userdata.sqlite`: annotations and reading
//! positions.
//!
//! It is separate from the index (`library.db`), has its own `user_version`
//! migrations and is used through one connection behind a mutex ([`UserData`]): the
//! data is small and writes are rare. Functions take a [`Connection`] and never open a
//! transaction of their own unless they say so.
//!
//! Rows are grouped by the book's content key. Deletion is soft (`deleted_at`): rows
//! deleted by the user stay as tombstones, rows of a removed book are restored when it
//! comes back and purged after the retention period.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::anchor::{Anchor, Quote};
use super::{Annotation, BookSnapshot, DeletedReason};
use crate::format::model::BlockId;
use crate::storage::{StorageError, migrations, open_wal};

/// Schema migrations of the user-data database; index `i` upgrades version `i` to
/// `i + 1`. Append only: user data cannot be rebuilt.
const MIGRATIONS: &[&str] = &[V1];

/// Version 1: annotations and reading positions.
///
/// - `id` is a UUID; `book_key` the content key; `book_id` a hint only.
/// - Blocks are ordinals (`b12` → 12), offsets code points of rendered text.
/// - Times are Unix seconds.
const V1: &str = r#"
CREATE TABLE annotations (
    id             TEXT    PRIMARY KEY,
    book_key       BLOB    NOT NULL,
    book_id        INTEGER NOT NULL,
    start_block    INTEGER NOT NULL,
    start_offset   INTEGER NOT NULL,
    end_block      INTEGER NOT NULL,
    end_offset     INTEGER NOT NULL,
    text           TEXT    NOT NULL,
    prefix         TEXT    NOT NULL,
    suffix         TEXT    NOT NULL,
    section_title  TEXT,
    book_title     TEXT    NOT NULL,
    book_authors   TEXT    NOT NULL,
    anchor_lost    INTEGER NOT NULL DEFAULT 0 CHECK (anchor_lost IN (0, 1)),
    note           TEXT,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL,
    deleted_at     INTEGER,
    deleted_reason TEXT CHECK (deleted_reason IN ('user', 'book_removed'))
);

CREATE INDEX annotations_book ON annotations (book_key, start_block, start_offset);

CREATE TABLE reading_positions (
    book_key       BLOB    PRIMARY KEY,
    book_id        INTEGER NOT NULL,
    block          INTEGER NOT NULL,
    offset         INTEGER NOT NULL DEFAULT 0,
    suffix         TEXT    NOT NULL,
    progress       INTEGER NOT NULL,
    book_title     TEXT    NOT NULL,
    book_authors   TEXT    NOT NULL,
    updated_at     INTEGER NOT NULL,
    deleted_at     INTEGER,
    deleted_reason TEXT CHECK (deleted_reason IN ('book_removed'))
);
"#;

/// The schema version this build creates and understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// The user-data database: one connection behind a mutex.
#[derive(Debug)]
pub struct UserData {
    conn: Mutex<Connection>,
}

impl UserData {
    /// Opens (creating and migrating if needed) the database at `path`, in WAL mode.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::annotations::store::{UserData, for_book};
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let data = UserData::open(&dir.path().join("userdata.sqlite")).unwrap();
    /// assert!(for_book(&data.lock(), b"key").unwrap().is_empty());
    /// ```
    pub fn open(path: &Path) -> Result<Self, StorageError> {
        let mut conn = open_wal(path)?;
        migrations::apply(&mut conn, MIGRATIONS)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// A migrated in-memory database (for tests).
    pub fn open_in_memory() -> Result<Self, StorageError> {
        let mut conn = Connection::open_in_memory()?;
        crate::storage::configure(&conn)?;
        migrations::apply(&mut conn, MIGRATIONS)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// The connection, locked (call from a blocking thread).
    pub fn lock(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A new annotation, before it is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAnnotation {
    /// The book it is made in.
    pub book: BookSnapshot,
    /// Where it is.
    pub anchor: Anchor,
    /// The text and its context (derived from the book).
    pub quote: Quote,
    /// The title of the section around the start.
    pub section_title: Option<String>,
    /// The note.
    pub note: Option<String>,
}

const COLUMNS: &str = "id, book_key, book_id, start_block, start_offset, end_block, \
    end_offset, text, prefix, suffix, section_title, book_title, book_authors, anchor_lost, \
    note, created_at, updated_at, deleted_at, deleted_reason";

fn block(n: i64) -> BlockId {
    BlockId::new(u32::try_from(n).unwrap_or(0)).unwrap_or(BlockId::UNASSIGNED)
}

fn annotation_row(row: &Row<'_>) -> rusqlite::Result<Annotation> {
    Ok(Annotation {
        id: row.get(0)?,
        book_key: row.get(1)?,
        book_id: row.get(2)?,
        anchor: Anchor {
            start_block: block(row.get(3)?),
            start_offset: row.get(4)?,
            end_block: block(row.get(5)?),
            end_offset: row.get(6)?,
        },
        quote: Quote {
            text: row.get(7)?,
            prefix: row.get(8)?,
            suffix: row.get(9)?,
        },
        section_title: row.get(10)?,
        book_title: row.get(11)?,
        book_authors: row.get(12)?,
        anchor_lost: row.get(13)?,
        note: row.get(14)?,
        created_at: row.get(15)?,
        updated_at: row.get(16)?,
        deleted_at: row.get(17)?,
        deleted_reason: row
            .get::<_, Option<String>>(18)?
            .as_deref()
            .and_then(DeletedReason::parse),
    })
}

/// A note as stored: blank notes are `NULL`.
fn stored_note(note: Option<&str>) -> Option<&str> {
    note.filter(|n| !n.trim().is_empty())
}

/// Stores a new annotation with a fresh UUID and returns it.
pub fn insert(
    conn: &Connection,
    new: &NewAnnotation,
    now: i64,
) -> Result<Annotation, StorageError> {
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        &format!(
            "INSERT INTO annotations ({COLUMNS}) VALUES
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0, ?14, ?15, ?15,
              NULL, NULL)"
        ),
        params![
            id,
            new.book.key,
            new.book.id,
            new.anchor.start_block.ordinal(),
            new.anchor.start_offset,
            new.anchor.end_block.ordinal(),
            new.anchor.end_offset,
            new.quote.text,
            new.quote.prefix,
            new.quote.suffix,
            new.section_title,
            new.book.title,
            new.book.authors,
            stored_note(new.note.as_deref()),
            now,
        ],
    )?;
    Ok(get(conn, &id)?.expect("the row was just inserted"))
}

/// The annotation with `id`, deleted or not.
pub fn get(conn: &Connection, id: &str) -> Result<Option<Annotation>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM annotations WHERE id = ?1"),
            [id],
            annotation_row,
        )
        .optional()?)
}

/// Replaces the note of a live annotation; returns it, or `None` if there is no such
/// live annotation.
pub fn set_note(
    conn: &Connection,
    id: &str,
    note: Option<&str>,
    now: i64,
) -> Result<Option<Annotation>, StorageError> {
    let changed = conn.execute(
        "UPDATE annotations SET note = ?2, updated_at = ?3
         WHERE id = ?1 AND deleted_at IS NULL",
        params![id, stored_note(note), now],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get(conn, id)
}

/// Deletes a live annotation (soft, reason [`DeletedReason::User`]); returns it, or
/// `None` if there is no such live annotation.
pub fn delete(conn: &Connection, id: &str, now: i64) -> Result<Option<Annotation>, StorageError> {
    let changed = conn.execute(
        "UPDATE annotations SET deleted_at = ?2, deleted_reason = 'user', updated_at = ?2
         WHERE id = ?1 AND deleted_at IS NULL",
        params![id, now],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get(conn, id)
}

/// The live annotations of a book, in book order.
pub fn for_book(conn: &Connection, key: &[u8]) -> Result<Vec<Annotation>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM annotations
         WHERE book_key = ?1 AND deleted_at IS NULL
         ORDER BY start_block, start_offset, end_block, end_offset, created_at, id"
    ))?;
    let rows = stmt.query_map([key], annotation_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The number of live bookmarks and marks of a book.
pub fn counts(conn: &Connection, key: &[u8]) -> Result<(u64, u64), StorageError> {
    Ok(conn.query_row(
        "SELECT count(*) FILTER (WHERE text = ''), count(*) FILTER (WHERE text != '')
         FROM annotations WHERE book_key = ?1 AND deleted_at IS NULL",
        [key],
        |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
    )?)
}

/// Moves an annotation to a new place (and book), after re-attaching.
#[allow(clippy::too_many_arguments)]
pub fn reanchor(
    conn: &Connection,
    id: &str,
    book: &BookSnapshot,
    anchor: &Anchor,
    section_title: Option<&str>,
    lost: bool,
    now: i64,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE annotations SET book_key = ?2, book_id = ?3, start_block = ?4,
             start_offset = ?5, end_block = ?6, end_offset = ?7, section_title = ?8,
             anchor_lost = ?9, updated_at = ?10
         WHERE id = ?1",
        params![
            id,
            book.key,
            book.id,
            anchor.start_block.ordinal(),
            anchor.start_offset,
            anchor.end_block.ordinal(),
            anchor.end_offset,
            section_title,
            lost,
            now
        ],
    )?;
    Ok(())
}

/// The rows of one book (content key) across both tables: what re-attaching and the
/// cleanup work on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// The content key.
    pub key: Vec<u8>,
    /// The most recent book id hint.
    pub book_id: i64,
    /// The most recent title snapshot.
    pub title: String,
    /// The most recent authors snapshot.
    pub authors: String,
    /// When the group was soft-deleted because its book was removed (all its rows are
    /// deleted then); `None` while it is live.
    pub removed_at: Option<i64>,
}

/// Every group with live rows or rows deleted because the book was removed (user
/// tombstones alone do not make a group).
pub fn groups(conn: &Connection) -> Result<Vec<Group>, StorageError> {
    let mut stmt = conn.prepare(
        "WITH rows AS (
             SELECT book_key, book_id, book_title, book_authors, updated_at, deleted_at
               FROM annotations
              WHERE deleted_reason IS NULL OR deleted_reason = 'book_removed'
             UNION ALL
             SELECT book_key, book_id, book_title, book_authors, updated_at, deleted_at
               FROM reading_positions
         ),
         ranked AS (
             SELECT *,
                    row_number() OVER (PARTITION BY book_key ORDER BY updated_at DESC) AS n,
                    count(*) FILTER (WHERE deleted_at IS NULL)
                        OVER (PARTITION BY book_key) AS live,
                    max(deleted_at) OVER (PARTITION BY book_key) AS removed_at
               FROM rows
         )
         SELECT book_key, book_id, book_title, book_authors,
                CASE WHEN live = 0 THEN removed_at END
           FROM ranked WHERE n = 1 ORDER BY book_key",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Group {
            key: row.get(0)?,
            book_id: row.get(1)?,
            title: row.get(2)?,
            authors: row.get(3)?,
            removed_at: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Soft-deletes the live rows of a group because its book was removed.
pub fn remove_group(conn: &Connection, key: &[u8], now: i64) -> Result<(), StorageError> {
    for table in ["annotations", "reading_positions"] {
        conn.execute(
            &format!(
                "UPDATE {table} SET deleted_at = ?2, deleted_reason = 'book_removed'
                 WHERE book_key = ?1 AND deleted_at IS NULL"
            ),
            params![key, now],
        )?;
    }
    Ok(())
}

/// Restores the rows of a group deleted because its book was removed.
pub fn restore_group(conn: &Connection, key: &[u8]) -> Result<(), StorageError> {
    for table in ["annotations", "reading_positions"] {
        conn.execute(
            &format!(
                "UPDATE {table} SET deleted_at = NULL, deleted_reason = NULL
                 WHERE book_key = ?1 AND deleted_reason = 'book_removed'"
            ),
            [key],
        )?;
    }
    Ok(())
}

/// Deletes for good the rows deleted because their book was removed, at or before
/// `cutoff`; returns how many. User tombstones are never purged.
pub fn purge_removed(conn: &Connection, cutoff: i64) -> Result<u64, StorageError> {
    let mut purged = 0;
    for table in ["annotations", "reading_positions"] {
        purged += conn.execute(
            &format!(
                "DELETE FROM {table}
                 WHERE deleted_reason = 'book_removed' AND deleted_at <= ?1"
            ),
            [cutoff],
        )? as u64;
    }
    Ok(purged)
}

/// The reading position in a book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadingPosition {
    /// The book (content key and snapshot).
    pub book: BookSnapshot,
    /// The topmost visible block.
    pub block: BlockId,
    /// The start of the block (fingerprint for re-attaching).
    pub suffix: String,
    /// Progress through the main text in per mille (1000 = finished).
    pub progress: u16,
    /// When it was saved (Unix seconds).
    pub updated_at: i64,
}

/// Saves a reading position; the latest `updated_at` wins.
pub fn save_position(conn: &Connection, position: &ReadingPosition) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO reading_positions
             (book_key, book_id, block, offset, suffix, progress, book_title, book_authors,
              updated_at)
         VALUES (?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (book_key) DO UPDATE SET
             book_id = excluded.book_id, block = excluded.block, offset = 0,
             suffix = excluded.suffix, progress = excluded.progress,
             book_title = excluded.book_title, book_authors = excluded.book_authors,
             updated_at = excluded.updated_at, deleted_at = NULL, deleted_reason = NULL
         WHERE excluded.updated_at >= reading_positions.updated_at
            OR reading_positions.deleted_at IS NOT NULL",
        params![
            position.book.key,
            position.book.id,
            position.block.ordinal(),
            position.suffix,
            position.progress,
            position.book.title,
            position.book.authors,
            position.updated_at
        ],
    )?;
    Ok(())
}

/// The live reading position of a book.
pub fn position(conn: &Connection, key: &[u8]) -> Result<Option<ReadingPosition>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT book_key, book_id, book_title, book_authors, block, suffix, progress,
                    updated_at
               FROM reading_positions WHERE book_key = ?1 AND deleted_at IS NULL",
            [key],
            |row| {
                Ok(ReadingPosition {
                    book: BookSnapshot {
                        key: row.get(0)?,
                        id: row.get(1)?,
                        title: row.get(2)?,
                        authors: row.get(3)?,
                    },
                    block: block(row.get(4)?),
                    suffix: row.get(5)?,
                    progress: row.get(6)?,
                    updated_at: row.get(7)?,
                })
            },
        )
        .optional()?)
}

/// Moves a group's reading position to another book (re-attaching), unless that book
/// has a newer one.
pub fn move_position(
    conn: &Connection,
    from: &[u8],
    to: &BookSnapshot,
    block: BlockId,
    progress: u16,
) -> Result<(), StorageError> {
    let Some(old) = position(conn, from)? else {
        return Ok(());
    };
    conn.execute("DELETE FROM reading_positions WHERE book_key = ?1", [from])?;
    save_position(
        conn,
        &ReadingPosition {
            book: to.clone(),
            block,
            suffix: old.suffix,
            progress,
            updated_at: old.updated_at,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(n: u32) -> BlockId {
        BlockId::new(n).unwrap()
    }

    fn snapshot(key: &[u8]) -> BookSnapshot {
        BookSnapshot {
            key: key.to_vec(),
            id: 7,
            title: "Title".into(),
            authors: "Author".into(),
        }
    }

    fn new(key: &[u8], block: u32, text: &str) -> NewAnnotation {
        NewAnnotation {
            book: snapshot(key),
            anchor: Anchor {
                start_block: b(block),
                start_offset: 1,
                end_block: b(block),
                end_offset: 1 + text.chars().count() as u32,
            },
            quote: Quote {
                text: text.into(),
                prefix: "p".into(),
                suffix: "s".into(),
            },
            section_title: Some("Chapter".into()),
            note: None,
        }
    }

    #[test]
    fn round_trip_and_book_order() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let later = insert(&conn, &new(b"k", 9, "later"), 10).unwrap();
        let first = insert(&conn, &new(b"k", 2, "first"), 11).unwrap();
        let mark = insert(&conn, &new(b"k", 5, ""), 12).unwrap();
        insert(&conn, &new(b"other", 1, "x"), 12).unwrap();
        assert_eq!(later.id.len(), 36, "a UUID");
        assert_ne!(later.id, first.id);
        let list = for_book(&conn, b"k").unwrap();
        let ids: Vec<&str> = list.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, [&first.id, &mark.id, &later.id]);
        assert_eq!(list[0], first);
        assert_eq!(first.quote.text, "first");
        assert_eq!(first.section_title.as_deref(), Some("Chapter"));
        assert_eq!(first.created_at, 11);
        assert_eq!(counts(&conn, b"k").unwrap(), (1, 2));
    }

    #[test]
    fn notes_and_soft_delete() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let a = insert(&conn, &new(b"k", 2, "text"), 1).unwrap();
        let edited = set_note(&conn, &a.id, Some("my note"), 5).unwrap().unwrap();
        assert_eq!(edited.note.as_deref(), Some("my note"));
        assert_eq!(edited.updated_at, 5);
        assert_eq!(edited.quote, a.quote, "the text is immutable");
        let cleared = set_note(&conn, &a.id, Some("  "), 6).unwrap().unwrap();
        assert_eq!(cleared.note, None);
        let deleted = delete(&conn, &a.id, 7).unwrap().unwrap();
        assert_eq!(deleted.deleted_reason, Some(DeletedReason::User));
        assert!(for_book(&conn, b"k").unwrap().is_empty());
        assert_eq!(counts(&conn, b"k").unwrap(), (0, 0));
        assert!(delete(&conn, &a.id, 8).unwrap().is_none());
        assert!(set_note(&conn, &a.id, Some("x"), 8).unwrap().is_none());
        assert!(set_note(&conn, "missing", Some("x"), 8).unwrap().is_none());
        // The tombstone is kept.
        assert!(get(&conn, &a.id).unwrap().unwrap().deleted_at.is_some());
    }

    #[test]
    fn groups_removal_restore_and_purge() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let a = insert(&conn, &new(b"gone", 2, "a"), 1).unwrap();
        let tomb = insert(&conn, &new(b"gone", 3, "b"), 1).unwrap();
        delete(&conn, &tomb.id, 2).unwrap();
        insert(&conn, &new(b"live", 2, "c"), 1).unwrap();
        let only_tombstones = insert(&conn, &new(b"tomb", 2, "d"), 1).unwrap();
        delete(&conn, &only_tombstones.id, 2).unwrap();
        let keys = |conn: &Connection| -> Vec<(Vec<u8>, Option<i64>)> {
            groups(conn)
                .unwrap()
                .into_iter()
                .map(|g| (g.key, g.removed_at))
                .collect()
        };
        assert_eq!(
            keys(&conn),
            [(b"gone".to_vec(), None), (b"live".to_vec(), None)]
        );

        remove_group(&conn, b"gone", 100).unwrap();
        assert!(for_book(&conn, b"gone").unwrap().is_empty());
        assert_eq!(keys(&conn)[0], (b"gone".to_vec(), Some(100)));
        restore_group(&conn, b"gone").unwrap();
        assert_eq!(for_book(&conn, b"gone").unwrap()[0].id, a.id);
        assert!(
            get(&conn, &tomb.id).unwrap().unwrap().deleted_at.is_some(),
            "user tombstones stay deleted"
        );

        remove_group(&conn, b"gone", 100).unwrap();
        assert_eq!(purge_removed(&conn, 99).unwrap(), 0);
        assert_eq!(purge_removed(&conn, 100).unwrap(), 1);
        assert!(get(&conn, &a.id).unwrap().is_none());
        assert!(get(&conn, &tomb.id).unwrap().is_some(), "never purged");
        assert_eq!(keys(&conn), [(b"live".to_vec(), None)]);
    }

    fn position_at(key: &[u8], block: u32, at: i64) -> ReadingPosition {
        ReadingPosition {
            book: snapshot(key),
            block: b(block),
            suffix: "start".into(),
            progress: 250,
            updated_at: at,
        }
    }

    #[test]
    fn positions_last_write_wins() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        assert_eq!(position(&conn, b"k").unwrap(), None);
        save_position(&conn, &position_at(b"k", 5, 10)).unwrap();
        save_position(&conn, &position_at(b"k", 9, 20)).unwrap();
        assert_eq!(position(&conn, b"k").unwrap().unwrap().block, b(9));
        save_position(&conn, &position_at(b"k", 3, 15)).unwrap();
        assert_eq!(
            position(&conn, b"k").unwrap().unwrap().block,
            b(9),
            "an older write loses"
        );
        save_position(&conn, &position_at(b"k", 4, 20)).unwrap();
        assert_eq!(position(&conn, b"k").unwrap().unwrap().block, b(4));

        // Positions form groups and follow removal and restore.
        assert_eq!(groups(&conn).unwrap().len(), 1);
        remove_group(&conn, b"k", 50).unwrap();
        assert_eq!(position(&conn, b"k").unwrap(), None);
        restore_group(&conn, b"k").unwrap();
        assert!(position(&conn, b"k").unwrap().is_some());

        let to = snapshot(b"new");
        move_position(&conn, b"k", &to, b(6), 300).unwrap();
        assert_eq!(position(&conn, b"k").unwrap(), None);
        let moved = position(&conn, b"new").unwrap().unwrap();
        assert_eq!(
            (moved.block, moved.progress, moved.updated_at),
            (b(6), 300, 20)
        );
    }

    #[test]
    fn file_database_is_migrated_and_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("userdata.sqlite");
        let id = {
            let data = UserData::open(&path).unwrap();
            insert(&data.lock(), &new(b"k", 2, "kept"), 1).unwrap().id
        };
        let data = UserData::open(&path).unwrap();
        let conn = data.lock();
        let version: u32 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        assert!(get(&conn, &id).unwrap().is_some());
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("userdata.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);
        let err = UserData::open(&path).unwrap_err();
        assert!(matches!(err, StorageError::NewerSchema { .. }), "{err}");
    }
}
