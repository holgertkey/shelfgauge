//! The library's Showcase: the folders an admin opened for the catalog on the hub.
//!
//! Folders are the unit and closed by default. An open folder is open recursively,
//! sub-folders added later included; a sub-folder cannot be closed inside an open one,
//! but its own mark is kept (unchecking the parent leaves it open). A folder is a
//! directory or a zip archive (a virtual folder), as in the sidebar's folder mode.
//! Marks are kept by path in `userdata.sqlite` (`showcase_folders`): a renamed or moved
//! folder is closed, and its mark is reported as missing ([`missing_marks`]) until an
//! admin forgets it.
//!
//! Whether a book is on the Showcase is decided from the index *now*
//! ([`on_showcase`]): the library checks every request itself, since a member's
//! catalog may be stale and anyone with the library key can ask for any book id.

use rusqlite::{Connection, params};

use crate::storage::StorageError;
use crate::storage::books::live_books_by_key;
use crate::storage::containers::parse_kind;
use crate::storage::folders::{FolderPath, book_folder};

/// The folders marked open, in a stable order.
pub fn marks(conn: &Connection) -> Result<Vec<FolderPath>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT kind, path, inner FROM showcase_folders ORDER BY kind, path, inner")?;
    let rows = stmt.query_map([], |row| {
        let kind: String = row.get(0)?;
        let path: String = row.get(1)?;
        let inner: String = row.get(2)?;
        Ok(if kind == "zip" {
            FolderPath::Archive {
                archive: path,
                inner,
            }
        } else {
            FolderPath::Dir(path)
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Marks `folder` open, or forgets its mark; `now` in Unix seconds.
pub fn set_mark(
    conn: &Connection,
    folder: &FolderPath,
    open: bool,
    now: i64,
) -> Result<(), StorageError> {
    let (kind, path, inner) = match normalize(folder) {
        FolderPath::Dir(path) => ("dir", path, String::new()),
        FolderPath::Archive { archive, inner } => ("zip", archive, inner),
    };
    if open {
        conn.execute(
            "INSERT INTO showcase_folders (kind, path, inner, marked_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT DO NOTHING",
            params![kind, path, inner, now],
        )?;
    } else {
        conn.execute(
            "DELETE FROM showcase_folders WHERE kind = ?1 AND path = ?2 AND inner = ?3",
            params![kind, path, inner],
        )?;
    }
    Ok(())
}

/// `folder` with no slash around its parts and `/` inside archives.
fn normalize(folder: &FolderPath) -> FolderPath {
    let clean = |s: &str| s.replace('\\', "/").trim_matches('/').to_owned();
    match folder {
        FolderPath::Dir(path) => FolderPath::Dir(clean(path)),
        FolderPath::Archive { archive, inner } => FolderPath::Archive {
            archive: archive.trim_matches('/').to_owned(),
            inner: clean(inner),
        },
    }
}

/// Whether `folder` is `mark` or below it: a directory holds its sub-directories and
/// the archives in them, an archive (or a directory in it) its inner directories.
///
/// # Examples
///
/// ```
/// use shelfgauge::hub_client::showcase::is_under;
/// use shelfgauge::storage::folders::FolderPath;
///
/// let dir = |p: &str| FolderPath::Dir(p.into());
/// let zip = |a: &str, i: &str| FolderPath::Archive { archive: a.into(), inner: i.into() };
/// assert!(is_under(&dir("sf"), &dir("sf/lem")));
/// assert!(!is_under(&dir("sf"), &dir("sfx")));
/// assert!(is_under(&dir(""), &zip("a.zip", "x")));
/// assert!(is_under(&dir("sf"), &zip("sf/all.zip", "")));
/// assert!(is_under(&zip("all.zip", "a"), &zip("all.zip", "a/b")));
/// assert!(!is_under(&zip("all.zip", "a"), &zip("all.zip", "ab")));
/// assert!(!is_under(&zip("all.zip", ""), &dir("")));
/// ```
pub fn is_under(mark: &FolderPath, folder: &FolderPath) -> bool {
    under(&normalize(mark), &normalize(folder))
}

/// [`is_under`] for paths already normalized (stored marks and [`book_folder`]'s
/// folders are), without copying them: the Showcase's books are found by testing
/// every book against every mark.
fn under(mark: &FolderPath, folder: &FolderPath) -> bool {
    fn below(parent: &str, path: &str) -> bool {
        parent.is_empty()
            || path == parent
            || path
                .strip_prefix(parent)
                .is_some_and(|rest| rest.starts_with('/'))
    }
    match (mark, folder) {
        (FolderPath::Dir(m), FolderPath::Dir(f)) => below(m, f),
        (FolderPath::Dir(m), FolderPath::Archive { archive, .. }) => below(m, archive),
        (
            FolderPath::Archive {
                archive: a,
                inner: i,
            },
            FolderPath::Archive {
                archive: b,
                inner: j,
            },
        ) => a == b && below(i, j),
        (FolderPath::Archive { .. }, FolderPath::Dir(_)) => false,
    }
}

/// How a folder stands on the Showcase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkState {
    /// Not on the Showcase.
    Closed,
    /// Marked open itself.
    Open,
    /// Open through a parent's mark (whether or not it has its own).
    ThroughParent,
}

/// The state of `folder` among `marks`.
pub fn folder_state(marks: &[FolderPath], folder: &FolderPath) -> MarkState {
    let folder = normalize(folder);
    if marks
        .iter()
        .any(|m| normalize(m) != folder && is_under(m, &folder))
    {
        MarkState::ThroughParent
    } else if marks.iter().any(|m| normalize(m) == folder) {
        MarkState::Open
    } else {
        MarkState::Closed
    }
}

/// A book of the index with the folder it is listed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocatedBook {
    /// The book id.
    pub id: i64,
    /// Its folder.
    pub folder: FolderPath,
}

/// Every book that is not removed, with its folder.
fn live_books(index: &Connection, only: Option<i64>) -> Result<Vec<LocatedBook>, StorageError> {
    let mut stmt = index.prepare(
        "SELECT b.id, c.path, c.kind, b.entry_path
           FROM books b JOIN containers c ON c.id = b.container_id
          WHERE b.status != 'removed' AND (?1 IS NULL OR b.id = ?1)",
    )?;
    let rows = stmt.query_map([only], |row| {
        let path: String = row.get(1)?;
        let kind = parse_kind(&row.get::<_, String>(2)?);
        let entry: Option<String> = row.get(3)?;
        Ok(LocatedBook {
            id: row.get(0)?,
            folder: book_folder(&path, kind, entry.as_deref()),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The books on the Showcase now: not removed, in a folder below one of `marks`, by
/// id. (Duplicates, the same content in several folders, are all listed; the catalog
/// merges them by content.)
pub fn showcase_books(
    index: &Connection,
    marks: &[FolderPath],
) -> Result<Vec<LocatedBook>, StorageError> {
    if marks.is_empty() {
        return Ok(Vec::new());
    }
    let marks: Vec<FolderPath> = marks.iter().map(normalize).collect();
    let mut books: Vec<LocatedBook> = live_books(index, None)?
        .into_iter()
        .filter(|b| marks.iter().any(|m| under(m, &b.folder)))
        .collect();
    books.sort_by_key(|b| b.id);
    Ok(books)
}

/// Whether book `id` (this copy) is on the Showcase now.
pub fn on_showcase(
    index: &Connection,
    marks: &[FolderPath],
    id: i64,
) -> Result<bool, StorageError> {
    if marks.is_empty() {
        return Ok(false);
    }
    let marks: Vec<FolderPath> = marks.iter().map(normalize).collect();
    Ok(live_books(index, Some(id))?
        .iter()
        .any(|b| marks.iter().any(|m| under(m, &b.folder))))
}

/// Whether the book with `content_key` is on the Showcase now: any live copy of it in
/// a folder below a mark (a book in an open and a closed folder is on it through the
/// open one). The check for a member's request, which names the content.
pub fn content_on_showcase(
    index: &Connection,
    marks: &[FolderPath],
    content_key: &[u8],
) -> Result<bool, StorageError> {
    for id in live_books_by_key(index, content_key)? {
        if on_showcase(index, marks, id)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// The marks whose folder holds no book any more (renamed, moved or deleted): their
/// books left the Showcase. The library root is never missing.
pub fn missing_marks(
    index: &Connection,
    marks: &[FolderPath],
) -> Result<Vec<FolderPath>, StorageError> {
    let candidates: Vec<&FolderPath> = marks
        .iter()
        .filter(|m| !matches!(m, FolderPath::Dir(p) if p.is_empty()))
        .collect();
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let books = live_books(index, None)?;
    Ok(candidates
        .into_iter()
        .filter(|m| {
            let m = normalize(m);
            !books.iter().any(|b| under(&m, &b.folder))
        })
        .cloned()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::store::UserData;

    fn dir(p: &str) -> FolderPath {
        FolderPath::Dir(p.into())
    }

    fn zip(a: &str, i: &str) -> FolderPath {
        FolderPath::Archive {
            archive: a.into(),
            inner: i.into(),
        }
    }

    #[test]
    fn marks_are_stored_once_and_forgotten() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        set_mark(&conn, &dir("/sf/"), true, 1).unwrap();
        set_mark(&conn, &dir("sf"), true, 2).unwrap();
        set_mark(&conn, &zip("all.zip", "a\\b"), true, 3).unwrap();
        assert_eq!(marks(&conn).unwrap(), [dir("sf"), zip("all.zip", "a/b")]);
        set_mark(&conn, &dir("sf"), false, 4).unwrap();
        set_mark(&conn, &dir("never"), false, 4).unwrap();
        assert_eq!(marks(&conn).unwrap(), [zip("all.zip", "a/b")]);
    }

    #[test]
    fn states_follow_the_parents() {
        let marks = [dir("sf"), dir("sf/lem"), zip("sf/all.zip", "x")];
        assert_eq!(folder_state(&marks, &dir("sf")), MarkState::Open);
        assert_eq!(
            folder_state(&marks, &dir("sf/lem")),
            MarkState::ThroughParent
        );
        assert_eq!(
            folder_state(&marks, &dir("sf/other")),
            MarkState::ThroughParent
        );
        assert_eq!(
            folder_state(&marks, &zip("sf/all.zip", "")),
            MarkState::ThroughParent
        );
        assert_eq!(folder_state(&marks, &dir("")), MarkState::Closed);
        assert_eq!(
            folder_state(&marks[2..], &zip("sf/all.zip", "x")),
            MarkState::Open
        );
        assert_eq!(
            folder_state(&marks[2..], &zip("sf/all.zip", "x/y")),
            MarkState::ThroughParent
        );
        assert_eq!(
            folder_state(&marks[2..], &zip("sf/all.zip", "")),
            MarkState::Closed
        );
    }
}
