//! The library root directory and the paths of shelfgauge's working data inside it.
//!
//! Everything shelfgauge writes lives in [`DATA_DIR_NAME`] (`.shelfgauge/`) inside the
//! library root, so the library stays self-contained and portable with its books.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// Name of the working-data directory inside the library root.
pub const DATA_DIR_NAME: &str = ".shelfgauge";

/// A library: a root directory of books plus its `.shelfgauge/` data directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Library {
    root: PathBuf,
}

impl Library {
    /// Opens the library at `root`, creating its `.shelfgauge/` directory if needed.
    ///
    /// `root` is made absolute (relative to the current directory) but symlinks are not
    /// resolved. Fails if `root` does not exist or is not a directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::library::Library;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let library = Library::open(dir.path()).unwrap();
    /// assert!(library.data_dir().is_dir());
    /// assert_eq!(library.config_path(), library.data_dir().join("config.toml"));
    /// ```
    pub fn open(root: &Path) -> Result<Self> {
        let root = std::path::absolute(root).map_err(|e| Error::io(root, e))?;
        match std::fs::metadata(&root) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(Error::NotADirectory(root)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::LibraryNotFound(root));
            }
            Err(e) => return Err(Error::io(root, e)),
        }
        let library = Self { root };
        let data_dir = library.data_dir();
        std::fs::create_dir_all(&data_dir).map_err(|e| Error::io(&data_dir, e))?;
        Ok(library)
    }

    /// The absolute path of the library root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The `.shelfgauge/` working-data directory.
    pub fn data_dir(&self) -> PathBuf {
        self.root.join(DATA_DIR_NAME)
    }

    /// The config file, `.shelfgauge/config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.data_dir().join("config.toml")
    }

    /// The SQLite index database, `.shelfgauge/library.db`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir().join("library.db")
    }

    /// The cover image cache directory, `.shelfgauge/covers/`.
    pub fn covers_dir(&self) -> PathBuf {
        self.data_dir().join("covers")
    }

    /// The parsed-content cache directory, `.shelfgauge/content/`.
    pub fn content_dir(&self) -> PathBuf {
        self.data_dir().join("content")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_creates_data_dir_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        assert_eq!(library.data_dir(), dir.path().join(DATA_DIR_NAME));
        assert!(library.data_dir().is_dir());
        assert_eq!(Library::open(dir.path()).unwrap(), library);
    }

    #[test]
    fn paths_live_inside_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let data = library.data_dir();
        assert_eq!(library.config_path(), data.join("config.toml"));
        assert_eq!(library.db_path(), data.join("library.db"));
        assert_eq!(library.covers_dir(), data.join("covers"));
        assert_eq!(library.content_dir(), data.join("content"));
    }

    #[test]
    fn missing_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let err = Library::open(&missing).unwrap_err();
        assert!(matches!(err, Error::LibraryNotFound(p) if p == missing));
        assert!(!missing.exists());
    }

    #[test]
    fn file_root_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("book.fb2");
        std::fs::write(&file, b"").unwrap();
        let err = Library::open(&file).unwrap_err();
        assert!(matches!(err, Error::NotADirectory(p) if p == file));
    }
}
