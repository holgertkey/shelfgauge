//! Static assets (CSS, JS, icons) embedded into the binary from `assets/`.
//!
//! Assets are embedded in debug builds too (`rust-embed`'s `debug-embed` feature), so a
//! debug binary started from any directory serves the same files as a release build.
//! Responses carry an `ETag` (from the SHA-256 of the content); a URL with the current
//! version (`?v=…`, see [`Urls::asset`](super::Urls::asset)) is cached as immutable.

use axum::extract::{Path, RawQuery};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

use super::AppError;

#[derive(Embed)]
#[folder = "assets/"]
// The hub's files are served by the hub only (`crate::hub`).
#[exclude = "hub/*"]
struct Assets;

/// `Cache-Control` for a URL with the current version.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// The content version of an embedded asset: 16 hex digits of its SHA-256.
pub(crate) fn version(name: &str) -> Option<String> {
    Assets::get(name).map(|file| hex_prefix(&file.metadata.sha256_hash()))
}

pub(crate) fn hex_prefix(hash: &[u8; 32]) -> String {
    hash[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// The `Content-Type` of an asset, by extension.
pub(crate) fn content_type(name: &str) -> &'static str {
    match name.rsplit_once('.').map(|(_, ext)| ext) {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// `GET /assets/{*path}`.
pub(crate) async fn serve(
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let file = Assets::get(&name).ok_or(AppError::NotFound)?;
    let version = hex_prefix(&file.metadata.sha256_hash());
    let etag = format!("\"{version}\"");
    let current = query
        .as_deref()
        .is_some_and(|q| q.split('&').any(|pair| pair == format!("v={version}")));
    let cache_control = if current { IMMUTABLE } else { "no-cache" };
    let response_headers = [
        (
            header::ETAG,
            HeaderValue::from_str(&etag).expect("hex is a valid header"),
        ),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        ),
    ];

    let not_modified = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(',')
                .map(str::trim)
                .any(|tag| tag == "*" || tag.strip_prefix("W/").unwrap_or(tag) == etag)
        });
    if not_modified {
        return Ok((StatusCode::NOT_MODIFIED, response_headers).into_response());
    }
    Ok((
        response_headers,
        [(header::CONTENT_TYPE, content_type(&name))],
        file.data,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_assets_are_embedded() {
        for name in [
            "style.css",
            "app.js",
            "theme.js",
            "icon.svg",
            "reader.css",
            "reader.js",
        ] {
            assert!(Assets::get(name).is_some(), "{name}");
            assert_eq!(version(name).unwrap().len(), 16);
        }
        assert_eq!(version("missing.css"), None);
        assert_eq!(version("../Cargo.toml"), None);
    }

    #[test]
    fn hub_files_are_not_library_assets() {
        assert!(Assets::get("hub/shell.html").is_none());
        assert!(Assets::get("hub/hub.css").is_none());
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type("style.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("icon.svg"), "image/svg+xml");
        assert_eq!(content_type("README"), "application/octet-stream");
    }
}
