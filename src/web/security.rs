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
//! - **Auth slot**: the read and admin route groups pass through [`Auth`]; v1 has no
//!   authentication, so it lets every request through.

use std::net::IpAddr;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::ServerConfig;

use super::{AppState, WebError};

/// The `Content-Security-Policy` of every response: only our own scripts, styles and
/// images, no framing.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data:; \
     object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'";

/// Authentication for the read and admin route groups.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Auth {
    /// No authentication (v1): only safe on a loopback address.
    #[default]
    None,
}

impl Auth {
    /// Whether any authentication is configured.
    pub fn is_configured(self) -> bool {
        match self {
            Self::None => false,
        }
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
        for host in &server.allowed_hosts {
            let host = host.to_ascii_lowercase();
            match split_port(&host) {
                (_, Some(_)) => exact.push(host),
                (name, None) => any_port.push(name.to_owned()),
            }
        }
        Self { exact, any_port }
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
            tracing::warn!("rejected a request with Host {host:?}");
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
            tracing::warn!(
                "rejected a {} request to {} with Origin {origin:?}",
                req.method(),
                req.uri().path()
            );
            forbidden("403 Forbidden: missing or foreign Origin header.\n")
        }
    }
}

/// Middleware: the authentication slot of the read and admin route groups.
pub(crate) async fn require_auth(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    match state.auth() {
        Auth::None => next.run(req).await,
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
        }
    }
}
