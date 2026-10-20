//! The request's profile: who is reading.
//!
//! [`require_profile`] runs on the read, admin and write route groups (after the auth
//! slot) and resolves the profile into a [`CurrentProfile`] request extension:
//!
//! 1. with only one profile, the owner (no cookie needed, no profile UI);
//! 2. with a valid session cookie (`shelfgauge-profile`), its profile;
//! 3. otherwise a page request is redirected (`303`) to the picker with a `next`
//!    path, and a fragment, JSON or `POST` request gets `401` (the scripts reload the
//!    page, which then goes to the picker).
//!
//! A request from another device (see [`Reach`](super::security::Reach)) has no
//! implicit owner and only counts password sessions; without one it goes to the login
//! page instead of the picker (the auth slot has already sent it there).
//!
//! The profile id is recorded on the `request` span (`profile = N`). The extension is
//! also put on the response, so [`render_errors`](super::error::render_errors) renders
//! error pages in the profile's theme. [`require_admin`] answers `403` to a reader on
//! the admin routes, and to a remote admin unless `[server] remote_admin` allows it.
//!
//! A PIN is not authentication (see [`crate::profiles`]): this never replaces the auth
//! slot, and the bind rule is unchanged.

use std::sync::Arc;

use axum::extract::{FromRequestParts, MatchedPath, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use super::security::peer_of;
use super::{AppError, AppState, Urls};
use crate::annotations::now;
use crate::profiles::session::{self, Level, Session, Token};
use crate::profiles::{Profile, ProfileId, store};

/// The session cookie. Cookies ignore the port: libraries served from one host share
/// the name, and a token another library does not know just leads to its picker.
pub(crate) const PROFILE_COOKIE: &str = "shelfgauge-profile";

/// The session cookie's lifetime (about 400 days, the most browsers keep).
const COOKIE_MAX_AGE: u64 = 400 * 24 * 3600;

/// The routes whose `GET` answers a script, not a navigation: without a profile they
/// get `401` instead of the picker. Templates under the base path.
const FRAGMENT_ROUTES: &[&str] = &[
    "/sidebar/authors",
    "/sidebar/author/{id}",
    "/sidebar/folder",
    "/scan/status",
    "/book/{id}/annotations",
    "/book/{id}/find",
    "/cover/{id}",
    "/book/{id}/image/{image}",
];

/// The profile of a request.
#[derive(Clone)]
pub(crate) struct CurrentProfile {
    /// The profile.
    pub(crate) profile: Arc<Profile>,
    /// How many profiles there are (the profile UI shows with two or more).
    pub(crate) count: usize,
    /// The session token, if the profile came from a session.
    pub(crate) token: Option<Token>,
    /// Whether the request comes from another device.
    pub(crate) remote: bool,
}

impl CurrentProfile {
    /// The profile id.
    pub(crate) fn id(&self) -> ProfileId {
        self.profile.id
    }

    /// Whether there are several profiles (the profile UI is shown).
    pub(crate) fn several(&self) -> bool {
        self.count > 1
    }

    /// Whether it has the admin role.
    pub(crate) fn is_admin(&self) -> bool {
        self.profile.is_admin()
    }

    /// Whether it may use the admin pages in this request: an admin, and from another
    /// device only with `[server] remote_admin`.
    pub(crate) fn admin_here(&self, state: &AppState) -> bool {
        self.is_admin() && (!self.remote || state.server().remote_admin)
    }
}

impl<S: Send + Sync> FromRequestParts<S> for CurrentProfile {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<CurrentProfile>()
            .cloned()
            .ok_or_else(|| AppError::Internal("a route without the profile middleware".into()))
    }
}

/// The session tokens in the request's cookies (there may be several with one name,
/// set by libraries with different base paths on one host).
pub(crate) fn cookie_tokens(headers: &HeaderMap) -> Vec<Token> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(name, value)| *name == PROFILE_COOKIE && !value.is_empty())
        .map(|(_, value)| Token::from_cookie(value))
        .collect()
}

/// Resolves the profile of a request with `tokens`; `None` if a profile must be
/// chosen (locally) or logged into (`remote`).
pub(crate) fn resolve(
    state: &AppState,
    tokens: &[Token],
    remote: bool,
) -> Result<Option<CurrentProfile>, AppError> {
    let conn = state.userdata().lock();
    let count = store::count(&conn)?;
    if count <= 1 && !remote {
        let owner = store::get(&conn, ProfileId::OWNER)?
            .ok_or_else(|| AppError::Internal("the owner profile is missing".into()))?;
        return Ok(Some(CurrentProfile {
            profile: Arc::new(owner),
            count: 1,
            token: None,
            remote,
        }));
    }
    let now = now();
    for token in tokens {
        if let Some(found) = session::lookup(&conn, token, now)?
            && (!remote || found.level == Level::Password)
            && let Some(profile) = store::get(&conn, found.profile)?
        {
            return Ok(Some(CurrentProfile {
                profile: Arc::new(profile),
                count,
                token: Some(token.clone()),
                remote,
            }));
        }
    }
    Ok(None)
}

/// The first password session among `tokens` whose profile exists.
pub(crate) fn password_session(
    state: &AppState,
    tokens: &[Token],
) -> Result<Option<Session>, AppError> {
    let conn = state.userdata().lock();
    let now = now();
    for token in tokens {
        if let Some(found) = session::lookup(&conn, token, now)?
            && found.level == Level::Password
            && store::get(&conn, found.profile)?.is_some()
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Middleware: resolves the request's profile (see the module docs).
pub(crate) async fn require_profile(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let tokens = cookie_tokens(req.headers());
    let remote = peer_of(&req).is_remote();
    let s = state.clone();
    let resolved = match super::blocking(move || resolve(&s, &tokens, remote)).await {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(e)) | Err(e) => return e.into_response(),
    };
    let Some(current) = resolved else {
        return if remote {
            login_first(&state, &req)
        } else {
            choose_first(&state, &req)
        };
    };
    tracing::Span::current().record("profile", current.id().0);
    req.extensions_mut().insert(current.clone());
    let mut response = next.run(req).await;
    response.extensions_mut().insert(current);
    response
}

/// The answer to a request that needs a profile chosen first.
fn choose_first(state: &AppState, req: &Request) -> Response {
    let urls = state.urls();
    redirect_page(state, req, "choose a profile", |next| {
        urls.profiles(Some(next))
    })
}

/// The answer to a request from another device that needs a login first.
pub(crate) fn login_first(state: &AppState, req: &Request) -> Response {
    let urls = state.urls();
    redirect_page(state, req, "log in", |next| urls.login(Some(next)))
}

/// A page request goes (`303`) to `to(next)`, with the request's path as `next`; a
/// fragment, JSON or `POST` request gets `401` ("`what` first").
fn redirect_page(
    state: &AppState,
    req: &Request,
    what: &str,
    to: impl FnOnce(&str) -> String,
) -> Response {
    let urls = state.urls();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(MatchedPath::as_str)
        .unwrap_or("");
    let fragment = FRAGMENT_ROUTES.iter().any(|r| urls.path(r) == route);
    if req.method() != Method::GET || fragment {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CACHE_CONTROL, "no-store")],
            format!("401 Unauthorized: {what} first.\n"),
        )
            .into_response();
    }
    let next = req
        .uri()
        .path_and_query()
        .map_or_else(|| urls.home(), |p| p.as_str().to_owned());
    Redirect::to(&to(&local_path(urls, Some(&next)))).into_response()
}

/// Middleware: the admin routes answer `403` with the error page to a reader, and to
/// an admin on another device unless `[server] remote_admin` is on.
pub(crate) async fn require_admin(
    State(state): State<AppState>,
    current: Option<axum::Extension<CurrentProfile>>,
    req: Request,
    next: Next,
) -> Response {
    match current {
        Some(axum::Extension(current)) if current.admin_here(&state) => next.run(req).await,
        Some(axum::Extension(current)) if current.is_admin() => AppError::LocalOnly.into_response(),
        Some(_) => AppError::Forbidden.into_response(),
        None => AppError::Internal("the admin routes need the profile middleware".into())
            .into_response(),
    }
}

/// A local page to go to: `next` if it is a path under the base path (one leading
/// `/`, so no scheme; no `//`, backslash or control characters), else the home page.
pub(crate) fn local_path(urls: &Urls, next: Option<&str>) -> String {
    let home = urls.home();
    match next {
        Some(next)
            if next.starts_with(&home)
                && next.starts_with('/')
                && !next.starts_with("//")
                && !next.contains(|c: char| c == '\\' || c.is_control()) =>
        {
            next.to_owned()
        }
        _ => home,
    }
}

/// How long a session cookie is kept by the browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Keep {
    /// About 400 days ("Remember this device", and every local session).
    Remember,
    /// Until the browser is closed (no `Max-Age`).
    BrowserSession,
}

/// The `Set-Cookie` value that stores `token`: HttpOnly, SameSite=Strict, scoped to
/// the base path. `Secure` when the request's `Origin` is `https://` (the browser
/// sets it on every form `POST`, and the Origin check has accepted it): through an
/// HTTPS reverse proxy, never for plain HTTP (localhost, or a private network address
/// over WireGuard), where a `Secure` cookie would be dropped. `X-Forwarded-Proto` is
/// never read.
pub(crate) fn set_cookie(
    state: &AppState,
    headers: &HeaderMap,
    token: &Token,
    keep: Keep,
) -> HeaderValue {
    let secure = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(|origin| origin.get(..8))
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"));
    let max_age = match keep {
        Keep::Remember => format!("; Max-Age={COOKIE_MAX_AGE}"),
        Keep::BrowserSession => String::new(),
    };
    let value = format!(
        "{PROFILE_COOKIE}={}; Path={}{max_age}; HttpOnly; SameSite=Strict{}",
        token.as_str(),
        state.urls().home(),
        if secure { "; Secure" } else { "" }
    );
    HeaderValue::from_str(&value).expect("a token and a base path are valid header text")
}

/// The `Set-Cookie` value that removes the session cookie.
pub(crate) fn clear_cookie(state: &AppState) -> HeaderValue {
    let value = format!(
        "{PROFILE_COOKIE}=; Path={}; Max-Age=0; HttpOnly; SameSite=Strict",
        state.urls().home()
    );
    HeaderValue::from_str(&value).expect("a base path is valid header text")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_stays_local() {
        let urls = Urls::new("/books");
        assert_eq!(
            local_path(&urls, Some("/books/book/1?part=2")),
            "/books/book/1?part=2"
        );
        for bad in [
            "//evil.example/books/",
            "https://evil.example/books/",
            "/books\\..",
            "/\\evil.example",
            "/other/",
            "/books/\r\nX: y",
            "",
        ] {
            assert_eq!(local_path(&urls, Some(bad)), "/books/", "{bad}");
        }
        assert_eq!(local_path(&urls, None), "/books/");
        assert_eq!(local_path(&urls, Some("/books")), "/books/");
        let root = Urls::new("/");
        for bad in ["//evil.example", "/\\evil.example", "https://evil", "evil"] {
            assert_eq!(local_path(&root, Some(bad)), "/", "{bad}");
        }
        assert_eq!(local_path(&root, Some("/stats")), "/stats");
        assert_eq!(
            local_path(&root, Some("/search?q=http://x")),
            "/search?q=http://x"
        );
    }

    #[test]
    fn tokens_from_cookies() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("a=b; shelfgauge-profile=one; shelfgauge-profile="),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("shelfgauge-profile=two"),
        );
        let tokens: Vec<String> = cookie_tokens(&headers)
            .iter()
            .map(|t| t.as_str().to_owned())
            .collect();
        assert_eq!(tokens, ["one", "two"]);
    }
}
