//! Loading a book's [`BookContent`]: from the content cache, or by reading and parsing
//! the book (and caching the result).
//!
//! The cache key is the book's content identity recorded by the last scan
//! ([`content_key`]). A book read from disk must still match it: a plain file is
//! hashed like the scanner hashes it, and a zip entry is checked against its size and
//! CRC32 by the container layer. A mismatch means the index is out of date, so the book
//! is reported as needing a rescan instead of being shown (or cached) with the wrong
//! identity. A cache hit is served even if the file has gone since: the content is
//! still the indexed one.
//!
//! Parsing recurses deeply: call [`load`] on a thread with
//! [`PARSER_STACK_SIZE`](crate::format::PARSER_STACK_SIZE) of stack (see
//! [`run_with_parser_stack`](crate::format::run_with_parser_stack)).

use std::path::PathBuf;

use crate::cache::content::{ContentCache, content_hash};
use crate::container::{BookLocation, EntryRef, ReadLimits, read_book};
use crate::format::Registry;
use crate::format::model::BookContent;
use crate::library::Library;
use crate::storage::books::{BookDetails, BookStatus};

/// Why a book's content could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// The book's file is gone or changed since the last scan: a rescan is needed.
    #[error("the book's file is missing or changed: {0}")]
    NeedsRescan(String),
    /// The book cannot be read or parsed.
    #[error("{0}")]
    Unreadable(String),
}

/// The content-cache key of a book: a plain file's content hash, or a zip entry's
/// size and CRC32 (little-endian `u64` + `u32`). `None` if the scan recorded neither.
pub fn content_key(book: &BookDetails) -> Option<Vec<u8>> {
    match &book.entry {
        Some(entry) => {
            let mut key = entry.size.to_le_bytes().to_vec();
            key.extend_from_slice(&entry.crc32.to_le_bytes());
            Some(key)
        }
        None => book.content_hash.clone(),
    }
}

/// Loads the content of `book` from the cache, or reads, verifies, parses and caches
/// it.
pub fn load(
    library: &Library,
    registry: &Registry,
    limits: &ReadLimits,
    book: &BookDetails,
) -> Result<BookContent, LoadError> {
    if book.status == BookStatus::Removed {
        return Err(LoadError::NeedsRescan(
            "the book was removed from the library".to_owned(),
        ));
    }
    let cache = ContentCache::new(library.content_dir());
    let key = content_key(book);
    if let Some(key) = &key
        && let Some(content) = cache.load(book.id, key)
    {
        return Ok(content);
    }

    let location = BookLocation {
        container_path: PathBuf::from(&book.container_path),
        entry: book.entry.as_ref().map(|entry| EntryRef {
            name: entry.name.clone(),
            index: entry.index as usize,
            size: entry.size,
            crc32: entry.crc32,
        }),
    };
    let data = read_book(library.root(), &location, limits).map_err(|e| {
        if e.needs_rescan() {
            LoadError::NeedsRescan(e.to_string())
        } else {
            LoadError::Unreadable(e.to_string())
        }
    })?;
    if book.entry.is_none()
        && let Some(expected) = &book.content_hash
        && content_hash(&data) != *expected
    {
        return Err(LoadError::NeedsRescan(format!(
            "{} changed since the last scan",
            book.container_path
        )));
    }
    let parser = registry
        .for_extension(&book.format)
        .ok_or_else(|| LoadError::Unreadable(format!("unsupported format {:?}", book.format)))?;
    let content = parser
        .parse_content(&data)
        .map_err(|e| LoadError::Unreadable(e.to_string()))?;
    if let Some(key) = &key
        && let Err(e) = cache.store(book.id, key, &content)
    {
        tracing::warn!("cannot cache the content of book {}: {e}", book.id);
    }
    Ok(content)
}
