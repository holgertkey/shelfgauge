//! Addresses of hub accounts, `name@handle`: the `name` of a person in a library and
//! the library's `handle` on the hub. Checked alike by the hub, the library and the
//! hub's reader app (through `shelfgauge-wasm`).
//!
//! Both parts follow one format, chosen for easy typing at sign-in: ASCII `a–z`,
//! `0–9`, `.`, `_` and `-`, 3 to 32 characters, the first and last a letter or a
//! digit; letters are compared in lower case and spaces at the ends are trimmed.
//! Other scripts belong in the display name, which the library keeps.

/// The fewest characters of a name or handle.
pub const MIN_LABEL_LEN: usize = 3;

/// The most characters of a name or handle.
pub const MAX_LABEL_LEN: usize = 32;

/// A name or handle in its stored form (trimmed, lower case), or `None` if it breaks
/// the format.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::address::normalize_label;
///
/// assert_eq!(normalize_label(" Olga.S ").as_deref(), Some("olga.s"));
/// assert_eq!(normalize_label("ab"), None);
/// assert_eq!(normalize_label("-olga"), None);
/// assert_eq!(normalize_label("ольга"), None);
/// ```
pub fn normalize_label(text: &str) -> Option<String> {
    let label = text.trim().to_ascii_lowercase();
    let bytes = label.as_bytes();
    let valid = (MIN_LABEL_LEN..=MAX_LABEL_LEN).contains(&bytes.len())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b))
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric();
    valid.then_some(label)
}

/// An account address `name@handle` split into its normalized parts, or `None` if it
/// is not one.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::address::parse_address;
///
/// let (name, handle) = parse_address(" Olga@Smiths ").unwrap();
/// assert_eq!((name.as_str(), handle.as_str()), ("olga", "smiths"));
/// assert_eq!(parse_address("olga"), None);
/// assert_eq!(parse_address("olga@smiths@x"), None);
/// ```
pub fn parse_address(text: &str) -> Option<(String, String)> {
    let (name, handle) = text.trim().split_once('@')?;
    Some((normalize_label(name)?, normalize_label(handle)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_follow_the_format() {
        for ok in ["abc", "a.b", "a_b-c", "0lga", &"x".repeat(32), "ABC"] {
            assert!(normalize_label(ok).is_some(), "{ok}");
        }
        for bad in [
            "",
            "ab",
            &"x".repeat(33),
            "a b",
            ".ab",
            "ab_",
            "a@b",
            "ёлка",
            "a/b",
        ] {
            assert_eq!(normalize_label(bad), None, "{bad}");
        }
        assert_eq!(normalize_label("  MiXeD  ").as_deref(), Some("mixed"));
    }

    #[test]
    fn addresses_have_one_at_sign() {
        assert_eq!(
            parse_address("olga@smiths"),
            Some(("olga".to_owned(), "smiths".to_owned()))
        );
        for bad in [
            "@smiths",
            "olga@",
            "ol@smiths",
            "olga@sm",
            "a@b@c",
            "olga smiths",
        ] {
            assert_eq!(parse_address(bad), None, "{bad}");
        }
    }
}
