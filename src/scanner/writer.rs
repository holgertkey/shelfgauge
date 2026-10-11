//! The single database writer: applies scan results in batched transactions.
//!
//! All index changes of a scan go through one thread, so there is never more than one
//! writer and many small results share a transaction. Messages are applied in the order
//! they are received; a plain file's container and book arrive in one message, so they
//! are always committed together.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::cache::covers;
use crate::format::model::BookMetadata;
use crate::storage::StorageError;
use crate::storage::books::{
    BookRecord, EntryRecord, find_live_at, mark_removed, relocate, upsert_book,
};
use crate::storage::containers::{
    ContainerRecord, container_by_path, mark_container_missing, upsert_container,
};

/// Maximum number of operations per transaction.
const BATCH_SIZE: usize = 500;

/// Maximum time a batch is kept open waiting for more operations.
const BATCH_TIME: Duration = Duration::from_millis(500);

/// Which container a book belongs to.
#[derive(Debug)]
pub(super) enum ContainerRef {
    /// A plain file; the container is written with the book.
    File(ContainerRecord),
    /// An archive started with [`Op::StartArchive`], by path.
    Archive(String),
}

/// The result of parsing one book.
#[derive(Debug)]
pub(super) enum Outcome {
    /// Parsed metadata (including the cover).
    Parsed(Box<BookMetadata>),
    /// Why the book could not be parsed.
    Failed(String),
}

/// One change to the index.
#[derive(Debug)]
pub(super) enum Op {
    /// An archive is about to be (re)indexed: record it with an unfinished stat.
    StartArchive(ContainerRecord),
    /// All books of an archive are written: record its real stat.
    FinishArchive(ContainerRecord),
    /// A parsed (or failed) book at a location.
    Book {
        container: ContainerRef,
        entry: Option<EntryRecord>,
        content_hash: Option<Vec<u8>>,
        format: &'static str,
        fallback_title: String,
        outcome: Outcome,
    },
    /// A plain file whose stat changed but whose content did not.
    Touch(ContainerRecord),
    /// A book moved to a new location.
    Relocate {
        book_id: i64,
        container: ContainerRef,
        entry: Option<EntryRecord>,
    },
    /// A book whose file is gone.
    Remove(i64),
    /// A container that no longer exists.
    ContainerMissing(i64),
}

/// What the writer did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct WriterStats {
    pub added: u64,
    pub updated: u64,
    pub moved: u64,
    pub removed: u64,
    pub failed_books: u64,
}

/// Applies operations from `rx` until all senders are gone.
pub(super) fn run(
    conn: &mut Connection,
    rx: Receiver<Op>,
    covers_dir: PathBuf,
) -> Result<WriterStats, StorageError> {
    let mut writer = Writer {
        stats: WriterStats::default(),
        archives: HashMap::new(),
        covers_dir,
    };
    while let Ok(first) = rx.recv() {
        let tx = conn.transaction()?;
        writer.apply(&tx, first)?;
        let deadline = Instant::now() + BATCH_TIME;
        let mut count = 1;
        let mut done = false;
        while count < BATCH_SIZE {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(op) => {
                    writer.apply(&tx, op)?;
                    count += 1;
                }
                Err(RecvTimeoutError::Timeout) => break,
                Err(RecvTimeoutError::Disconnected) => {
                    done = true;
                    break;
                }
            }
        }
        tx.commit()?;
        if done {
            break;
        }
    }
    Ok(writer.stats)
}

struct Writer {
    stats: WriterStats,
    archives: HashMap<String, i64>,
    covers_dir: PathBuf,
}

impl Writer {
    fn apply(&mut self, conn: &Connection, op: Op) -> Result<(), StorageError> {
        match op {
            Op::StartArchive(record) | Op::FinishArchive(record) => {
                let id = upsert_container(conn, &record)?;
                self.archives.insert(record.path, id);
            }
            Op::Touch(record) => {
                upsert_container(conn, &record)?;
            }
            Op::Book {
                container,
                entry,
                content_hash,
                format,
                fallback_title,
                outcome,
            } => {
                let container_id = self.container_id(conn, &container)?;
                let entry_name = entry.as_ref().map_or(&[][..], |e| &e.name[..]);
                let existed = find_live_at(conn, container_id, entry_name)?.is_some();
                let (record, cover) = match outcome {
                    Outcome::Parsed(metadata) => (
                        BookRecord::parsed(
                            container_id,
                            entry,
                            content_hash,
                            format,
                            &metadata,
                            &fallback_title,
                        ),
                        metadata.cover,
                    ),
                    Outcome::Failed(message) => {
                        self.stats.failed_books += 1;
                        let record = BookRecord::failed(
                            container_id,
                            entry,
                            content_hash,
                            format,
                            &fallback_title,
                            &message,
                        );
                        (record, None)
                    }
                };
                let id = upsert_book(conn, &record)?;
                let cover_result = match &cover {
                    Some(image) => covers::store(&self.covers_dir, id, image),
                    None => covers::remove(&self.covers_dir, id),
                };
                if let Err(e) = cover_result {
                    tracing::warn!("cannot update the cover of book {id}: {e}");
                }
                if existed {
                    self.stats.updated += 1;
                } else {
                    self.stats.added += 1;
                }
            }
            Op::Relocate {
                book_id,
                container,
                entry,
            } => {
                let container_id = self.container_id(conn, &container)?;
                relocate(conn, book_id, container_id, entry.as_ref())?;
                self.stats.moved += 1;
            }
            Op::Remove(id) => {
                mark_removed(conn, id)?;
                self.stats.removed += 1;
            }
            Op::ContainerMissing(id) => mark_container_missing(conn, id)?,
        }
        Ok(())
    }

    fn container_id(
        &mut self,
        conn: &Connection,
        container: &ContainerRef,
    ) -> Result<i64, StorageError> {
        match container {
            ContainerRef::File(record) => upsert_container(conn, record),
            ContainerRef::Archive(path) => {
                if let Some(&id) = self.archives.get(path) {
                    return Ok(id);
                }
                let id = match container_by_path(conn, path)? {
                    Some(stored) => stored.id,
                    None => {
                        return Err(StorageError::Sqlite(rusqlite::Error::QueryReturnedNoRows));
                    }
                };
                self.archives.insert(path.clone(), id);
                Ok(id)
            }
        }
    }
}
