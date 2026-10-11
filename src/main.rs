//! shelfgauge command-line entry point.

mod cli;

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use shelfgauge::config::Config;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::library::Library;
use shelfgauge::scanner::{ScanPhase, ScanProgress, Scanner};

use cli::{Cli, Mode};

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

    match invocation.mode {
        Mode::Scan => scan(&library, &config),
        Mode::Serve { scan } => {
            // Temporary stub until the web server (Phase 8) exists.
            println!("Library: {}", library.root().display());
            println!("Config:  {}", config_path.display());
            println!();
            println!("Effective configuration:");
            println!();
            print!("{}", config.to_toml());
            println!();
            println!(
                "Serving is not implemented yet (background scan: {}).",
                if scan { "on" } else { "off" }
            );
            Ok(())
        }
    }
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
