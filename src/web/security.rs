//! Request checks that protect a server bound to a local port.
//!
//! - **Host allowlist** against DNS rebinding: a malicious site can point its own domain
//!   at `127.0.0.1` and then read our responses as same-origin. Only `localhost:<port>`,
//!   `127.0.0.1:<port>`, `[::1]:<port>`, the bind address and `server.allowed_hosts` are
//!   accepted.
//! - **Origin check** against CSRF: every request with a method other than
//!   `GET`/`HEAD`/`OPTIONS`/`TRACE` needs an `Origin` whose host is on the same allowlist.
//!   The allowlist rather than the request's `Host` is used because a reverse proxy may
//!   rewrite `Host` while the browser sends its public origin.
//! - **Bind safety**: a non-loopback `bind` without authentication is refused.
//! - **Reach**: a layer tells requests from this computer ([`Reach::Local`])
//!   from those of other devices ([`Reach::Remote`]): a request is remote when its TCP
//!   peer is not a loopback address, when its `Host` is an `allowed_hosts` name (a
//!   reverse proxy on the same machine connects from loopback), or when it carries a
//!   header that proxies add ([`PROXY_HEADERS`], for a proxy that rewrites `Host` to
//!   `127.0.0.1`). The values of forwarded headers are never read: they can only make
//!   a request remote, never local. Without [`Auth::Password`], remote requests are
//!   refused.
//! - **Auth slot**: the read, write and admin route groups pass through
//!   the auth slot: a remote request needs a password session of its profile, else
//!   it is sent to the login page (`303`) or gets `401` (scripts). Local requests are
//!   left to the profile middleware.

use std::net::{IpAddr, SocketAddr};

use axum::extract::connect_info::{ConnectInfo, MockConnectInfo};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::{AuthMode, ServerConfig};

use super::{AppState, WebError};

/// The `Content-Security-Policy` of every response: only our own scripts, styles and
/// images, no framing.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data:; \
     object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Authentication of requests from other devices.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Auth {
    /// No authentication: only requests from this computer are served, and only a
    /// loopback address may be bound.
    #[default]
    None,
    /// Requests from other devices need a password session of their profile.
    Password,
}

impl Auth {
    /// The authentication `[server] auth` asks for.
    pub fn from_config(server: &ServerConfig) -> Self {
        match server.auth {
            AuthMode::None => Self::None,
            AuthMode::Password => Self::Password,
        }
    }

    /// Whether any authentication is configured.
    pub fn is_configured(self) -> bool {
        match self {
            Self::None => false,
            Self::Password => true,
        }
    }
}

/// Where a request comes from (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// This computer: a loopback peer with a loopback or bind-address `Host`.
    Local,
    /// Another device: a non-loopback peer, or a name from `allowed_hosts`.
    Remote,
}

/// Headers that reverse proxies add: their presence alone makes a request remote (a
/// proxy that rewrites `Host` to its upstream, `127.0.0.1:8742`, would otherwise look
/// like this computer). Their values are never read.
pub const PROXY_HEADERS: &[&str] = &[
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-host",
    "x-real-ip",
    "tailscale-user-login",
];

/// The request extension set by [`classify_reach`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Peer {
    /// Where the request comes from.
    pub(crate) reach: Reach,
    /// The client's address when it is not a loopback address, the key of the
    /// per-address login budget; `None` behind a reverse proxy on this computer, where
    /// all clients share the proxy's address. (A proxy on another machine shares one
    /// budget among its clients.)
    pub(crate) direct: Option<IpAddr>,
}

impl Peer {
    /// Whether the request comes from another device.
    pub(crate) fn is_remote(self) -> bool {
        self.reach == Reach::Remote
    }
}

/// Classifies a request: its TCP peer (`None` if unknown) and whether it came through
/// a proxy (its `Host` is an `allowed_hosts` name, or it has one of the
/// [`PROXY_HEADERS`]).
///
/// # Examples
///
/// ```
/// use shelfgauge::web::security::{Reach, reach};
///
/// let local = "127.0.0.1:50000".parse().ok();
/// assert_eq!(reach(local, false), Reach::Local);
/// assert_eq!(reach(local, true), Reach::Remote);
/// assert_eq!(reach("100.64.0.2:50000".parse().ok(), false), Reach::Remote);
/// assert_eq!(reach(None, false), Reach::Remote);
/// ```
pub fn reach(peer: Option<SocketAddr>, proxied: bool) -> Reach {
    match peer {
        Some(peer) if peer.ip().to_canonical().is_loopback() && !proxied => Reach::Local,
        _ => Reach::Remote,
    }
}

/// Refuses to serve on a non-loopback address without authentication.
///
/// # Examples
///
/// ```
/// use shelfgauge::web::security::{Auth, check_bind};
///
/// assert!(check_bind("127.0.0.1".parse().unwrap(), Auth::None).is_ok());
/// assert!(check_bind("::1".parse().unwrap(), Auth::None).is_ok());
/// assert!(check_bind("0.0.0.0".parse().unwrap(), Auth::None).is_err());
/// ```
pub fn check_bind(bind: IpAddr, auth: Auth) -> Result<(), WebError> {
    if bind.to_canonical().is_loopback() || auth.is_configured() {
        Ok(())
    } else {
        Err(WebError::UnsafeBind(bind))
    }
}

/// The accepted `Host` header values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAllowlist {
    /// `host:port` values that must match exactly (lowercase).
    exact: Vec<String>,
    /// Host names from `allowed_hosts` without a port: accepted with any port or none.
    any_port: Vec<String>,
    /// The `allowed_hosts` entries with a port (also in `exact`).
    configured: Vec<String>,
}

impl HostAllowlist {
    /// The allowlist for a server configuration.
    ///
    /// An `allowed_hosts` entry with a port (`books.example.org:8080`) must match
    /// exactly; one without a port matches that host with any port or none, which is
    /// what a browser sends through a reverse proxy on the default HTTPS port.
    pub fn new(server: &ServerConfig) -> Self {
        let port = server.port;
        let mut exact = vec![
            format!("localhost:{port}"),
            format!("127.0.0.1:{port}"),
            format!("[::1]:{port}"),
        ];
        let bind = server.bind.to_canonical();
        if !bind.is_unspecified() {
            let bind = match bind {
                IpAddr::V4(ip) => format!("{ip}:{port}"),
                IpAddr::V6(ip) => format!("[{ip}]:{port}"),
            };
            if !exact.contains(&bind) {
                exact.push(bind);
            }
        }
        let mut any_port = Vec::new();
        let mut configured = Vec::new();
        for host in &server.allowed_hosts {
            let host = host.to_ascii_lowercase();
            match split_port(&host) {
                (_, Some(_)) => {
                    exact.push(host.clone());
                    configured.push(host);
                }
                (name, None) => any_port.push(name.to_owned()),
            }
        }
        Self {
            exact,
            any_port,
            configured,
        }
    }

    /// Whether `host` is accepted because of a `server.allowed_hosts` entry (a name
    /// behind a reverse proxy), not as a loopback or bind address.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::config::ServerConfig;
    /// use shelfgauge::web::security::HostAllowlist;
    ///
    /// let server = ServerConfig {
    ///     allowed_hosts: vec!["books.example.org".into()],
    ///     ..ServerConfig::default()
    /// };
    /// let hosts = HostAllowlist::new(&server);
    /// assert!(hosts.is_configured_host("books.example.org"));
    /// assert!(!hosts.is_configured_host("localhost:8742"));
    /// ```
    pub fn is_configured_host(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        if self.configured.contains(&host) {
            return true;
        }
        let (name, _) = split_port(&host);
        self.any_port.iter().any(|allowed| *allowed == name)
    }

    /// Whether a `Host` header value (or the authority of an `Origin`) is accepted.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::config::ServerConfig;
    /// use shelfgauge::web::security::HostAllowlist;
    ///
    /// let hosts = HostAllowlist::new(&ServerConfig::default());
    /// assert!(hosts.allows("localhost:8742"));
    /// assert!(!hosts.allows("evil.example:8742"));
    /// ```
    pub fn allows(&self, host: &str) -> bool {
        let host = host.to_ascii_lowercase();
        if self.exact.contains(&host) {
            return true;
        }
        let (name, _) = split_port(&host);
        self.any_port.iter().any(|allowed| *allowed == name)
    }

    /// Whether an `Origin` header value is a `http`/`https` origin with an accepted host.
    pub fn allows_origin(&self, origin: &str) -> bool {
        let Some((scheme, authority)) = origin.split_once("://") else {
            return false;
        };
        (scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            && !authority.is_empty()
            && !authority.contains(['/', '?', '#', '@'])
            && self.allows(authority)
    }
}

/// Splits `host[:port]` (with `[v6]` literals) into the host and the port, if any.
fn split_port(host: &str) -> (&str, Option<&str>) {
    let port_start = if host.starts_with('[') {
        host.find(']').map(|end| end + 1)
    } else {
        host.rfind(':')
    };
    match port_start {
        Some(i) if host[i..].starts_with(':') => {
            let port = &host[i + 1..];
            if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) {
                (&host[..i], Some(port))
            } else {
                (host, None)
            }
        }
        _ => (host, None),
    }
}

fn forbidden(message: &'static str) -> Response {
    (StatusCode::FORBIDDEN, message).into_response()
}

/// Middleware: rejects requests whose `Host` is not on the allowlist.
///
/// HTTP/2 requests may carry the host only in the URI authority, which is used when
/// there is no `Host` header.
pub(crate) async fn check_host(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let host = match req.headers().get(header::HOST) {
        Some(value) => value.to_str().ok(),
        None => req.uri().authority().map(|a| a.as_str()),
    };
    match host {
        Some(host) if state.hosts().allows(host) => next.run(req).await,
        _ => {
            tracing::warn!(host = ?host, "rejected a request with an unknown Host");
            forbidden(
                "403 Forbidden: unknown Host header.\n\nTo reach this server under another \
                 name, add it to `server.allowed_hosts` in .shelfgauge/config.toml.\n",
            )
        }
    }
}

/// Middleware: state-changing requests need an `Origin` on the allowlist.
pub(crate) async fn check_origin(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if matches!(
        *req.method(),
        Method::GET | Method::HEAD | Method::OPTIONS | Method::TRACE
    ) {
        return next.run(req).await;
    }
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    match origin {
        Some(origin) if state.hosts().allows_origin(origin) => next.run(req).await,
        _ => {
            // The request span has the method and the route; never the raw path.
            tracing::warn!(origin = ?origin, "rejected a request with a foreign Origin");
            forbidden("403 Forbidden: missing or foreign Origin header.\n")
        }
    }
}

/// The TCP peer of a request: the server's connection info, or the mock of the
/// tests.
fn peer_addr(req: &Request) -> Option<SocketAddr> {
    let extensions = req.extensions();
    extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| *addr)
        .or_else(|| {
            extensions
                .get::<MockConnectInfo<SocketAddr>>()
                .map(|MockConnectInfo(addr)| *addr)
        })
}

/// Middleware (after the `Host` check): sets the [`Peer`] extension, records
/// `remote = true` on the request span, and refuses remote requests without
/// [`Auth::Password`]. A request without a known peer counts as remote.
pub(crate) async fn classify_reach(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let host = match req.headers().get(header::HOST) {
        Some(value) => value.to_str().ok(),
        None => req.uri().authority().map(|a| a.as_str()),
    };
    let proxied = host.is_some_and(|h| state.hosts().is_configured_host(h))
        || PROXY_HEADERS
            .iter()
            .any(|name| req.headers().contains_key(*name));
    let addr = peer_addr(&req);
    let reach = reach(addr, proxied);
    let direct = addr
        .map(|a| a.ip().to_canonical())
        .filter(|ip| !ip.is_loopback());
    if reach == Reach::Remote {
        tracing::Span::current().record("remote", true);
        if !state.auth().is_configured() {
            tracing::warn!("refused a request from another device: auth is not configured");
            return forbidden(
                "403 Forbidden: this shelfgauge serves only the computer it runs on.\n\n\
                 Requests through a name from `server.allowed_hosts` or a reverse proxy \
                 come from other devices. To use the library from them, set `auth = \"password\"` in the \
                 [server] section of .shelfgauge/config.toml, give an admin profile a \
                 password (`shelfgauge profiles set-password <name>`) and restart \
                 shelfgauge.\n",
            );
        }
    }
    req.extensions_mut().insert(Peer { reach, direct });
    next.run(req).await
}

/// The [`Peer`] of a request; remote if the layer did not run.
pub(crate) fn peer_of(req: &Request) -> Peer {
    peer_in(req.extensions())
}

fn peer_in(extensions: &axum::http::Extensions) -> Peer {
    extensions.get::<Peer>().copied().unwrap_or(Peer {
        reach: Reach::Remote,
        direct: None,
    })
}

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for Peer {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(peer_in(&parts.extensions))
    }
}

/// Middleware: the authentication slot of the read, write and admin route groups.
///
/// A local request passes (the profile middleware decides). A remote one needs a
/// password session in its cookies; otherwise a page request is sent to the login
/// page and a fragment, JSON or `POST` request gets `401`.
pub(crate) async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    if !peer_of(&req).is_remote() {
        return next.run(req).await;
    }
    if !state.auth().is_configured() {
        return forbidden("403 Forbidden: only this computer is served.\n");
    }
    let tokens = super::profile::cookie_tokens(req.headers());
    let s = state.clone();
    let found = super::blocking(move || super::profile::password_session(&s, &tokens)).await;
    match found {
        Ok(Ok(Some(_))) => next.run(req).await,
        Ok(Ok(None)) => super::profile::login_first(&state, &req),
        Ok(Err(e)) | Err(e) => e.into_response(),
    }
}

/// Middleware: hardening headers on every response.
pub(crate) async fn security_headers(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Not `no-referrer`: with it, browsers send `Origin: null` on form posts, which
    // `check_origin` rejects.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(bind: &str, allowed_hosts: &[&str]) -> ServerConfig {
        ServerConfig {
            bind: bind.parse().unwrap(),
            port: 8742,
            allowed_hosts: allowed_hosts.iter().map(|h| (*h).to_owned()).collect(),
            ..ServerConfig::default()
        }
    }

    #[test]
    fn loopback_hosts_with_the_port_are_allowed() {
        let hosts = HostAllowlist::new(&server("127.0.0.1", &[]));
        for host in [
            "localhost:8742",
            "127.0.0.1:8742",
            "[::1]:8742",
            "LOCALHOST:8742",
        ] {
            assert!(hosts.allows(host), "{host}");
        }
    }

    #[test]
    fn rebinding_and_wrong_port_hosts_are_rejected() {
        let hosts = HostAllowlist::new(&server("127.0.0.1", &[]));
        for host in [
            "evil.example:8742",
            "evil.example",
            "localhost",
            "localhost:80",
            "127.0.0.1",
            "127.0.0.1:8743",
            "localhost.:8742",
            "127.0.0.1.evil.example:8742",
            "localhost:8742@evil.example",
            "",
        ] {
            assert!(!hosts.allows(host), "{host}");
        }
    }

    #[test]
    fn bind_address_is_allowed() {
        let hosts = HostAllowlist::new(&server("192.168.1.5", &[]));
        assert!(hosts.allows("192.168.1.5:8742"));
        let hosts = HostAllowlist::new(&server("fe80::1", &[]));
        assert!(hosts.allows("[fe80::1]:8742"));
        let hosts = HostAllowlist::new(&server("0.0.0.0", &[]));
        assert!(!hosts.allows("0.0.0.0:8742"));
    }

    #[test]
    fn allowed_hosts_without_port_match_any_port() {
        let hosts = HostAllowlist::new(&server(
            "127.0.0.1",
            &["Books.Example.org", "nas.local:8080", "[fd00::2]"],
        ));
        assert!(hosts.allows("books.example.org"));
        assert!(hosts.allows("books.example.org:443"));
        assert!(hosts.allows("nas.local:8080"));
        assert!(!hosts.allows("nas.local"));
        assert!(!hosts.allows("nas.local:8081"));
        assert!(hosts.allows("[fd00::2]:8742"));
        assert!(!hosts.allows("sub.books.example.org"));
    }

    #[test]
    fn origin_must_be_http_with_an_allowed_host() {
        let hosts = HostAllowlist::new(&server("127.0.0.1", &["books.example.org"]));
        for origin in [
            "http://localhost:8742",
            "http://127.0.0.1:8742",
            "https://books.example.org",
        ] {
            assert!(hosts.allows_origin(origin), "{origin}");
        }
        for origin in [
            "null",
            "",
            "http://evil.example",
            "http://localhost:8743",
            "file://localhost:8742",
            "http://",
            "http://localhost:8742/path",
            "http://user@localhost:8742",
        ] {
            assert!(!hosts.allows_origin(origin), "{origin}");
        }
    }

    #[test]
    fn port_splitting() {
        assert_eq!(split_port("a:1"), ("a", Some("1")));
        assert_eq!(split_port("a"), ("a", None));
        assert_eq!(split_port("[::1]:80"), ("[::1]", Some("80")));
        assert_eq!(split_port("[::1]"), ("[::1]", None));
        assert_eq!(split_port("a:"), ("a:", None));
        assert_eq!(split_port("a:x"), ("a:x", None));
    }

    #[test]
    fn reach_rule() {
        let peer = |s: &str| Some(s.parse::<SocketAddr>().unwrap());
        for local in [
            "127.0.0.1:1",
            "127.0.0.2:1",
            "[::1]:1",
            "[::ffff:127.0.0.1]:1",
        ] {
            assert_eq!(reach(peer(local), false), Reach::Local, "{local}");
            assert_eq!(
                reach(peer(local), true),
                Reach::Remote,
                "{local} via a name"
            );
        }
        for remote in [
            "192.168.1.5:1",
            "100.64.0.1:1",
            "[fd7a::1]:1",
            "[::ffff:10.0.0.1]:1",
        ] {
            assert_eq!(reach(peer(remote), false), Reach::Remote, "{remote}");
        }
        assert_eq!(reach(None, false), Reach::Remote, "unknown peer");
    }

    #[test]
    fn bind_safety_rule() {
        for bind in ["127.0.0.1", "127.0.0.2", "::1", "::ffff:127.0.0.1"] {
            assert!(
                check_bind(bind.parse().unwrap(), Auth::None).is_ok(),
                "{bind}"
            );
        }
        for bind in ["0.0.0.0", "::", "192.168.1.5", "fe80::1", "::ffff:10.0.0.1"] {
            let err = check_bind(bind.parse().unwrap(), Auth::None).unwrap_err();
            assert!(matches!(err, WebError::UnsafeBind(_)), "{bind}: {err:?}");
            assert!(
                check_bind(bind.parse().unwrap(), Auth::Password).is_ok(),
                "{bind}"
            );
        }
        let server = ServerConfig {
            auth: AuthMode::Password,
            ..ServerConfig::default()
        };
        assert_eq!(Auth::from_config(&server), Auth::Password);
        assert_eq!(Auth::from_config(&ServerConfig::default()), Auth::None);
    }
}
