//! shelfgauge command-line entry point.

mod cli;

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use shelfgauge::config::Config;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::{PARSER_STACK_SIZE, Registry};
use shelfgauge::library::Library;
use shelfgauge::scanner::{BackgroundScan, ScanPhase, ScanProgress, Scanner};
use shelfgauge::web;

use cli::{Cli, Mode};

/// Read connections for the web handlers.
const READ_POOL_SIZE: u32 = 4;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli::init_logging(cli.verbose);
    let invocation = cli.invocation();

    let library = if invocation.explicit_path {
        Library::open(&invocation.path)?
    } else {
        // Without an explicit PATH, never turn the current directory into a library.
        Library::open_existing(&invocation.path).map_err(|e| match e {
            shelfgauge::Error::NotALibrary(_) => anyhow::anyhow!(
                "{e}\nTo create a library here, run `shelfgauge .` or `shelfgauge scan .`; \
                 or pass the path of an existing library."
            ),
            e => e.into(),
        })?
    };
    let config_path = library.config_path();
    let loaded = Config::load_or_create(&config_path)?;
    if loaded.created {
        tracing::info!("created default config {}", config_path.display());
    }
    for key in &loaded.unknown_keys {
        tracing::warn!(
            "unknown key `{key}` in {} is ignored",
            config_path.display()
        );
    }
    let mut config = loaded.config;
    config.apply_overrides(&invocation.overrides)?;
    tracing::debug!(
        "config {}, effective values:\n{}",
        config_path.display(),
        config.to_toml().trim_end()
    );

    match invocation.mode {
        Mode::Scan => scan(&library, &config),
        Mode::Serve { scan } => serve(library, &config, scan),
    }
}

/// The default command: serves the library, scanning it in the background.
fn serve(library: Library, config: &Config, scan: bool) -> anyhow::Result<()> {
    // Fail fast, before touching the index: unsafe bind address or busy port.
    let listener = web::bind(&config.server, web::Auth::None)?;
    let db_path = library.db_path();
    // The writer creates and migrates the database; readers need it to exist.
    let writer = shelfgauge::storage::open_writer(&db_path)
        .with_context(|| format!("cannot open the index {}", db_path.display()))?;
    let pool = shelfgauge::storage::open_read_pool(&db_path, READ_POOL_SIZE)
        .with_context(|| format!("cannot open the index {}", db_path.display()))?;
    drop(writer);

    let background = Arc::new(BackgroundScan::new(
        library.clone(),
        config.scan.exclude.clone(),
    ));
    let state = web::AppState::new(library, config, pool, Arc::clone(&background));
    let url = web::display_url(&config.server, state.urls());
    let router = web::router(state);

    // Handlers may parse books on blocking threads, which need the parser stack.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(PARSER_STACK_SIZE)
        .build()
        .context("cannot start the async runtime")?;
    let result = runtime.block_on(async {
        let shutdown = shutdown_signal().context("cannot listen for Ctrl+C")?;
        println!("Serving {url}  (Ctrl+C to stop)");
        if scan {
            background.start();
        }
        web::serve(listener, router, shutdown)
            .await
            .context("the web server failed")
    });
    // The writer commits what it has received before the scan thread ends.
    if background.is_running() {
        eprintln!("Stopping the scan…");
    }
    background.shutdown();
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

/// Listens for Ctrl+C (and SIGTERM on Unix); the returned future completes on the first
/// signal, and a second Ctrl+C then exits at once.
///
/// The listeners are registered right away (inside the runtime), so a Ctrl+C that comes
/// as soon as the URL is printed is not lost.
#[cfg(unix)]
fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send + 'static> {
    use tokio::signal::unix::{SignalKind, signal};
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    Ok(async move {
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
        announce_shutdown();
        tokio::spawn(async move {
            if interrupt.recv().await.is_some() {
                std::process::exit(130);
            }
        });
    })
}

/// Listens for Ctrl+C; the returned future completes on the first one, and a second one
/// then exits at once.
#[cfg(windows)]
fn shutdown_signal() -> std::io::Result<impl Future<Output = ()> + Send + 'static> {
    let mut ctrl_c = tokio::signal::windows::ctrl_c()?;
    Ok(async move {
        ctrl_c.recv().await;
        announce_shutdown();
        tokio::spawn(async move {
            if ctrl_c.recv().await.is_some() {
                std::process::exit(130);
            }
        });
    })
}

fn announce_shutdown() {
    eprintln!("Shutting down… (press Ctrl+C again to quit immediately)");
}

/// `shelfgauge scan`: updates the index and prints a summary.
fn scan(library: &Library, config: &Config) -> anyhow::Result<()> {
    let mut conn = shelfgauge::storage::open_writer(&library.db_path())
        .with_context(|| format!("cannot open the index {}", library.db_path().display()))?;
    let registry = Registry::with_builtin_formats();
    let progress = ScanProgress::default();
    let cancel = AtomicBool::new(false);
    let scanner = Scanner {
        library,
        exclude: &config.scan.exclude,
        registry: &registry,
        limits: ReadLimits::default(),
        threads: 0,
        progress: &progress,
        cancel: &cancel,
    };
    let started = Instant::now();
    let finished = AtomicBool::new(false);
    let summary = std::thread::scope(|scope| {
        if std::io::stderr().is_terminal() {
            scope.spawn(|| show_progress(&progress, &finished));
        }
        let result = scanner.run(&mut conn);
        finished.store(true, Ordering::Relaxed);
        result
    })?;
    println!(
        "Scanned {} in {:.1} s: {} added, {} updated, {} moved, {} removed, {} unchanged, \
         {} errors ({} parsed){}.",
        library.root().display(),
        started.elapsed().as_secs_f64(),
        summary.added,
        summary.updated,
        summary.moved,
        summary.removed,
        summary.unchanged,
        summary.errors,
        summary.parsed,
        if summary.skipped > 0 {
            format!(", {} skipped (non-UTF-8 paths)", summary.skipped)
        } else {
            String::new()
        }
    );
    Ok(())
}

/// Redraws a progress line on stderr until `finished` is set.
fn show_progress(progress: &ScanProgress, finished: &AtomicBool) {
    let mut stderr = std::io::stderr();
    while !finished.load(Ordering::Relaxed) {
        let p = progress.snapshot();
        let line = match p.phase {
            ScanPhase::Indexing if p.total > 0 => {
                format!("Indexing: {} / {} ({} errors)", p.done, p.total, p.errors)
            }
            ScanPhase::Indexing | ScanPhase::Finishing | ScanPhase::Done => "Finishing…".to_owned(),
            _ => "Looking for changes…".to_owned(),
        };
        let _ = write!(stderr, "\r\x1b[2K{line}");
        let _ = stderr.flush();
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = write!(stderr, "\r\x1b[2K");
    let _ = stderr.flush();
}
