//! The reader app shell: `GET /` (the shelf), `GET /pair`, `GET /open` and
//! `GET /device` (this device's page) serve the start page,
//! `GET /r/{id}` the reading view, and `GET /assets/{*path}` their files: the app's own
//! from `assets/hub/` and the library's reading files (`style.css`, `reader.css`,
//! `theme.js`, `app.js`, `reader.js`, `icon.svg`) from `assets/`.
//!
//! The pages are static (`templates/hub/`): the same for every well-formed
//! publication id and served without a database lookup, so they say nothing about
//! whether a publication exists. The app (`assets/hub/hub.js`) fills them in.

use askama::Template;
use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use rust_embed::Embed;
use sha2::{Digest, Sha256};

use super::{HubError, HubState};
use crate::annotations::lists::Lists;
use crate::annotations::results::FindResults;
use crate::publication::format::PublicationId;
use crate::publication::format::container::MIME;
use crate::web::assets::{content_type, hex_prefix};

#[derive(Embed)]
#[folder = "assets/hub/"]
struct HubAssets;

/// The library's files the reading view shares.
#[derive(Embed)]
#[folder = "assets/"]
#[include = "style.css"]
#[include = "reader.css"]
#[include = "theme.js"]
#[include = "app.js"]
#[include = "reader.js"]
#[include = "icon.svg"]
#[exclude = "hub/*"]
struct SharedAssets;

/// `/` and `/pair`.
#[derive(Template)]
#[template(path = "hub/home.html")]
struct HomePage {
    base: String,
}

/// `/terms`.
#[derive(Template)]
#[template(path = "hub/terms.html")]
struct TermsPage {
    base: String,
    /// Who runs the hub (may be empty).
    name: String,
    /// The terms' paragraphs (none: no terms).
    paragraphs: Vec<String>,
    /// The contact as written (may be empty).
    contact: String,
    /// A link for the contact: `mailto:` for an address, the URL for `https://`.
    contact_href: Option<String>,
}

/// `/r/{id}`.
#[derive(Template)]
#[template(path = "hub/read.html")]
struct ReadPage {
    base: String,
    /// The limits of a mark's text and note: the library's defaults.
    max_text: u32,
    max_note: u32,
    /// Empty lists (the app fills them in).
    lists: Lists,
    /// No search yet (the app shows results).
    find: FindPanel,
}

/// The Search panel of the reading view, empty.
struct FindPanel {
    results: Option<FindResults>,
}

/// The shell routes.
pub(super) fn routes(state: &HubState) -> Router<HubState> {
    let base = state.path("");
    let mut router = Router::new().route(&format!("{base}/"), get(page));
    if !base.is_empty() {
        // `/hub` as well as `/hub/` (the app's start URL and the Service Worker's
        // cached shell).
        router = router.route(&base, get(page));
    }
    router
        .route(&state.path("/r/{id}"), get(reader))
        .route(&state.path("/pair"), get(page))
        // Book files: opened by the installed app (`file_handlers`) and shared to it
        // (`share_target`, a POST the Service Worker answers).
        .route(&state.path("/open"), get(page))
        .route(&state.path("/share"), post(share_fallback))
        .route(&state.path("/device"), get(page))
        .route(&state.path("/terms"), get(terms))
        .route(&state.path("/assets/{*path}"), get(asset))
        .route(&state.path("/sw.js"), get(service_worker))
        .route(&state.path("/manifest.webmanifest"), get(web_manifest))
}

/// `GET /terms`: the terms of use and the operator's contact (not cached by the app:
/// the current text comes from the hub).
async fn terms(State(state): State<HubState>) -> Response {
    let operator = &state.config().operator;
    let paragraphs = state.hub().terms().map_or_else(Vec::new, |text| {
        text.replace("\r\n", "\n")
            .split("\n\n")
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_owned)
            .collect()
    });
    let page = TermsPage {
        base: state.path(""),
        name: operator.name.clone(),
        paragraphs,
        contact: operator.contact.clone(),
        contact_href: contact_href(&operator.contact),
    };
    match page.render() {
        Ok(html) => ([(header::CACHE_CONTROL, "no-cache")], Html(html)).into_response(),
        Err(_) => {
            HubError::Io(std::io::Error::other("the terms page did not render")).into_response()
        }
    }
}

/// A link for an operator contact: `mailto:` for an e-mail address, the URL itself for
/// an `https://` URL, none for anything else.
fn contact_href(contact: &str) -> Option<String> {
    let contact = contact.trim();
    if contact.starts_with("https://") && !contact.contains(char::is_whitespace) {
        return Some(contact.to_owned());
    }
    let (user, domain) = contact.split_once('@')?;
    let plain =
        |s: &str| !s.is_empty() && s.chars().all(|c| c.is_alphanumeric() || "._+-".contains(c));
    (plain(user) && plain(domain) && domain.contains('.')).then(|| format!("mailto:{contact}"))
}

/// The Service Worker (`templates/hub/sw.js`).
#[derive(Template)]
#[template(path = "hub/sw.js", escape = "none")]
struct ServiceWorker<'a> {
    base: String,
    version: &'a str,
    files: &'a [String],
}

/// The names of the shell's files, sorted, and their version: 16 hex digits of a
/// SHA-256 over every file's name and hash (a new build with changed files gives the
/// Service Worker a new cache).
fn shell_files() -> &'static (Vec<String>, String) {
    static FILES: std::sync::OnceLock<(Vec<String>, String)> = std::sync::OnceLock::new();
    FILES.get_or_init(|| {
        let mut names: Vec<String> = HubAssets::iter()
            .chain(SharedAssets::iter())
            .map(|name| name.into_owned())
            .collect();
        names.sort();
        names.dedup();
        let mut hasher = Sha256::new();
        for name in &names {
            let file = HubAssets::get(name)
                .or_else(|| SharedAssets::get(name))
                .expect("listed files exist");
            hasher.update(name.as_bytes());
            hasher.update([0]);
            hasher.update(file.metadata.sha256_hash());
        }
        // Templates of the pages are part of the shell too.
        hasher.update(
            HomePage {
                base: String::new(),
            }
            .render()
            .unwrap_or_default(),
        );
        hasher.update(read_page(String::new()).render().unwrap_or_default());
        let version = hex_prefix(&hasher.finalize().into());
        (names, version)
    })
}

/// The Service Worker for the base path `base` (`""` or `/hub`).
fn service_worker_text(base: &str) -> String {
    let (files, version) = shell_files();
    ServiceWorker {
        base: base.to_owned(),
        version,
        files,
    }
    .render()
    .expect("the Service Worker renders")
}

/// `GET /sw.js`: the Service Worker, scoped to the hub's base path.
async fn service_worker(State(state): State<HubState>) -> Response {
    let script = service_worker_text(&state.path(""));
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/javascript; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        script,
    )
        .into_response()
}

/// `GET /manifest.webmanifest`: the web app manifest, so the reader can be installed
/// (on iOS, a home screen app keeps its data longer than a Safari tab).
async fn web_manifest(State(state): State<HubState>) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/manifest+json"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
        ],
        manifest_text(&state.path("")),
    )
        .into_response()
}

/// The web app manifest for the base path `base`.
fn manifest_text(base: &str) -> String {
    let manifest = serde_json::json!({
        "name": "shelfgauge",
        "short_name": "shelfgauge",
        "description": "Your books from your shelfgauge library, also offline.",
        "start_url": format!("{base}/"),
        "scope": format!("{base}/"),
        "display": "standalone",
        "background_color": "#ffffff",
        "theme_color": "#ffffff",
        // Chrome installs the app only with 192 and 512 pixel raster icons; the
        // padded PNGs also serve as maskable icons on Android.
        "icons": [
            {
                "src": format!("{base}/assets/icon-192.png"),
                "sizes": "192x192",
                "type": "image/png",
                "purpose": "any",
            },
            {
                "src": format!("{base}/assets/icon-512.png"),
                "sizes": "512x512",
                "type": "image/png",
                "purpose": "any",
            },
            {
                "src": format!("{base}/assets/icon-512.png"),
                "sizes": "512x512",
                "type": "image/png",
                "purpose": "maskable",
            },
            {
                "src": format!("{base}/assets/icon.svg"),
                "sizes": "any",
                "type": "image/svg+xml",
            },
        ],
        // Book files (`.sgbook`; `.sgb` opens too): double-click on desktop Chromium,
        // "Share" on Android.
        "file_handlers": [{
            "action": format!("{base}/open"),
            "accept": { MIME: [".sgbook", ".sgb"] },
        }],
        "share_target": {
            "action": format!("{base}/share"),
            "method": "POST",
            "enctype": "multipart/form-data",
            "params": {
                "files": [{ "name": "book", "accept": [MIME, ".sgbook", ".sgb"] }],
            },
        },
    });
    manifest.to_string()
}

/// Every file of the reader app as a hub with the base path `base` serves it: the
/// path below the base and the bytes. What `shelfgauge hub check` compares; the
/// reading view stands for every `/r/{id}` (the same page for each id).
pub(crate) fn expected_files(base: &str) -> Vec<(String, Vec<u8>)> {
    fn page(template: &impl Template) -> Vec<u8> {
        template
            .render()
            .expect("the shell pages render")
            .into_bytes()
    }
    let mut files = vec![
        (
            "/".to_owned(),
            page(&HomePage {
                base: base.to_owned(),
            }),
        ),
        (
            format!("/r/{}", PublicationId::new([0; 16])),
            page(&read_page(base.to_owned())),
        ),
        ("/sw.js".to_owned(), service_worker_text(base).into_bytes()),
        (
            "/manifest.webmanifest".to_owned(),
            manifest_text(base).into_bytes(),
        ),
    ];
    for name in &shell_files().0 {
        let file = HubAssets::get(name)
            .or_else(|| SharedAssets::get(name))
            .expect("listed files exist");
        files.push((format!("/assets/{name}"), file.data.into_owned()));
    }
    files
}

/// The reader app's version: the Service Worker's `VERSION`.
pub(crate) fn app_version() -> &'static str {
    &shell_files().1
}

/// `POST /share` when no Service Worker took it (the app was never opened in this
/// browser, or its worker is gone): the shared file is not read; the shelf says so.
async fn share_fallback(State(state): State<HubState>) -> Redirect {
    Redirect::to(&state.path("/?shared=lost"))
}

/// A page of the shell.
fn shell(page: &impl Template) -> Response {
    let text = page.render().expect("the shell pages render");
    (
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"))],
        Html(text),
    )
        .into_response()
}

/// `GET /pair`, `GET /open`, `GET /device` and `GET /`.
async fn page(State(state): State<HubState>) -> Response {
    shell(&HomePage {
        base: state.path(""),
    })
}

/// `GET /r/{id}`: the reading view for any well-formed publication id.
async fn reader(
    State(state): State<HubState>,
    Path(id): Path<String>,
) -> Result<Response, HubError> {
    id.parse::<PublicationId>()
        .map_err(|_| HubError::NotFound)?;
    Ok(shell(&read_page(state.path(""))))
}

/// The reading view under `base`.
fn read_page(base: String) -> ReadPage {
    let limits = crate::config::AnnotationsConfig::default();
    ReadPage {
        base,
        max_text: limits.max_text_length,
        max_note: limits.max_note_length,
        lists: Lists::default(),
        find: FindPanel { results: None },
    }
}

/// `GET /assets/{*path}`: a file of the shell, revalidated by its `ETag`.
async fn asset(Path(name): Path<String>, headers: HeaderMap) -> Result<Response, HubError> {
    let file = HubAssets::get(&name)
        .or_else(|| SharedAssets::get(&name))
        .ok_or(HubError::NotFound)?;
    let etag = format!("\"{}\"", hex_prefix(&file.metadata.sha256_hash()));
    let etag_value = HeaderValue::from_str(&etag).expect("hex is a valid header");
    let cache = (header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|tag| tag.trim() == etag));
    if matches {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag_value), cache],
        )
            .into_response());
    }
    Ok((
        [
            (header::ETAG, etag_value),
            cache,
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(content_type(&name)),
            ),
        ],
        file.data,
    )
        .into_response())
}
