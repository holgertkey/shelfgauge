//! The hub: `shelfgauge hub --data <dir>` stores accounts, devices, publications and
//! encrypted sync records for the instances and devices of its accounts, and never
//! sees plaintext (`docs/ARCHITECTURE.md`, "V2: hub").
//!
//! A hub's data directory holds `hub.toml` ([`config`]), `hub.sqlite` (the database)
//! and `blobs/` (one file per publication blob). [`Hub`] opens it; [`HubState`] and
//! [`router`] serve it over HTTP (axum, no TLS of its own: a reverse proxy adds HTTPS).
//!
//! Authentication is a device token (`Authorization: Bearer <token>`), issued when an
//! account is created, a device is paired or an account is recovered; every call with
//! it is bound to the token's account, and another account's publication answers like
//! a missing one. The hub sets no cookies and checks no `Origin` (a bearer header is
//! no ambient credential, so there is nothing to forge across sites) and sends no
//! CORS headers. Requests pass the request trace, the hardening headers and the `Host`
//! check shared with the library server.
//!
//! Never logged: device tokens, pairing secrets, invite codes, recovery secrets and
//! their hashes, public account ids, publication ids (only route templates are logged)
//! and request bodies. Accounts appear as `account = N`, the internal id.

pub mod config;

mod api;
mod blobs;
mod limits;
mod shell;
mod store;

use std::fmt;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

use crate::logging::in_current_span;
use crate::publication::format::PublicationId;
use crate::storage::StorageError;
use crate::web::HostAllowlist;
use crate::web::security::{request_host, set_hardening_headers};
use crate::web::trace;

use blobs::BlobStore;
use config::HubConfig;
use limits::{DeviceLimiter, PublicLimiter};

pub use store::{AccountSummary, PAIRING_TTL, Usage};

/// The file name of a hub's config file in its data directory.
pub const CONFIG_FILE: &str = "hub.toml";

/// The file name of a hub's database in its data directory.
pub const DATABASE_FILE: &str = "hub.sqlite";

/// The `Content-Security-Policy` of every hub response: the reader app's scripts and
/// WASM from the hub itself, images through `blob:` URLs, no inline code, no framing
/// (concept §11).
pub const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; \
     script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' blob:; \
     connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; \
     form-action 'none'; frame-ancestors 'none'";

/// Errors of the hub: opening it, and the answers of its API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HubError {
    /// The directory is not a hub's data directory (it has no `hub.toml`).
    #[error(
        "{} is not a hub data directory (no {CONFIG_FILE}); `shelfgauge hub --data <dir>` \
         creates one",
        .0.display()
    )]
    NotAHub(PathBuf),
    /// The config file cannot be read or is invalid.
    #[error(transparent)]
    Config(#[from] crate::Error),
    /// A database operation failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// Reading or writing a blob file failed.
    #[error("blob storage error: {0}")]
    Io(#[from] std::io::Error),
    /// The request has no valid device token.
    #[error("a valid device token is required")]
    Unauthorized,
    /// The publication, blob, device or route does not exist (or is another
    /// account's).
    #[error("not found")]
    NotFound,
    /// The publication was taken down by the hub's operator.
    #[error("the publication was taken down")]
    Gone,
    /// Registration needs a valid invite code.
    #[error("registration on this hub needs a valid invite code")]
    InviteRequired,
    /// The pairing or recovery secret is wrong, used up or expired.
    #[error("the pairing or recovery secret is not valid")]
    InvalidSecret,
    /// A quota of the account (or the hub's account limit) is used up.
    #[error("the quota of {0} is used up")]
    Quota(&'static str),
    /// The request body is larger than allowed.
    #[error("the request body is too large")]
    TooLarge,
    /// The request is malformed.
    #[error("bad request: {0}")]
    BadRequest(&'static str),
    /// Too many requests; retry after the duration.
    #[error("too many requests")]
    RateLimited(Duration),
}

impl HubError {
    /// The status and the stable error code of the JSON answer.
    fn status_and_code(&self) -> (StatusCode, &'static str) {
        match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::Gone => (StatusCode::GONE, "taken_down"),
            Self::InviteRequired => (StatusCode::FORBIDDEN, "invite_required"),
            Self::InvalidSecret => (StatusCode::FORBIDDEN, "invalid_secret"),
            Self::Quota(_) => (StatusCode::FORBIDDEN, "quota_exceeded"),
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::RateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Self::NotAHub(_) | Self::Config(_) | Self::Storage(_) | Self::Io(_) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal")
            }
        }
    }
}

impl IntoResponse for HubError {
    fn into_response(self) -> Response {
        let (status, code) = self.status_and_code();
        let message = if status == StatusCode::INTERNAL_SERVER_ERROR {
            // The request span carries the route; the error has no secrets or paths.
            tracing::error!(error = %self, "internal error");
            "internal error".to_owned()
        } else {
            self.to_string()
        };
        let body = serde_json::json!({ "error": code, "message": message });
        let mut response = (status, axum::Json(body)).into_response();
        if let Self::Unauthorized = self {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Self::RateLimited(wait) = self {
            // Rounded up: a client that waits this long gets through.
            let secs = (wait.as_secs() + u64::from(wait.subsec_nanos() > 0))
                .max(1)
                .to_string();
            if let Ok(value) = HeaderValue::from_str(&secs) {
                response.headers_mut().insert(header::RETRY_AFTER, value);
            }
        }
        response
    }
}

impl From<rusqlite::Error> for HubError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.into())
    }
}

/// The 500 answer of a panicking handler (the panic hook has logged it).
fn panic_response() -> Response {
    let body = serde_json::json!({ "error": "internal", "message": "internal error" });
    (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(body)).into_response()
}

/// A secret of the hub protocol: a device token, pairing secret, invite code or
/// recovery secret. 32 random bytes as unpadded base64url; the hub stores only
/// [`hash`](Self::hash). `Debug` hides the value, and there is no `Display`.
///
/// # Examples
///
/// ```
/// use shelfgauge::hub::Secret;
///
/// let secret = Secret::generate().unwrap();
/// assert_eq!(secret.as_str().len(), 43);
/// assert_eq!(format!("{secret:?}"), "Secret(..)");
/// assert_eq!(Secret::from_text(secret.as_str()).hash(), secret.hash());
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// A new random secret.
    pub fn generate() -> Result<Self, HubError> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(|e| StorageError::Random(e.to_string()))?;
        Ok(Self(URL_SAFE_NO_PAD.encode(bytes)))
    }

    /// A secret received from a client (not checked: lookups find it or not).
    pub fn from_text(text: &str) -> Self {
        Self(text.to_owned())
    }

    /// The secret's text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The SHA-256 of the text: what the hub stores, and for a recovery secret what
    /// the instance sends at registration.
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// The public id of an account: 128 random bits, 32 lowercase hex digits (in the API
/// and the recovery code). Never logged; `Debug` hides it.
///
/// # Examples
///
/// ```
/// use shelfgauge::hub::AccountId;
///
/// let id = AccountId::new([0x0f; 16]);
/// assert_eq!(id.to_string(), "0f".repeat(16));
/// assert_eq!(id.to_string().parse::<AccountId>().ok(), Some(id));
/// assert_eq!(format!("{id:?}"), "AccountId(..)");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountId([u8; 16]);

impl AccountId {
    /// The id with these bytes.
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// A new random id.
    pub fn generate() -> Result<Self, HubError> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes).map_err(|e| StorageError::Random(e.to_string()))?;
        Ok(Self(bytes))
    }

    /// The id's bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountId(..)")
    }
}

impl fmt::Display for AccountId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The same hex form as publication ids.
        PublicationId::new(self.0).fmt(f)
    }
}

impl FromStr for AccountId {
    type Err = HubError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<PublicationId>()
            .map(|id| Self(*id.as_bytes()))
            .map_err(|_| HubError::BadRequest("invalid account id"))
    }
}

/// The current time in Unix seconds, replaceable in tests.
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// An opened hub data directory: its config, database and blobs.
pub struct Hub {
    config: HubConfig,
    db: Mutex<Connection>,
    blobs: BlobStore,
}

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Hub").finish_non_exhaustive()
    }
}

/// The result of [`Hub::open`].
#[derive(Debug)]
pub struct OpenedHub {
    /// The hub.
    pub hub: Hub,
    /// Whether the data directory (its `hub.toml`) was created now.
    pub created: bool,
    /// Dotted paths of unknown keys in `hub.toml`.
    pub unknown_keys: Vec<String>,
}

impl Hub {
    /// Opens the hub in `dir`; with `create`, a missing directory and `hub.toml` are
    /// created first. Without it, `dir` must already be a hub.
    pub fn open(dir: &Path, create: bool) -> Result<OpenedHub, HubError> {
        let config_path = dir.join(CONFIG_FILE);
        let created = if create {
            std::fs::create_dir_all(dir).map_err(|e| crate::Error::io(dir, e))?;
            HubConfig::create_default(&config_path)?
        } else if config_path.is_file() {
            false
        } else {
            return Err(HubError::NotAHub(dir.to_owned()));
        };
        let loaded = HubConfig::load(&config_path)?;
        let hub = Self::with_config(dir, loaded.config)?;
        Ok(OpenedHub {
            hub,
            created,
            unknown_keys: loaded.unknown_keys,
        })
    }

    /// Opens the database and blobs in `dir` with `config` instead of `hub.toml` (the
    /// directory must exist).
    pub fn with_config(dir: &Path, config: HubConfig) -> Result<Self, HubError> {
        let conn = store::open(&dir.join(DATABASE_FILE))?;
        Ok(Self {
            config,
            db: Mutex::new(conn),
            blobs: BlobStore::new(dir.join("blobs")),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Creates an invite code valid for `[accounts] invite_days`; returns it and its
    /// expiry time.
    pub fn invite(&self, now: i64) -> Result<(Secret, i64), HubError> {
        let days = self.config.accounts.invite_days;
        let code = store::create_invite(&self.lock(), days, now)?;
        Ok((code, now + i64::from(days) * 86_400))
    }

    /// Takes the publication `id` down (deletes it whichever account it belongs to,
    /// and refuses the id from now on); returns whether it existed.
    pub fn takedown(&self, id: &PublicationId, now: i64) -> Result<bool, HubError> {
        let account = store::takedown(&self.lock(), id, now)?;
        self.blobs.remove(id)?;
        if let Some(account) = account {
            tracing::info!(account, "publication taken down");
        }
        Ok(account.is_some())
    }

    /// Every account with its usage (internal ids, no secrets).
    pub fn accounts(&self) -> Result<Vec<AccountSummary>, HubError> {
        store::accounts(&self.lock())
    }

    /// Deletes the publications not opened for `publication_expiry_days`; returns how
    /// many.
    pub fn expire(&self, now: i64) -> Result<usize, HubError> {
        let days = self.config.limits.publication_expiry_days;
        let ids = store::expire(&self.lock(), days, now)?;
        for id in &ids {
            if let Err(e) = self.blobs.remove(id) {
                tracing::warn!(error = %e, "cannot remove the blobs of an expired publication");
            }
        }
        if !ids.is_empty() {
            tracing::info!(count = ids.len(), "expired publications deleted");
        }
        Ok(ids.len())
    }
}

/// The shared state of the hub's handlers.
#[derive(Clone)]
pub struct HubState(Arc<Inner>);

struct Inner {
    hub: Hub,
    hosts: HostAllowlist,
    base: String,
    devices: DeviceLimiter,
    public: PublicLimiter,
    clock: Clock,
    slow_request: Duration,
}

impl fmt::Debug for HubState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubState").finish_non_exhaustive()
    }
}

impl HubState {
    /// The state serving `hub`.
    pub fn new(hub: Hub) -> Self {
        Self::build(hub, Arc::new(crate::annotations::now))
    }

    /// The state with a clock (Unix seconds) instead of the system time, for tests.
    pub fn with_clock(hub: Hub, clock: impl Fn() -> i64 + Send + Sync + 'static) -> Self {
        Self::build(hub, Arc::new(clock))
    }

    fn build(hub: Hub, clock: Clock) -> Self {
        let config = &hub.config;
        let hosts = HostAllowlist::new(&config.server.as_server_config());
        let base = match config.server.base_path.as_str() {
            "/" => String::new(),
            path => path.to_owned(),
        };
        let devices = DeviceLimiter::new(config.limits.requests_per_minute);
        let public = PublicLimiter::new(config.limits.public_requests_per_hour);
        Self(Arc::new(Inner {
            hub,
            hosts,
            base,
            devices,
            public,
            clock,
            slow_request: trace::SLOW_REQUEST,
        }))
    }

    /// The hub.
    pub fn hub(&self) -> &Hub {
        &self.0.hub
    }

    /// The configuration.
    pub fn config(&self) -> &HubConfig {
        &self.0.hub.config
    }

    /// The current time in Unix seconds.
    pub fn now(&self) -> i64 {
        (self.0.clock)()
    }

    /// `path` under the configured base path (`/api/info` → `/hub/api/info`).
    pub fn path(&self, path: &str) -> String {
        format!("{}{path}", self.0.base)
    }

    /// The address to show for the listener (the hub is reached through its proxy).
    pub fn display_url(&self) -> String {
        let server = self.config().server.as_server_config();
        crate::web::display_url(&server, &crate::web::Urls::new(&server.base_path))
    }

    /// Runs `f` with the database on a blocking thread, in the request's span.
    async fn db<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T, HubError> + Send + 'static,
    ) -> Result<T, HubError> {
        let state = self.clone();
        let run = in_current_span(move || f(&mut state.0.hub.lock()));
        match tokio::task::spawn_blocking(run).await {
            Ok(result) => result,
            // The panic hook has logged it.
            Err(_) => Err(HubError::Io(std::io::Error::other(
                "a database task failed",
            ))),
        }
    }

    /// Runs `f` with the blob store on a blocking thread, in the request's span.
    async fn files<T: Send + 'static>(
        &self,
        f: impl FnOnce(&BlobStore) -> std::io::Result<T> + Send + 'static,
    ) -> Result<T, HubError> {
        let state = self.clone();
        let run = in_current_span(move || f(&state.0.hub.blobs));
        match tokio::task::spawn_blocking(run).await {
            Ok(result) => Ok(result?),
            Err(_) => Err(HubError::Io(std::io::Error::other("a blob task failed"))),
        }
    }
}

/// Checks that the hub may listen on its address and port, and listens.
///
/// Any address is allowed: device tokens authenticate every request.
pub fn bind(config: &HubConfig) -> Result<std::net::TcpListener, crate::web::WebError> {
    let server = config.server.as_server_config();
    crate::web::bind(&server, crate::web::Auth::Password)
}

/// Middleware: rejects requests whose `Host` is not on the allowlist.
async fn check_host(State(state): State<HubState>, req: Request, next: Next) -> Response {
    let host = request_host(&req);
    match host {
        Some(host) if state.0.hosts.allows(host) => next.run(req).await,
        _ => {
            tracing::warn!(host = ?host, "rejected a request with an unknown Host");
            (
                StatusCode::FORBIDDEN,
                "403 Forbidden: unknown Host header.\n\nTo reach this hub under another \
                 name, add it to `server.allowed_hosts` in hub.toml.\n",
            )
                .into_response()
        }
    }
}

/// Middleware: hardening headers on every response; API answers are never cached.
async fn security_headers(req: Request, next: Next) -> Response {
    let api = req.uri().path().contains("/api/");
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    // `no-referrer`: publication ids are in the paths. Nothing is posted by forms.
    set_hardening_headers(headers, CONTENT_SECURITY_POLICY, "no-referrer");
    if api && !headers.contains_key(header::CACHE_CONTROL) {
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    }
    response
}

/// The JSON 404 for unknown routes.
async fn not_found() -> HubError {
    HubError::NotFound
}

/// Builds the hub app: the API, the reader shell and its assets, behind the request
/// trace, the hardening headers and the `Host` check.
pub fn router(state: HubState) -> Router {
    let app = api::routes(&state).merge(shell::routes(&state));
    let trace = trace::TraceSettings {
        assets: state.path("/assets/{*path}").into(),
        slow: state.0.slow_request,
    };
    app.fallback(not_found)
        .layer(from_fn_with_state(
            panic_response as fn() -> Response,
            trace::catch_panics_with,
        ))
        .layer(from_fn_with_state(state.clone(), check_host))
        .layer(from_fn(security_headers))
        .layer(from_fn_with_state(trace, trace::trace_requests))
        .with_state(state)
}

/// Serves the hub on `listener` until `shutdown` completes, deleting expired
/// publications on start and daily.
pub async fn serve(
    listener: std::net::TcpListener,
    state: HubState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let expiry = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut daily = tokio::time::interval(Duration::from_secs(86_400));
            loop {
                daily.tick().await;
                let state = state.clone();
                let run = tokio::task::spawn_blocking(move || state.hub().expire(state.now()));
                if let Ok(Err(e)) = run.await {
                    tracing::error!(error = %e, "cannot delete expired publications");
                }
            }
        })
    };
    let listener = tokio::net::TcpListener::from_std(listener)?;
    let result = axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    expiry.abort();
    result
}
