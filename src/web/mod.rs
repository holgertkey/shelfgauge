//! The web server: an axum app over the read pool, with embedded assets.
//!
//! Startup is split so the binary can fail fast: [`bind`] checks the bind safety rule
//! and takes the fixed port (a busy port is an error, never a fallback), [`router`]
//! builds the app, and [`serve`] runs it until a shutdown signal.
//!
//! Routes come in three groups: *public* (assets), *read* (browsing) and *admin*
//! (rescan); read and admin pass through the authentication slot
//! ([`security::Auth`]). Every request goes through the `Host` check and, for
//! state-changing methods, the `Origin` check (see [`security`]). All paths are
//! registered and generated through [`Urls`], which applies `server.base_path`.

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{get, post};

use crate::config::{Config, ServerConfig};
use crate::library::Library;
use crate::scanner::BackgroundScan;
use crate::storage::ReadPool;

mod assets;
mod error;
mod layout;
mod routes;
pub mod security;
mod sidebar;
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
    page_size: u32,
}

impl AppState {
    /// Creates the state for `library` with its (validated) `config`.
    pub fn new(
        library: Library,
        config: &Config,
        pool: ReadPool,
        scan: Arc<BackgroundScan>,
    ) -> Self {
        Self(Arc::new(Inner {
            library,
            urls: Urls::new(&config.server.base_path),
            hosts: HostAllowlist::new(&config.server),
            auth: Auth::None,
            pool,
            scan,
            page_size: config.library.page_size,
        }))
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

    /// The read-connection pool (use it from `spawn_blocking`).
    pub fn pool(&self) -> &ReadPool {
        &self.0.pool
    }

    /// The background scan.
    pub fn scan(&self) -> &BackgroundScan {
        &self.0.scan
    }

    /// Entries per list page (`library.page_size`).
    pub fn page_size(&self) -> u32 {
        self.0.page_size
    }

    /// Runs `f` with a pooled read connection on a blocking thread.
    pub(crate) async fn db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&rusqlite::Connection) -> Result<T, AppError> + Send + 'static,
    ) -> Result<T, AppError> {
        let pool = self.0.pool.clone();
        tokio::task::spawn_blocking(move || f(&*pool.get()?)).await?
    }
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

/// Builds the app with all routes and security layers.
pub fn router(state: AppState) -> Router {
    let urls = state.urls();
    let auth = || from_fn_with_state(state.clone(), security::require_auth);

    let public = Router::new().route(&urls.path("/assets/{*path}"), get(assets::serve));
    let read = Router::new()
        .route(&urls.home(), get(routes::library::home))
        .route(&urls.path("/authors"), get(routes::library::authors))
        .route(&urls.path("/author/{id}"), get(routes::library::author))
        .route(&urls.path("/folder"), get(routes::library::folder))
        .route(&urls.path("/book/{id}"), get(routes::book::card))
        .route(&urls.path("/cover/{id}"), get(routes::book::cover))
        .route(&urls.path("/search"), get(routes::search::search))
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
        .route_layer(auth());
    let admin = Router::new()
        .route(&urls.path("/scan"), post(routes::scan::rescan))
        .route_layer(auth());

    let mut app = public.merge(read).merge(admin);
    if let Some(prefix) = urls.prefix() {
        app = app.route(prefix, get(routes::library::to_home));
    }
    // The last layer added runs first: headers, then Host, then Origin.
    app.fallback(error::not_found)
        .layer(from_fn_with_state(state.clone(), error::render_errors))
        .layer(from_fn_with_state(state.clone(), security::check_origin))
        .layer(from_fn_with_state(state.clone(), security::check_host))
        .layer(from_fn(security::security_headers))
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
