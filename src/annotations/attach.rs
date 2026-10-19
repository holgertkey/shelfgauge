//! Keeping annotations with their books: exporting a book's file, re-attaching
//! orphans lazily, and the cleanup after a scan.
//!
//! - An *orphan group* is the annotations (and reading position) of a content key that
//!   no live book has any more: the file was replaced or moved between a plain file and
//!   an archive. Orphans of a book still in the library are never deleted.
//! - **Re-attaching** happens when a book card or the reader is opened, never during a
//!   scan: candidate groups are orphans whose book id hint is the opened book or whose
//!   title + authors snapshot equals its metadata. Their quotes are searched for in the
//!   book; a group moves to the book only if at least [`REATTACH_THRESHOLD_PERCENT`] of
//!   its annotations are found. The others move too, marked `anchor_lost`, pointing
//!   at the start of their old section.
//! - **Cleanup** after a complete, uncancelled scan: a group whose book is gone (no
//!   live book with its key, its id hint removed or gone, and no live book with its
//!   title + authors) is soft-deleted and its export file removed; it is restored if
//!   the book comes back, and purged after the retention period. The export files of
//!   the other groups are brought up to date (renamed books, new orphans).
//!
//! Groups span all profiles: one content key is one group, whoever made the rows.
//! Every profile's export files are kept in its own folder (see
//! [`Exporter::for_profile`]).

use std::collections::HashSet;
use std::path::PathBuf;

use rusqlite::Connection;

use super::anchor::{Anchor, find};
use super::export::{ExportBook, Exporter};
use super::store::{self, Group};
use super::text::BookText;
use super::{BookSnapshot, now};
use crate::format::model::BlockId;
use crate::profiles::{ProfileId, store as profile_store};
use crate::storage::books::{
    BookDetails, BookStatus, book_by_id, live_books_by_key, live_books_by_title,
};
use crate::storage::{StorageError, normalize_key};

/// The share of a group's annotations that must be found in a book to move the group.
pub const REATTACH_THRESHOLD_PERCENT: usize = 80;

/// Errors while keeping annotations and their export in sync.
#[derive(Debug, thiserror::Error)]
pub enum AttachError {
    /// A database failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Writing an export file failed.
    #[error("cannot write the annotation export: {0}")]
    Export(#[from] std::io::Error),
}

impl From<rusqlite::Error> for AttachError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.into())
    }
}

/// The snapshot of an indexed book, or `None` if the scan recorded no content key.
pub fn snapshot(book: &BookDetails) -> Option<BookSnapshot> {
    Some(BookSnapshot {
        key: crate::render::load::content_key(book)?,
        id: book.id,
        title: book.title.clone(),
        authors: authors(book),
    })
}

/// A book's author display names joined with `, `.
pub fn authors(book: &BookDetails) -> String {
    book.authors
        .iter()
        .map(|a| a.display_name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The export book for a content key: the live book with that key, else the latest
/// snapshot of the group (an orphan).
fn export_book(
    index: &Connection,
    key: &[u8],
    fallback: &Group,
) -> Result<ExportBook, AttachError> {
    for id in live_books_by_key(index, key)? {
        if let Some(book) = book_by_id(index, id)? {
            return Ok(ExportBook {
                key: key.to_vec(),
                id: book.id,
                title: book.title.clone(),
                authors: authors(&book),
                orphan: false,
            });
        }
    }
    Ok(ExportBook {
        key: key.to_vec(),
        id: fallback.book_id,
        title: fallback.title.clone(),
        authors: fallback.authors.clone(),
        orphan: true,
    })
}

/// The profiles whose export files are kept: `(id, name)`.
fn export_profiles(user: &Connection) -> Result<Vec<(ProfileId, String)>, AttachError> {
    Ok(profile_store::list(user)?
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect())
}

/// Rewrites the export files of content key `key` from the database, one per profile
/// (or removes a profile's file if it has no live annotations in the book); returns
/// the written paths.
pub fn export_key(
    index: &Connection,
    user: &Connection,
    exporter: &Exporter,
    key: &[u8],
) -> Result<Vec<PathBuf>, AttachError> {
    let mut written = Vec::new();
    for (id, name) in export_profiles(user)? {
        let exporter = exporter.for_profile(id, &name);
        if let Some(path) = export_profile_key(index, user, &exporter, id, key)? {
            written.push(path);
        }
    }
    Ok(written)
}

/// Rewrites the export file of `profile` for content key `key` (or removes it).
fn export_profile_key(
    index: &Connection,
    user: &Connection,
    exporter: &Exporter,
    profile: ProfileId,
    key: &[u8],
) -> Result<Option<PathBuf>, AttachError> {
    let annotations = store::for_book(user, profile, key)?;
    let Some(latest) = annotations.iter().max_by_key(|a| a.updated_at) else {
        exporter.remove(key)?;
        return Ok(None);
    };
    let fallback = Group {
        key: key.to_vec(),
        book_id: latest.book_id,
        title: latest.book_title.clone(),
        authors: latest.book_authors.clone(),
        removed_at: None,
    };
    let book = export_book(index, key, &fallback)?;
    Ok(exporter.write(&book, &annotations)?)
}

/// Removes the export files of content key `key` of every profile.
fn remove_exports(user: &Connection, exporter: &Exporter, key: &[u8]) -> Result<(), AttachError> {
    for (id, name) in export_profiles(user)? {
        exporter.for_profile(id, &name).remove(key)?;
    }
    Ok(())
}

/// What [`export_all`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Files written.
    pub files: usize,
    /// Annotations in them.
    pub annotations: usize,
    /// Stale generated files removed.
    pub removed: usize,
}

/// Rewrites every export file of every profile, or only of `only`, and removes stale
/// generated files from their folders (`shelfgauge annotations export`).
pub fn export_all(
    index: &Connection,
    user: &Connection,
    exporter: &Exporter,
    only: Option<ProfileId>,
) -> Result<ExportSummary, AttachError> {
    let mut summary = ExportSummary::default();
    let groups = store::groups(user)?;
    for (id, name) in export_profiles(user)? {
        if only.is_some_and(|only| only != id) {
            continue;
        }
        let exporter = exporter.for_profile(id, &name);
        let mut keep = HashSet::new();
        for group in &groups {
            if group.removed_at.is_some() {
                continue;
            }
            let count = store::for_book(user, id, &group.key)?.len();
            if let Some(path) = export_profile_key(index, user, &exporter, id, &group.key)? {
                summary.files += 1;
                summary.annotations += count;
                keep.insert(path);
            }
        }
        summary.removed += exporter.remove_stale(&keep)?;
    }
    Ok(summary)
}

/// Whether two snapshots name the same title and authors.
fn same_metadata(group: &Group, book: &BookSnapshot) -> bool {
    normalize_key(&group.title) == normalize_key(&book.title)
        && normalize_key(&group.authors) == normalize_key(&book.authors)
}

/// The orphan groups that may belong to `book`: cheap (no book content needed), so a
/// caller can skip loading the book when there are none.
pub fn orphan_candidates(
    index: &Connection,
    user: &Connection,
    book: &BookSnapshot,
) -> Result<Vec<Group>, AttachError> {
    let mut candidates = Vec::new();
    for group in store::groups(user)? {
        if group.key == book.key
            || group.removed_at.is_some()
            || !(group.book_id == book.id || same_metadata(&group, book))
        {
            continue;
        }
        if live_books_by_key(index, &group.key)?.is_empty() {
            candidates.push(group);
        }
    }
    Ok(candidates)
}

/// Re-attaches the `candidates` (from [`orphan_candidates`]) to `book`, whose text is
/// `text`; returns how many groups moved.
#[tracing::instrument(level = "debug", skip_all, fields(book_id = book.id))]
pub fn reattach(
    index: &Connection,
    user: &mut Connection,
    exporter: &Exporter,
    book: &BookSnapshot,
    text: &BookText,
    candidates: &[Group],
) -> Result<usize, AttachError> {
    let mut moved = 0;
    for group in candidates {
        // The quotes of all profiles count towards the threshold: the file moved for
        // everyone.
        let annotations = store::for_key(user, &group.key)?;
        let found: Vec<Option<Anchor>> = annotations
            .iter()
            .map(|a| find(text, &a.quote, a.anchor.start_block))
            .collect();
        let hits = found.iter().filter(|f| f.is_some()).count();
        let positions: Vec<_> = store::positions_for_key(user, &group.key)?
            .into_iter()
            .map(|(profile, p)| {
                let found = find(text, &bookmark_quote(&p.suffix), p.block);
                (profile, p, found)
            })
            .collect();
        let enough = if annotations.is_empty() {
            positions.iter().any(|(_, _, found)| found.is_some())
        } else {
            hits * 100 >= annotations.len() * REATTACH_THRESHOLD_PERCENT
        };
        if !enough {
            tracing::debug!(
                book_id = book.id,
                found = hits,
                annotations = annotations.len(),
                "not re-attaching annotations"
            );
            continue;
        }
        let at = now();
        let tx = user.transaction()?;
        for (annotation, found) in annotations.iter().zip(&found) {
            let (anchor, lost) = match found {
                Some(anchor) => (*anchor, false),
                None => {
                    let start = annotation
                        .section_title
                        .as_deref()
                        .and_then(|title| text.section_start(title))
                        .or_else(|| text.leaves().first().copied())
                        .unwrap_or(BlockId::UNASSIGNED);
                    (Anchor::at_block(start), true)
                }
            };
            let section = if lost {
                annotation.section_title.clone()
            } else {
                text.section_title(anchor.start_block).map(str::to_owned)
            };
            store::reanchor(
                &tx,
                &annotation.id,
                book,
                &anchor,
                section.as_deref(),
                lost,
                at,
            )?;
        }
        for (profile, old, found) in &positions {
            let block = found.map(|a| a.start_block).unwrap_or_else(|| {
                if text.is_leaf(old.block) {
                    old.block
                } else {
                    text.leaves().first().copied().unwrap_or(old.block)
                }
            });
            let progress = text.progress(block).unwrap_or(0);
            store::move_position(&tx, *profile, &group.key, book, block, progress)?;
        }
        tx.commit()?;
        tracing::info!(
            book_id = book.id,
            annotations = annotations.len(),
            not_found = annotations.len() - hits,
            "re-attached annotations"
        );
        remove_exports(user, exporter, &group.key)?;
        moved += 1;
    }
    if moved > 0 {
        export_key(index, user, exporter, &book.key)?;
    }
    Ok(moved)
}

fn bookmark_quote(suffix: &str) -> super::anchor::Quote {
    super::anchor::Quote {
        suffix: suffix.to_owned(),
        ..Default::default()
    }
}

/// [`cleanup_after_scan`] for a library: opens its index and uses `user` with the
/// current time.
#[tracing::instrument(level = "debug", skip_all, name = "cleanup")]
pub fn cleanup_library(
    library: &crate::library::Library,
    user: &super::store::UserData,
    exporter: &Exporter,
    retention_days: u32,
) -> Result<CleanupSummary, AttachError> {
    let index = crate::storage::open_writer(&library.db_path())?;
    cleanup_after_scan(&index, &user.lock(), exporter, retention_days, now())
}

/// What [`cleanup_after_scan`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CleanupSummary {
    /// Groups soft-deleted because their book was removed.
    pub removed: usize,
    /// Groups restored because their book came back.
    pub restored: usize,
    /// Rows deleted for good after the retention period.
    pub purged: u64,
}

/// The cleanup after a complete, uncancelled scan (never call it after a cancelled
/// one): soft-deletes the groups of removed books, restores those whose book came
/// back, and purges removed groups older than `retention_days` (0: at once).
pub fn cleanup_after_scan(
    index: &Connection,
    user: &Connection,
    exporter: &Exporter,
    retention_days: u32,
    now: i64,
) -> Result<CleanupSummary, AttachError> {
    let mut summary = CleanupSummary::default();
    for group in store::groups(user)? {
        let hint_live = book_by_id(index, group.book_id)?
            .is_some_and(|book| book.status != BookStatus::Removed);
        let present = !live_books_by_key(index, &group.key)?.is_empty()
            || hint_live
            || !live_books_by_title(index, &group.title, &group.authors)?.is_empty();
        match (present, group.removed_at.is_some()) {
            (true, true) => {
                store::restore_group(user, &group.key)?;
                export_key(index, user, exporter, &group.key)?;
                summary.restored += 1;
            }
            (false, false) => {
                store::remove_group(user, &group.key, now)?;
                remove_exports(user, exporter, &group.key)?;
                summary.removed += 1;
            }
            // Present: the book's metadata may have changed, or it became an orphan
            // (its file was replaced or moved); the file says so. Unchanged files are
            // not rewritten.
            (true, false) => {
                export_key(index, user, exporter, &group.key)?;
            }
            (false, true) => {}
        }
    }
    let cutoff = now - i64::from(retention_days) * 86_400;
    summary.purged = store::purge_removed(user, cutoff)?;
    if summary != CleanupSummary::default() {
        tracing::info!(
            removed = summary.removed,
            restored = summary.restored,
            purged = summary.purged,
            "cleaned up the annotations of removed books"
        );
    }
    Ok(summary)
}
