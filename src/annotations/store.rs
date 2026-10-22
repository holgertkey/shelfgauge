//! The user-data database, `.shelfgauge/userdata.sqlite`: profiles, annotations and
//! reading positions.
//!
//! It is separate from the index (`library.db`), has its own `user_version`
//! migrations and is used through one connection behind a mutex ([`UserData`]): the
//! data is small and writes are rare. Functions take a [`Connection`] and never open a
//! transaction of their own unless they say so.
//!
//! Annotations and positions belong to a profile ([`ProfileId`]); the functions for one
//! person take it, and an id of another profile's annotation behaves as "not found".
//! The library-wide maintenance (groups, removal, restore, purge, re-anchoring) works
//! on content keys for all profiles at once.
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
use crate::profiles::ProfileId;
use crate::storage::{StorageError, migrations, open_wal};

/// Schema migrations of the user-data database; index `i` upgrades version `i` to
/// `i + 1`. Append only: user data cannot be rebuilt.
const MIGRATIONS: &[&str] = &[V1, V2, V3, V4, V5];

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

/// Version 2: `reading_positions.dismissed_at`, set when a book is removed from the
/// "Currently reading" list; the next saved position clears it.
const V2: &str = r#"
ALTER TABLE reading_positions ADD COLUMN dismissed_at INTEGER;
"#;

/// Version 3: profiles (see [`crate::profiles`]) and their sessions; annotations and
/// reading positions get a `profile_id`, the existing rows go to the owner profile
/// (id 1, created here).
///
/// Both tables are rebuilt (SQLite's documented procedure): a column with a
/// `REFERENCES` clause cannot be added with a non-NULL default while foreign keys are
/// on. `profiles.name` is unique ignoring ASCII case; the store also compares names
/// with Unicode lowercasing.
const V3: &str = r#"
CREATE TABLE profiles (
    id             INTEGER PRIMARY KEY,
    name           TEXT    NOT NULL COLLATE NOCASE UNIQUE,
    color          INTEGER NOT NULL DEFAULT 0 CHECK (color BETWEEN 0 AND 7),
    role           TEXT    NOT NULL CHECK (role IN ('admin', 'reader')),
    pin_hash       TEXT,
    share_activity INTEGER NOT NULL DEFAULT 0 CHECK (share_activity IN (0, 1)),
    settings       TEXT,
    sort           INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL
);

INSERT INTO profiles (id, name, color, role, created_at) VALUES (1, 'Me', 0, 'admin', unixepoch());

CREATE TABLE profile_sessions (
    token_hash   BLOB    PRIMARY KEY,
    profile_id   INTEGER NOT NULL REFERENCES profiles (id) ON DELETE CASCADE,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);

CREATE INDEX profile_sessions_profile ON profile_sessions (profile_id);

CREATE TABLE annotations_new (
    id             TEXT    PRIMARY KEY,
    profile_id     INTEGER NOT NULL REFERENCES profiles (id) ON DELETE CASCADE,
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

INSERT INTO annotations_new
    SELECT id, 1, book_key, book_id, start_block, start_offset, end_block, end_offset, text,
           prefix, suffix, section_title, book_title, book_authors, anchor_lost, note,
           created_at, updated_at, deleted_at, deleted_reason
      FROM annotations;
DROP TABLE annotations;
ALTER TABLE annotations_new RENAME TO annotations;

CREATE INDEX annotations_book ON annotations (profile_id, book_key, start_block, start_offset);
CREATE INDEX annotations_key ON annotations (book_key);

CREATE TABLE reading_positions_new (
    profile_id     INTEGER NOT NULL REFERENCES profiles (id) ON DELETE CASCADE,
    book_key       BLOB    NOT NULL,
    book_id        INTEGER NOT NULL,
    block          INTEGER NOT NULL,
    offset         INTEGER NOT NULL DEFAULT 0,
    suffix         TEXT    NOT NULL,
    progress       INTEGER NOT NULL,
    book_title     TEXT    NOT NULL,
    book_authors   TEXT    NOT NULL,
    updated_at     INTEGER NOT NULL,
    deleted_at     INTEGER,
    deleted_reason TEXT CHECK (deleted_reason IN ('book_removed')),
    dismissed_at   INTEGER,
    PRIMARY KEY (profile_id, book_key)
);

INSERT INTO reading_positions_new
    SELECT 1, book_key, book_id, block, offset, suffix, progress, book_title, book_authors,
           updated_at, deleted_at, deleted_reason, dismissed_at
      FROM reading_positions;
DROP TABLE reading_positions;
ALTER TABLE reading_positions_new RENAME TO reading_positions;

CREATE INDEX reading_positions_recent ON reading_positions (profile_id, updated_at);
CREATE INDEX reading_positions_key ON reading_positions (book_key);
"#;

/// Version 4: profile passwords (login from other devices) and session levels.
///
/// - `password_hash` is an argon2id PHC string; `password_changed_at` Unix seconds.
/// - `level`: `0` = a local session (no secret or a PIN), `1` = logged in with the
///   password; `remote`: created by a request from another device. Existing sessions
///   become local ones, so they never serve a remote request.
const V4: &str = r#"
ALTER TABLE profiles ADD COLUMN password_hash TEXT;
ALTER TABLE profiles ADD COLUMN password_changed_at INTEGER;
ALTER TABLE profile_sessions ADD COLUMN level INTEGER NOT NULL DEFAULT 0 CHECK (level IN (0, 1));
ALTER TABLE profile_sessions ADD COLUMN remote INTEGER NOT NULL DEFAULT 0 CHECK (remote IN (0, 1));
"#;

/// Version 5: the hub (see [`crate::hub_client`]).
///
/// - `hub_accounts`: a profile's hub account (at most one): the hub it was made on,
///   the public account id (16 bytes), the account key (32 bytes) and its id, this
///   instance's device id and token, the recovery code until the person confirms they
///   saved it, and the sync state (`last_seq` pulled, last success and error).
/// - `hub_shelf`: the books on the profile's shelf, from this instance or pulled from
///   the account's other instances. `book_key` is the content key; `publication_id`
///   and `cipher_key` (the publication's book key) are set once it is published;
///   `own` = published by this instance; `state`: `uploading`, `published`, `expired`
///   or `removing` (the hub's copy is still to be deleted); `updated_at` orders
///   changes of the shelf item.
/// - `hub_outbox`: changes still to push, by kind and key (an annotation id as text,
///   a content key as blob). Triggers add annotations and positions of shelf books;
///   `book_removed` deletions are not pushed (the book may come back).
const V5: &str = r#"
CREATE TABLE hub_accounts (
    profile_id    INTEGER PRIMARY KEY REFERENCES profiles (id) ON DELETE CASCADE,
    hub_url       TEXT    NOT NULL,
    account_id    BLOB    NOT NULL,
    account_key   BLOB    NOT NULL,
    key_id        INTEGER NOT NULL DEFAULT 1,
    device_id     INTEGER NOT NULL,
    device_token  TEXT    NOT NULL,
    recovery_code TEXT,
    created_at    INTEGER NOT NULL,
    last_seq      INTEGER NOT NULL DEFAULT 0,
    synced_at     INTEGER,
    sync_error    TEXT
);

CREATE TABLE hub_shelf (
    profile_id     INTEGER NOT NULL REFERENCES hub_accounts (profile_id) ON DELETE CASCADE,
    book_key       BLOB    NOT NULL,
    publication_id BLOB,
    cipher_key     BLOB,
    own            INTEGER NOT NULL DEFAULT 0 CHECK (own IN (0, 1)),
    kind           TEXT    NOT NULL DEFAULT 'upload' CHECK (kind IN ('upload', 'file')),
    state          TEXT    NOT NULL CHECK (state IN ('uploading', 'published', 'expired', 'removing')),
    instance       TEXT    NOT NULL,
    title          TEXT    NOT NULL,
    authors        TEXT    NOT NULL,
    published_at   INTEGER,
    updated_at     INTEGER NOT NULL,
    PRIMARY KEY (profile_id, book_key)
);

CREATE TABLE hub_outbox (
    profile_id INTEGER NOT NULL REFERENCES hub_accounts (profile_id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL CHECK (kind IN ('shelf_item', 'annotation', 'position')),
    key        ANY     NOT NULL,
    PRIMARY KEY (profile_id, kind, key)
) STRICT;

CREATE TRIGGER hub_outbox_annotation_insert AFTER INSERT ON annotations
WHEN NEW.deleted_reason IS NOT 'book_removed' AND EXISTS (
    SELECT 1 FROM hub_shelf s
     WHERE s.profile_id = NEW.profile_id AND s.book_key = NEW.book_key AND s.state <> 'removing')
BEGIN
    INSERT OR IGNORE INTO hub_outbox (profile_id, kind, key) VALUES (NEW.profile_id, 'annotation', NEW.id);
END;

CREATE TRIGGER hub_outbox_annotation_update AFTER UPDATE ON annotations
WHEN NEW.deleted_reason IS NOT 'book_removed' AND EXISTS (
    SELECT 1 FROM hub_shelf s
     WHERE s.profile_id = NEW.profile_id AND s.book_key = NEW.book_key AND s.state <> 'removing')
BEGIN
    INSERT OR IGNORE INTO hub_outbox (profile_id, kind, key) VALUES (NEW.profile_id, 'annotation', NEW.id);
END;

CREATE TRIGGER hub_outbox_position_insert AFTER INSERT ON reading_positions
WHEN NEW.deleted_at IS NULL AND EXISTS (
    SELECT 1 FROM hub_shelf s
     WHERE s.profile_id = NEW.profile_id AND s.book_key = NEW.book_key AND s.state <> 'removing')
BEGIN
    INSERT OR IGNORE INTO hub_outbox (profile_id, kind, key) VALUES (NEW.profile_id, 'position', NEW.book_key);
END;

CREATE TRIGGER hub_outbox_position_update AFTER UPDATE ON reading_positions
WHEN NEW.deleted_at IS NULL AND EXISTS (
    SELECT 1 FROM hub_shelf s
     WHERE s.profile_id = NEW.profile_id AND s.book_key = NEW.book_key AND s.state <> 'removing')
BEGIN
    INSERT OR IGNORE INTO hub_outbox (profile_id, kind, key) VALUES (NEW.profile_id, 'position', NEW.book_key);
END;
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
    /// use shelfgauge::profiles::ProfileId;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let data = UserData::open(&dir.path().join("userdata.sqlite")).unwrap();
    /// assert!(for_book(&data.lock(), ProfileId::OWNER, b"key").unwrap().is_empty());
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
    note, created_at, updated_at, deleted_at, deleted_reason, profile_id";

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
        profile: row.get(19)?,
    })
}

/// A note as stored: blank notes are `NULL`.
fn stored_note(note: Option<&str>) -> Option<&str> {
    note.filter(|n| !n.trim().is_empty())
}

/// Stores a new annotation of `profile` with a fresh UUID and returns it.
pub fn insert(
    conn: &Connection,
    profile: ProfileId,
    new: &NewAnnotation,
    now: i64,
) -> Result<Annotation, StorageError> {
    let id = uuid::Uuid::new_v4().to_string();
    conn.execute(
        &format!(
            "INSERT INTO annotations ({COLUMNS}) VALUES
             (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0, ?14, ?15, ?15,
              NULL, NULL, ?16)"
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
            profile,
        ],
    )?;
    Ok(get(conn, profile, &id)?.expect("the row was just inserted"))
}

/// The annotation of `profile` with `id`, deleted or not.
pub fn get(
    conn: &Connection,
    profile: ProfileId,
    id: &str,
) -> Result<Option<Annotation>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM annotations WHERE id = ?1 AND profile_id = ?2"),
            params![id, profile],
            annotation_row,
        )
        .optional()?)
}

/// Replaces the note of a live annotation of `profile`; returns it, or `None` if
/// `profile` has no such live annotation.
pub fn set_note(
    conn: &Connection,
    profile: ProfileId,
    id: &str,
    note: Option<&str>,
    now: i64,
) -> Result<Option<Annotation>, StorageError> {
    let changed = conn.execute(
        "UPDATE annotations SET note = ?2, updated_at = ?3
         WHERE id = ?1 AND profile_id = ?4 AND deleted_at IS NULL",
        params![id, stored_note(note), now, profile],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get(conn, profile, id)
}

/// Deletes a live annotation of `profile` (soft, reason [`DeletedReason::User`]);
/// returns it, or `None` if `profile` has no such live annotation.
pub fn delete(
    conn: &Connection,
    profile: ProfileId,
    id: &str,
    now: i64,
) -> Result<Option<Annotation>, StorageError> {
    let changed = conn.execute(
        "UPDATE annotations SET deleted_at = ?2, deleted_reason = 'user', updated_at = ?2
         WHERE id = ?1 AND profile_id = ?3 AND deleted_at IS NULL",
        params![id, now, profile],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    get(conn, profile, id)
}

const BOOK_ORDER: &str = "start_block, start_offset, end_block, end_offset, created_at, id";

/// The live annotations of `profile` in a book, in book order.
pub fn for_book(
    conn: &Connection,
    profile: ProfileId,
    key: &[u8],
) -> Result<Vec<Annotation>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM annotations
         WHERE profile_id = ?1 AND book_key = ?2 AND deleted_at IS NULL
         ORDER BY {BOOK_ORDER}"
    ))?;
    let rows = stmt.query_map(params![profile, key], annotation_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The live annotations of every profile in a book, in book order (re-attaching).
pub fn for_key(conn: &Connection, key: &[u8]) -> Result<Vec<Annotation>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM annotations
         WHERE book_key = ?1 AND deleted_at IS NULL
         ORDER BY {BOOK_ORDER}"
    ))?;
    let rows = stmt.query_map([key], annotation_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The number of live bookmarks and marks of `profile` in a book.
pub fn counts(
    conn: &Connection,
    profile: ProfileId,
    key: &[u8],
) -> Result<(u64, u64), StorageError> {
    Ok(conn.query_row(
        "SELECT count(*) FILTER (WHERE text = ''), count(*) FILTER (WHERE text != '')
         FROM annotations WHERE profile_id = ?1 AND book_key = ?2 AND deleted_at IS NULL",
        params![profile, key],
        |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
    )?)
}

/// The number of live bookmarks and marks of `profile` in every book that has any:
/// `(book key, bookmarks, marks)`.
pub fn counts_by_book(
    conn: &Connection,
    profile: ProfileId,
) -> Result<Vec<(Vec<u8>, u64, u64)>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT book_key, count(*) FILTER (WHERE text = ''), count(*) FILTER (WHERE text != '')
         FROM annotations WHERE profile_id = ?1 AND deleted_at IS NULL GROUP BY book_key",
    )?;
    let rows = stmt.query_map([profile], |row| {
        Ok((
            row.get(0)?,
            row.get::<_, i64>(1)? as u64,
            row.get::<_, i64>(2)? as u64,
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Live marks (not bookmarks) of `profile`, most recently created first.
pub fn latest_marks(
    conn: &Connection,
    profile: ProfileId,
    limit: u32,
) -> Result<Vec<Annotation>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM annotations
         WHERE profile_id = ?1 AND text != '' AND deleted_at IS NULL
         ORDER BY created_at DESC, id LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![profile, limit], annotation_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The live reading positions of `profile` in all books: `(book key, progress in per
/// mille)`.
pub fn progress_by_book(
    conn: &Connection,
    profile: ProfileId,
) -> Result<Vec<(Vec<u8>, u16)>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT book_key, progress FROM reading_positions
          WHERE profile_id = ?1 AND deleted_at IS NULL",
    )?;
    let rows = stmt.query_map([profile], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Moves an annotation (of any profile) to a new place (and book), after
/// re-attaching.
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

/// The rows of one book (content key) across both tables and all profiles: what
/// re-attaching and the cleanup work on.
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
/// tombstones alone do not make a group), over all profiles.
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

/// Soft-deletes the live rows of a group (all profiles) because its book was removed.
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

/// Restores the rows of a group (all profiles) deleted because its book was removed.
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

/// Deletes for good the rows (of all profiles) deleted because their book was
/// removed, at or before `cutoff`; returns how many. User tombstones are never purged.
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

/// Saves a reading position of `profile`; the latest `updated_at` wins. A saved
/// position puts the book back on the profile's "Currently reading" list.
pub fn save_position(
    conn: &Connection,
    profile: ProfileId,
    position: &ReadingPosition,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO reading_positions
             (profile_id, book_key, book_id, block, offset, suffix, progress, book_title,
              book_authors, updated_at)
         VALUES (?9, ?1, ?2, ?3, 0, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT (profile_id, book_key) DO UPDATE SET
             book_id = excluded.book_id, block = excluded.block, offset = 0,
             suffix = excluded.suffix, progress = excluded.progress,
             book_title = excluded.book_title, book_authors = excluded.book_authors,
             updated_at = excluded.updated_at, deleted_at = NULL, deleted_reason = NULL,
             dismissed_at = NULL
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
            position.updated_at,
            profile,
        ],
    )?;
    Ok(())
}

const POSITION_COLUMNS: &str =
    "book_key, book_id, book_title, book_authors, block, suffix, progress, updated_at";

/// The live reading position of `profile` in a book.
pub fn position(
    conn: &Connection,
    profile: ProfileId,
    key: &[u8],
) -> Result<Option<ReadingPosition>, StorageError> {
    Ok(conn
        .query_row(
            &format!(
                "SELECT {POSITION_COLUMNS} FROM reading_positions
                  WHERE profile_id = ?1 AND book_key = ?2 AND deleted_at IS NULL"
            ),
            params![profile, key],
            position_row,
        )
        .optional()?)
}

/// The live reading positions of every profile in a book (re-attaching).
pub fn positions_for_key(
    conn: &Connection,
    key: &[u8],
) -> Result<Vec<(ProfileId, ReadingPosition)>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {POSITION_COLUMNS}, profile_id FROM reading_positions
          WHERE book_key = ?1 AND deleted_at IS NULL ORDER BY profile_id"
    ))?;
    let rows = stmt.query_map([key], |row| Ok((row.get(8)?, position_row(row)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

fn position_row(row: &Row<'_>) -> rusqlite::Result<ReadingPosition> {
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
}

/// Moves the reading position of `profile` in a group to another book (re-attaching),
/// unless that book has a newer one.
pub fn move_position(
    conn: &Connection,
    profile: ProfileId,
    from: &[u8],
    to: &BookSnapshot,
    block: BlockId,
    progress: u16,
) -> Result<(), StorageError> {
    let Some(old) = position(conn, profile, from)? else {
        return Ok(());
    };
    let dismissed: Option<i64> = conn.query_row(
        "SELECT dismissed_at FROM reading_positions WHERE profile_id = ?1 AND book_key = ?2",
        params![profile, from],
        |row| row.get(0),
    )?;
    conn.execute(
        "DELETE FROM reading_positions WHERE profile_id = ?1 AND book_key = ?2",
        params![profile, from],
    )?;
    save_position(
        conn,
        profile,
        &ReadingPosition {
            book: to.clone(),
            block,
            suffix: old.suffix,
            progress,
            updated_at: old.updated_at,
        },
    )?;
    // Moving is not reading: a dismissed book stays off the list.
    conn.execute(
        "UPDATE reading_positions SET dismissed_at = ?3 WHERE profile_id = ?1 AND book_key = ?2",
        params![profile, to.key, dismissed],
    )?;
    Ok(())
}

/// The "Currently reading" list of `profile`: live positions of unfinished books not
/// dismissed from the list, most recently read first.
pub fn reading_list(
    conn: &Connection,
    profile: ProfileId,
) -> Result<Vec<ReadingPosition>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {POSITION_COLUMNS} FROM reading_positions
          WHERE profile_id = ?1 AND deleted_at IS NULL AND dismissed_at IS NULL
            AND progress < 1000
          ORDER BY updated_at DESC, book_key"
    ))?;
    let rows = stmt.query_map([profile], position_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// What the other profiles are reading, for "Family is reading": the "Currently
/// reading" lists of the profiles that share their activity, except `profile`'s own;
/// most recently read first. Never annotations.
pub fn shared_reading(
    conn: &Connection,
    profile: ProfileId,
) -> Result<Vec<(ProfileId, ReadingPosition)>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT r.book_key, r.book_id, r.book_title, r.book_authors, r.block, r.suffix,
                r.progress, r.updated_at, r.profile_id
           FROM reading_positions r JOIN profiles p ON p.id = r.profile_id
          WHERE p.share_activity = 1 AND r.profile_id != ?1 AND r.deleted_at IS NULL
            AND r.dismissed_at IS NULL AND r.progress < 1000
          ORDER BY r.updated_at DESC, r.profile_id, r.book_key",
    )?;
    let rows = stmt.query_map([profile], |row| Ok((row.get(8)?, position_row(row)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Removes every book from the "Currently reading" list of `profile`; the positions
/// are kept.
pub fn dismiss_all(conn: &Connection, profile: ProfileId, now: i64) -> Result<u64, StorageError> {
    Ok(conn.execute(
        "UPDATE reading_positions SET dismissed_at = ?1
          WHERE profile_id = ?2 AND dismissed_at IS NULL AND deleted_at IS NULL",
        params![now, profile],
    )? as u64)
}

/// Marks a book as finished for `profile` (progress 1000, which takes it off the
/// "Currently reading" list); its position is kept. Returns whether the book had a
/// position.
pub fn mark_finished(
    conn: &Connection,
    profile: ProfileId,
    key: &[u8],
    now: i64,
) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "UPDATE reading_positions SET progress = 1000, updated_at = ?2
          WHERE profile_id = ?3 AND book_key = ?1 AND deleted_at IS NULL",
        params![key, now, profile],
    )? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: ProfileId = ProfileId::OWNER;

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
        let later = insert(&conn, P, &new(b"k", 9, "later"), 10).unwrap();
        let first = insert(&conn, P, &new(b"k", 2, "first"), 11).unwrap();
        let mark = insert(&conn, P, &new(b"k", 5, ""), 12).unwrap();
        insert(&conn, P, &new(b"other", 1, "x"), 12).unwrap();
        assert_eq!(later.id.len(), 36, "a UUID");
        assert_ne!(later.id, first.id);
        let list = for_book(&conn, P, b"k").unwrap();
        let ids: Vec<&str> = list.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, [&first.id, &mark.id, &later.id]);
        assert_eq!(list[0], first);
        assert_eq!(first.quote.text, "first");
        assert_eq!(first.section_title.as_deref(), Some("Chapter"));
        assert_eq!(first.created_at, 11);
        assert_eq!(counts(&conn, P, b"k").unwrap(), (1, 2));
    }

    #[test]
    fn notes_and_soft_delete() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let a = insert(&conn, P, &new(b"k", 2, "text"), 1).unwrap();
        let edited = set_note(&conn, P, &a.id, Some("my note"), 5)
            .unwrap()
            .unwrap();
        assert_eq!(edited.note.as_deref(), Some("my note"));
        assert_eq!(edited.updated_at, 5);
        assert_eq!(edited.quote, a.quote, "the text is immutable");
        let cleared = set_note(&conn, P, &a.id, Some("  "), 6).unwrap().unwrap();
        assert_eq!(cleared.note, None);
        let deleted = delete(&conn, P, &a.id, 7).unwrap().unwrap();
        assert_eq!(deleted.deleted_reason, Some(DeletedReason::User));
        assert!(for_book(&conn, P, b"k").unwrap().is_empty());
        assert_eq!(counts(&conn, P, b"k").unwrap(), (0, 0));
        assert!(delete(&conn, P, &a.id, 8).unwrap().is_none());
        assert!(set_note(&conn, P, &a.id, Some("x"), 8).unwrap().is_none());
        assert!(
            set_note(&conn, P, "missing", Some("x"), 8)
                .unwrap()
                .is_none()
        );
        // The tombstone is kept.
        assert!(get(&conn, P, &a.id).unwrap().unwrap().deleted_at.is_some());
    }

    #[test]
    fn groups_removal_restore_and_purge() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let a = insert(&conn, P, &new(b"gone", 2, "a"), 1).unwrap();
        let tomb = insert(&conn, P, &new(b"gone", 3, "b"), 1).unwrap();
        delete(&conn, P, &tomb.id, 2).unwrap();
        insert(&conn, P, &new(b"live", 2, "c"), 1).unwrap();
        let only_tombstones = insert(&conn, P, &new(b"tomb", 2, "d"), 1).unwrap();
        delete(&conn, P, &only_tombstones.id, 2).unwrap();
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
        assert!(for_book(&conn, P, b"gone").unwrap().is_empty());
        assert_eq!(keys(&conn)[0], (b"gone".to_vec(), Some(100)));
        restore_group(&conn, b"gone").unwrap();
        assert_eq!(for_book(&conn, P, b"gone").unwrap()[0].id, a.id);
        assert!(
            get(&conn, P, &tomb.id)
                .unwrap()
                .unwrap()
                .deleted_at
                .is_some(),
            "user tombstones stay deleted"
        );

        remove_group(&conn, b"gone", 100).unwrap();
        assert_eq!(purge_removed(&conn, 99).unwrap(), 0);
        assert_eq!(purge_removed(&conn, 100).unwrap(), 1);
        assert!(get(&conn, P, &a.id).unwrap().is_none());
        assert!(get(&conn, P, &tomb.id).unwrap().is_some(), "never purged");
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
        assert_eq!(position(&conn, P, b"k").unwrap(), None);
        save_position(&conn, P, &position_at(b"k", 5, 10)).unwrap();
        save_position(&conn, P, &position_at(b"k", 9, 20)).unwrap();
        assert_eq!(position(&conn, P, b"k").unwrap().unwrap().block, b(9));
        save_position(&conn, P, &position_at(b"k", 3, 15)).unwrap();
        assert_eq!(
            position(&conn, P, b"k").unwrap().unwrap().block,
            b(9),
            "an older write loses"
        );
        save_position(&conn, P, &position_at(b"k", 4, 20)).unwrap();
        assert_eq!(position(&conn, P, b"k").unwrap().unwrap().block, b(4));

        // Positions form groups and follow removal and restore.
        assert_eq!(groups(&conn).unwrap().len(), 1);
        remove_group(&conn, b"k", 50).unwrap();
        assert_eq!(position(&conn, P, b"k").unwrap(), None);
        restore_group(&conn, b"k").unwrap();
        assert!(position(&conn, P, b"k").unwrap().is_some());

        let to = snapshot(b"new");
        move_position(&conn, P, b"k", &to, b(6), 300).unwrap();
        assert_eq!(position(&conn, P, b"k").unwrap(), None);
        let moved = position(&conn, P, b"new").unwrap().unwrap();
        assert_eq!(
            (moved.block, moved.progress, moved.updated_at),
            (b(6), 300, 20)
        );
    }

    #[test]
    fn reading_list_dismiss_and_finish() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        save_position(&conn, P, &position_at(b"a", 5, 10)).unwrap();
        save_position(&conn, P, &position_at(b"b", 5, 30)).unwrap();
        save_position(&conn, P, &position_at(b"c", 5, 20)).unwrap();
        let keys = |conn: &Connection| -> Vec<Vec<u8>> {
            reading_list(conn, P)
                .unwrap()
                .into_iter()
                .map(|p| p.book.key)
                .collect()
        };
        assert_eq!(keys(&conn), [b"b".to_vec(), b"c".to_vec(), b"a".to_vec()]);

        assert!(mark_finished(&conn, P, b"c", 40).unwrap());
        assert!(!mark_finished(&conn, P, b"x", 40).unwrap());
        assert_eq!(position(&conn, P, b"c").unwrap().unwrap().progress, 1000);
        assert_eq!(keys(&conn), [b"b".to_vec(), b"a".to_vec()]);

        assert_eq!(dismiss_all(&conn, P, 50).unwrap(), 3);
        assert!(keys(&conn).is_empty());
        assert!(
            position(&conn, P, b"a").unwrap().is_some(),
            "positions are kept"
        );
        // Reading again puts a book back.
        save_position(&conn, P, &position_at(b"a", 6, 60)).unwrap();
        assert_eq!(keys(&conn), [b"a".to_vec()]);
        // Re-attaching is not reading: a dismissed book stays off the list.
        move_position(&conn, P, b"b", &snapshot(b"moved"), b(2), 100).unwrap();
        assert_eq!(keys(&conn), [b"a".to_vec()]);
        // A deleted (removed book) position is not listed.
        remove_group(&conn, b"a", 70).unwrap();
        assert!(keys(&conn).is_empty());
    }

    #[test]
    fn version_1_positions_get_dismissed_at() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations::apply(&mut conn, &MIGRATIONS[..1]).unwrap();
        conn.execute(
            "INSERT INTO reading_positions (book_key, book_id, block, suffix, progress,
                 book_title, book_authors, updated_at)
             VALUES (x'01', 1, 3, 's', 10, 'T', 'A', 5)",
            [],
        )
        .unwrap();
        migrations::apply(&mut conn, MIGRATIONS).unwrap();
        assert_eq!(reading_list(&conn, P).unwrap().len(), 1);
    }

    #[test]
    fn version_2_rows_move_to_the_owner_profile() {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::storage::configure(&conn).unwrap();
        migrations::apply(&mut conn, &MIGRATIONS[..2]).unwrap();
        conn.execute_batch(
            "INSERT INTO annotations (id, book_key, book_id, start_block, start_offset,
                 end_block, end_offset, text, prefix, suffix, section_title, book_title,
                 book_authors, note, created_at, updated_at)
             VALUES ('a1', x'01', 1, 3, 0, 3, 4, 'text', 'p', 's', 'Ch', 'T', 'A', 'n', 5, 6);
             INSERT INTO annotations (id, book_key, book_id, start_block, start_offset,
                 end_block, end_offset, text, prefix, suffix, book_title, book_authors,
                 created_at, updated_at, deleted_at, deleted_reason)
             VALUES ('a2', x'02', 2, 1, 0, 1, 0, '', '', 's', 'T2', 'A2', 1, 2, 3, 'user');
             INSERT INTO reading_positions (book_key, book_id, block, suffix, progress,
                 book_title, book_authors, updated_at, dismissed_at)
             VALUES (x'01', 1, 3, 's', 10, 'T', 'A', 5, 7);
             INSERT INTO reading_positions (book_key, book_id, block, suffix, progress,
                 book_title, book_authors, updated_at)
             VALUES (x'02', 2, 4, 's', 20, 'T2', 'A2', 9);",
        )
        .unwrap();
        migrations::apply(&mut conn, MIGRATIONS).unwrap();

        let a = get(&conn, P, "a1").unwrap().unwrap();
        assert_eq!(a.profile, P);
        assert_eq!(
            (a.note.as_deref(), a.created_at, a.updated_at),
            (Some("n"), 5, 6)
        );
        let tomb = get(&conn, P, "a2").unwrap().unwrap();
        assert_eq!(tomb.deleted_reason, Some(DeletedReason::User));
        // The dismissed position stays dismissed; the other one is on the list.
        let list = reading_list(&conn, P).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].book.key, vec![2]);
        assert_eq!(position(&conn, P, &[1]).unwrap().unwrap().progress, 10);
        let profile_ids: Vec<i64> = conn
            .prepare("SELECT profile_id FROM reading_positions")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(profile_ids, [1, 1]);
        let violations: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
        // Positions are keyed by profile: another profile has its own on the same key.
        conn.execute(
            "INSERT INTO profiles (id, name, role, created_at) VALUES (2, 'Anna', 'reader', 0)",
            [],
        )
        .unwrap();
        let anna = ProfileId(2);
        save_position(&conn, anna, &position_at(&[1], 8, 100)).unwrap();
        assert_eq!(position(&conn, P, &[1]).unwrap().unwrap().block, b(3));
        assert_eq!(position(&conn, anna, &[1]).unwrap().unwrap().block, b(8));
    }

    #[test]
    fn profiles_are_kept_apart() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let anna =
            crate::profiles::store::create(&conn, "Anna", 1, crate::profiles::Role::Reader, 0)
                .unwrap()
                .id;
        let mine = insert(&conn, P, &new(b"k", 2, "mine"), 1).unwrap();
        let hers = insert(&conn, anna, &new(b"k", 3, "hers"), 2).unwrap();
        assert_eq!(
            for_book(&conn, P, b"k").unwrap(),
            std::slice::from_ref(&mine)
        );
        assert_eq!(
            for_book(&conn, anna, b"k").unwrap(),
            std::slice::from_ref(&hers)
        );
        assert_eq!(for_key(&conn, b"k").unwrap().len(), 2);
        assert_eq!(counts(&conn, anna, b"k").unwrap(), (0, 1));
        assert_eq!(
            latest_marks(&conn, P, 10).unwrap(),
            std::slice::from_ref(&mine)
        );
        assert_eq!(counts_by_book(&conn, anna).unwrap().len(), 1);
        // Another profile's annotation id is "not found".
        assert!(get(&conn, P, &hers.id).unwrap().is_none());
        assert!(
            set_note(&conn, P, &hers.id, Some("x"), 5)
                .unwrap()
                .is_none()
        );
        assert!(delete(&conn, P, &hers.id, 5).unwrap().is_none());
        assert!(
            get(&conn, anna, &hers.id)
                .unwrap()
                .unwrap()
                .deleted_at
                .is_none()
        );

        save_position(&conn, P, &position_at(b"k", 5, 10)).unwrap();
        save_position(&conn, anna, &position_at(b"k", 9, 20)).unwrap();
        assert_eq!(progress_by_book(&conn, anna).unwrap().len(), 1);
        assert_eq!(positions_for_key(&conn, b"k").unwrap().len(), 2);
        assert!(mark_finished(&conn, anna, b"k", 30).unwrap());
        assert_eq!(reading_list(&conn, P).unwrap().len(), 1);
        assert!(reading_list(&conn, anna).unwrap().is_empty());
        assert_eq!(dismiss_all(&conn, P, 40).unwrap(), 1);

        // Moving positions keeps each profile's own block and dismissal.
        save_position(&conn, anna, &position_at(b"k", 9, 50)).unwrap();
        let to = snapshot(b"new");
        move_position(&conn, P, b"k", &to, b(6), 300).unwrap();
        move_position(&conn, anna, b"k", &to, b(7), 400).unwrap();
        assert!(
            reading_list(&conn, P).unwrap().is_empty(),
            "still dismissed"
        );
        assert_eq!(reading_list(&conn, anna).unwrap()[0].block, b(7));

        // Library-wide maintenance covers both; deleting a profile removes its rows.
        remove_group(&conn, b"k", 60).unwrap();
        assert!(for_book(&conn, anna, b"k").unwrap().is_empty());
        restore_group(&conn, b"k").unwrap();
        assert_eq!(for_key(&conn, b"k").unwrap().len(), 2);
        remove_group(&conn, b"k", 60).unwrap();
        assert_eq!(purge_removed(&conn, 60).unwrap(), 2, "both profiles' rows");
        crate::profiles::store::delete(&conn, anna).unwrap();
        assert!(
            positions_for_key(&conn, b"new")
                .unwrap()
                .iter()
                .all(|(p, _)| *p == P)
        );
    }

    #[test]
    fn shared_reading_lists_only_sharing_profiles() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let reader = |name| {
            crate::profiles::store::create(&conn, name, 0, crate::profiles::Role::Reader, 0)
                .unwrap()
                .id
        };
        let (anna, ben) = (reader("Anna"), reader("Ben"));
        save_position(&conn, anna, &position_at(b"a", 1, 10)).unwrap();
        save_position(&conn, ben, &position_at(b"b", 1, 20)).unwrap();
        save_position(&conn, P, &position_at(b"c", 1, 30)).unwrap();
        assert!(
            shared_reading(&conn, P).unwrap().is_empty(),
            "off by default"
        );
        crate::profiles::store::set_sharing(&conn, anna, true).unwrap();
        crate::profiles::store::set_sharing(&conn, P, true).unwrap();
        let shared = shared_reading(&conn, P).unwrap();
        assert_eq!(shared.len(), 1);
        assert_eq!(
            (shared[0].0, shared[0].1.book.key.clone()),
            (anna, b"a".to_vec())
        );
        // Never the own reading.
        let for_anna = shared_reading(&conn, anna).unwrap();
        assert_eq!(for_anna.len(), 1);
        assert_eq!(for_anna[0].0, P);
        mark_finished(&conn, P, b"c", 40).unwrap();
        assert!(shared_reading(&conn, anna).unwrap().is_empty());
    }

    #[test]
    fn version_3_sessions_become_local_ones() {
        use crate::profiles::session::{self, Level, Token};
        use sha2::Digest as _;
        let mut conn = Connection::open_in_memory().unwrap();
        crate::storage::configure(&conn).unwrap();
        migrations::apply(&mut conn, &MIGRATIONS[..3]).unwrap();
        conn.execute_batch(
            "UPDATE profiles SET pin_hash = '$argon2id$pin' WHERE id = 1;
             INSERT INTO profile_sessions (token_hash, profile_id, created_at, last_seen_at)
             VALUES (x'aa', 1, 5, 6);",
        )
        .unwrap();
        // A session as 0.15.0 stored it: the SHA-256 of a known token.
        let token = Token::from_cookie("old-session");
        let hash: [u8; 32] = sha2::Sha256::digest(b"old-session").into();
        conn.execute(
            "INSERT INTO profile_sessions (token_hash, profile_id, created_at, last_seen_at)
             VALUES (?1, 1, 5, 6)",
            [&hash],
        )
        .unwrap();
        migrations::apply(&mut conn, MIGRATIONS).unwrap();

        let found = session::lookup(&conn, &token, 7).unwrap().unwrap();
        assert_eq!(
            (found.profile, found.level, found.remote),
            (P, Level::Local, false)
        );
        let owner = crate::profiles::store::get(&conn, P).unwrap().unwrap();
        assert!(owner.has_pin && !owner.has_password);
        let violations: i64 = conn
            .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0);
    }

    #[test]
    fn file_database_is_migrated_and_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("userdata.sqlite");
        let id = {
            let data = UserData::open(&path).unwrap();
            insert(&data.lock(), P, &new(b"k", 2, "kept"), 1)
                .unwrap()
                .id
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
        assert!(get(&conn, P, &id).unwrap().is_some());
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
