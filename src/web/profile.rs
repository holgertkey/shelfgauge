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
//! The profile id is recorded on the `request` span (`profile = N`). The extension is
//! also put on the response, so [`render_errors`](super::error::render_errors) renders
//! error pages in the profile's theme. [`require_admin`] answers `403` to a reader on
//! the admin routes.
//!
//! A profile is not authentication (see [`crate::profiles`]): this never replaces the
//! auth slot, and the bind rule is unchanged.

use std::sync::Arc;

use axum::extract::{FromRequestParts, MatchedPath, Request, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

use super::{AppError, AppState, Urls};
use crate::annotations::now;
use crate::profiles::session::{self, Token};
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

    /// Whether it may use the admin pages.
    pub(crate) fn is_admin(&self) -> bool {
        self.profile.is_admin()
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
/// chosen.
pub(crate) fn resolve(
    state: &AppState,
    tokens: &[Token],
) -> Result<Option<CurrentProfile>, AppError> {
    let conn = state.userdata().lock();
    let count = store::count(&conn)?;
    if count <= 1 {
        let owner = store::get(&conn, ProfileId::OWNER)?
            .ok_or_else(|| AppError::Internal("the owner profile is missing".into()))?;
        return Ok(Some(CurrentProfile {
            profile: Arc::new(owner),
            count: 1,
            token: None,
        }));
    }
    let now = now();
    for token in tokens {
        if let Some(id) = session::lookup(&conn, token, now)?
            && let Some(profile) = store::get(&conn, id)?
        {
            return Ok(Some(CurrentProfile {
                profile: Arc::new(profile),
                count,
                token: Some(token.clone()),
            }));
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
    let s = state.clone();
    let resolved = match super::blocking(move || resolve(&s, &tokens)).await {
        Ok(Ok(resolved)) => resolved,
        Ok(Err(e)) | Err(e) => return e.into_response(),
    };
    let Some(current) = resolved else {
        return choose_first(&state, &req);
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
            "401 Unauthorized: choose a profile first.\n",
        )
            .into_response();
    }
    let next = req
        .uri()
        .path_and_query()
        .map_or_else(|| urls.home(), |p| p.as_str().to_owned());
    Redirect::to(&urls.profiles(Some(&local_path(urls, Some(&next))))).into_response()
}

/// Middleware: the admin routes answer `403` with the error page to a reader.
pub(crate) async fn require_admin(
    current: Option<axum::Extension<CurrentProfile>>,
    req: Request,
    next: Next,
) -> Response {
    match current {
        Some(axum::Extension(current)) if current.is_admin() => next.run(req).await,
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

/// The `Set-Cookie` value that stores `token`: HttpOnly, SameSite=Strict, scoped to
/// the base path; `Secure` only behind an HTTPS reverse proxy (a configured
/// `allowed_hosts` name with `X-Forwarded-Proto: https`), never for plain-HTTP
/// localhost.
pub(crate) fn set_cookie(state: &AppState, headers: &HeaderMap, token: &Token) -> HeaderValue {
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case("https"));
    let secure = https && state.hosts().is_configured_host(host);
    let value = format!(
        "{PROFILE_COOKIE}={}; Path={}; Max-Age={COOKIE_MAX_AGE}; HttpOnly; SameSite=Strict{}",
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
