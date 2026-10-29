//! The hub: `shelfgauge hub --data <dir>` stores libraries with their accounts
//! (`name@handle`, [`address`]), devices, publications and encrypted sync records, and
//! never sees plaintext, passwords or keys (`docs/ARCHITECTURE.md`, "V2: hub").
//!
//! A hub's data directory holds `hub.toml` ([`config`]), `hub.sqlite` (the database)
//! and `blobs/` (one file per publication blob). [`Hub`] opens it; [`HubState`] and
//! [`router`] serve it over HTTP (axum, no TLS of its own: a reverse proxy adds HTTPS).
//!
//! Authentication is a bearer token (`Authorization: Bearer <token>`): a library's own
//! token, issued when the library is created, or a device token, issued when a person
//! signs in or redeems an invite or the owner's phone is paired. Every call is bound to
//! the token's library (and a device's to its account); another library's publication
//! answers like a missing one. The hub sets no cookies and checks no `Origin` (a bearer header is
//! no ambient credential, so there is nothing to forge across sites) and sends no
//! CORS headers. Requests pass the request trace, the hardening headers and the `Host`
//! check shared with the library server.
//!
//! Never logged: tokens, pairing and invite secrets, hub invite codes and their hashes,
//! login keys, salts, key bundles, invite payloads, addresses, names and handles,
//! public ids, publication ids (only route templates are logged) and request bodies.
//! Libraries and accounts appear as `library = N` and `account = N`, internal ids.

pub mod check;
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
use limits::{DAY, HOUR};

use blobs::BlobStore;
use config::HubConfig;
use limits::{DeviceLimiter, IpRange, LoginLimiter, WindowLimiter};

pub use api::ACCOUNT_HEADER;
pub use shelfgauge_core::address;
pub use store::{
    DeviceInfo, Expired, LibraryQuotas, LibrarySummary, MAX_LIBRARY_ACCOUNTS, PAIRING_TTL, Usage,
};

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
    /// The database holds accounts of shelfgauge 0.18–0.20, which this version does
    /// not read (`shelfgauge hub migrate` deletes them).
    #[error(
        "the hub database holds {accounts} accounts and {publications} publications of \
         shelfgauge 0.18–0.20, which 0.21 replaces by libraries and does not convert; run \
         `shelfgauge hub migrate` to delete them (their libraries publish again and invite \
         their members anew), or start the hub with another data directory"
    )]
    OldAccounts {
        /// The number of old accounts.
        accounts: u64,
        /// The number of old publications.
        publications: u64,
    },
    /// The request has no valid token.
    #[error("a valid token is required")]
    Unauthorized,
    /// The token may not do this (a member's device publishing, a library token
    /// acting as a device, the owner leaving).
    #[error("not allowed: {0}")]
    Forbidden(&'static str),
    /// The publication, blob, device, account or route does not exist (or is another
    /// library's).
    #[error("not found")]
    NotFound,
    /// The publication was taken down by the hub's operator.
    #[error("the publication was taken down")]
    Gone,
    /// Creating a library needs a valid hub invite code.
    #[error("creating a library on this hub needs a valid invite code")]
    InviteRequired,
    /// The pairing or invite secret is wrong, used up or expired.
    #[error("the pairing secret or invite is not valid, used or expired")]
    InvalidSecret,
    /// The address or the password is wrong (never told apart).
    #[error("the address or the password is wrong")]
    InvalidLogin,
    /// The name is taken by another account of the library.
    #[error("this name is taken in the library")]
    NameTaken,
    /// The handle is taken by another library on the hub.
    #[error("this handle is taken on the hub")]
    HandleTaken,
    /// The account has as many devices as allowed: one of them must be removed first.
    #[error("the account has as many devices as allowed; remove one first")]
    DeviceLimit(Vec<DeviceInfo>),
    /// A quota of the library (or the hub's library limit) is used up.
    #[error("the quota of {0} is used up")]
    Quota(&'static str),
    /// The hub stores as many bytes as `[limits] max_total_bytes` allows.
    #[error("the hub is full and accepts no new data for now")]
    Full,
    /// This client address created as many libraries today as the hub allows; retry
    /// after the duration.
    #[error("this hub allows only a few new libraries a day from one address")]
    LibraryLimit(Duration),
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
            Self::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found"),
            Self::Gone => (StatusCode::GONE, "taken_down"),
            Self::InviteRequired => (StatusCode::FORBIDDEN, "invite_required"),
            Self::InvalidSecret => (StatusCode::FORBIDDEN, "invalid_secret"),
            Self::InvalidLogin => (StatusCode::FORBIDDEN, "invalid_login"),
            Self::NameTaken => (StatusCode::CONFLICT, "name_taken"),
            Self::HandleTaken => (StatusCode::CONFLICT, "handle_taken"),
            Self::DeviceLimit(_) => (StatusCode::CONFLICT, "device_limit"),
            Self::Quota(_) => (StatusCode::FORBIDDEN, "quota_exceeded"),
            Self::Full => (StatusCode::INSUFFICIENT_STORAGE, "hub_full"),
            Self::LibraryLimit(_) => (StatusCode::TOO_MANY_REQUESTS, "library_limit"),
            Self::TooLarge => (StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
            Self::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            Self::RateLimited(_) => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            Self::NotAHub(_)
            | Self::OldAccounts { .. }
            | Self::Config(_)
            | Self::Storage(_)
            | Self::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal"),
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
        let mut body = serde_json::json!({ "error": code, "message": message });
        if let Self::DeviceLimit(devices) = &self {
            body["devices"] = api::device_list(devices, None);
        }
        let mut response = (status, axum::Json(body)).into_response();
        if let Self::Unauthorized = self {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        if let Self::RateLimited(wait) | Self::LibraryLimit(wait) = self {
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

/// A secret of the hub protocol: a library or device token, a pairing or invite
/// secret, a hub invite code. 32 random bytes as unpadded base64url; the hub stores
/// only [`hash`](Self::hash). `Debug` hides the value, and there is no `Display`.
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

    /// The SHA-256 of the text: what the hub stores.
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// The public id of a library, an account or a member invite: 128 random bits, 32
/// lowercase hex digits in the API. Never logged; `Debug` hides it.
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
            .map_err(|_| HubError::BadRequest("invalid id"))
    }
}

/// The current time in Unix seconds, replaceable in tests.
type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The largest terms of use file read, in bytes.
pub const MAX_TERMS_BYTES: u64 = 64 * 1024;

/// Publications, or libraries and accounts, deleted per transaction by
/// [`Hub::expire`].
const EXPIRY_BATCH: usize = 20;

/// The pause between two expiry transactions, so waiting requests get the database.
const EXPIRY_PAUSE: std::time::Duration = std::time::Duration::from_millis(10);

/// An opened hub data directory: its config, database and blobs.
pub struct Hub {
    config: HubConfig,
    /// The text of `[operator] terms_file`, read when the hub is opened.
    terms: Option<String>,
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
    /// created first. Without it, `dir` must already be a hub. A database with
    /// accounts of 0.18–0.20 is refused ([`HubError::OldAccounts`]; see
    /// [`Hub::migrate`]).
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
    /// directory must exist), and reads the terms of use file (a missing one is no
    /// terms; a larger one than [`MAX_TERMS_BYTES`] or one that is not UTF-8 is an
    /// error).
    pub fn with_config(dir: &Path, config: HubConfig) -> Result<Self, HubError> {
        let conn = store::open(&dir.join(DATABASE_FILE))?;
        let terms = read_terms(dir, &config.operator.terms_file)?;
        Ok(Self {
            config,
            terms,
            db: Mutex::new(conn),
            blobs: BlobStore::new(dir.join("blobs")),
        })
    }

    /// The configuration.
    pub fn config(&self) -> &HubConfig {
        &self.config
    }

    /// The terms of use, if the hub has them.
    pub fn terms(&self) -> Option<&str> {
        self.terms.as_deref()
    }

    /// Moves the database in the hub data directory `dir` to the current schema even
    /// if it holds accounts of 0.18–0.20: deletes them with their devices,
    /// publications, records and blob files (taken-down ids stay refused). Returns how
    /// many accounts and publications were deleted.
    pub fn migrate(dir: &Path) -> Result<(u64, u64), HubError> {
        if !dir.join(CONFIG_FILE).is_file() {
            return Err(HubError::NotAHub(dir.to_owned()));
        }
        let deleted = store::migrate_old(&dir.join(DATABASE_FILE))?;
        // After the database: blob files left by a failure later are only unused.
        let blobs = dir.join("blobs");
        if deleted.1 > 0 && blobs.is_dir() {
            std::fs::remove_dir_all(&blobs).map_err(|e| crate::Error::io(&blobs, e))?;
        }
        Ok(deleted)
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Creates a hub invite code valid for `[libraries] invite_days`; returns it and
    /// its expiry time.
    pub fn invite(&self, now: i64) -> Result<(Secret, i64), HubError> {
        let days = self.config.libraries.invite_days;
        let code = store::create_hub_invite(&self.lock(), days, now)?;
        Ok((code, now + i64::from(days) * 86_400))
    }

    /// Takes the publication `id` down (deletes it whichever library it belongs to,
    /// and refuses the id from now on); returns whether it existed.
    pub fn takedown(&self, id: &PublicationId, now: i64) -> Result<bool, HubError> {
        let library = store::takedown(&self.lock(), id, now)?;
        self.blobs.remove(id)?;
        if let Some(library) = library {
            tracing::info!(library, "publication taken down");
        }
        Ok(library.is_some())
    }

    /// Every library with its usage (internal ids, no handles, names or secrets).
    pub fn libraries(&self) -> Result<Vec<LibrarySummary>, HubError> {
        store::libraries(&self.lock())
    }

    /// Deletes the publications not opened for `publication_expiry_days`, and the
    /// libraries and member accounts idle for `[libraries] expiry_days`; returns how
    /// many publications, and libraries and accounts together.
    ///
    /// Works in transactions of `EXPIRY_BATCH` and lets requests in between: one
    /// transaction for 1000 idle accounts kept the hub from answering for 12 s
    /// (`.debug/perf-hub.md`).
    pub fn expire(&self, now: i64) -> Result<(usize, usize), HubError> {
        let days = self.config.limits.publication_expiry_days;
        let mut publications = 0;
        loop {
            let ids = store::expire(&self.lock(), days, now, EXPIRY_BATCH)?;
            self.remove_blobs(&ids);
            publications += ids.len();
            if ids.len() < EXPIRY_BATCH {
                break;
            }
            std::thread::sleep(EXPIRY_PAUSE);
        }
        if publications > 0 {
            tracing::info!(count = publications, "expired publications deleted");
        }
        let days = self.config.libraries.expiry_days;
        let (mut libraries, mut accounts) = (0, 0);
        loop {
            let expired = store::expire_libraries(&mut self.lock(), days, now, EXPIRY_BATCH)?;
            self.remove_blobs(&expired.publications);
            libraries += expired.libraries.len();
            accounts += expired.accounts.len();
            if expired.len() < EXPIRY_BATCH {
                break;
            }
            std::thread::sleep(EXPIRY_PAUSE);
        }
        if libraries > 0 {
            tracing::info!(count = libraries, "idle libraries deleted");
        }
        if accounts > 0 {
            tracing::info!(count = accounts, "idle member accounts deleted");
        }
        Ok((publications, libraries + accounts))
    }

    /// Deletes the library with the internal id `library` and everything it stores;
    /// returns whether it existed.
    pub fn delete_library(&self, library: i64) -> Result<bool, HubError> {
        let Some(ids) = store::delete_library(&mut self.lock(), library)? else {
            return Ok(false);
        };
        self.remove_blobs(&ids);
        tracing::info!(library, "library deleted");
        Ok(true)
    }

    /// The own quotas of the library `library` (`None`: no such library).
    pub fn library_quotas(&self, library: i64) -> Result<Option<LibraryQuotas>, HubError> {
        store::library_quotas(&self.lock(), library)
    }

    /// Sets the own quotas of the library `library`; returns whether it exists.
    pub fn set_library_quotas(
        &self,
        library: i64,
        quotas: &LibraryQuotas,
    ) -> Result<bool, HubError> {
        store::set_library_quotas(&self.lock(), library, quotas)
    }

    /// The bytes stored for all libraries (blobs and records).
    pub fn total_bytes(&self) -> Result<u64, HubError> {
        store::total_bytes(&self.lock())
    }

    fn remove_blobs(&self, ids: &[PublicationId]) {
        for id in ids {
            if let Err(e) = self.blobs.remove(id) {
                tracing::warn!(error = %e, "cannot remove the blobs of a deleted publication");
            }
        }
    }
}

/// The text of the terms file `name` in `dir` (`None`: no name, no file, or only
/// whitespace).
fn read_terms(dir: &Path, name: &str) -> Result<Option<String>, HubError> {
    if name.is_empty() {
        return Ok(None);
    }
    let path = dir.join(name);
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(crate::Error::io(&path, e).into()),
    };
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(file, MAX_TERMS_BYTES + 1),
        &mut bytes,
    )
    .map_err(|e| crate::Error::io(&path, e))?;
    if bytes.len() as u64 > MAX_TERMS_BYTES {
        return Err(crate::Error::io(
            &path,
            std::io::Error::other(format!(
                "the terms file is larger than {MAX_TERMS_BYTES} bytes"
            )),
        )
        .into());
    }
    let text = String::from_utf8(bytes).map_err(|_| {
        crate::Error::io(
            &path,
            std::io::Error::other("the terms file is not UTF-8 text"),
        )
    })?;
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

/// The shared state of the hub's handlers.
#[derive(Clone)]
pub struct HubState(Arc<Inner>);

struct Inner {
    hub: Hub,
    hosts: HostAllowlist,
    base: String,
    devices: DeviceLimiter,
    public: WindowLimiter,
    new_libraries: WindowLimiter,
    logins: LoginLimiter,
    trusted_proxies: Vec<IpRange>,
    /// Whether the hub is below `max_total_bytes`, and when that was checked.
    accepting: Mutex<Option<(std::time::Instant, bool)>>,
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
        let public = WindowLimiter::new(config.limits.public_requests_per_hour, HOUR);
        let new_libraries =
            WindowLimiter::new(config.libraries.new_libraries_per_address_per_day, DAY);
        // Validated with the config.
        let trusted_proxies = (config.server.trusted_proxies.iter())
            .filter_map(|proxy| IpRange::parse(proxy))
            .collect();
        Self(Arc::new(Inner {
            hub,
            hosts,
            base,
            devices,
            public,
            new_libraries,
            logins: LoginLimiter::default(),
            trusted_proxies,
            accepting: Mutex::new(None),
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

    /// Whether the hub accepts new data (below `max_total_bytes`); checked at most once
    /// a minute, since `GET /api/info` asks without a token.
    async fn accepting(&self) -> Result<bool, HubError> {
        let max = self.config().limits.max_total_bytes;
        if max == 0 {
            return Ok(true);
        }
        let cached = *self.0.accepting.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, accepting)) = cached
            && at.elapsed() < Duration::from_secs(60)
        {
            return Ok(accepting);
        }
        let total = self.db(|conn| store::total_bytes(conn)).await?;
        let accepting = total < max;
        *self.0.accepting.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((std::time::Instant::now(), accepting));
        Ok(accepting)
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
/// Any address is allowed: tokens authenticate every request.
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
/// publications and idle libraries and accounts on start and daily.
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
                    tracing::error!(error = %e, "cannot delete expired publications or libraries");
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_deletes_the_old_accounts_and_their_blob_files() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            Hub::migrate(dir.path()),
            Err(HubError::NotAHub(_))
        ));
        HubConfig::create_default(&dir.path().join(CONFIG_FILE)).unwrap();
        store::write_old_database(&dir.path().join(DATABASE_FILE));
        let blob = dir.path().join("blobs/ff/manifest");
        std::fs::create_dir_all(blob.parent().unwrap()).unwrap();
        std::fs::write(&blob, b"sealed").unwrap();
        assert!(matches!(
            Hub::open(dir.path(), false),
            Err(HubError::OldAccounts { .. })
        ));
        assert_eq!(Hub::migrate(dir.path()).unwrap(), (1, 1));
        assert!(!blob.exists());
        let hub = Hub::open(dir.path(), false).unwrap().hub;
        assert!(hub.libraries().unwrap().is_empty());
        // Once moved, there is nothing more to delete.
        drop(hub);
        assert_eq!(Hub::migrate(dir.path()).unwrap(), (0, 0));
    }
}
