//! The container layer: resolves the raw bytes of a book, independent of its format.
//!
//! A book is either a plain file or one entry of a `.zip` archive. Paths are stored
//! relative to the library root; zip entries are identified by their raw name bytes
//! (old archives use CP866 names without the UTF-8 flag), with index, size and CRC32
//! recorded to detect changes.

use std::path::{Component, Path, PathBuf};

pub mod file;
pub mod zip;

/// The kind of container a path is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContainerKind {
    /// A plain book file.
    File,
    /// A `.zip` archive with any number of books inside.
    Zip,
}

impl ContainerKind {
    /// The kind of `path`, by its extension (`.zip`, case-insensitive, is an archive).
    pub fn of(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("zip") => Self::Zip,
            _ => Self::File,
        }
    }
}

/// Where a book's bytes are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookLocation {
    /// Path of the file or archive, relative to the library root.
    pub container_path: PathBuf,
    /// The entry inside the archive; `None` for a plain file.
    pub entry: Option<EntryRef>,
}

/// Identifies one entry of a zip archive.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EntryRef {
    /// The raw name bytes as stored in the archive.
    pub name: Vec<u8>,
    /// The entry's index in the central directory (a hint; the name is authoritative).
    pub index: usize,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// CRC32 of the uncompressed data.
    pub crc32: u32,
}

/// Limits applied when reading books.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadLimits {
    /// Maximum uncompressed size of one book (zip bomb and memory protection).
    pub max_book_size: u64,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_book_size: 256 * 1024 * 1024,
        }
    }
}

/// Errors of the container layer.
#[derive(Debug, thiserror::Error)]
pub enum ContainerError {
    /// Reading the file or archive failed.
    #[error("{}: {error}", path.display())]
    Io {
        /// The file or archive.
        path: PathBuf,
        /// The underlying error (part of the message, not a separate `source()`).
        error: std::io::Error,
    },

    /// The archive is not a valid zip file, or an entry cannot be decompressed.
    #[error("{}: invalid zip archive: {message}", path.display())]
    Zip {
        /// The archive.
        path: PathBuf,
        /// What is wrong.
        message: String,
    },

    /// The archive no longer contains the entry.
    #[error("{}: entry {name:?} not found", path.display())]
    EntryNotFound {
        /// The archive.
        path: PathBuf,
        /// Display name of the entry.
        name: String,
    },

    /// The entry's size or CRC32 differs from the recorded values.
    #[error("{}: entry {name:?} changed since the last scan", path.display())]
    EntryChanged {
        /// The archive.
        path: PathBuf,
        /// Display name of the entry.
        name: String,
    },

    /// The entry is encrypted.
    #[error("{}: entry {name:?} is encrypted", path.display())]
    Encrypted {
        /// The archive.
        path: PathBuf,
        /// Display name of the entry.
        name: String,
    },

    /// The book is larger than [`ReadLimits::max_book_size`].
    #[error("{}: book of {size} bytes exceeds the limit of {limit} bytes", path.display())]
    TooLarge {
        /// The file or archive.
        path: PathBuf,
        /// The (declared) size.
        size: u64,
        /// The limit.
        limit: u64,
    },

    /// A stored container path is absolute or leaves the library root.
    #[error("unsafe container path {}", .0.display())]
    UnsafePath(PathBuf),
}

impl ContainerError {
    /// The error without the file path, for messages shown in the UI: the path is
    /// absolute and would reveal the server's directory layout.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::container::ContainerError;
    ///
    /// let err = ContainerError::TooLarge { path: "/srv/books/a.fb2".into(), size: 9, limit: 8 };
    /// assert_eq!(err.detail(), "book of 9 bytes exceeds the limit of 8 bytes");
    /// ```
    pub fn detail(&self) -> String {
        match self {
            Self::Io { error, .. } => error.to_string(),
            Self::Zip { message, .. } => format!("invalid zip archive: {message}"),
            Self::EntryNotFound { name, .. } => format!("entry {name:?} not found"),
            Self::EntryChanged { name, .. } => {
                format!("entry {name:?} changed since the last scan")
            }
            Self::Encrypted { name, .. } => format!("entry {name:?} is encrypted"),
            Self::TooLarge { size, limit, .. } => {
                format!("book of {size} bytes exceeds the limit of {limit} bytes")
            }
            Self::UnsafePath(path) => format!("unsafe container path {}", path.display()),
        }
    }

    /// Whether the book is gone or changed, so a rescan is needed (rather than the
    /// file being broken).
    pub fn needs_rescan(&self) -> bool {
        match self {
            Self::Io { error, .. } => error.kind() == std::io::ErrorKind::NotFound,
            Self::EntryNotFound { .. } | Self::EntryChanged { .. } => true,
            _ => false,
        }
    }
}

/// Reads the bytes of the book at `location` inside the library at `root`.
///
/// # Examples
///
/// ```
/// use shelfgauge::container::{BookLocation, ReadLimits, read_book};
///
/// let root = tempfile::tempdir().unwrap();
/// std::fs::write(root.path().join("book.fb2"), b"<FictionBook/>").unwrap();
/// let location = BookLocation { container_path: "book.fb2".into(), entry: None };
/// let bytes = read_book(root.path(), &location, &ReadLimits::default()).unwrap();
/// assert_eq!(bytes, b"<FictionBook/>");
/// ```
pub fn read_book(
    root: &Path,
    location: &BookLocation,
    limits: &ReadLimits,
) -> Result<Vec<u8>, ContainerError> {
    let path = resolve(root, &location.container_path)?;
    match &location.entry {
        None => file::read(&path, limits),
        Some(entry) => zip::read_entry(&path, entry, limits),
    }
}

/// Joins a stored relative path to the root, rejecting paths that could escape it.
fn resolve(root: &Path, relative: &Path) -> Result<PathBuf, ContainerError> {
    let safe = relative.components().next().is_some()
        && relative
            .components()
            .all(|c| matches!(c, Component::Normal(_) | Component::CurDir));
    if !safe {
        return Err(ContainerError::UnsafePath(relative.to_owned()));
    }
    Ok(root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn container_kind_by_extension() {
        assert_eq!(ContainerKind::of(Path::new("a/b.zip")), ContainerKind::Zip);
        assert_eq!(ContainerKind::of(Path::new("B.ZIP")), ContainerKind::Zip);
        assert_eq!(ContainerKind::of(Path::new("b.fb2")), ContainerKind::File);
        assert_eq!(ContainerKind::of(Path::new("zip")), ContainerKind::File);
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        let root = Path::new("/library");
        assert_eq!(
            resolve(root, Path::new("a/b.fb2")).unwrap(),
            root.join("a/b.fb2")
        );
        for bad in ["", "../x.fb2", "a/../../x.fb2", "/etc/passwd"] {
            assert!(
                matches!(
                    resolve(root, Path::new(bad)),
                    Err(ContainerError::UnsafePath(_))
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn missing_file_needs_rescan() {
        let root = tempfile::tempdir().unwrap();
        let location = BookLocation {
            container_path: "gone.fb2".into(),
            entry: None,
        };
        let err = read_book(root.path(), &location, &ReadLimits::default()).unwrap_err();
        assert!(err.needs_rescan(), "{err}");
    }
}
