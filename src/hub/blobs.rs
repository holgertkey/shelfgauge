//! The blob directory, `<data>/blobs/`: one file per publication blob.
//!
//! Layout: `<first two hex digits of the id>/<publication id>/manifest`, `p/<n>`,
//! `i/<n>`. Paths are built only from a parsed [`PublicationId`] and [`BlobPath`],
//! never from request text, so no request can name a file outside the directory.
//! Writes are atomic: a temporary file in the target directory, then a rename, so a
//! reader sees the old blob or the new one, never a partial one.

use std::io::Write;
use std::path::PathBuf;

use crate::publication::format::{BlobPath, PublicationId};

/// The blob directory of a hub.
#[derive(Debug, Clone)]
pub(crate) struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// The store in `root` (created on the first write).
    pub(crate) fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// The directory of publication `id`.
    fn dir(&self, id: &PublicationId) -> PathBuf {
        let hex = id.to_string();
        self.root.join(&hex[..2]).join(hex)
    }

    /// The file of blob `path` of publication `id`.
    fn file(&self, id: &PublicationId, path: BlobPath) -> PathBuf {
        let dir = self.dir(id);
        match path {
            BlobPath::Manifest => dir.join("manifest"),
            BlobPath::Part(n) => dir.join("p").join(n.to_string()),
            BlobPath::Image(n) => dir.join("i").join(n.to_string()),
        }
    }

    /// Writes the blob atomically.
    pub(crate) fn write(
        &self,
        id: &PublicationId,
        path: BlobPath,
        data: &[u8],
    ) -> std::io::Result<()> {
        let target = self.file(id, path);
        let parent = target.parent().expect("blob files have a parent");
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(data)?;
        file.as_file().sync_data()?;
        file.persist(&target).map_err(|e| e.error)?;
        Ok(())
    }

    /// Reads the blob; `None` if its file does not exist.
    pub(crate) fn read(
        &self,
        id: &PublicationId,
        path: BlobPath,
    ) -> std::io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.file(id, path)) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Removes all blobs of publication `id` (nothing if there are none).
    pub(crate) fn remove(&self, id: &PublicationId) -> std::io::Result<()> {
        let dir = self.dir(id);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e),
        }
        // The prefix directory goes when it is empty; a failure only leaves it.
        if let Some(prefix) = dir.parent() {
            let _ = std::fs::remove_dir(prefix);
        }
        Ok(())
    }

    /// The root directory.
    #[cfg(test)]
    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

/// Whether `dir` holds any file, recursively.
#[cfg(test)]
fn has_files(dir: &std::path::Path) -> bool {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .any(|e| e.file_type().is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blobs_round_trip_and_are_removed_with_their_publication() {
        let dir = tempfile::tempdir().unwrap();
        let store = BlobStore::new(dir.path().join("blobs"));
        let id = PublicationId::new([0xab; 16]);
        assert_eq!(store.read(&id, BlobPath::Manifest).unwrap(), None);
        store.write(&id, BlobPath::Manifest, b"m").unwrap();
        store.write(&id, BlobPath::Part(3), b"p3").unwrap();
        store.write(&id, BlobPath::Image(0), b"i0").unwrap();
        store.write(&id, BlobPath::Part(3), b"new").unwrap();
        assert_eq!(
            store.read(&id, BlobPath::Part(3)).unwrap().as_deref(),
            Some(&b"new"[..])
        );
        let hex = id.to_string();
        assert!(
            store
                .root()
                .join("ab")
                .join(&hex)
                .join("p")
                .join("3")
                .is_file()
        );
        assert!(
            store
                .root()
                .join("ab")
                .join(&hex)
                .join("i")
                .join("0")
                .is_file()
        );
        store.remove(&id).unwrap();
        store.remove(&id).unwrap();
        assert!(!has_files(store.root()));
        assert!(!store.root().join("ab").exists());
    }
}
