//! The history of scans: when each scan ran and what it did.

use rusqlite::{Connection, OptionalExtension, params};

use super::StorageError;

/// A finished (or cancelled) scan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanRecord {
    /// When the scan started (Unix seconds).
    pub started_at: i64,
    /// When it finished (Unix seconds).
    pub finished_at: i64,
    /// New books.
    pub added: u64,
    /// Books re-indexed because their content changed.
    pub updated: u64,
    /// Books found at a new location.
    pub moved: u64,
    /// Books whose file is gone.
    pub removed: u64,
    /// Books left as they were.
    pub unchanged: u64,
    /// Books parsed.
    pub parsed: u64,
    /// Books and archives that could not be read.
    pub errors: u64,
    /// Files skipped because their path is not valid UTF-8.
    pub skipped: u64,
    /// Files of a book format that are not books.
    pub not_books: u64,
    /// Whether the scan was cancelled.
    pub cancelled: bool,
}

/// Records a scan; returns its id.
pub fn record_scan(conn: &Connection, scan: &ScanRecord) -> Result<i64, StorageError> {
    let n = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    conn.execute(
        "INSERT INTO scans (started_at, finished_at, added, updated, moved, removed,
                            unchanged, parsed, errors, skipped, not_books, cancelled)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            scan.started_at,
            scan.finished_at,
            n(scan.added),
            n(scan.updated),
            n(scan.moved),
            n(scan.removed),
            n(scan.unchanged),
            n(scan.parsed),
            n(scan.errors),
            n(scan.skipped),
            n(scan.not_books),
            scan.cancelled,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The most recent scan, if any was recorded.
pub fn last_scan(conn: &Connection) -> Result<Option<ScanRecord>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT started_at, finished_at, added, updated, moved, removed, unchanged,
                    parsed, errors, skipped, not_books, cancelled
               FROM scans ORDER BY id DESC LIMIT 1",
            [],
            |row| {
                let n = |i: usize| row.get::<_, i64>(i).map(|v| v.max(0) as u64);
                Ok(ScanRecord {
                    started_at: row.get(0)?,
                    finished_at: row.get(1)?,
                    added: n(2)?,
                    updated: n(3)?,
                    moved: n(4)?,
                    removed: n(5)?,
                    unchanged: n(6)?,
                    parsed: n(7)?,
                    errors: n(8)?,
                    skipped: n(9)?,
                    not_books: n(10)?,
                    cancelled: row.get(11)?,
                })
            },
        )
        .optional()?)
}

/// The id of the most recent scan (0 before the first): it changes whenever a scan
/// has changed the index, so figures derived from the index can be cached by it.
pub fn scan_generation(conn: &Connection) -> Result<i64, StorageError> {
    Ok(
        conn.query_row("SELECT coalesce(max(id), 0) FROM scans", [], |row| {
            row.get(0)
        })?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::memory_db;

    #[test]
    fn scans_are_recorded_and_the_last_one_read() {
        let conn = memory_db();
        assert_eq!(last_scan(&conn).unwrap(), None);
        assert_eq!(scan_generation(&conn).unwrap(), 0);
        let first = ScanRecord {
            started_at: 10,
            finished_at: 12,
            added: 3,
            errors: 1,
            ..ScanRecord::default()
        };
        let id = record_scan(&conn, &first).unwrap();
        assert_eq!(scan_generation(&conn).unwrap(), id);
        let second = ScanRecord {
            started_at: 20,
            finished_at: 21,
            unchanged: 3,
            cancelled: true,
            ..ScanRecord::default()
        };
        let id2 = record_scan(&conn, &second).unwrap();
        assert!(id2 > id);
        assert_eq!(last_scan(&conn).unwrap(), Some(second));
        assert_eq!(scan_generation(&conn).unwrap(), id2);
    }
}
