//! Scans that run on a background thread while the web server serves the index.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crate::container::ReadLimits;
use crate::format::Registry;
use crate::library::Library;
use crate::storage::open_writer;

use super::{ProgressSnapshot, ScanError, ScanProgress, ScanSummary, Scanner};

/// Runs at most one scan of a library at a time on a background thread.
///
/// Each scan opens its own writer connection, so it never blocks the read pool for
/// longer than one batched transaction. [`BackgroundScan::shutdown`] cancels a running
/// scan and waits for it: the writer commits the operations it has already received.
#[derive(Debug)]
pub struct BackgroundScan {
    library: Library,
    exclude: Vec<String>,
    progress: Arc<ScanProgress>,
    cancel: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    last: Arc<Mutex<Option<Result<ScanSummary, String>>>>,
}

impl BackgroundScan {
    /// Creates the controller for `library` with the `[scan].exclude` patterns; nothing
    /// runs until [`BackgroundScan::start`].
    pub fn new(library: Library, exclude: Vec<String>) -> Self {
        Self {
            library,
            exclude,
            progress: Arc::new(ScanProgress::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
            last: Arc::new(Mutex::new(None)),
        }
    }

    /// Starts a scan unless one is running or the controller is shut down.
    ///
    /// Returns whether a new scan was started.
    pub fn start(&self) -> bool {
        let mut thread = self.lock();
        if thread.as_ref().is_some_and(|t| !t.is_finished()) || self.cancel.load(Ordering::Relaxed)
        {
            return false;
        }
        let library = self.library.clone();
        let exclude = self.exclude.clone();
        let progress = Arc::clone(&self.progress);
        let cancel = Arc::clone(&self.cancel);
        let last = Arc::clone(&self.last);
        let spawned = std::thread::Builder::new()
            .name("scan".to_owned())
            .spawn(move || {
                tracing::info!("scanning {}", library.root().display());
                let result = run(&library, &exclude, &progress, &cancel);
                match &result {
                    Ok(summary) if summary.cancelled => {
                        tracing::info!("scan cancelled; changes found so far are saved");
                    }
                    Ok(summary) => tracing::info!(
                        "scan finished: {} added, {} updated, {} moved, {} removed, \
                         {} unchanged, {} errors",
                        summary.added,
                        summary.updated,
                        summary.moved,
                        summary.removed,
                        summary.unchanged,
                        summary.errors
                    ),
                    Err(e) => tracing::error!("scan failed: {e}"),
                }
                *last.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(result.map_err(|e| e.to_string()));
            });
        match spawned {
            Ok(handle) => {
                *thread = Some(handle);
                true
            }
            Err(e) => {
                tracing::error!("cannot start the scan thread: {e}");
                false
            }
        }
    }

    /// Whether a scan is running.
    pub fn is_running(&self) -> bool {
        self.lock().as_ref().is_some_and(|t| !t.is_finished())
    }

    /// The progress of the running or last scan.
    pub fn progress(&self) -> ProgressSnapshot {
        self.progress.snapshot()
    }

    /// The result of the last finished scan (the error message if it failed), or `None`
    /// before the first scan has finished.
    pub fn last_result(&self) -> Option<Result<ScanSummary, String>> {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Cancels a running scan, waits until it has stopped, and refuses later starts.
    ///
    /// Blocks the calling thread (up to the end of the current parse jobs and the
    /// writer's last transaction).
    pub fn shutdown(&self) {
        self.cancel.store(true, Ordering::Relaxed);
        let thread = self.lock().take();
        if let Some(thread) = thread
            && thread.join().is_err()
        {
            tracing::error!("the scan thread panicked");
        }
    }

    fn lock(&self) -> MutexGuard<'_, Option<JoinHandle<()>>> {
        self.thread.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// One scan with its own writer connection.
fn run(
    library: &Library,
    exclude: &[String],
    progress: &ScanProgress,
    cancel: &AtomicBool,
) -> Result<ScanSummary, ScanError> {
    let mut conn = open_writer(&library.db_path())?;
    let registry = Registry::with_builtin_formats();
    Scanner {
        library,
        exclude,
        registry: &registry,
        limits: ReadLimits::default(),
        threads: 0,
        progress,
        cancel,
    }
    .run(&mut conn)
}
