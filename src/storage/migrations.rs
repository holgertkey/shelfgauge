//! Schema migrations, tracked with `PRAGMA user_version`.
//!
//! Each migration upgrades the schema by one version and is applied in a
//! transaction together with the `user_version` bump. Migrations run with foreign key
//! enforcement off (SQLite ignores the pragma inside a transaction, and rebuilding a
//! table would otherwise delete the rows that reference it); each one must leave no
//! foreign key violations, which is checked before its commit.
//!
//! Policy: until the first public release, existing migrations may still be edited in
//! place (local databases are then rebuilt). Version 2 was appended instead because
//! local indexes of real libraries already exist and a full rescan of a large library
//! takes long.

use rusqlite::Connection;

use super::StorageError;

/// Schema migrations; index `i` upgrades version `i` to `i + 1`.
const MIGRATIONS: &[&str] = &[V1, V2, V3];

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

/// Version 2: `books.parser_version`, the
/// [`PARSER_VERSION`](crate::format::model::PARSER_VERSION) that produced the record
/// (0 for books written before). Books that failed to parse with an older parser are
/// parsed again by the next scan.
const V2: &str = r#"
ALTER TABLE books ADD COLUMN parser_version INTEGER NOT NULL DEFAULT 0;
"#;

/// Version 3: the status `protected` (a DRM-protected book: its metadata is indexed, its
/// content cannot be read) and `books.drm`, the protection scheme (non-NULL exactly for
/// protected books, so a moved book keeps its status); `containers.listed_formats`, the
/// book formats an archive was listed for (NULL before: every archive is listed once
/// more, which finds EPUB books in archives indexed before EPUB was supported).
///
/// SQLite cannot change a CHECK constraint, so `books` is rebuilt (the documented
/// "other kinds of table schema changes" procedure); ids are kept, so the rows of
/// `book_authors`, `book_series`, `book_genres` and `books_fts` stay valid.
const V3: &str = r#"
CREATE TABLE books_v3 (
    id             INTEGER PRIMARY KEY,
    container_id   INTEGER NOT NULL REFERENCES containers (id),
    entry_name     BLOB    NOT NULL DEFAULT x'',
    entry_path     TEXT,
    entry_index    INTEGER,
    entry_size     INTEGER,
    entry_crc32    INTEGER,
    content_hash   BLOB,
    format         TEXT    NOT NULL,
    title          TEXT    NOT NULL,
    sort_title     TEXT    NOT NULL,
    language       TEXT,
    annotation     TEXT    NOT NULL DEFAULT '',
    cover_mime     TEXT,
    status         TEXT    NOT NULL DEFAULT 'ok'
                   CHECK (status IN ('ok', 'error', 'removed', 'protected')),
    error_message  TEXT,
    added_at       INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    parser_version INTEGER NOT NULL DEFAULT 0,
    drm            TEXT
);

INSERT INTO books_v3 (
    id, container_id, entry_name, entry_path, entry_index, entry_size, entry_crc32,
    content_hash, format, title, sort_title, language, annotation, cover_mime, status,
    error_message, added_at, updated_at, parser_version
)
SELECT
    id, container_id, entry_name, entry_path, entry_index, entry_size, entry_crc32,
    content_hash, format, title, sort_title, language, annotation, cover_mime, status,
    error_message, added_at, updated_at, parser_version
FROM books;

DROP TABLE books;
ALTER TABLE books_v3 RENAME TO books;

CREATE UNIQUE INDEX books_location ON books (container_id, entry_name)
    WHERE status != 'removed';
CREATE INDEX books_container ON books (container_id);
CREATE INDEX books_entry_identity ON books (entry_size, entry_crc32)
    WHERE entry_crc32 IS NOT NULL;
CREATE INDEX books_content_hash ON books (content_hash)
    WHERE content_hash IS NOT NULL;

ALTER TABLE containers ADD COLUMN listed_formats TEXT;
"#;

/// Brings the schema to [`SCHEMA_VERSION`]; refuses databases from newer builds.
pub fn migrate(conn: &mut Connection) -> Result<(), StorageError> {
    apply(conn, MIGRATIONS)
}

/// Applies the `migrations` not yet recorded in `user_version` (index `i` upgrades
/// version `i` to `i + 1`), each in a transaction with its version bump; refuses a
/// database with a newer version.
pub(crate) fn apply(conn: &mut Connection, migrations: &[&str]) -> Result<(), StorageError> {
    let supported = migrations.len() as u32;
    let current: u32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if current > supported {
        return Err(StorageError::NewerSchema {
            found: current,
            supported,
        });
    }
    if current == supported {
        return Ok(());
    }
    let foreign_keys: bool = conn.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    conn.pragma_update(None, "foreign_keys", false)?;
    let result = apply_pending(conn, migrations, current);
    // Restored whatever happened.
    let restored = conn.pragma_update(None, "foreign_keys", foreign_keys);
    result?;
    restored?;
    Ok(())
}

/// Applies `migrations[current..]`, each in a transaction with its version bump and a
/// foreign key check.
fn apply_pending(
    conn: &mut Connection,
    migrations: &[&str],
    current: u32,
) -> Result<(), StorageError> {
    for (index, sql) in migrations.iter().enumerate().skip(current as usize) {
        let version = index as u32 + 1;
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        let violations: i64 =
            tx.query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
                row.get(0)
            })?;
        if violations > 0 {
            return Err(StorageError::Migration {
                version,
                message: format!("{violations} foreign key violations"),
            });
        }
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
    fn version_1_database_gets_parser_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(V1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO containers (path, kind, size, mtime_ns) VALUES ('a.fb2', 'file', 1, 1);
             INSERT INTO books (container_id, format, title, sort_title) VALUES (1, 'fb2', 'T', 't');",
        )
        .unwrap();
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), SCHEMA_VERSION);
        let parser_version: i64 = conn
            .query_row("SELECT parser_version FROM books", [], |r| r.get(0))
            .unwrap();
        assert_eq!(parser_version, 0, "books written before are marked unknown");
    }

    /// A version 2 database with one book linked to an author, a series, a genre and
    /// the search index.
    fn version_2_database() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        apply(&mut conn, &[V1, V2]).unwrap();
        conn.execute_batch(
            "INSERT INTO containers (path, kind, size, mtime_ns) VALUES ('a.fb2', 'file', 1, 1);
             INSERT INTO books (id, container_id, format, title, sort_title, status, error_message, parser_version)
                 VALUES (7, 1, 'fb2', 'T', 't', 'error', 'bad', 1);
             INSERT INTO authors (first_name, display_name, sort_key) VALUES ('A', 'A', 'a');
             INSERT INTO book_authors (book_id, position, author_id) VALUES (7, 0, 1);
             INSERT INTO series (name, sort_key) VALUES ('S', 's');
             INSERT INTO book_series (book_id, position, series_id) VALUES (7, 0, 1);
             INSERT INTO genres (code) VALUES ('g');
             INSERT INTO book_genres (book_id, genre_id) VALUES (7, 1);
             INSERT INTO books_fts (rowid, title, authors, series, annotation) VALUES (7, 'T', 'A', 'S', '');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn version_3_rebuild_keeps_every_reference() {
        let mut conn = version_2_database();
        migrate(&mut conn).unwrap();
        assert_eq!(version(&conn), 3);
        let count = |sql: &str| -> i64 { conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(
            count("SELECT count(*) FROM book_authors WHERE book_id = 7"),
            1
        );
        assert_eq!(
            count("SELECT count(*) FROM book_series WHERE book_id = 7"),
            1
        );
        assert_eq!(
            count("SELECT count(*) FROM book_genres WHERE book_id = 7"),
            1
        );
        assert_eq!(count("SELECT count(*) FROM books_fts WHERE rowid = 7"), 1);
        assert_eq!(
            count("SELECT count(*) FROM containers WHERE listed_formats IS NULL"),
            1,
            "archives are listed once more"
        );
        let (status, message, parser): (String, String, i64) = conn
            .query_row(
                "SELECT status, error_message, parser_version FROM books WHERE id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (status.as_str(), message.as_str(), parser),
            ("error", "bad", 1)
        );
        let foreign_keys: bool = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert!(foreign_keys, "foreign keys are enforced again");
        // The new status is accepted, unknown ones are not; deleting a book still
        // cascades.
        conn.execute(
            "UPDATE books SET status = 'protected', drm = 'X' WHERE id = 7",
            [],
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE books SET status = 'bogus' WHERE id = 7", [])
                .is_err()
        );
        assert!(
            conn.execute("DELETE FROM containers WHERE id = 1", [])
                .is_err()
        );
        conn.execute("DELETE FROM books WHERE id = 7", []).unwrap();
        assert_eq!(count("SELECT count(*) FROM book_authors"), 0);
        let indexes =
            count("SELECT count(*) FROM sqlite_master WHERE type = 'index' AND tbl_name = 'books'");
        assert_eq!(indexes, 4);
    }

    #[test]
    fn a_failed_migration_rolls_back_and_restores_foreign_keys() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        let broken = "CREATE TABLE p (id INTEGER PRIMARY KEY);
                      CREATE TABLE c (p INTEGER REFERENCES p (id));
                      INSERT INTO c VALUES (5);";
        let err = apply(&mut conn, &[broken]).unwrap_err();
        assert!(
            matches!(err, StorageError::Migration { version: 1, .. }),
            "{err}"
        );
        assert_eq!(version(&conn), 0);
        let foreign_keys: bool = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert!(foreign_keys);
        let err = apply(&mut conn, &["SELECT nonsense FROM nowhere"]).unwrap_err();
        assert!(matches!(err, StorageError::Sqlite(_)), "{err}");
        let foreign_keys: bool = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert!(foreign_keys);
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
