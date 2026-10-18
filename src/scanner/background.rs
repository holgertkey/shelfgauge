//! Scans that run on a background thread while the web server serves the index.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use crate::config::{Config, LiveConfig};
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
    config: LiveConfig,
    progress: Arc<ScanProgress>,
    cancel: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    last: Arc<Mutex<Option<Result<ScanSummary, String>>>>,
    after_complete: Option<AfterScan>,
}

/// A hook run on the scan thread after every complete, uncancelled scan.
#[derive(Clone)]
struct AfterScan(Arc<dyn Fn() + Send + Sync>);

impl std::fmt::Debug for AfterScan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AfterScan")
    }
}

impl BackgroundScan {
    /// Creates the controller for `library` with the `[scan].exclude` patterns and the
    /// [default formats](crate::format::DEFAULT_FORMATS); nothing runs until
    /// [`BackgroundScan::start`].
    pub fn new(library: Library, exclude: Vec<String>) -> Self {
        let mut config = Config::default();
        config.scan.exclude = exclude;
        Self {
            library,
            config: LiveConfig::new(config),
            progress: Arc::new(ScanProgress::default()),
            cancel: Arc::new(AtomicBool::new(false)),
            thread: Mutex::new(None),
            last: Arc::new(Mutex::new(None)),
            after_complete: None,
        }
    }

    /// Takes the `[scan]` settings from `config` when each scan starts, instead of the
    /// patterns given to [`new`](Self::new) and the default formats.
    pub fn with_config(mut self, config: LiveConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets a hook that runs on the scan thread after every scan that completed without
    /// being cancelled (and never after a failed or cancelled one), before the scan
    /// counts as finished. Used to clean up the annotations of removed books.
    pub fn after_complete_scan(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.after_complete = Some(AfterScan(Arc::new(hook)));
        self
    }

    /// Starts a scan unless one is running or the controller is shut down.
    ///
    /// Returns whether a new scan was started.
    pub fn start(&self) -> bool {
        self.start_with(false)
    }

    /// Starts a scan that parses every book again, even unchanged ones (after a parser
    /// fix or a damaged cache), unless one is running or the controller is shut down.
    ///
    /// Returns whether a new scan was started.
    pub fn start_reread(&self) -> bool {
        self.start_with(true)
    }

    fn start_with(&self, reread: bool) -> bool {
        let mut thread = self.lock();
        if thread.as_ref().is_some_and(|t| !t.is_finished()) || self.cancel.load(Ordering::Relaxed)
        {
            return false;
        }
        let library = self.library.clone();
        let scan = self.config.get().scan.clone();
        let progress = Arc::clone(&self.progress);
        let cancel = Arc::clone(&self.cancel);
        let last = Arc::clone(&self.last);
        let after_complete = self.after_complete.clone();
        let spawned = std::thread::Builder::new()
            .name("scan".to_owned())
            .spawn(move || {
                tracing::info!("scanning {}", library.root().display());
                let registry = scan.registry();
                let result = run(
                    &library,
                    &scan.exclude,
                    &registry,
                    reread,
                    &progress,
                    &cancel,
                );
                match &result {
                    Ok(summary) if summary.cancelled => {
                        tracing::info!("scan cancelled; changes found so far are saved");
                    }
                    Ok(summary) => tracing::info!(
                        "scan finished: {} added, {} updated, {} moved, {} removed, \
                         {} unchanged, {} errors, {} not books",
                        summary.added,
                        summary.updated,
                        summary.moved,
                        summary.removed,
                        summary.unchanged,
                        summary.errors,
                        summary.not_books
                    ),
                    Err(e) => tracing::error!("scan failed: {e}"),
                }
                run_after_complete(&result, after_complete.as_ref());
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

/// Runs `hook` if `result` is a complete, uncancelled scan; returns whether it ran.
fn run_after_complete(result: &Result<ScanSummary, ScanError>, hook: Option<&AfterScan>) -> bool {
    match (result, hook) {
        (Ok(summary), Some(hook)) if !summary.cancelled => {
            (hook.0)();
            true
        }
        _ => false,
    }
}

/// One scan with its own writer connection.
fn run(
    library: &Library,
    exclude: &[String],
    registry: &Registry,
    reread: bool,
    progress: &ScanProgress,
    cancel: &AtomicBool,
) -> Result<ScanSummary, ScanError> {
    let mut conn = open_writer(&library.db_path())?;
    Scanner {
        library,
        exclude,
        registry,
        reread,
        limits: ReadLimits::default(),
        threads: 0,
        progress,
        cancel,
    }
    .run(&mut conn)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn hook_runs_only_after_a_complete_scan() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let hook = AfterScan(Arc::new(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        }));
        let complete = Ok(ScanSummary::default());
        let cancelled = Ok(ScanSummary {
            cancelled: true,
            ..ScanSummary::default()
        });
        let failed = Err(ScanError::from(crate::storage::StorageError::NewerSchema {
            found: 9,
            supported: 1,
        }));
        assert!(run_after_complete(&complete, Some(&hook)));
        assert!(!run_after_complete(&cancelled, Some(&hook)));
        assert!(!run_after_complete(&failed, Some(&hook)));
        assert!(!run_after_complete(&complete, None));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn background_scan_runs_the_hook() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let scan = BackgroundScan::new(library, Vec::new()).after_complete_scan(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });
        assert!(scan.start());
        while scan.is_running() {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        scan.shutdown();
    }
}
