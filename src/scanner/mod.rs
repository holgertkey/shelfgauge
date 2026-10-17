//! The scanner: builds and incrementally updates the library index.
//!
//! A scan runs in stages:
//!
//! 1. **Walk** the library, collecting book files and archives with their
//!    `(size, mtime)`.
//! 2. **Compare** with the index: unchanged containers are skipped
//!    without being opened; changed archives are listed from their central directory.
//! 3. **Match** moved books ([`matcher`]): new zip entries are matched by
//!    `(size, CRC32)` before anything is decompressed; new plain files by content hash
//!    after they are read. Matched books keep their id and are not parsed again.
//! 4. **Parse** the rest in parallel (metadata only) on a dedicated thread pool with
//!    [`PARSER_STACK_SIZE`] stacks; a panicking parser is caught and the book is
//!    recorded with an `error` status.
//! 5. **Write** everything through a single writer thread in batched transactions,
//!    caching covers on the way; finally mark vanished books `removed` and record the
//!    scan ([`record_scan`]).
//!
//! The scan is crash-resistant: an archive's real `(size, mtime)` is committed only
//! after all of its books (until then it carries [`UNFINISHED_MTIME`]), and a plain
//! file's container and book are committed together, so an interrupted scan is
//! completed by the next one. Unreadable directories, archives that cannot be opened
//! and transient read errors never cause books to be removed; a cancelled scan removes
//! nothing.

use std::collections::{HashMap, HashSet};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};

use rayon::prelude::*;
use rusqlite::Connection;

use crate::container::zip::ArchiveReader;
use crate::container::{ContainerError, ContainerKind, EntryRef, ReadLimits, file};
use crate::format::{BookParser, PARSER_STACK_SIZE, Registry};
use crate::library::Library;
use crate::storage::StorageError;
use crate::storage::books::{BookStatus, EntryRecord, all_books};
use crate::storage::containers::{ContainerRecord, list_containers};
use crate::storage::scans::{ScanRecord, record_scan};

pub mod background;
mod detect;
pub mod matcher;
mod walk;
mod writer;

use detect::FileJob;
use walk::Found;
use writer::{ContainerRef, Op, Outcome};

pub use background::BackgroundScan;
pub use walk::mtime_ns;

/// The modification time recorded for an archive while its books are being written;
/// never equal to a real file time, so an interrupted scan re-examines the archive.
pub const UNFINISHED_MTIME: i64 = i64::MIN;

/// The minimum number of entries of one archive handled by one parallel job.
const CHUNK_SIZE: usize = 64;

/// Parallel jobs per parser thread that one archive is split into at most.
const CHUNKS_PER_THREAD: usize = 4;

/// Entries per job for an archive with `entries` entries to index.
///
/// Every job opens the archive, which parses its whole central directory, so a huge
/// archive must not be cut into many small jobs (that is quadratic: 200 000 entries in
/// chunks of 64 took minutes). At most `threads * CHUNKS_PER_THREAD` jobs per archive
/// keep all threads busy.
fn chunk_size(entries: usize, threads: usize) -> usize {
    CHUNK_SIZE.max(entries.div_ceil(threads.max(1) * CHUNKS_PER_THREAD))
}

/// Errors that abort a scan. Problems with individual books never do; they are
/// recorded and counted in [`ScanSummary::errors`].
#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    /// Reading or writing the index failed.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// A `[scan].exclude` pattern is invalid.
    #[error("invalid exclude pattern {pattern:?}: {message}")]
    InvalidExclude {
        /// The pattern.
        pattern: String,
        /// What is wrong with it.
        message: String,
    },

    /// The parser thread pool could not be created.
    #[error("cannot start scanner threads: {0}")]
    ThreadPool(String),

    /// The database writer thread panicked.
    #[error("the database writer thread panicked")]
    WriterPanicked,
}

/// The current stage of a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScanPhase {
    /// No scan has started.
    Idle,
    /// Walking the directory tree.
    Walking,
    /// Comparing files with the index.
    Comparing,
    /// Reading, parsing and writing books.
    Indexing,
    /// Removing vanished books and committing.
    Finishing,
    /// The scan is over.
    Done,
}

impl ScanPhase {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Walking,
            2 => Self::Comparing,
            3 => Self::Indexing,
            4 => Self::Finishing,
            5 => Self::Done,
            _ => Self::Idle,
        }
    }
}

/// Live progress counters of a scan, shared with the CLI or web UI.
#[derive(Debug)]
pub struct ScanProgress {
    phase: AtomicU8,
    total: AtomicU64,
    done: AtomicU64,
    errors: AtomicU64,
}

impl Default for ScanProgress {
    fn default() -> Self {
        Self {
            phase: AtomicU8::new(ScanPhase::Idle as u8),
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            errors: AtomicU64::new(0),
        }
    }
}

/// A consistent-enough reading of [`ScanProgress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgressSnapshot {
    /// The current stage.
    pub phase: ScanPhase,
    /// Books to index in this scan (known once comparing is done).
    pub total: u64,
    /// Books indexed so far.
    pub done: u64,
    /// Errors so far.
    pub errors: u64,
}

impl ScanProgress {
    /// The current values.
    pub fn snapshot(&self) -> ProgressSnapshot {
        ProgressSnapshot {
            phase: ScanPhase::from_u8(self.phase.load(Ordering::Relaxed)),
            total: self.total.load(Ordering::Relaxed),
            done: self.done.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }

    fn set_phase(&self, phase: ScanPhase) {
        self.phase.store(phase as u8, Ordering::Relaxed);
    }

    fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        self.done.store(0, Ordering::Relaxed);
        self.errors.store(0, Ordering::Relaxed);
    }
}

/// What a scan did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanSummary {
    /// New books.
    pub added: u64,
    /// Books re-indexed because their content changed.
    pub updated: u64,
    /// Books found at a new location (keeping their id).
    pub moved: u64,
    /// Books whose file is gone.
    pub removed: u64,
    /// Books left as they were.
    pub unchanged: u64,
    /// Books parsed (zero for a scan without changes).
    pub parsed: u64,
    /// Books that failed to parse, archives that could not be listed, and files that
    /// could not be read.
    pub errors: u64,
    /// Files skipped because their path is not valid UTF-8.
    pub skipped: u64,
    /// Files of a book format that are not books (a `readme.txt`; see
    /// [`ParseErrorKind::NotABook`](crate::format::ParseErrorKind::NotABook)): not
    /// recorded as books, and not counted again while they stay unchanged.
    pub not_books: u64,
    /// Whether the scan was cancelled (then nothing was removed).
    pub cancelled: bool,
}

/// Scans one library.
#[derive(Debug, Clone, Copy)]
pub struct Scanner<'a> {
    /// The library to scan.
    pub library: &'a Library,
    /// `[scan].exclude` patterns.
    pub exclude: &'a [String],
    /// The available book formats.
    pub registry: &'a Registry,
    /// Read limits for books.
    pub limits: ReadLimits,
    /// Parser threads; 0 for one per CPU.
    pub threads: usize,
    /// Progress counters to update.
    pub progress: &'a ScanProgress,
    /// Set to stop the scan early.
    pub cancel: &'a AtomicBool,
}

/// Shared state of the parallel part of a scan.
struct Run<'a> {
    scanner: &'a Scanner<'a>,
    tx: SyncSender<Op>,
    writer_gone: AtomicBool,
    parsed: AtomicU64,
    errors: AtomicU64,
    unchanged: AtomicU64,
    not_books: AtomicU64,
    file_candidates: HashMap<Vec<u8>, Vec<i64>>,
    /// New plain files set aside for matching: hash, CRC32 and the file.
    deferred: Mutex<Vec<(Vec<u8>, u32, Found)>>,
}

impl Scanner<'_> {
    /// Runs a scan, writing through `conn` (the index's writer connection).
    pub fn run(&self, conn: &mut Connection) -> Result<ScanSummary, ScanError> {
        let started_at = crate::annotations::now();
        let progress = self.progress;
        progress.reset();
        progress.set_phase(ScanPhase::Walking);
        let root = self.library.root();
        let excludes = walk::excludes(root, self.exclude)?;
        let walk = walk::walk(root, &excludes, self.registry, self.cancel);

        progress.set_phase(ScanPhase::Comparing);
        let containers = list_containers(conn)?;
        let books = all_books(conn)?;
        let extensions = self.registry.extensions();
        let plan = detect::plan(&walk, &containers, &books, &extensions);
        let formats = crate::storage::containers::listed_formats(&extensions);

        // Candidates for moved books: vanished in this scan, or removed earlier.
        let vanished: HashSet<i64> = plan.vanished.iter().copied().collect();
        let mut file_candidates: HashMap<Vec<u8>, Vec<i64>> = HashMap::new();
        let mut entry_candidates: HashMap<(u64, u32), Vec<i64>> = HashMap::new();
        for book in &books {
            if book.status != BookStatus::Removed && !vanished.contains(&book.id) {
                continue;
            }
            match (&book.content_hash, book.entry_size, book.entry_crc32) {
                (Some(hash), _, _) if book.entry_name.is_empty() => {
                    file_candidates
                        .entry(hash.clone())
                        .or_default()
                        .push(book.id);
                }
                (_, Some(size), Some(crc)) if !book.entry_name.is_empty() => {
                    entry_candidates
                        .entry((size, crc))
                        .or_default()
                        .push(book.id);
                }
                _ => {}
            }
        }

        // Match new archive entries before decompressing anything.
        let new_entries = plan
            .archives
            .iter()
            .enumerate()
            .flat_map(|(i, archive)| {
                archive
                    .new
                    .iter()
                    .map(move |e| ((e.size, e.crc32), (i, e.clone())))
            })
            .collect();
        let (zip_moves, zip_new) = matcher::match_unique(new_entries, &entry_candidates);
        let mut archive_work: Vec<Vec<EntryRecord>> =
            plan.archives.iter().map(|a| a.changed.clone()).collect();
        for (i, entry) in zip_new {
            archive_work[i].push(entry);
        }
        let relocated: HashSet<i64> = zip_moves.iter().map(|(id, _)| *id).collect();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads)
            .stack_size(PARSER_STACK_SIZE)
            .thread_name(|i| format!("scan-{i}"))
            .build()
            .map_err(|e| ScanError::ThreadPool(e.to_string()))?;
        let threads = pool.current_num_threads();

        let total =
            plan.files.len() + archive_work.iter().map(Vec::len).sum::<usize>() + zip_moves.len();
        progress.total.store(total as u64, Ordering::Relaxed);
        progress.errors.store(plan.errors, Ordering::Relaxed);
        progress.set_phase(ScanPhase::Indexing);

        let (tx, rx) = sync_channel::<Op>(256);
        let covers_dir = self.library.covers_dir();

        let writer_conn = &mut *conn;
        let (stats, counters) = std::thread::scope(|scope| {
            let writer = scope.spawn(move || writer::run(writer_conn, rx, covers_dir));
            let run = Run {
                scanner: self,
                tx,
                writer_gone: AtomicBool::new(false),
                parsed: AtomicU64::new(0),
                errors: AtomicU64::new(plan.errors),
                unchanged: AtomicU64::new(plan.unchanged_books),
                not_books: AtomicU64::new(0),
                file_candidates,
                deferred: Mutex::new(Vec::new()),
            };

            for archive in &plan.archives {
                run.send(Op::StartArchive(ContainerRecord {
                    mtime_ns: UNFINISHED_MTIME,
                    ..archive_record(&archive.found, &formats)
                }));
            }
            for (book_id, (i, entry)) in zip_moves {
                run.send(Op::Relocate {
                    book_id,
                    container: ContainerRef::Archive(plan.archives[i].found.path.clone()),
                    entry: Some(entry),
                });
                progress.done.fetch_add(1, Ordering::Relaxed);
            }

            // One job per plain file and per chunk of archive entries.
            let remaining: Vec<AtomicUsize> = archive_work
                .iter()
                .map(|work| AtomicUsize::new(work.len().div_ceil(chunk_size(work.len(), threads))))
                .collect();
            let failed: Vec<AtomicBool> = archive_work
                .iter()
                .map(|_| AtomicBool::new(false))
                .collect();
            for (i, work) in archive_work.iter().enumerate() {
                if work.is_empty() {
                    run.send(Op::FinishArchive(archive_record(
                        &plan.archives[i].found,
                        &formats,
                    )));
                }
            }
            let chunks: Vec<(usize, &[EntryRecord])> = archive_work
                .iter()
                .enumerate()
                .flat_map(|(i, work)| {
                    work.chunks(chunk_size(work.len(), threads))
                        .map(move |chunk| (i, chunk))
                })
                .collect();
            pool.install(|| {
                plan.files.par_iter().for_each(|job| run.file(job, true));
                chunks.par_iter().for_each(|&(i, chunk)| {
                    let archive = &plan.archives[i].found;
                    if !run.archive_chunk(archive, chunk) {
                        failed[i].store(true, Ordering::Relaxed);
                    }
                    if remaining[i].fetch_sub(1, Ordering::AcqRel) == 1
                        && !failed[i].load(Ordering::Acquire)
                    {
                        run.send(Op::FinishArchive(archive_record(archive, &formats)));
                    }
                });
            });

            // Plain files whose hash matches a moved-book candidate.
            let deferred =
                std::mem::take(&mut *run.deferred.lock().unwrap_or_else(|e| e.into_inner()));
            let keyed = deferred
                .into_iter()
                .map(|(hash, crc32, found)| (hash.clone(), (hash, crc32, found)))
                .collect();
            let (file_moves, file_new) = matcher::match_unique(keyed, &run.file_candidates);
            let mut relocated = relocated;
            for (book_id, (hash, crc32, found)) in file_moves {
                relocated.insert(book_id);
                run.send(Op::Relocate {
                    book_id,
                    container: ContainerRef::File(file_record(&found, Some((hash, crc32)))),
                    entry: None,
                });
                progress.done.fetch_add(1, Ordering::Relaxed);
            }
            let rest: Vec<FileJob> = file_new
                .into_iter()
                .map(|(_, _, found)| FileJob {
                    found,
                    existing: None,
                    reparse: false,
                })
                .collect();
            pool.install(|| rest.par_iter().for_each(|job| run.file(job, false)));

            progress.set_phase(ScanPhase::Finishing);
            if !self.cancelled() {
                for id in plan.vanished.iter().filter(|id| !relocated.contains(id)) {
                    run.send(Op::Remove(*id));
                }
                for id in &plan.missing_containers {
                    run.send(Op::ContainerMissing(*id));
                }
            }
            let Run {
                tx,
                parsed,
                errors,
                unchanged,
                not_books,
                ..
            } = run;
            drop(tx);
            let counters = (
                unchanged.into_inner(),
                parsed.into_inner(),
                errors.into_inner(),
                not_books.into_inner(),
            );
            (writer.join(), counters)
        });
        progress.set_phase(ScanPhase::Done);
        let stats = stats.map_err(|_| ScanError::WriterPanicked)??;
        let (unchanged, parsed, errors, not_books) = counters;

        let summary = ScanSummary {
            added: stats.added,
            updated: stats.updated,
            moved: stats.moved,
            removed: stats.removed,
            unchanged,
            parsed,
            errors: errors + stats.failed_books,
            skipped: walk.skipped_non_utf8,
            not_books,
            cancelled: self.cancelled(),
        };
        record_scan(
            conn,
            &ScanRecord {
                started_at,
                finished_at: crate::annotations::now(),
                added: summary.added,
                updated: summary.updated,
                moved: summary.moved,
                removed: summary.removed,
                unchanged: summary.unchanged,
                parsed: summary.parsed,
                errors: summary.errors,
                skipped: summary.skipped,
                not_books: summary.not_books,
                cancelled: summary.cancelled,
            },
        )?;
        Ok(summary)
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed)
    }
}

impl Run<'_> {
    fn send(&self, op: Op) {
        if self.tx.send(op).is_err() {
            // The writer failed; its error is reported when it is joined.
            self.writer_gone.store(true, Ordering::Relaxed);
        }
    }

    fn stopped(&self) -> bool {
        self.scanner.cancelled() || self.writer_gone.load(Ordering::Relaxed)
    }

    fn error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
        self.scanner.progress.errors.fetch_add(1, Ordering::Relaxed);
    }

    fn done(&self) {
        self.scanner.progress.done.fetch_add(1, Ordering::Relaxed);
    }

    /// Reads, hashes and (if needed) parses one plain file. With `defer`, new files
    /// whose hash matches a moved-book candidate are set aside for matching.
    fn file(&self, job: &FileJob, defer: bool) {
        if self.stopped() {
            return;
        }
        let found = &job.found;
        let Some(parser) = self.scanner.registry.for_path(&found.abs_path) else {
            return;
        };
        let data = match file::read(&found.abs_path, &self.scanner.limits) {
            Ok(data) => data,
            Err(error) if is_transient(&error) => {
                tracing::warn!("cannot read {:?}: {error}", found.path);
                self.error();
                self.done();
                return;
            }
            Err(error) => {
                self.send(Op::Book {
                    container: ContainerRef::File(file_record(found, None)),
                    entry: None,
                    content_hash: None,
                    format: parser.format(),
                    fallback_title: fallback_title(&found.path),
                    outcome: Outcome::Failed(error.detail()),
                });
                self.done();
                return;
            }
        };
        let hash = crate::cache::content::content_hash(&data);
        let crc32 = crc32fast::hash(&data);
        if let Some((_, Some(stored))) = &job.existing
            && *stored == hash
            && !job.reparse
        {
            self.send(Op::Touch(file_record(found, Some((hash, crc32)))));
            self.unchanged.fetch_add(1, Ordering::Relaxed);
            self.done();
            return;
        }
        if defer && job.existing.is_none() && self.file_candidates.contains_key(&hash) {
            self.deferred
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((hash, crc32, found.clone()));
            return;
        }
        // Parser log lines (e.g. a repaired book) name the book.
        let outcome = {
            let _book = tracing::info_span!("book", path = %found.path).entered();
            self.parse(parser, &data, file_name(&found.path))
        };
        self.send(Op::Book {
            container: ContainerRef::File(file_record(found, Some((hash.clone(), crc32)))),
            entry: None,
            content_hash: Some(hash),
            format: parser.format(),
            fallback_title: fallback_title(&found.path),
            outcome,
        });
        self.done();
    }

    /// Reads and parses a chunk of one archive's entries through one open handle.
    /// Returns `false` if the archive must not be marked complete (transient errors,
    /// cancellation).
    fn archive_chunk(&self, archive: &Found, entries: &[EntryRecord]) -> bool {
        if self.stopped() {
            return false;
        }
        let mut reader = match ArchiveReader::open(&archive.abs_path) {
            Ok(reader) => reader,
            Err(error) => {
                tracing::warn!("cannot open archive {:?}: {error}", archive.path);
                self.error();
                return false;
            }
        };
        let mut complete = true;
        for entry in entries {
            if self.stopped() {
                return false;
            }
            let Some(parser) = self.scanner.registry.for_path(entry.path.as_ref()) else {
                continue;
            };
            let entry_ref = EntryRef {
                name: entry.name.clone(),
                index: entry.index as usize,
                size: entry.size,
                crc32: entry.crc32,
            };
            let outcome = match reader.read_entry(&entry_ref, &self.scanner.limits) {
                Ok(data) => {
                    let _book =
                        tracing::info_span!("book", path = %format!("{}/{}", archive.path, entry.path))
                            .entered();
                    self.parse(parser, &data, file_name(&entry.path))
                }
                Err(error) if is_transient(&error) => {
                    tracing::warn!(
                        "cannot read {:?} in {:?}: {error}",
                        entry.path,
                        archive.path
                    );
                    self.error();
                    self.done();
                    complete = false;
                    continue;
                }
                Err(error) => Outcome::Failed(error.detail()),
            };
            self.send(Op::Book {
                container: ContainerRef::Archive(archive.path.clone()),
                entry: Some(entry.clone()),
                content_hash: None,
                format: parser.format(),
                fallback_title: fallback_title(&entry.path),
                outcome,
            });
            self.done();
        }
        complete
    }

    /// Parses metadata, turning errors and panics into [`Outcome::Failed`] and files
    /// that are not books into [`Outcome::NotABook`].
    fn parse(&self, parser: &dyn BookParser, data: &[u8], name: &str) -> Outcome {
        self.parsed.fetch_add(1, Ordering::Relaxed);
        match catch_unwind(AssertUnwindSafe(|| parser.parse_metadata_named(data, name))) {
            Ok(Ok(metadata)) => Outcome::Parsed(Box::new(metadata)),
            Ok(Err(error)) if error.is_not_a_book() => {
                tracing::debug!("not a book: {}", error.message);
                self.not_books.fetch_add(1, Ordering::Relaxed);
                Outcome::NotABook
            }
            Ok(Err(error)) => Outcome::Failed(error.message),
            Err(panic) => {
                let message = panic
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_owned())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                Outcome::Failed(format!("the parser panicked: {message}"))
            }
        }
    }
}

/// Errors that may go away on their own (the file is being changed or replaced), as
/// opposed to a broken book.
fn is_transient(error: &ContainerError) -> bool {
    matches!(
        error,
        ContainerError::Io { .. }
            | ContainerError::EntryNotFound { .. }
            | ContainerError::EntryChanged { .. }
    )
}

/// The record of an archive listed for `formats` (see
/// [`listed_formats`](crate::storage::containers::listed_formats)).
fn archive_record(found: &Found, formats: &str) -> ContainerRecord {
    ContainerRecord {
        path: found.path.clone(),
        kind: ContainerKind::Zip,
        size: found.size,
        mtime_ns: found.mtime_ns,
        content_hash: None,
        crc32: None,
        listed_formats: Some(formats.to_owned()),
    }
}

/// The record of a plain file with its content hash and CRC32, if it could be read.
fn file_record(found: &Found, identity: Option<(Vec<u8>, u32)>) -> ContainerRecord {
    let (content_hash, crc32) = identity.unzip();
    ContainerRecord {
        path: found.path.clone(),
        kind: ContainerKind::File,
        size: found.size,
        mtime_ns: found.mtime_ns,
        content_hash,
        crc32,
        listed_formats: None,
    }
}

/// The last component of a library path (`/`-separated, an archive entry may use `\\`).
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The title used when a book has none: its file name without the extension.
fn fallback_title(path: &str) -> String {
    let name = file_name(path);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_owned(),
        _ => name.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huge_archives_are_cut_into_few_jobs() {
        assert_eq!(chunk_size(10, 4), CHUNK_SIZE);
        assert_eq!(chunk_size(1_000, 4), CHUNK_SIZE);
        assert_eq!(chunk_size(2_000, 4), 125);
        assert_eq!(chunk_size(200_000, 4), 12_500);
        assert_eq!(200_000usize.div_ceil(chunk_size(200_000, 4)), 16);
        assert_eq!(chunk_size(1_000, 0), 250, "0 threads count as 1");
    }

    #[test]
    fn file_names() {
        assert_eq!(file_name("a/b/readme.txt"), "readme.txt");
        assert_eq!(file_name("dir\\Автор - Книга.txt"), "Автор - Книга.txt");
        assert_eq!(file_name("book.txt"), "book.txt");
    }

    #[test]
    fn fallback_titles() {
        assert_eq!(fallback_title("a/b/Война и мир.fb2"), "Война и мир");
        assert_eq!(fallback_title("dir\\book.FB2"), "book");
        assert_eq!(fallback_title(".fb2"), ".fb2");
        assert_eq!(fallback_title("noext"), "noext");
    }

    #[test]
    fn phase_round_trip() {
        for phase in [
            ScanPhase::Idle,
            ScanPhase::Walking,
            ScanPhase::Comparing,
            ScanPhase::Indexing,
            ScanPhase::Finishing,
            ScanPhase::Done,
        ] {
            assert_eq!(ScanPhase::from_u8(phase as u8), phase);
        }
    }
}
