//! The web server: an axum app over the read pool, with embedded assets.
//!
//! Startup is split so the binary can fail fast: [`bind`] checks the bind safety rule
//! and takes the fixed port (a busy port is an error, never a fallback), [`router`]
//! builds the app, and [`serve`] runs it until a shutdown signal.
//!
//! Routes come in four groups: *public* (assets), *read* (browsing), *admin* (rescan,
//! settings and maintenance)
//! and *user writes* (annotations, reading position); all but public pass through the
//! authentication slot
//! ([`security::Auth`]). Every request goes through the `Host` check and, for
//! state-changing methods, the `Origin` check (see [`security`]). All paths are
//! registered and generated through [`Urls`], which applies `server.base_path`.

use std::collections::VecDeque;
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{MethodRouter, get, post};

use crate::annotations::export::Exporter;
use crate::annotations::store::UserData;
use crate::annotations::text::BookText;
use crate::config::{AnnotationsConfig, Config, ConfigOverrides, LiveConfig, ServerConfig};
use crate::container::ReadLimits;
use crate::format::Registry;
use crate::format::model::BookContent;
use crate::library::Library;
use crate::logging::in_current_span;
use crate::profiles::pin::PinLimiter;
use crate::render::load;
use crate::scanner::BackgroundScan;
use crate::storage::ReadPool;
use crate::storage::StorageError;
use crate::storage::books::BookDetails;
use crate::storage::scans::scan_generation;
use crate::storage::stats::{DuplicateSummary, LibraryStats, identical_summary, library_stats};

mod assets;
mod error;
mod layout;
mod profile;
mod routes;
pub mod security;
mod sidebar;
mod trace;
mod urls;

use error::AppError;

pub use security::{Auth, HostAllowlist};
pub use urls::Urls;

/// Errors that keep the server from starting.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    /// `bind` is not a loopback address and no authentication is configured.
    #[error(
        "refusing to listen on {0}: it is not a loopback address and shelfgauge has no \
         authentication. Use 127.0.0.1 or ::1; for remote access, put a reverse proxy \
         with HTTPS and authentication in front of it, or use an SSH tunnel or a private \
         network"
    )]
    UnsafeBind(IpAddr),

    /// The fixed port is taken by another program.
    #[error(
        "port {} on {} is already in use (is shelfgauge already running?). The port is \
         fixed because it is part of the browser origin: stop the other program, or \
         choose another port with `--port <PORT>` or `server.port` in \
         .shelfgauge/config.toml",
        .0.port(),
        .0.ip()
    )]
    PortInUse(SocketAddr),

    /// Listening failed for another reason.
    #[error("cannot listen on {addr}: {error}")]
    Bind {
        /// The address.
        addr: SocketAddr,
        /// The underlying error.
        error: std::io::Error,
    },
}

/// Everything request handlers share.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    library: Library,
    urls: Urls,
    hosts: HostAllowlist,
    auth: Auth,
    pool: ReadPool,
    scan: Arc<BackgroundScan>,
    /// The configuration; its `[server]` section may differ from `server` after a save.
    config: LiveConfig,
    /// The `[server]` settings the server was started with.
    server: ServerConfig,
    /// The command-line overrides (shown on the settings page; they only touch
    /// `[server]`, which stays as started).
    overrides: ConfigOverrides,
    /// Held while the settings page checks and writes `config.toml`.
    save: Mutex<()>,
    userdata: Arc<UserData>,
    exporter: Exporter,
    registry: Registry,
    /// Recently read books, newest last.
    recent: Mutex<VecDeque<RecentBook>>,
    /// The library statistics, until the next scan.
    stats: ScanCache<Arc<LibraryStats>>,
    /// The identical copies (for the library page's notice), until the next scan.
    identical: ScanCache<DuplicateSummary>,
    /// Requests that take longer are logged as slow.
    slow_request: Duration,
    /// Wrong PINs per profile and their lockouts.
    pins: PinLimiter,
}

/// A value derived from the index, kept until a scan is recorded (see
/// [`scan_generation`]): the statistics are too slow to compute for every page.
struct ScanCache<T>(Mutex<Option<(i64, T)>>);

impl<T: Clone> ScanCache<T> {
    fn new() -> Self {
        Self(Mutex::new(None))
    }

    /// The value for scan `generation`, computed with `f` if needed.
    fn get(
        &self,
        generation: i64,
        conn: &rusqlite::Connection,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, StorageError>,
    ) -> Result<T, AppError> {
        if let Some((cached, value)) = &*self.0.lock().unwrap_or_else(|e| e.into_inner())
            && *cached == generation
        {
            return Ok(value.clone());
        }
        // Computed without the lock: two requests may both compute it, once.
        let value = f(conn)?;
        self.set(generation, value.clone());
        Ok(value)
    }

    fn set(&self, generation: i64, value: T) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some((generation, value));
    }
}

/// A parsed book kept in memory: `(book id, content key, content, its text)`; the
/// text is built when first asked for (images do not need it).
type RecentBook = (i64, Vec<u8>, Arc<BookContent>, LazyText);

/// A book's text, shared by the requests that need it.
type LazyText = Arc<OnceLock<Arc<BookText>>>;

/// The number of parsed books kept in memory (images and part changes reuse them).
const RECENT_BOOKS: usize = 4;

impl AppState {
    /// Creates the state for `library` with its (validated) `config`.
    ///
    /// Annotations go to an in-memory database until
    /// [`with_annotations`](Self::with_annotations) sets the library's own.
    pub fn new(
        library: Library,
        config: &Config,
        pool: ReadPool,
        scan: Arc<BackgroundScan>,
    ) -> Self {
        let urls = Urls::new(&config.server.base_path);
        let exporter = Exporter::new(
            library.annotations_dir(),
            &display_url(&config.server, &urls),
        );
        Self(Arc::new(Inner {
            library,
            urls,
            config: LiveConfig::new(config.clone()),
            server: config.server.clone(),
            overrides: ConfigOverrides::default(),
            save: Mutex::new(()),
            userdata: Arc::new(
                UserData::open_in_memory().expect("an in-memory database always opens"),
            ),
            exporter,
            hosts: HostAllowlist::new(&config.server),
            auth: Auth::None,
            pool,
            scan,
            registry: Registry::with_builtin_formats(),
            recent: Mutex::new(VecDeque::new()),
            stats: ScanCache::new(),
            identical: ScanCache::new(),
            slow_request: trace::SLOW_REQUEST,
            pins: PinLimiter::new(),
        }))
    }

    /// Uses `userdata` for annotations and reading positions and `exporter` for their
    /// Markdown export. Call it right after [`new`](Self::new), before the state is
    /// shared.
    ///
    /// # Panics
    ///
    /// If the state has already been cloned.
    pub fn with_annotations(mut self, userdata: Arc<UserData>, exporter: Exporter) -> Self {
        let inner = Arc::get_mut(&mut self.0).expect("the state is not shared yet");
        inner.userdata = userdata;
        inner.exporter = exporter;
        self
    }

    /// Shares `config` (the same handle as the background scan's) instead of a copy of
    /// the config given to [`new`](Self::new), and keeps the command-line `overrides`
    /// to apply again after the settings page saves. Call it right after
    /// [`new`](Self::new), before the state is shared; `config` must hold the same
    /// `[server]` settings.
    ///
    /// # Panics
    ///
    /// If the state has already been cloned.
    pub fn with_config(mut self, config: LiveConfig, overrides: ConfigOverrides) -> Self {
        let inner = Arc::get_mut(&mut self.0).expect("the state is not shared yet");
        inner.config = config;
        inner.overrides = overrides;
        self
    }

    /// Logs requests that take longer than `threshold` at `info` as slow requests
    /// (default: one second). Call it right after [`new`](Self::new), before the state
    /// is shared.
    ///
    /// # Panics
    ///
    /// If the state has already been cloned.
    pub fn with_slow_request(mut self, threshold: Duration) -> Self {
        let inner = Arc::get_mut(&mut self.0).expect("the state is not shared yet");
        inner.slow_request = threshold;
        self
    }

    /// The library being served.
    pub fn library(&self) -> &Library {
        &self.0.library
    }

    /// The URL helper.
    pub fn urls(&self) -> &Urls {
        &self.0.urls
    }

    /// The accepted `Host` values.
    pub fn hosts(&self) -> &HostAllowlist {
        &self.0.hosts
    }

    /// The configured authentication.
    pub fn auth(&self) -> Auth {
        self.0.auth
    }

    /// The read-connection pool (use it on a blocking thread).
    pub fn pool(&self) -> &ReadPool {
        &self.0.pool
    }

    /// The background scan.
    pub fn scan(&self) -> &BackgroundScan {
        &self.0.scan
    }

    /// The current configuration: take it once per request. Its `[server]` section is
    /// always the one the server was started with.
    pub fn config(&self) -> Arc<Config> {
        self.0.config.get()
    }

    /// The shared configuration handle.
    pub fn live_config(&self) -> &LiveConfig {
        &self.0.config
    }

    /// The `[server]` settings the server runs with (fixed at startup).
    pub fn server(&self) -> &ServerConfig {
        &self.0.server
    }

    /// The command-line overrides of the config file.
    pub fn overrides(&self) -> &ConfigOverrides {
        &self.0.overrides
    }

    /// Entries per list page (`library.page_size`).
    pub fn page_size(&self) -> u32 {
        self.config().library.page_size
    }

    /// The user-data database.
    pub fn userdata(&self) -> &UserData {
        &self.0.userdata
    }

    /// The annotation export.
    pub fn exporter(&self) -> &Exporter {
        &self.0.exporter
    }

    /// The lockout of wrong profile PINs.
    pub(crate) fn pins(&self) -> &PinLimiter {
        &self.0.pins
    }

    /// The content of `book`, from memory, the content cache or the book file.
    ///
    /// Parses and decodes recursively: call it on a thread with the parser stack.
    pub(crate) fn content(&self, book: &BookDetails) -> Result<Arc<BookContent>, AppError> {
        self.loaded(book).map(|(content, _)| content)
    }

    /// The content of `book` and its rendered text, kept in memory with the content.
    ///
    /// Parses and decodes recursively: call it on a thread with the parser stack.
    pub(crate) fn book_text(
        &self,
        book: &BookDetails,
    ) -> Result<(Arc<BookContent>, Arc<BookText>), AppError> {
        let (content, text) = self.loaded(book)?;
        let text = Arc::clone(text.get_or_init(|| Arc::new(BookText::new(&content))));
        Ok((content, text))
    }

    fn loaded(&self, book: &BookDetails) -> Result<(Arc<BookContent>, LazyText), AppError> {
        let key = load::content_key(book);
        let lock = || self.0.recent.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(key) = &key
            && let Some((_, _, content, text)) = lock()
                .iter()
                .find(|(id, k, _, _)| *id == book.id && k == key)
        {
            return Ok((Arc::clone(content), Arc::clone(text)));
        }
        let content = load::load(
            self.library(),
            &self.0.registry,
            &ReadLimits::default(),
            book,
        )
        .map_err(|e| match e {
            load::LoadError::NeedsRescan(reason) => {
                tracing::info!(book_id = book.id, reason = ?reason, "the book needs a rescan");
                AppError::FileMissing
            }
            load::LoadError::Unreadable(reason) => AppError::Unreadable(reason),
            load::LoadError::Protected(scheme) => AppError::Protected(scheme),
        })?;
        let content = Arc::new(content);
        let text = Arc::new(OnceLock::new());
        if let Some(key) = key {
            let mut recent = lock();
            recent.retain(|(id, _, _, _)| *id != book.id);
            recent.push_back((book.id, key, Arc::clone(&content), Arc::clone(&text)));
            while recent.len() > RECENT_BOOKS {
                recent.pop_front();
            }
        }
        Ok((content, text))
    }

    /// Locks out other saves of the settings page while `config.toml` is checked and
    /// written.
    pub(crate) fn save_lock(&self) -> std::sync::MutexGuard<'_, ()> {
        self.0.save.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Drops the parsed books kept in memory (after the content cache was cleared).
    pub(crate) fn forget_loaded_books(&self) {
        self.0
            .recent
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// The library statistics, cached until the next scan.
    pub(crate) fn library_stats(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<Arc<LibraryStats>, AppError> {
        let generation = scan_generation(conn)?;
        let stats = self
            .0
            .stats
            .get(generation, conn, |conn| library_stats(conn).map(Arc::new))?;
        self.0.identical.set(generation, stats.duplicates);
        Ok(stats)
    }

    /// The counts of identical copies, cached until the next scan.
    pub(crate) fn identical_copies(
        &self,
        conn: &rusqlite::Connection,
    ) -> Result<DuplicateSummary, AppError> {
        let generation = scan_generation(conn)?;
        self.0.identical.get(generation, conn, identical_summary)
    }

    /// Runs `f` with a pooled read connection on a blocking thread, in the request's
    /// span.
    pub(crate) async fn db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, AppError> + Send + 'static,
    ) -> Result<T, AppError> {
        let pool = self.0.pool.clone();
        blocking(move || f(&*pool.get()?)).await?
    }

    /// Runs `f` on a blocking thread with the parser stack and a pooled index
    /// connection, in the request's span, for work that may load a book and use the
    /// user-data database.
    ///
    /// `f` locks [`userdata`](Self::userdata) itself, only around its database calls:
    /// never while a book is loaded or parsed.
    pub(crate) async fn user_db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&AppState, &rusqlite::Connection) -> Result<T, AppError> + Send + 'static,
    ) -> Result<T, AppError> {
        let state = self.clone();
        blocking(move || {
            crate::format::run_with_parser_stack(|| {
                let index = state.0.pool.get()?;
                f(&state, &index)
            })
        })
        .await?
    }
}

/// Runs `f` on a blocking thread in the caller's span (see [`in_current_span`]).
///
/// A panic in `f` becomes [`AppError::Internal`]; the panic hook has logged it.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> T + Send + 'static,
) -> Result<T, AppError> {
    Ok(tokio::task::spawn_blocking(in_current_span(f)).await?)
}

/// The request body limit of the annotation write routes: the longest note, at up to
/// 12 bytes per code point (a UTF-8 sequence percent-encoded in a form, or a
/// surrogate pair escaped in JSON), plus room for the other fields.
pub fn write_body_limit(config: &AnnotationsConfig) -> usize {
    (config.max_note_length as usize + 256) * 12 + 4096
}

/// Middleware: limits the body of a user write to [`write_body_limit`] of the current
/// `[annotations]` settings.
///
/// A body over the limit is refused with `413 Payload Too Large`: at once if its
/// `Content-Length` says so, otherwise by the extractor that reads it (axum maps the
/// [`Limited`](http_body_util::Limited) error to 413).
async fn limit_write_body(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let limit = write_body_limit(&state.config().annotations);
    let declared = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > limit as u64) {
        return (
            StatusCode::PAYLOAD_TOO_LARGE,
            "Failed to buffer the request body",
        )
            .into_response();
    }
    let (parts, body) = req.into_parts();
    let body = Body::new(http_body_util::Limited::new(body, limit));
    next.run(Request::from_parts(parts, body)).await
}

/// Checks the bind safety rule and listens on the configured address and fixed port.
///
/// The returned listener is non-blocking, ready for [`serve`].
pub fn bind(server: &ServerConfig, auth: Auth) -> Result<std::net::TcpListener, WebError> {
    security::check_bind(server.bind, auth)?;
    let addr = SocketAddr::new(server.bind, server.port);
    let listener = std::net::TcpListener::bind(addr).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            WebError::PortInUse(addr)
        } else {
            WebError::Bind { addr, error }
        }
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|error| WebError::Bind { addr, error })?;
    Ok(listener)
}

/// The admin routes (rescan, settings, maintenance, profiles): `(path, handlers)`.
/// [`router`] builds the admin group from it, and the role tests try every path.
fn admin_routes(urls: &Urls) -> Vec<(String, MethodRouter<AppState>)> {
    vec![
        (urls.path("/scan"), post(routes::scan::rescan)),
        (
            urls.settings(),
            get(routes::settings::page).post(routes::settings::save),
        ),
        (
            urls.settings_action("reread"),
            post(routes::settings::reread),
        ),
        (
            urls.settings_action("content-cache"),
            post(routes::settings::clear_content_cache),
        ),
        (
            urls.settings_action("export"),
            post(routes::settings::export),
        ),
        (urls.manage_profiles(), post(routes::profiles::add)),
        (
            urls.path("/settings/profiles/{id}/{action}"),
            get(routes::profiles::confirm_delete).post(routes::profiles::manage),
        ),
    ]
}

/// Builds the app with all routes and security layers.
pub fn router(state: AppState) -> Router {
    let urls = state.urls();
    // The last route layer added runs first: auth, then the profile, then the role.
    let auth = || from_fn_with_state(state.clone(), security::require_auth);
    let profile = || from_fn_with_state(state.clone(), profile::require_profile);

    let public = Router::new().route(&urls.path("/assets/{*path}"), get(assets::serve));
    // The picker needs no profile (it is where one is chosen).
    let picker = Router::new()
        .route(&urls.path("/profiles"), get(routes::profiles::picker))
        .route(
            &urls.path("/profiles/{id}/select"),
            post(routes::profiles::select),
        )
        .route(
            &urls.path("/profiles/{id}/pin"),
            get(routes::profiles::pin_page).post(routes::profiles::pin),
        )
        .route_layer(auth());
    let read = Router::new()
        .route(&urls.home(), get(routes::library::home))
        .route(&urls.path("/authors"), get(routes::library::authors))
        .route(&urls.path("/author/{id}"), get(routes::library::author))
        .route(&urls.path("/folder"), get(routes::library::folder))
        .route(&urls.path("/book/{id}"), get(routes::book::card))
        .route(&urls.path("/cover/{id}"), get(routes::book::cover))
        .route(&urls.path("/book/{id}/read"), get(routes::read::read))
        .route(&urls.path("/book/{id}/find"), get(routes::find::find))
        .route(
            &urls.path("/book/{id}/annotations"),
            get(routes::annotations::lists),
        )
        .route(
            &urls.path("/book/{id}/image/{image}"),
            get(routes::read::image),
        )
        .route(&urls.path("/search"), get(routes::search::search))
        .route(&urls.path("/errors"), get(routes::library::errors))
        .route(&urls.path("/stats"), get(routes::stats::stats))
        .route(&urls.path("/books"), get(routes::stats::books))
        .route(&urls.path("/duplicates"), get(routes::stats::duplicates))
        .route(&urls.path("/scan/status"), get(routes::scan::status))
        .route(
            &urls.path("/sidebar/authors"),
            get(routes::sidebar::authors),
        )
        .route(
            &urls.path("/sidebar/author/{id}"),
            get(routes::sidebar::author),
        )
        .route(&urls.path("/sidebar/folder"), get(routes::sidebar::folder))
        .route(&urls.my_profile(), get(routes::profiles::my_profile))
        .route_layer(profile())
        .route_layer(auth());
    let admin = admin_routes(urls)
        .into_iter()
        .fold(Router::new(), |router, (path, handlers)| {
            router.route(&path, handlers)
        })
        .route_layer(from_fn(profile::require_admin))
        .route_layer(profile())
        .route_layer(auth());
    // User writes (annotations, reading position, the own profile): behind the Origin
    // check like every POST, and with a body limit derived from the configured lengths.
    let writes = Router::new()
        .route(
            &urls.path("/book/{id}/annotations"),
            post(routes::annotations::create),
        )
        .route(
            &urls.path("/annotation/{id}/note"),
            post(routes::annotations::note),
        )
        .route(
            &urls.path("/annotation/{id}/delete"),
            post(routes::annotations::delete),
        )
        .route(
            &urls.path("/book/{id}/position"),
            post(routes::annotations::position),
        )
        .route(
            &urls.path("/book/{id}/finished"),
            post(routes::annotations::finished),
        )
        .route(
            &urls.path("/reading/clear"),
            post(routes::annotations::clear_reading),
        )
        .route(&urls.leave_profile(), post(routes::profiles::leave))
        .route(&urls.my_profile(), post(routes::profiles::save_my_profile))
        .route(
            &urls.profile_settings(),
            post(routes::profiles::save_settings),
        )
        .route_layer(profile())
        .route_layer(auth())
        // Not `DefaultBodyLimit::max`: that is fixed when the router is built, and the
        // settings page can raise `max_note_length` at run time.
        .layer(from_fn_with_state(state.clone(), limit_write_body))
        .layer(DefaultBodyLimit::disable());

    let mut app = public.merge(picker).merge(read).merge(admin).merge(writes);
    if let Some(prefix) = urls.prefix() {
        app = app.route(prefix, get(routes::library::to_home));
    }
    with_layers(app, state)
}

/// Adds the fallback and the layers every request passes through to the routes `app`.
fn with_layers(app: Router<AppState>, state: AppState) -> Router {
    let trace = trace::TraceSettings {
        assets: state.urls().path("/assets/{*path}").into(),
        slow: state.0.slow_request,
    };
    // The last layer added runs first: the request span, headers, Host, Origin, error
    // pages; a panic is caught inside, so it gets the error page and the request id.
    app.fallback(error::not_found)
        .layer(from_fn(trace::catch_panics))
        .layer(from_fn_with_state(state.clone(), error::render_errors))
        .layer(from_fn_with_state(state.clone(), security::check_origin))
        .layer(from_fn_with_state(state.clone(), security::check_host))
        .layer(from_fn(security::security_headers))
        .layer(from_fn_with_state(trace, trace::trace_requests))
        .with_state(state)
}

/// Serves `router` on `listener` until `shutdown` completes, then lets open requests
/// finish.
pub async fn serve(
    listener: std::net::TcpListener,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::from_std(listener)?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await
}

/// The address to show to the user for a listening address: loopback stays as is, and
/// an unspecified address is shown as `127.0.0.1`.
pub fn display_url(server: &ServerConfig, urls: &Urls) -> String {
    let ip = server.bind.to_canonical();
    let host = match ip {
        IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
        IpAddr::V6(ip) if ip.is_unspecified() => "127.0.0.1".to_owned(),
        IpAddr::V4(ip) => ip.to_string(),
        IpAddr::V6(ip) => format!("[{ip}]"),
    };
    format!("http://{host}:{}{}", server.port, urls.home())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;
    use crate::logging::capture::Capture;

    /// A threshold no test request reaches.
    const SLOW_REQUEST_FOR_TESTS: Duration = Duration::from_secs(10);

    /// The threshold of the level tests; `/slow` takes longer, the other test routes
    /// stay far below it even on a loaded machine.
    const SLOW_FOR_TESTS: Duration = Duration::from_millis(500);

    /// The state of an empty library with the default config.
    fn test_state(slow: Duration) -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        crate::storage::open_writer(&library.db_path()).unwrap();
        let pool = crate::storage::open_read_pool(&library.db_path(), 1).unwrap();
        let scan = Arc::new(BackgroundScan::new(library.clone(), Vec::new()));
        let state = AppState::new(library, &Config::default(), pool, scan).with_slow_request(slow);
        (dir, state)
    }

    /// Test-only routes behind the layers of the real app: any status, a slow answer,
    /// a panic, a stand-in for the assets.
    fn test_app(state: AppState) -> Router {
        use axum::extract::Path;
        let routes = Router::new()
            .route(
                "/status/{code}",
                get(|Path(code): Path<u16>| async move { StatusCode::from_u16(code).unwrap() }),
            )
            .route(
                "/slow",
                get(|| async {
                    tokio::time::sleep(SLOW_FOR_TESTS + Duration::from_millis(100)).await;
                    "done"
                }),
            )
            .route(
                "/panic",
                get(|| async {
                    if true {
                        panic!("a bug in a handler");
                    }
                    "unreachable"
                }),
            )
            .route("/assets/{*path}", get(|| async { "body {}" }));
        with_layers(routes, state)
    }

    fn get_request(uri: &str) -> Request<Body> {
        Request::get(uri)
            .header(header::HOST, "localhost:8742")
            .body(Body::empty())
            .unwrap()
    }

    /// The log line of the request to route template `route` ending with `status`.
    fn end_line(log: &str, route: &str, status: u16) -> String {
        log.lines()
            .find(|l| {
                l.contains(&format!(" route={route}}}"))
                    && l.contains(&format!(" status={status} "))
            })
            .unwrap_or_else(|| panic!("no line for {route} {status} in\n{log}"))
            .to_owned()
    }

    /// Sends panics to the log as the binary does, and still to the default hook so
    /// failing tests stay readable.
    fn log_panics() {
        static HOOK: std::sync::Once = std::sync::Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                crate::logging::log_panic(info);
                previous(info);
            }));
        });
    }

    #[tokio::test]
    async fn request_end_events_have_levels_by_status_and_time() {
        let capture = Capture::default();
        let _log = tracing::dispatcher::set_default(&capture.dispatch("shelfgauge=trace"));
        let (_dir, state) = test_state(SLOW_FOR_TESTS);
        let app = test_app(state);
        for uri in [
            "/status/200",
            "/status/404",
            "/status/503",
            "/slow",
            "/assets/style.css",
            "/unknown?q=secret",
        ] {
            app.clone().oneshot(get_request(uri)).await.unwrap();
        }
        let log = capture.text();
        let ok = end_line(&log, "/status/{code}", 200);
        assert!(ok.contains(" DEBUG ") && ok.contains("method=GET"), "{ok}");
        assert!(ok.contains("latency_ms="), "{ok}");
        assert!(end_line(&log, "/status/{code}", 404).contains(" DEBUG "));
        let failed = end_line(&log, "/status/{code}", 503);
        assert!(failed.contains(" WARN ") && failed.contains("request failed"));
        let slow = end_line(&log, "/slow", 200);
        assert!(
            slow.contains(" INFO ") && slow.contains("slow request"),
            "{slow}"
        );
        assert!(end_line(&log, "/assets/{*path}", 200).contains(" TRACE "));
        let unmatched = end_line(&log, "-", 404);
        assert!(unmatched.contains(" DEBUG "), "{unmatched}");
        assert!(
            !log.contains("secret") && !log.contains("/unknown"),
            "{log}"
        );
        // Ids are distinct per request.
        let ids: std::collections::HashSet<&str> = log
            .lines()
            .filter_map(|l| l.split("request{id=").nth(1)?.split(' ').next())
            .collect();
        assert_eq!(ids.len(), 6, "{log}");
    }

    #[tokio::test]
    async fn default_filter_shows_only_failed_and_slow_requests() {
        let capture = Capture::default();
        let _log = tracing::dispatcher::set_default(&capture.dispatch("warn,shelfgauge=info"));
        let (_dir, state) = test_state(SLOW_FOR_TESTS);
        let app = test_app(state);
        for uri in ["/status/200", "/status/404", "/status/500", "/slow"] {
            app.clone().oneshot(get_request(uri)).await.unwrap();
        }
        let log = capture.text();
        assert_eq!(log.lines().count(), 2, "{log}");
        end_line(&log, "/status/{code}", 500);
        end_line(&log, "/slow", 200);
    }

    #[tokio::test]
    async fn handler_panics_answer_500_with_the_request_id() {
        log_panics();
        let capture = Capture::default();
        let _log = tracing::dispatcher::set_default(&capture.dispatch("shelfgauge=info"));
        let (_dir, state) = test_state(SLOW_REQUEST_FOR_TESTS);
        let app = test_app(state);

        let response = app.clone().oneshot(get_request("/panic")).await.unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains("Internal error"), "{body}");
        let id = body
            .split("Error id <code>")
            .nth(1)
            .and_then(|rest| rest.split("</code>").next())
            .unwrap_or_else(|| panic!("no error id in {body}"));
        assert!(!body.contains("a bug in a handler"), "{body}");

        let log = capture.text();
        let panic = log
            .lines()
            .find(|l| l.contains("panicked"))
            .unwrap_or_else(|| panic!("no panic line in\n{log}"));
        assert!(panic.contains(" ERROR "), "{panic}");
        assert!(
            panic.contains(&format!("request{{id={id} method=GET route=/panic}}")),
            "{panic}"
        );
        assert!(panic.contains("a bug in a handler"), "{panic}");
        assert!(end_line(&log, "/panic", 500).contains(&format!("id={id} ")));

        // The server keeps serving.
        let response = app.oneshot(get_request("/status/204")).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    /// The internal error page names the request id; other error pages do not.
    #[tokio::test]
    async fn only_the_internal_error_page_shows_the_request_id() {
        let (_dir, state) = test_state(SLOW_REQUEST_FOR_TESTS);
        let app = with_layers(
            Router::new()
                .route(
                    "/broken",
                    get(|| async { Err::<(), _>(AppError::Internal("detail".into())) }),
                )
                .route(
                    "/unreadable",
                    get(|| async { Err::<(), _>(AppError::Unreadable("bad".into())) }),
                ),
            state,
        );
        let body = |uri: &'static str| {
            let app = app.clone();
            async move {
                let response = app.oneshot(get_request(uri)).await.unwrap();
                let body = response.into_body().collect().await.unwrap().to_bytes();
                String::from_utf8(body.to_vec()).unwrap()
            }
        };
        assert!(body("/broken").await.contains("Error id <code>"));
        assert!(!body("/unreadable").await.contains("Error id"));
        assert!(!body("/nowhere").await.contains("Error id"));
    }

    /// A reader gets 403 on every admin route (both methods), so a new admin route is
    /// covered without changing this test.
    #[tokio::test]
    async fn readers_are_refused_on_every_admin_route() {
        let (_dir, state) = test_state(SLOW_REQUEST_FOR_TESTS);
        let token = {
            let conn = state.userdata().lock();
            let reader = crate::profiles::store::create(
                &conn,
                "Reader",
                0,
                crate::profiles::Role::Reader,
                0,
            )
            .unwrap();
            crate::profiles::session::create(&conn, reader.id, 0).unwrap()
        };
        let paths: Vec<String> = admin_routes(state.urls())
            .into_iter()
            .map(|(path, _)| path.replace("{id}", "1").replace("{action}", "delete"))
            .collect();
        assert!(paths.len() >= 7, "{paths:?}");
        let app = router(state);
        for path in paths {
            let mut refused = 0;
            for method in ["GET", "POST"] {
                let request = Request::builder()
                    .method(method)
                    .uri(&path)
                    .header(header::HOST, "localhost:8742")
                    .header(header::ORIGIN, "http://localhost:8742")
                    .header(
                        header::COOKIE,
                        format!("shelfgauge-profile={}", token.as_str()),
                    )
                    .body(Body::empty())
                    .unwrap();
                let status = app.clone().oneshot(request).await.unwrap().status();
                assert!(
                    status == StatusCode::FORBIDDEN || status == StatusCode::METHOD_NOT_ALLOWED,
                    "{method} {path}: {status}"
                );
                refused += usize::from(status == StatusCode::FORBIDDEN);
            }
            assert!(refused > 0, "{path}");
        }
    }

    /// Error pages that no route produces yet render with the layout and the right status.
    #[tokio::test]
    async fn error_pages() {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        crate::storage::open_writer(&library.db_path()).unwrap();
        let pool = crate::storage::open_read_pool(&library.db_path(), 1).unwrap();
        let mut config = Config::default();
        config.server.base_path = "/books".to_owned();
        config.validate().unwrap();
        let scan = Arc::new(BackgroundScan::new(library.clone(), Vec::new()));
        let state = AppState::new(library, &config, pool, scan);
        let app = Router::new()
            .route(
                "/missing",
                get(|| async { Err::<(), _>(AppError::FileMissing) }),
            )
            .route(
                "/broken",
                get(|| async { Err::<(), _>(AppError::Internal("secret detail".into())) }),
            )
            .layer(from_fn_with_state(state.clone(), error::render_errors))
            .with_state(state);

        for (uri, status, text, rescan) in [
            ("/missing", StatusCode::NOT_FOUND, "File not found", true),
            (
                "/broken",
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal error",
                false,
            ),
        ] {
            let request = Request::get(uri).body(Body::empty()).unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), status, "{uri}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains(text), "{body}");
            assert!(body.contains("href=\"/books/assets/style.css?v="), "{body}");
            assert_eq!(
                body.contains("Rescan the library</button>"),
                rescan,
                "{body}"
            );
            assert!(!body.contains("secret detail"), "{body}");
        }
    }
}
