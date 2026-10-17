//! Containers: plain book files and zip archives, with their change-detection data.

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::StorageError;
use crate::container::ContainerKind;

/// What the scanner records about a file or archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerRecord {
    /// Path relative to the library root, `/`-separated.
    pub path: String,
    /// Plain file or zip archive.
    pub kind: ContainerKind,
    /// Size in bytes.
    pub size: u64,
    /// Modification time in nanoseconds since the Unix epoch.
    pub mtime_ns: i64,
    /// Content hash of a plain file (not used for archives).
    pub content_hash: Option<Vec<u8>>,
    /// CRC32 of a plain file (not used for archives); with the size, the identity that
    /// finds identical copies among plain files and zip entries.
    pub crc32: Option<u32>,
    /// For an archive: the book formats its entries were listed for (see
    /// [`listed_formats`]). An archive listed for other formats is listed again even if
    /// its stat is unchanged, so books of a newly supported format are found.
    pub listed_formats: Option<String>,
}

/// The value of [`ContainerRecord::listed_formats`] for the file `extensions`: sorted,
/// comma-separated.
///
/// # Examples
///
/// ```
/// use shelfgauge::storage::containers::listed_formats;
///
/// assert_eq!(listed_formats(&["fb2", "epub"]), "epub,fb2");
/// ```
pub fn listed_formats(extensions: &[&str]) -> String {
    let mut extensions: Vec<String> = extensions.iter().map(|e| e.to_lowercase()).collect();
    extensions.sort();
    extensions.dedup();
    extensions.join(",")
}

/// A container as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredContainer {
    /// Row id.
    pub id: i64,
    /// The recorded data.
    pub record: ContainerRecord,
    /// Whether the file existed at the last scan.
    pub present: bool,
}

/// Inserts or updates the container at `record.path` and marks it present.
///
/// Returns the container id, which stays the same for the same path.
pub fn upsert_container(conn: &Connection, record: &ContainerRecord) -> Result<i64, StorageError> {
    let id = conn.query_row(
        "INSERT INTO containers
             (path, kind, size, mtime_ns, content_hash, present, listed_formats, crc32)
         VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6, ?7)
         ON CONFLICT (path) DO UPDATE SET
             kind = excluded.kind, size = excluded.size, mtime_ns = excluded.mtime_ns,
             content_hash = excluded.content_hash, present = 1,
             listed_formats = excluded.listed_formats, crc32 = excluded.crc32
         RETURNING id",
        params![
            record.path,
            kind_str(record.kind),
            record.size as i64,
            record.mtime_ns,
            record.content_hash,
            record.listed_formats,
            record.crc32.map(i64::from),
        ],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Marks a container as missing (its file was not found by the scan).
pub fn mark_container_missing(conn: &Connection, id: i64) -> Result<(), StorageError> {
    conn.execute("UPDATE containers SET present = 0 WHERE id = ?1", [id])?;
    Ok(())
}

/// The container at `path`, if recorded.
pub fn container_by_path(
    conn: &Connection,
    path: &str,
) -> Result<Option<StoredContainer>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT id, path, kind, size, mtime_ns, content_hash, present, listed_formats, crc32
             FROM containers WHERE path = ?1",
            [path],
            stored_container,
        )
        .optional()?)
}

/// All recorded containers, ordered by path.
pub fn list_containers(conn: &Connection) -> Result<Vec<StoredContainer>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT id, path, kind, size, mtime_ns, content_hash, present, listed_formats, crc32
         FROM containers ORDER BY path",
    )?;
    let rows = stmt.query_map([], stored_container)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn kind_str(kind: ContainerKind) -> &'static str {
    match kind {
        ContainerKind::File => "file",
        ContainerKind::Zip => "zip",
    }
}

pub(crate) fn parse_kind(kind: &str) -> ContainerKind {
    if kind == "zip" {
        ContainerKind::Zip
    } else {
        ContainerKind::File
    }
}

fn stored_container(row: &Row<'_>) -> rusqlite::Result<StoredContainer> {
    Ok(StoredContainer {
        id: row.get(0)?,
        record: ContainerRecord {
            path: row.get(1)?,
            kind: parse_kind(&row.get::<_, String>(2)?),
            size: row.get::<_, i64>(3)? as u64,
            mtime_ns: row.get(4)?,
            content_hash: row.get(5)?,
            listed_formats: row.get(7)?,
            crc32: row.get::<_, Option<i64>>(8)?.map(|v| v as u32),
        },
        present: row.get(6)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::tests::memory_db;

    pub(crate) fn record(path: &str, kind: ContainerKind) -> ContainerRecord {
        ContainerRecord {
            path: path.to_owned(),
            kind,
            size: 10,
            mtime_ns: 1_700_000_000_123_456_789,
            content_hash: (kind == ContainerKind::File).then(|| vec![1, 2, 3]),
            // Above `i32::MAX`: stored as a positive INTEGER, like `entry_crc32`.
            crc32: (kind == ContainerKind::File).then_some(0xFFFF_FFF0),
            listed_formats: (kind == ContainerKind::Zip).then(|| "fb2".to_owned()),
        }
    }

    #[test]
    fn upsert_keeps_id_and_updates_fields() {
        let conn = memory_db();
        let mut r = record("a/b.fb2", ContainerKind::File);
        let id = upsert_container(&conn, &r).unwrap();
        mark_container_missing(&conn, id).unwrap();
        assert!(
            !container_by_path(&conn, "a/b.fb2")
                .unwrap()
                .unwrap()
                .present
        );

        r.size = 20;
        assert_eq!(upsert_container(&conn, &r).unwrap(), id);
        let stored = container_by_path(&conn, "a/b.fb2").unwrap().unwrap();
        assert_eq!(stored.record, r);
        assert!(stored.present, "upsert marks the container present again");
        let crc32: i64 = conn
            .query_row("SELECT crc32 FROM containers WHERE id = ?1", [id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(crc32, 0xFFFF_FFF0, "stored without a sign flip");
        assert_eq!(container_by_path(&conn, "missing").unwrap(), None);
    }

    #[test]
    fn list_is_ordered_by_path() {
        let conn = memory_db();
        for path in ["b.zip", "a/x.fb2", "a.fb2"] {
            upsert_container(&conn, &record(path, ContainerKind::of(path.as_ref()))).unwrap();
        }
        let paths: Vec<String> = list_containers(&conn)
            .unwrap()
            .into_iter()
            .map(|c| c.record.path)
            .collect();
        assert_eq!(paths, ["a.fb2", "a/x.fb2", "b.zip"]);
        let zip = container_by_path(&conn, "b.zip").unwrap().unwrap();
        assert_eq!(zip.record.kind, ContainerKind::Zip);
    }
}
