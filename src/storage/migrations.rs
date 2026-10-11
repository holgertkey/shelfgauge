//! Schema migrations, tracked with `PRAGMA user_version`.
//!
//! Each migration upgrades the schema by one version and is applied in a
//! transaction together with the `user_version` bump.
//!
//! Policy: until the first public release, schema v1 may still be edited in place
//! (existing local databases are then simply rebuilt). After a release, existing
//! migrations are frozen and changes are only ever appended as new migrations.

use rusqlite::Connection;

use super::StorageError;

/// Schema migrations; index `i` upgrades version `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[V1];

/// The schema version this build creates and understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// Version 1: the initial schema.
///
/// - Paths are relative to the library root, `/`-separated TEXT.
/// - `mtime_ns` is the modification time in nanoseconds since the Unix epoch, compared
///   for exact equality.
/// - A book's content identity is `content_hash` (plain files) or `entry_size` +
///   `entry_crc32` (zip entries); it stays with the book when the path is reused.
/// - Plain-file books have an empty `entry_name` (not NULL: NULLs are distinct in
///   unique indexes). Locations are unique among books that are not `removed`.
/// - `books_fts` is a contentful FTS5 table whose rowid is the book id; its text is
///   folded with [`fold_for_search`](super::fold_for_search) (`ё` → `е`).
const V1: &str = r#"
CREATE TABLE containers (
    id           INTEGER PRIMARY KEY,
    path         TEXT    NOT NULL UNIQUE,
    kind         TEXT    NOT NULL CHECK (kind IN ('file', 'zip')),
    size         INTEGER NOT NULL,
    mtime_ns     INTEGER NOT NULL,
    content_hash BLOB,
    present      INTEGER NOT NULL DEFAULT 1 CHECK (present IN (0, 1))
);

CREATE INDEX containers_content_hash ON containers (content_hash)
    WHERE content_hash IS NOT NULL;

CREATE TABLE books (
    id            INTEGER PRIMARY KEY,
    container_id  INTEGER NOT NULL REFERENCES containers (id),
    entry_name    BLOB    NOT NULL DEFAULT x'',
    entry_path    TEXT,
    entry_index   INTEGER,
    entry_size    INTEGER,
    entry_crc32   INTEGER,
    content_hash  BLOB,
    format        TEXT    NOT NULL,
    title         TEXT    NOT NULL,
    sort_title    TEXT    NOT NULL,
    language      TEXT,
    annotation    TEXT    NOT NULL DEFAULT '',
    cover_mime    TEXT,
    status        TEXT    NOT NULL DEFAULT 'ok' CHECK (status IN ('ok', 'error', 'removed')),
    error_message TEXT,
    added_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
);

CREATE UNIQUE INDEX books_location ON books (container_id, entry_name)
    WHERE status != 'removed';
CREATE INDEX books_container ON books (container_id);
CREATE INDEX books_entry_identity ON books (entry_size, entry_crc32)
    WHERE entry_crc32 IS NOT NULL;
CREATE INDEX books_content_hash ON books (content_hash)
    WHERE content_hash IS NOT NULL;

CREATE TABLE authors (
    id           INTEGER PRIMARY KEY,
    first_name   TEXT,
    middle_name  TEXT,
    last_name    TEXT,
    nickname     TEXT,
    display_name TEXT NOT NULL,
    sort_key     TEXT NOT NULL UNIQUE
);

CREATE TABLE book_authors (
    book_id   INTEGER NOT NULL REFERENCES books (id) ON DELETE CASCADE,
    position  INTEGER NOT NULL,
    author_id INTEGER NOT NULL REFERENCES authors (id),
    PRIMARY KEY (book_id, position)
);

CREATE INDEX book_authors_author ON book_authors (author_id);

CREATE TABLE series (
    id       INTEGER PRIMARY KEY,
    name     TEXT NOT NULL,
    sort_key TEXT NOT NULL UNIQUE
);

CREATE TABLE book_series (
    book_id     INTEGER NOT NULL REFERENCES books (id) ON DELETE CASCADE,
    position    INTEGER NOT NULL,
    series_id   INTEGER NOT NULL REFERENCES series (id),
    number      TEXT,
    number_sort REAL,
    PRIMARY KEY (book_id, position)
);

CREATE INDEX book_series_series ON book_series (series_id);

CREATE TABLE genres (
    id   INTEGER PRIMARY KEY,
    code TEXT NOT NULL UNIQUE
);

CREATE TABLE book_genres (
    book_id  INTEGER NOT NULL REFERENCES books (id) ON DELETE CASCADE,
    genre_id INTEGER NOT NULL REFERENCES genres (id),
    PRIMARY KEY (book_id, genre_id)
);

CREATE INDEX book_genres_genre ON book_genres (genre_id);

CREATE VIRTUAL TABLE books_fts USING fts5 (
    title, authors, series, annotation,
    tokenize = 'unicode61 remove_diacritics 2',
    prefix = '2 3'
);
"#;

/// Brings the schema to [`SCHEMA_VERSION`]; refuses databases from newer builds.
pub fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    let current: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(StorageError::NewerSchema {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
        let version = index as u32 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
        tracing::debug!("database schema migrated to version {version}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(conn: &Connection) -> u32 {
        conn.pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn migrates_empty_database_and_is_idempotent() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION);
        let tables: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = 'books'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tables, 1);
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION);
    }

    #[test]
    fn newer_schema_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let err = migrate(&mut conn).unwrap_err();
        assert!(matches!(err, StorageError::NewerSchema { .. }), "{err}");
        assert!(err.to_string().contains("newer"), "{err}");
    }
}
