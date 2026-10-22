//! shelfgauge command-line entry point.

mod cli;

use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use shelfgauge::annotations::attach;
use shelfgauge::annotations::export::Exporter;
use shelfgauge::annotations::store::UserData;
use shelfgauge::config::{Config, ConfigOverrides, LiveConfig};
use shelfgauge::container::ReadLimits;
use shelfgauge::format::PARSER_STACK_SIZE;
use shelfgauge::library::Library;
use shelfgauge::profiles::{Profile, Role, password, session, store as profile_store};
use shelfgauge::scanner::{BackgroundScan, ScanPhase, ScanProgress, Scanner};
use shelfgauge::web;

use cli::{Cli, HubAction, Mode, ProfilesAction};

/// Read connections for the web handlers.
const READ_POOL_SIZE: u32 = 4;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    cli::init_logging(cli.log_level(), cli.log_format.into());
    let invocation = cli.invocation();
    if let Mode::Hub(action) = invocation.mode {
        return hub(&invocation.path, invocation.explicit_path, action);
    }

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
        tracing::info!(config = ?config_path, "created the default config");
    }
    for key in &loaded.unknown_keys {
        tracing::warn!(key = ?key, config = ?config_path, "unknown config key is ignored");
    }
    let mut config = loaded.config;
    config.apply_overrides(&invocation.overrides)?;
    // Debug-escaped: one log line, not one per config line.
    tracing::debug!(
        config = ?config_path,
        values = ?config.to_toml().trim_end(),
        "effective config"
    );

    match invocation.mode {
        Mode::Scan => scan(&library, &config),
        Mode::Serve { scan } => serve(library, &config, &invocation.overrides, scan),
        Mode::ExportAnnotations { profile } => {
            export_annotations(&library, &config, profile.as_deref())
        }
        Mode::Profiles(action) => profiles(&library, action),
        Mode::Errors => errors(&library),
        Mode::Hub(_) => unreachable!("handled before the library is opened"),
    }
}

/// `shelfgauge hub …`: serves or manages the hub in `data` (created only if
/// `explicit`, when serving).
fn hub(data: &std::path::Path, explicit: bool, action: HubAction) -> anyhow::Result<()> {
    use shelfgauge::hub::{self, Hub, HubState};

    let create = explicit && action == HubAction::Serve;
    let opened = Hub::open(data, create)?;
    let config_path = data.join(hub::CONFIG_FILE);
    if opened.created {
        tracing::info!(config = ?config_path, "created a new hub");
    }
    for key in &opened.unknown_keys {
        tracing::warn!(key = ?key, config = ?config_path, "unknown config key is ignored");
    }
    let hub = opened.hub;
    let now = shelfgauge::annotations::now();
    match action {
        HubAction::Serve => {
            let listener = hub::bind(hub.config())?;
            let state = HubState::new(hub);
            let url = state.display_url();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("cannot start the async runtime")?;
            runtime.block_on(async {
                let shutdown = shutdown_signal().context("cannot listen for Ctrl+C")?;
                println!("Hub serving {url}  (Ctrl+C to stop)");
                hub::serve(listener, state, shutdown)
                    .await
                    .context("the hub server failed")
            })?;
            runtime.shutdown_timeout(Duration::from_secs(1));
        }
        HubAction::Takedown { id } => {
            let id: shelfgauge::publication::format::PublicationId = id
                .trim()
                .to_ascii_lowercase()
                .parse()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if hub.takedown(&id, now)? {
                println!("Took the publication down; its id is refused from now on.");
            } else {
                println!("There is no such publication; its id is refused from now on.");
            }
        }
        HubAction::Accounts => {
            let accounts = hub.accounts()?;
            for a in &accounts {
                let days = |t: i64| (now - t).max(0) / 86_400;
                println!(
                    "{:>5}  created {} days ago  last seen {}  {} devices  {} publications  \
                     {} records  {} bytes",
                    a.id,
                    days(a.created_at),
                    a.last_seen_at
                        .map_or("never".to_owned(), |t| format!("{} days ago", days(t))),
                    a.usage.devices,
                    a.usage.publications,
                    a.usage.records,
                    a.usage.bytes
                );
            }
            match accounts.len() {
                1 => println!("1 account."),
                n => println!("{n} accounts."),
            }
        }
        HubAction::Invite => {
            let (code, _) = hub.invite(now)?;
            println!("{}", code.as_str());
            eprintln!(
                "A one-time invite code, valid for {} days: enter it when registering a \
                 library on this hub.",
                hub.config().accounts.invite_days
            );
        }
    }
    Ok(())
}

/// The exporter of a library's annotations, linking to the configured server address.
fn exporter(library: &Library, config: &Config) -> Exporter {
    let urls = web::Urls::new(&config.server.base_path);
    Exporter::new(
        library.annotations_dir(),
        &web::display_url(&config.server, &urls),
    )
}

/// `shelfgauge annotations export`: rewrites every export file (of every profile, or
/// of `profile`) and removes stale ones.
fn export_annotations(
    library: &Library,
    config: &Config,
    profile: Option<&str>,
) -> anyhow::Result<()> {
    let index = shelfgauge::storage::open_writer(&library.db_path())
        .with_context(|| format!("cannot open the index {}", library.db_path().display()))?;
    let user = open_userdata(library)?;
    let user = user.lock();
    let exporter = exporter(library, config);
    let (only, dir) = match profile {
        Some(name) => {
            let profile = find_profile(&user, name)?;
            (
                Some(profile.id),
                exporter.profile_dir(profile.id, &profile.name),
            )
        }
        None => (None, library.annotations_dir()),
    };
    let summary = attach::export_all(&index, &user, &exporter, only)?;
    println!(
        "Exported {} annotations of {} books to {}; removed {} stale files.",
        summary.annotations,
        summary.files,
        dir.display(),
        summary.removed
    );
    Ok(())
}

/// The profile named `name`; an unknown name is an error that lists the profiles.
fn find_profile(user: &rusqlite::Connection, name: &str) -> anyhow::Result<Profile> {
    if let Some(profile) = profile_store::by_name(user, name)? {
        return Ok(profile);
    }
    let names: Vec<String> = profile_store::list(user)?
        .into_iter()
        .map(|p| p.name)
        .collect();
    anyhow::bail!(
        "there is no profile named {name:?}; the profiles are: {}",
        names.join(", ")
    )
}

/// `shelfgauge profiles …`.
fn profiles(library: &Library, action: ProfilesAction) -> anyhow::Result<()> {
    let user = open_userdata(library)?;
    let conn = user.lock();
    match action {
        ProfilesAction::List => {
            for profile in profile_store::list(&conn)? {
                println!(
                    "{:>3}  {}  {}{}  {}  {}",
                    profile.id,
                    profile.name,
                    profile.role.as_str(),
                    if profile.id.is_owner() { ", owner" } else { "" },
                    if profile.has_pin { "PIN" } else { "no PIN" },
                    if profile.has_password {
                        "password"
                    } else {
                        "no password"
                    }
                );
            }
        }
        ProfilesAction::Add { name, admin } => {
            let role = if admin { Role::Admin } else { Role::Reader };
            let profile =
                profile_store::create(&conn, &name, 0, role, shelfgauge::annotations::now())?;
            println!(
                "Added the profile {} ({}).",
                profile.name,
                profile.role.as_str()
            );
        }
        ProfilesAction::Remove { name, yes } => {
            let profile = find_profile(&conn, &name)?;
            if profile.id.is_owner() {
                anyhow::bail!("the owner profile {} cannot be removed", profile.name);
            }
            if !yes
                && !confirm(&format!(
                    "Delete the profile {} with its bookmarks, quotes and reading places? \
                 Its Markdown export is kept. [y/N] ",
                    profile.name
                ))?
            {
                println!("Nothing was deleted.");
                return Ok(());
            }
            profile_store::delete(&conn, profile.id)?;
            println!("Deleted the profile {}.", profile.name);
        }
        ProfilesAction::SetPassword { name } => {
            let profile = find_profile(&conn, &name)?;
            // Not while the database is locked: the prompt waits for the user.
            drop(conn);
            let entered = read_new_password(&profile.name)?;
            let hash = password::hash(&entered).map_err(|e| anyhow::anyhow!("{e}"))?;
            let conn = user.lock();
            let now = shelfgauge::annotations::now();
            profile_store::set_password_hash(&conn, profile.id, Some(&hash), now)?;
            let ended = session::end_password_sessions(&conn, profile.id, None)?;
            println!(
                "Set the password of {}; signed out {ended} devices that logged in with \
                 the old one.",
                profile.name
            );
        }
        ProfilesAction::RemovePassword { name } => {
            let profile = find_profile(&conn, &name)?;
            let now = shelfgauge::annotations::now();
            profile_store::set_password_hash(&conn, profile.id, None, now)?;
            let ended = session::end_password_sessions(&conn, profile.id, None)?;
            println!(
                "Removed the password of {}; signed out {ended} devices. The profile can be \
                 used only on this computer now.",
                profile.name
            );
        }
        ProfilesAction::ResetPin { name } => {
            let profile = find_profile(&conn, &name)?;
            profile_store::set_pin_hash(&conn, profile.id, None)?;
            let ended = shelfgauge::profiles::session::end_others(&conn, profile.id, None)?;
            println!(
                "Removed the PIN of {}; signed out {ended} sessions.",
                profile.name
            );
        }
    }
    Ok(())
}

/// A new password: asked twice on the terminal without echo, or one line of standard
/// input when it is not a terminal (for scripts; only the line break is removed, so
/// spaces stay part of the password). Checked like on My profile.
fn read_new_password(name: &str) -> anyhow::Result<String> {
    let entered = if std::io::stdin().is_terminal() {
        let first = rpassword::prompt_password(format!(
            "New password of {name} (at least {} characters): ",
            password::MIN_CHARS
        ))?;
        password::check_format(&first).map_err(|e| anyhow::anyhow!("{e}"))?;
        let second = rpassword::prompt_password("Repeat the password: ")?;
        if first != second {
            anyhow::bail!("the two passwords differ; nothing was changed");
        }
        first
    } else {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let line = line.strip_suffix('\n').unwrap_or(&line);
        line.strip_suffix('\r').unwrap_or(line).to_owned()
    };
    password::check_format(&entered).map_err(|e| anyhow::anyhow!("{e}; nothing was changed"))?;
    Ok(entered)
}

/// Asks a yes/no question on the terminal; anything but `y`/`yes` is no.
fn confirm(question: &str) -> anyhow::Result<bool> {
    print!("{question}");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// `shelfgauge errors`: lists the books that could not be parsed, one per line.
fn errors(library: &Library) -> anyhow::Result<()> {
    let index = shelfgauge::storage::open_writer(&library.db_path())
        .with_context(|| format!("cannot open the index {}", library.db_path().display()))?;
    let failed = shelfgauge::storage::books::failed_books(&index)?;
    for book in &failed {
        println!("{}: {}", book.path, book.message);
    }
    match failed.len() {
        0 => println!("No books with errors."),
        1 => println!("1 book could not be read."),
        n => println!("{n} books could not be read."),
    }
    Ok(())
}

fn open_userdata(library: &Library) -> anyhow::Result<UserData> {
    let path = library.userdata_path();
    UserData::open(&path).with_context(|| format!("cannot open {}", path.display()))
}

/// The default command: serves the library, scanning it in the background.
fn serve(
    library: Library,
    config: &Config,
    overrides: &ConfigOverrides,
    scan: bool,
) -> anyhow::Result<()> {
    // Fail fast, before touching the index: no admin can log in from other devices,
    // unsafe bind address or busy port.
    let auth = web::Auth::from_config(&config.server);
    if auth == web::Auth::Password {
        let userdata = open_userdata(&library)?;
        if !profile_store::admin_has_password(&userdata.lock())? {
            return Err(web::WebError::NoAdminPassword.into());
        }
    }
    let listener = web::bind(&config.server, auth)?;
    let db_path = library.db_path();
    // The writer creates and migrates the database; readers need it to exist.
    let writer = shelfgauge::storage::open_writer(&db_path)
        .with_context(|| format!("cannot open the index {}", db_path.display()))?;
    let pool = shelfgauge::storage::open_read_pool(&db_path, READ_POOL_SIZE)
        .with_context(|| format!("cannot open the index {}", db_path.display()))?;
    drop(writer);

    let userdata = Arc::new(open_userdata(&library)?);
    let exporter = exporter(&library, config);
    // One handle for the handlers and the scan: the settings page swaps it.
    let live = LiveConfig::new(config.clone());
    let background = {
        let (library, userdata, exporter, live) = (
            library.clone(),
            Arc::clone(&userdata),
            exporter.clone(),
            live.clone(),
        );
        Arc::new(
            BackgroundScan::new(library.clone(), Vec::new())
                .with_config(live.clone())
                .after_complete_scan(move || {
                    let retention = live.get().annotations.removed_book_retention_days;
                    if let Err(e) =
                        attach::cleanup_library(&library, &userdata, &exporter, retention)
                    {
                        tracing::error!(
                            error = %e,
                            "cannot clean up the annotations of removed books"
                        );
                    }
                }),
        )
    };
    let state = web::AppState::new(library, config, pool, Arc::clone(&background))
        .with_config(live, overrides.clone())
        .with_annotations(userdata, exporter);
    let url = web::display_url(&config.server, state.urls());
    let sync_state = state.clone();
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
            background.start(shelfgauge::scanner::ScanTrigger::Startup);
        }
        let sync = web::hub_sync::start(sync_state);
        let served = web::serve(listener, router, shutdown).await;
        sync.abort();
        served.context("the web server failed")
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
    let _span = tracing::info_span!("scan", trigger = %"cli").entered();
    let mut conn = shelfgauge::storage::open_writer(&library.db_path())
        .with_context(|| format!("cannot open the index {}", library.db_path().display()))?;
    let registry = config.scan.registry();
    let progress = ScanProgress::default();
    let cancel = AtomicBool::new(false);
    let scanner = Scanner {
        library,
        exclude: &config.scan.exclude,
        registry: &registry,
        reread: false,
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
    if !summary.cancelled {
        let user = open_userdata(library)?;
        attach::cleanup_library(
            library,
            &user,
            &exporter(library, config),
            config.annotations.removed_book_retention_days,
        )
        .context("cannot clean up the annotations of removed books")?;
    }
    println!(
        "Scanned {} in {:.1} s: {} added, {} updated, {} moved, {} removed, {} unchanged, \
         {} errors ({} parsed){}{}.",
        library.root().display(),
        started.elapsed().as_secs_f64(),
        summary.added,
        summary.updated,
        summary.moved,
        summary.removed,
        summary.unchanged,
        summary.errors,
        summary.parsed,
        if summary.not_books > 0 {
            format!(", {} not books", summary.not_books)
        } else {
            String::new()
        },
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
