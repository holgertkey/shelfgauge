//! The SQLite index in `.shelfgauge/library.db`.
//!
//! One writer connection ([`open_writer`]) is used by the scanner's single writer
//! thread; web handlers use a pool of read-only connections ([`ReadPool`]). Write
//! functions take a [`Connection`] (usually a transaction, via deref) and never open
//! their own transaction, so callers can batch many books into one.

use std::path::Path;
use std::time::Duration;

use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, OpenFlags};

pub mod authors;
pub mod books;
pub mod containers;
pub mod folders;
pub mod genres;
pub mod migrations;
pub mod scans;
pub mod search;
pub mod stats;

/// How long a connection waits for a lock held by another connection.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors of the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// An SQLite operation failed.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A pooled connection could not be obtained.
    #[error("database connection pool error: {0}")]
    Pool(#[from] r2d2::Error),

    /// The database was created by a newer shelfgauge.
    #[error(
        "the database has schema version {found}, but this shelfgauge supports up to \
         {supported}; it was created by a newer version"
    )]
    NewerSchema {
        /// The schema version found in the database.
        found: u32,
        /// The newest schema version this build supports.
        supported: u32,
    },

    /// A schema migration left the database inconsistent and was rolled back.
    #[error("migrating the database to schema version {version} failed: {message}")]
    Migration {
        /// The schema version the migration was to create.
        version: u32,
        /// What went wrong.
        message: String,
    },

    /// The operating system's random number generator failed (session tokens).
    #[error("the system random number generator failed: {0}")]
    Random(String),
}

/// Opens (creating if needed) the database for writing and migrates it.
///
/// Uses WAL mode so readers are never blocked by the writer. On file systems where WAL
/// is unavailable (some network shares), a warning is logged and SQLite's rollback
/// journal is used instead.
///
/// # Examples
///
/// ```
/// let dir = tempfile::tempdir().unwrap();
/// let conn = shelfgauge::storage::open_writer(&dir.path().join("library.db")).unwrap();
/// let books: i64 = conn.query_row("SELECT count(*) FROM books", [], |r| r.get(0)).unwrap();
/// assert_eq!(books, 0);
/// ```
pub fn open_writer(path: &Path) -> Result<Connection, StorageError> {
    let mut conn = open_wal(path)?;
    migrations::migrate(&mut conn)?;
    Ok(conn)
}

/// Opens (creating if needed) a database for writing in WAL mode (falling back to the
/// rollback journal with a warning), with the shared connection settings.
pub(crate) fn open_wal(path: &Path) -> Result<Connection, StorageError> {
    let conn = Connection::open(path)?;
    let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        tracing::warn!(
            database = ?path.file_name().unwrap_or_default(),
            journal_mode = ?mode,
            "WAL mode is not available; readers may be blocked while it is written"
        );
    }
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    configure(&conn)?;
    Ok(conn)
}

/// A pool of read-only connections for the web layer.
pub type ReadPool = r2d2::Pool<SqliteConnectionManager>;

/// Opens a pool of up to `size` read-only connections.
///
/// The database must already exist and be migrated: call [`open_writer`] first.
pub fn open_read_pool(path: &Path, size: u32) -> Result<ReadPool, StorageError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_URI;
    // Surface the real error (e.g. a missing file) at once; the pool would only retry
    // until its timeout and then report a generic error.
    Connection::open_with_flags(path, flags)?;
    let manager = SqliteConnectionManager::file(path)
        .with_flags(flags)
        .with_init(|conn| {
            configure(conn)?;
            conn.pragma_update(None, "query_only", true)
        });
    let pool = r2d2::Pool::builder()
        .max_size(size.max(1))
        .min_idle(Some(1))
        .connection_timeout(Duration::from_secs(5))
        .build(manager)?;
    Ok(pool)
}

/// Per-connection settings shared by the writer and readers.
pub(crate) fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(BUSY_TIMEOUT)
}

/// Folds text for full-text search, applied to indexed text and queries alike.
///
/// `unicode61` folds case for all scripts and Latin diacritics, but not `ё`; it is
/// mapped to `е` here, as Russian text uses both spellings. `й` stays distinct.
pub fn fold_for_search(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            'ё' => 'е',
            'Ё' => 'Е',
            c => c,
        })
        .collect()
}

/// A normalized key for sorting and de-duplicating names: lowercase, `ё` → `е`,
/// whitespace collapsed and trimmed.
pub fn normalize_key(text: &str) -> String {
    let lower = fold_for_search(&text.to_lowercase());
    lower.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The smallest string greater than every string starting with `prefix`, for index
/// range scans (`key >= prefix AND key < upper`).
fn prefix_upper_bound(prefix: &str) -> String {
    format!("{prefix}\u{10ffff}")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A migrated in-memory database for unit tests.
    pub(crate) fn memory_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        configure(&conn).unwrap();
        migrations::migrate(&mut conn).unwrap();
        conn
    }

    #[test]
    fn writer_uses_wal_and_foreign_keys() {
        let dir = tempfile::tempdir().unwrap();
        let conn = open_writer(&dir.path().join("library.db")).unwrap();
        let mode: String = conn
            .pragma_query_value(None, "journal_mode", |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        let fk: bool = conn
            .pragma_query_value(None, "foreign_keys", |r| r.get(0))
            .unwrap();
        assert!(fk);
    }

    #[test]
    fn reopening_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let conn = open_writer(&path).unwrap();
        conn.execute("INSERT INTO genres (code) VALUES ('sf')", [])
            .unwrap();
        drop(conn);
        let conn = open_writer(&path).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM genres", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn read_pool_is_read_only_and_sees_committed_data_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("library.db");
        let mut writer = open_writer(&path).unwrap();
        writer
            .execute("INSERT INTO genres (code) VALUES ('committed')", [])
            .unwrap();
        let pool = open_read_pool(&path, 2).unwrap();
        let reader = pool.get().unwrap();
        let count = |conn: &Connection| -> i64 {
            conn.query_row("SELECT count(*) FROM genres", [], |r| r.get(0))
                .unwrap()
        };

        // While the writer holds an open transaction, readers are not blocked and see
        // the last committed state.
        let tx = writer.transaction().unwrap();
        tx.execute("INSERT INTO genres (code) VALUES ('pending')", [])
            .unwrap();
        assert_eq!(count(&reader), 1);
        tx.commit().unwrap();
        assert_eq!(count(&reader), 2);

        let err = reader
            .execute("INSERT INTO genres (code) VALUES ('x')", [])
            .unwrap_err();
        assert!(err.to_string().contains("readonly"), "{err}");
    }

    #[test]
    fn read_pool_on_missing_database_fails_fast() {
        let dir = tempfile::tempdir().unwrap();
        let started = std::time::Instant::now();
        let err = open_read_pool(&dir.path().join("missing.db"), 1).unwrap_err();
        assert!(matches!(err, StorageError::Sqlite(_)), "{err}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn normalization() {
        assert_eq!(
            normalize_key("  Толстой   Лев\tНиколаевич "),
            "толстой лев николаевич"
        );
        assert_eq!(normalize_key("Ёжиков"), "ежиков");
        assert_eq!(fold_for_search("Ёлка ёж й"), "Елка еж й");
        assert!(prefix_upper_bound("то").as_str() > "то\u{ffff}");
    }
}
