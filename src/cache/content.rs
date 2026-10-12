//! Parsed [`BookContent`], cached as `.shelfgauge/content/<book id>.bin`.
//!
//! An entry is valid only for the same content identity (the book's content key: a
//! plain file's hash, or a zip entry's size and CRC32) and the same
//! [`PARSER_VERSION`]; anything else is a miss. Keying by content rather than location
//! keeps the cache valid when a book is moved. The payload is postcard-encoded.

use std::path::{Path, PathBuf};

use super::write_atomic;
use crate::format::model::{BookContent, PARSER_VERSION};

/// The content hash of a plain-file book, as recorded by the scanner (xxh3-128,
/// big-endian).
pub fn content_hash(data: &[u8]) -> Vec<u8> {
    xxhash_rust::xxh3::xxh3_128(data).to_be_bytes().to_vec()
}

/// File signature of a cache entry.
const MAGIC: &[u8; 4] = b"SGC1";

/// The cache of parsed book content.
#[derive(Debug, Clone)]
pub struct ContentCache {
    dir: PathBuf,
}

impl ContentCache {
    /// A cache stored in `dir` (created on first store).
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    fn path(&self, id: i64) -> PathBuf {
        self.dir.join(format!("{id}.bin"))
    }

    /// The cached content of book `id`, if present and valid for `key`.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::cache::content::ContentCache;
    /// use shelfgauge::format::model::BookContent;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let cache = ContentCache::new(dir.path());
    /// cache.store(1, b"hash", &BookContent::default()).unwrap();
    /// assert!(cache.load(1, b"hash").is_some());
    /// assert!(cache.load(1, b"other").is_none());
    /// ```
    pub fn load(&self, id: i64, key: &[u8]) -> Option<BookContent> {
        let data = std::fs::read(self.path(id)).ok()?;
        let payload = strip_header(&data, key)?;
        match postcard::from_bytes(payload) {
            Ok(content) => Some(content),
            Err(e) => {
                tracing::debug!("discarding unreadable content cache for book {id}: {e}");
                None
            }
        }
    }

    /// Stores the content of book `id` for content key `key`.
    pub fn store(&self, id: i64, key: &[u8], content: &BookContent) -> std::io::Result<()> {
        let payload = postcard::to_stdvec(content).map_err(std::io::Error::other)?;
        let mut data = header(key);
        data.extend_from_slice(&payload);
        write_atomic(&self.path(id), &data)
    }

    /// Removes the cached content of book `id`, if any.
    pub fn remove(&self, id: i64) -> std::io::Result<()> {
        match std::fs::remove_file(self.path(id)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    /// The cache directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

fn header(key: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + key.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&PARSER_VERSION.to_le_bytes());
    out.extend_from_slice(&(key.len() as u32).to_le_bytes());
    out.extend_from_slice(key);
    out
}

/// The payload after a header matching `key` and the current parser version.
fn strip_header<'a>(data: &'a [u8], key: &[u8]) -> Option<&'a [u8]> {
    let expected = header(key);
    data.strip_prefix(expected.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::model::{Block, Body, Section};

    fn content() -> BookContent {
        let section = Section {
            title: vec![Block::paragraph("Глава")],
            blocks: vec![Block::paragraph("Текст.")],
            ..Section::default()
        };
        BookContent::new(vec![Body::main(section)], Default::default())
    }

    #[test]
    fn round_trip_and_key_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::new(dir.path().join("content"));
        assert!(cache.load(1, b"k").is_none());
        cache.store(1, b"k", &content()).unwrap();
        assert_eq!(cache.load(1, b"k").unwrap(), content());
        assert!(cache.load(1, b"k2").is_none());
        assert!(cache.load(2, b"k").is_none());
        cache.remove(1).unwrap();
        assert!(cache.load(1, b"k").is_none());
        cache.remove(1).unwrap();
    }

    #[test]
    fn other_parser_version_or_garbage_is_a_miss() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ContentCache::new(dir.path());
        cache.store(1, b"k", &content()).unwrap();
        let path = dir.path().join("1.bin");
        let mut data = std::fs::read(&path).unwrap();
        data[4] ^= 0xff; // parser version
        std::fs::write(&path, &data).unwrap();
        assert!(cache.load(1, b"k").is_none());

        let mut data = header(b"k");
        data.extend_from_slice(b"\xff\xff\xff garbage");
        std::fs::write(&path, &data).unwrap();
        assert!(cache.load(1, b"k").is_none());
        std::fs::write(&path, b"").unwrap();
        assert!(cache.load(1, b"k").is_none());
    }
}
