//! Paths inside the EPUB container: resolving relative URLs of the package and the
//! content documents to entry names.

use percent_encoding::percent_decode_str;

/// The directory part of an entry path, with a trailing `/` (empty at the root).
pub(super) fn directory(path: &str) -> &str {
    path.rfind('/').map_or("", |i| &path[..=i])
}

/// Splits an URL into the entry path it points to (resolved against the entry `base`)
/// and its fragment; `None` for an URL with a scheme (external) or one that leaves the
/// container.
///
/// The path is percent-decoded; a query is dropped. An URL with only a fragment points
/// into `base` itself.
pub(super) fn resolve(base: &str, url: &str) -> Option<(String, Option<String>)> {
    let url = url.trim();
    if has_scheme(url) || url.starts_with("//") {
        return None;
    }
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, Some(fragment)),
        None => (url, None),
    };
    let rest = rest.split_once('?').map_or(rest, |(path, _)| path);
    let fragment = fragment
        .map(|f| percent_decode_str(f).decode_utf8_lossy().into_owned())
        .filter(|f| !f.is_empty());
    if rest.is_empty() {
        return Some((base.to_owned(), fragment));
    }
    let decoded = percent_decode_str(rest).decode_utf8_lossy();
    let joined = if let Some(absolute) = decoded.strip_prefix('/') {
        absolute.to_owned()
    } else {
        format!("{}{decoded}", directory(base))
    };
    Some((normalize(&joined)?, fragment))
}

/// Whether `url` starts with a scheme (`http:`, `mailto:`, ...).
pub(super) fn has_scheme(url: &str) -> bool {
    match url.split_once(':') {
        Some((scheme, _)) => {
            let mut chars = scheme.chars();
            chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

/// Removes `.` and `..` segments and empty segments; `None` if `..` leaves the root.
fn normalize(path: &str) -> Option<String> {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split(['/', '\\']) {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop()?;
            }
            segment => segments.push(segment),
        }
    }
    Some(segments.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(base: &str, url: &str) -> Option<(String, Option<String>)> {
        resolve(base, url)
    }

    #[test]
    fn relative_urls_resolve_against_the_base() {
        let base = "OEBPS/text/ch1.xhtml";
        assert_eq!(
            r(base, "ch2.xhtml#n1"),
            Some(("OEBPS/text/ch2.xhtml".into(), Some("n1".into())))
        );
        assert_eq!(
            r(base, "../img/a%20b.png"),
            Some(("OEBPS/img/a b.png".into(), None))
        );
        assert_eq!(
            r(base, "#top"),
            Some(("OEBPS/text/ch1.xhtml".into(), Some("top".into())))
        );
        assert_eq!(
            r(base, "./x.xhtml?q=1#"),
            Some(("OEBPS/text/x.xhtml".into(), None))
        );
        assert_eq!(r(base, "/root.xhtml"), Some(("root.xhtml".into(), None)));
        assert_eq!(r("ch.xhtml", "ch2.xhtml"), Some(("ch2.xhtml".into(), None)));
    }

    #[test]
    fn external_and_escaping_urls_are_not_entries() {
        let base = "OEBPS/ch1.xhtml";
        assert_eq!(r(base, "https://example.org/x"), None);
        assert_eq!(r(base, "mailto:a@b"), None);
        assert_eq!(r(base, "//example.org/x"), None);
        assert_eq!(r(base, "../../x.xhtml"), None);
        assert!(has_scheme("javascript:alert(1)"));
        assert!(!has_scheme("ch1.xhtml"));
        assert!(!has_scheme("1:2"));
    }
}
