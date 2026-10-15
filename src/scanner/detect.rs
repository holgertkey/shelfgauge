//! Change detection: comparing the files found by the walk with the index.
//!
//! - A container whose `(size, mtime)` equals the recorded values (and which was present
//!   at the last scan) is unchanged and not opened at all.
//! - A changed plain file is read and hashed later; an identical hash means only its
//!   stat changed.
//! - A changed archive's central directory is listed (no decompression); entries whose
//!   `(size, CRC32)` equal the recorded ones are unchanged.
//! - A book that failed to parse with an older parser
//!   ([`LocatedBook::retry_parse`]) is parsed again even if its file is unchanged.
//! - Books are considered vanished only when it is certain that their file is gone:
//!   never below a directory that could not be read, never for an archive that could
//!   not be listed, and never after an incomplete walk.

use std::collections::{HashMap, HashSet};

use super::walk::{Found, Walk};
use crate::container::ContainerKind;
use crate::container::zip::list_books;
use crate::storage::books::{BookStatus, EntryRecord, LocatedBook};
use crate::storage::containers::{StoredContainer, listed_formats};

/// A plain file that must be read.
#[derive(Debug, Clone)]
pub(super) struct FileJob {
    pub found: Found,
    /// The live book at this path and the recorded content hash, if any.
    pub existing: Option<(i64, Option<Vec<u8>>)>,
    /// Parse even if the content hash is unchanged (a retry with a newer parser).
    pub reparse: bool,
}

/// A changed archive that was listed successfully.
#[derive(Debug, Clone)]
pub(super) struct ArchivePlan {
    pub found: Found,
    /// Entries at locations with a live book whose content changed.
    pub changed: Vec<EntryRecord>,
    /// Entries at locations without a live book.
    pub new: Vec<EntryRecord>,
}

/// What a scan has to do.
#[derive(Debug, Default)]
pub(super) struct Plan {
    pub unchanged_books: u64,
    pub files: Vec<FileJob>,
    pub archives: Vec<ArchivePlan>,
    /// Live books whose location is gone.
    pub vanished: Vec<i64>,
    /// Containers that no longer exist.
    pub missing_containers: Vec<i64>,
    /// Archives that could not be listed.
    pub errors: u64,
}

/// Compares the walk with the recorded containers and books.
pub(super) fn plan(
    walk: &Walk,
    containers: &[StoredContainer],
    books: &[LocatedBook],
    extensions: &[&str],
) -> Plan {
    let mut live: HashMap<i64, Vec<&LocatedBook>> = HashMap::new();
    for book in books.iter().filter(|b| b.status != BookStatus::Removed) {
        live.entry(book.container_id).or_default().push(book);
    }
    let by_path: HashMap<&str, &StoredContainer> = containers
        .iter()
        .map(|c| (c.record.path.as_str(), c))
        .collect();

    // An archive listed for other formats is listed again.
    let formats = listed_formats(extensions);
    let mut plan = Plan::default();
    for found in &walk.found {
        let stored = by_path.get(found.path.as_str()).copied();
        let live_books = stored
            .and_then(|c| live.get(&c.id))
            .map_or(&[][..], Vec::as_slice);
        let unchanged_stat = stored.is_some_and(|c| {
            c.present
                && c.record.kind == found.kind
                && c.record.size == found.size
                && c.record.mtime_ns == found.mtime_ns
                && (found.kind == ContainerKind::File
                    || c.record.listed_formats.as_deref() == Some(formats.as_str()))
        });
        match found.kind {
            ContainerKind::File => {
                let book = live_books.iter().find(|b| b.entry_name.is_empty());
                let reparse = book.is_some_and(|b| b.retry_parse());
                if unchanged_stat && book.is_some() && !reparse {
                    plan.unchanged_books += 1;
                    continue;
                }
                plan.files.push(FileJob {
                    found: found.clone(),
                    existing: book.map(|b| (b.id, b.content_hash.clone())),
                    reparse,
                });
            }
            ContainerKind::Zip => {
                if unchanged_stat && !live_books.iter().any(|b| b.retry_parse()) {
                    plan.unchanged_books += live_books.len() as u64;
                    continue;
                }
                let entries = match list_books(&found.abs_path, extensions) {
                    Ok(entries) => entries,
                    Err(error) => {
                        tracing::warn!("cannot list archive {:?}: {error}", found.path);
                        plan.errors += 1;
                        continue;
                    }
                };
                let by_name: HashMap<&[u8], &LocatedBook> = live_books
                    .iter()
                    .map(|b| (b.entry_name.as_slice(), *b))
                    .collect();
                let mut archive = ArchivePlan {
                    found: found.clone(),
                    changed: Vec::new(),
                    new: Vec::new(),
                };
                let mut seen: HashSet<&[u8]> = HashSet::with_capacity(entries.len());
                for zip_entry in &entries {
                    let entry = EntryRecord {
                        name: zip_entry.entry.name.clone(),
                        path: zip_entry.display_name.clone(),
                        index: zip_entry.entry.index as u64,
                        size: zip_entry.entry.size,
                        crc32: zip_entry.entry.crc32,
                    };
                    match by_name.get(entry.name.as_slice()) {
                        Some(book) => {
                            seen.insert(book.entry_name.as_slice());
                            if book.entry_size == Some(entry.size)
                                && book.entry_crc32 == Some(entry.crc32)
                                && !book.retry_parse()
                            {
                                plan.unchanged_books += 1;
                            } else {
                                archive.changed.push(entry);
                            }
                        }
                        None => archive.new.push(entry),
                    }
                }
                for book in live_books {
                    if !seen.contains(book.entry_name.as_slice()) {
                        plan.vanished.push(book.id);
                    }
                }
                plan.archives.push(archive);
            }
        }
    }

    if walk.complete {
        let found: HashSet<&str> = walk.found.iter().map(|f| f.path.as_str()).collect();
        for container in containers.iter().filter(|c| c.present) {
            let path = container.record.path.as_str();
            let protected = walk.unreadable_dirs.iter().any(|dir| {
                path == dir
                    || path
                        .strip_prefix(dir.as_str())
                        .is_some_and(|r| r.starts_with('/'))
            });
            if found.contains(path) || protected {
                continue;
            }
            plan.missing_containers.push(container.id);
            if let Some(books) = live.get(&container.id) {
                plan.vanished.extend(books.iter().map(|b| b.id));
            }
        }
    }
    plan
}
