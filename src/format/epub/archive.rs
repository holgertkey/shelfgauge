//! The zip container of an EPUB (OCF), read from memory with limits.
//!
//! An EPUB is handed to the parser as bytes (a plain file, or an entry of a library
//! archive), so its own zip structure is read here, not by the container layer. Every
//! read is bounded: the number of entries, the uncompressed size of one entry (declared
//! and actual), its compression ratio, and the total uncompressed size read from one
//! book. Lying headers cannot inflate memory use: decompression stops one byte after
//! the declared size.

use std::collections::HashMap;
use std::io::{Cursor, Read};

use zip::ZipArchive;

use crate::format::ParseError;

/// Maximum number of entries in an EPUB.
const MAX_ENTRIES: usize = 10_000;

/// Maximum uncompressed size of one entry.
const MAX_ENTRY_SIZE: u64 = 64 * 1024 * 1024;

/// Maximum uncompressed size read from one EPUB in total.
const MAX_TOTAL_SIZE: u64 = 512 * 1024 * 1024;

/// Entries larger than this must not be compressed more than [`MAX_RATIO`] times.
const RATIO_MIN_SIZE: u64 = 1024 * 1024;

/// Maximum compression ratio of a large entry (zip bombs reach millions).
const MAX_RATIO: u64 = 1000;

/// An opened EPUB container.
pub(super) struct Archive<'a> {
    zip: ZipArchive<Cursor<&'a [u8]>>,
    /// Entry index by exact name.
    names: HashMap<String, usize>,
    /// Entry index by lowercase name, for books whose links differ in case.
    folded: HashMap<String, usize>,
    /// Uncompressed bytes read so far.
    total: u64,
}

impl<'a> Archive<'a> {
    /// Opens the zip structure of `data`.
    pub(super) fn open(data: &'a [u8]) -> Result<Self, ParseError> {
        let zip = ZipArchive::new(Cursor::new(data))
            .map_err(|e| ParseError::new(format!("not an EPUB: invalid zip container: {e}")))?;
        if zip.len() > MAX_ENTRIES {
            return Err(ParseError::new(format!(
                "the EPUB has {} entries, more than the limit of {MAX_ENTRIES}",
                zip.len()
            )));
        }
        let mut names = HashMap::new();
        let mut folded = HashMap::new();
        for index in 0..zip.len() {
            let Ok(file) = zip.by_index_data(index) else {
                continue;
            };
            if file.is_dir() {
                continue;
            }
            let name = String::from_utf8_lossy(file.name_raw()).into_owned();
            folded.entry(name.to_lowercase()).or_insert(index);
            names.entry(name).or_insert(index);
        }
        Ok(Self {
            zip,
            names,
            folded,
            total: 0,
        })
    }

    /// The index of the entry at `path`; exact match first, then ignoring case.
    fn index(&self, path: &str) -> Option<usize> {
        self.names
            .get(path)
            .or_else(|| self.folded.get(&path.to_lowercase()))
            .copied()
    }

    /// Whether the container has an entry at `path`.
    pub(super) fn contains(&self, path: &str) -> bool {
        self.index(path).is_some()
    }

    /// Reads the entry at `path`.
    ///
    /// `Ok(None)` if there is no such entry; an error if the entry is encrypted (zip
    /// encryption), broken or over a limit.
    pub(super) fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, ParseError> {
        let Some(index) = self.index(path) else {
            return Ok(None);
        };
        let error = |message: String| ParseError::new(format!("{path}: {message}"));
        let (size, compressed) = {
            let file = self
                .zip
                .by_index_data(index)
                .map_err(|e| error(e.to_string()))?;
            if file.encrypted() {
                return Err(error("the entry is encrypted".to_owned()));
            }
            (file.size(), file.compressed_size())
        };
        if size > MAX_ENTRY_SIZE {
            return Err(error(format!(
                "{size} bytes, more than the limit of {MAX_ENTRY_SIZE} bytes"
            )));
        }
        if size > RATIO_MIN_SIZE && size / compressed.max(1) > MAX_RATIO {
            return Err(error(format!(
                "compressed {} times, more than the limit of {MAX_RATIO}",
                size / compressed.max(1)
            )));
        }
        let total = self.total + size;
        if total > MAX_TOTAL_SIZE {
            return Err(ParseError::new(format!(
                "the EPUB unpacks to more than {MAX_TOTAL_SIZE} bytes"
            )));
        }
        let file = self.zip.by_index(index).map_err(|e| error(e.to_string()))?;
        let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        file.take(size + 1)
            .read_to_end(&mut data)
            .map_err(|e| error(format!("cannot unpack: {e}")))?;
        if data.len() as u64 != size {
            return Err(error("the entry has an unexpected size".to_owned()));
        }
        self.total = total;
        Ok(Some(data))
    }
}

/// Builds an EPUB-like zip from `(name, data)` pairs (stored or deflated), for tests.
#[cfg(test)]
pub(crate) fn zip_bytes(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in entries {
        let method = if *name == "mimetype" {
            zip::CompressionMethod::Stored
        } else {
            zip::CompressionMethod::Deflated
        };
        out.start_file(
            *name,
            SimpleFileOptions::default().compression_method(method),
        )
        .unwrap();
        out.write_all(data).unwrap();
    }
    out.finish().unwrap().into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_entries_by_exact_or_folded_name() {
        let data = zip_bytes(&[("OEBPS/Text/Ch1.xhtml", b"one"), ("mimetype", b"x")]);
        let mut archive = Archive::open(&data).unwrap();
        assert_eq!(
            archive.read("OEBPS/Text/Ch1.xhtml").unwrap().unwrap(),
            b"one"
        );
        assert_eq!(
            archive.read("oebps/text/ch1.xhtml").unwrap().unwrap(),
            b"one"
        );
        assert!(archive.read("missing").unwrap().is_none());
        assert!(archive.contains("mimetype"));
    }

    #[test]
    fn zip_bombs_are_refused() {
        let big = vec![0u8; 3 * 1024 * 1024];
        let data = zip_bytes(&[("bomb.xhtml", &big)]);
        let mut archive = Archive::open(&data).unwrap();
        let err = archive.read("bomb.xhtml").unwrap_err();
        assert!(err.message.contains("compressed"), "{err}");
    }

    #[test]
    fn non_zip_is_an_error() {
        let err = Archive::open(b"<FictionBook/>").err().unwrap();
        assert!(err.message.contains("not an EPUB"), "{err}");
    }
}
