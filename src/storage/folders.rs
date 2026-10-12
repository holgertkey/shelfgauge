//! The folder view: directories of the library, with zip archives as virtual folders.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};

use super::books::{BookSummary, SUMMARY_COLUMNS, SUMMARY_FROM, summary_row};
use super::containers::parse_kind;
use super::{StorageError, normalize_key};
use crate::container::ContainerKind;

/// A folder of the library.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum FolderPath {
    /// A directory, `/`-separated relative to the library root (`""` is the root).
    Dir(String),
    /// A directory inside a zip archive.
    Archive {
        /// The archive's container path.
        archive: String,
        /// The `/`-separated directory inside the archive (`""` for its top level).
        inner: String,
    },
}

/// What kind of folder an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderKind {
    /// A directory.
    Directory,
    /// A zip archive.
    Archive,
    /// A directory inside a zip archive.
    ArchiveDirectory,
}

/// A sub-folder in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderEntry {
    /// The last path segment.
    pub name: String,
    /// Where it leads.
    pub path: FolderPath,
    /// What it is.
    pub kind: FolderKind,
    /// Books (not removed) in it, recursively.
    pub book_count: u64,
}

/// The contents of one folder.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderListing {
    /// Sub-folders, by name.
    pub folders: Vec<FolderEntry>,
    /// One page of the books directly in the folder, by title.
    pub books: Vec<BookSummary>,
    /// The number of books directly in the folder, across all pages.
    pub book_total: u64,
}

/// Lists one level of the folder tree with one page of its books (`limit` books from
/// `offset`); folders without books (all removed) are hidden.
pub fn folder_children(
    conn: &Connection,
    folder: &FolderPath,
    limit: u32,
    offset: u64,
) -> Result<FolderListing, StorageError> {
    let (folders, direct) = read_folder(conn, folder)?;
    let (books, book_total) = match direct {
        Direct::Files(files) => {
            let ids = id_list(&files);
            let total: i64 = conn.query_row(
                "SELECT count(*) FROM books
                 WHERE status != 'removed' AND container_id IN (SELECT value FROM json_each(?1))",
                [&ids],
                |row| row.get(0),
            )?;
            let books = if total == 0 {
                Vec::new()
            } else {
                let sql = format!(
                    "SELECT {SUMMARY_COLUMNS} {SUMMARY_FROM}
                     WHERE b.status != 'removed'
                       AND b.container_id IN (SELECT value FROM json_each(?1))
                     ORDER BY b.sort_title, b.id
                     LIMIT ?2 OFFSET ?3"
                );
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map(
                    params![ids, limit, i64::try_from(offset).unwrap_or(i64::MAX)],
                    summary_row,
                )?;
                rows.collect::<Result<_, _>>()?
            };
            (books, total as u64)
        }
        Direct::Archive(books) => {
            let total = books.len() as u64;
            let page = books
                .into_iter()
                .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                .take(limit as usize)
                .collect();
            (page, total)
        }
    };
    Ok(FolderListing {
        folders,
        books,
        book_total,
    })
}

/// The position of book `book_id` among the books directly in `folder` (in the order
/// of [`folder_children`]), or `None` if it is not there.
pub fn folder_book_index(
    conn: &Connection,
    folder: &FolderPath,
    book_id: i64,
) -> Result<Option<u64>, StorageError> {
    let (_, direct) = read_folder(conn, folder)?;
    let ids: Vec<i64> = match direct {
        Direct::Files(files) => {
            let mut stmt = conn.prepare(
                "SELECT id FROM books
                 WHERE status != 'removed' AND container_id IN (SELECT value FROM json_each(?1))
                 ORDER BY sort_title, id",
            )?;
            let rows = stmt.query_map([id_list(&files)], |row| row.get(0))?;
            rows.collect::<Result<_, _>>()?
        }
        Direct::Archive(books) => books.iter().map(|b| b.id).collect(),
    };
    Ok(ids.iter().position(|&id| id == book_id).map(|i| i as u64))
}

/// The books directly in a folder: plain-file containers, or an archive's books.
enum Direct {
    /// Ids of the plain-file containers directly in a directory.
    Files(Vec<i64>),
    /// The books of an archive directory, in listing order.
    Archive(Vec<BookSummary>),
}

fn read_folder(
    conn: &Connection,
    folder: &FolderPath,
) -> Result<(Vec<FolderEntry>, Direct), StorageError> {
    match folder {
        FolderPath::Dir(dir) => directory(conn, dir.trim_matches('/')),
        FolderPath::Archive { archive, inner } => {
            archive_dir(conn, archive, inner.trim_matches('/'))
        }
    }
}

fn id_list(ids: &[i64]) -> String {
    format!(
        "[{}]",
        ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
    )
}

fn directory(conn: &Connection, dir: &str) -> Result<(Vec<FolderEntry>, Direct), StorageError> {
    // `path >= 'a/' AND path < 'a0'` selects everything below `a` ('0' follows '/').
    let (lower, upper) = if dir.is_empty() {
        (String::new(), "\u{10ffff}".to_owned())
    } else {
        (format!("{dir}/"), format!("{dir}0"))
    };
    let mut stmt = conn.prepare(
        "SELECT c.id, c.path, c.kind, count(b.id)
           FROM containers c
           JOIN books b ON b.container_id = c.id AND b.status != 'removed'
          WHERE c.path >= ?1 AND c.path < ?2
          GROUP BY c.id",
    )?;
    let rows = stmt.query_map(params![lower, upper], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            parse_kind(&row.get::<_, String>(2)?),
            row.get::<_, i64>(3)? as u64,
        ))
    })?;

    let mut folders: BTreeMap<String, FolderEntry> = BTreeMap::new();
    let mut files = Vec::new();
    for row in rows {
        let (id, path, kind, count) = row?;
        let rest = &path[lower.len()..];
        let entry = match rest.split_once('/') {
            Some((name, _)) => FolderEntry {
                name: name.to_owned(),
                path: FolderPath::Dir(join(dir, name)),
                kind: FolderKind::Directory,
                book_count: 0,
            },
            None if kind == ContainerKind::Zip => FolderEntry {
                name: rest.to_owned(),
                path: FolderPath::Archive {
                    archive: path.clone(),
                    inner: String::new(),
                },
                kind: FolderKind::Archive,
                book_count: 0,
            },
            None => {
                files.push(id);
                continue;
            }
        };
        let key = format!("{}\u{0}{}", normalize_key(&entry.name), entry.name);
        folders.entry(key).or_insert(entry).book_count += count;
    }
    Ok((folders.into_values().collect(), Direct::Files(files)))
}

fn archive_dir(
    conn: &Connection,
    archive: &str,
    inner: &str,
) -> Result<(Vec<FolderEntry>, Direct), StorageError> {
    let prefix = if inner.is_empty() {
        String::new()
    } else {
        format!("{inner}/")
    };
    let sql = format!(
        "SELECT {SUMMARY_COLUMNS}, coalesce(b.entry_path, '') {SUMMARY_FROM}
         JOIN containers c ON c.id = b.container_id
         WHERE c.path = ?1 AND b.status != 'removed'
         ORDER BY b.sort_title, b.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([archive], |row| {
        Ok((row.get::<_, String>(6)?, summary_row(row)?))
    })?;

    let mut folders: BTreeMap<String, FolderEntry> = BTreeMap::new();
    let mut books = Vec::new();
    for row in rows {
        let (entry_path, summary) = row?;
        let entry_path = entry_path.replace('\\', "/");
        let Some(rest) = entry_path.strip_prefix(&prefix) else {
            continue;
        };
        match rest.split_once('/') {
            Some((name, _)) if !name.is_empty() => {
                let key = format!("{}\u{0}{name}", normalize_key(name));
                folders
                    .entry(key)
                    .or_insert_with(|| FolderEntry {
                        name: name.to_owned(),
                        path: FolderPath::Archive {
                            archive: archive.to_owned(),
                            inner: join(inner, name),
                        },
                        kind: FolderKind::ArchiveDirectory,
                        book_count: 0,
                    })
                    .book_count += 1;
            }
            _ => books.push(summary),
        }
    }
    Ok((folders.into_values().collect(), Direct::Archive(books)))
}

fn join(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{parent}/{name}")
    }
}
