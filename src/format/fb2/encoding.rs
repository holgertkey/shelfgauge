//! Decoding FB2 bytes to UTF-8 text before XML parsing.
//!
//! `roxmltree` parses only `&str` and ignores the `encoding` attribute of the XML
//! declaration, so the raw bytes are decoded first, in this order:
//!
//! 1. A byte order mark (UTF-8, UTF-16 LE/BE) wins; it is removed from the text.
//! 2. UTF-16 without a BOM is recognized by the `<?` pattern of its first two characters.
//! 3. The `encoding="..."` attribute of the XML declaration, resolved with
//!    [`Encoding::for_label_no_replacement`]. Declarations are known to lie, so:
//!    - declared UTF-8 but not valid UTF-8 → treated as undeclared (step 4);
//!    - declared another encoding but the bytes are valid UTF-8 *and* contain non-ASCII
//!      bytes → UTF-8 (valid multi-byte UTF-8 is practically never legacy-encoded text);
//!    - a label that is unknown or names an encoding that cannot be read through an
//!      ASCII declaration (UTF-16) → treated as undeclared.
//! 4. No usable declaration: valid UTF-8 → UTF-8, otherwise the encoding guessed by
//!    `chardetng` (which never guesses UTF-8 here).
//!
//! Decoding never fails: malformed sequences become U+FFFD and are reported in
//! [`Decoded::had_errors`] so the caller can log or reject the book.

use std::borrow::Cow;

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};

/// How many leading bytes are searched for the XML declaration.
const DECLARATION_SEARCH_LIMIT: usize = 1024;

/// The decoded text of an FB2 file and how its encoding was determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded<'a> {
    /// The text, without a BOM. The XML declaration is left as it was.
    pub text: Cow<'a, str>,
    /// The encoding used for decoding.
    pub encoding: &'static Encoding,
    /// How [`Decoded::encoding`] was chosen.
    pub detection: Detection,
    /// The `encoding` label of the XML declaration, if there was one (even if unused).
    pub declared_label: Option<String>,
    /// Whether malformed input was replaced with U+FFFD.
    pub had_errors: bool,
}

/// How the encoding of a file was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// From a byte order mark.
    Bom,
    /// UTF-16 without BOM, recognized by its first characters.
    Utf16Pattern,
    /// From the XML declaration.
    Declaration,
    /// The bytes are valid UTF-8 (no usable declaration, or one that was contradicted).
    ValidUtf8,
    /// Guessed by the statistical detector.
    Guessed,
}

/// Decodes the raw bytes of an FB2 file to text (see the [module docs](self)).
///
/// # Examples
///
/// ```
/// use shelfgauge::format::fb2::encoding::{decode, Detection};
///
/// let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(
///     "<?xml version=\"1.0\" encoding=\"windows-1251\"?><p>Привет</p>",
/// );
/// let decoded = decode(&bytes);
/// assert_eq!(decoded.encoding, encoding_rs::WINDOWS_1251);
/// assert_eq!(decoded.detection, Detection::Declaration);
/// assert!(decoded.text.contains("Привет"));
/// ```
pub fn decode(bytes: &[u8]) -> Decoded<'_> {
    if let Some((encoding, bom_len)) = Encoding::for_bom(bytes) {
        return finish(encoding, &bytes[bom_len..], Detection::Bom, None);
    }
    if let Some(encoding) = utf16_pattern(bytes) {
        return finish(encoding, bytes, Detection::Utf16Pattern, None);
    }

    let declared_label = declared_label(bytes).map(|l| String::from_utf8_lossy(l).into_owned());
    let declared = declared_label
        .as_deref()
        .and_then(|label| Encoding::for_label_no_replacement(label.as_bytes()))
        .filter(|encoding| encoding.is_ascii_compatible());
    let valid_utf8 = std::str::from_utf8(bytes).is_ok();

    let (encoding, detection) = match declared {
        Some(encoding) if encoding == UTF_8 && valid_utf8 => (UTF_8, Detection::Declaration),
        Some(encoding) if encoding == UTF_8 => (guess(bytes), Detection::Guessed),
        Some(_) if valid_utf8 && !bytes.is_ascii() => (UTF_8, Detection::ValidUtf8),
        Some(encoding) => (encoding, Detection::Declaration),
        None if valid_utf8 => (UTF_8, Detection::ValidUtf8),
        None => (guess(bytes), Detection::Guessed),
    };
    finish(encoding, bytes, detection, declared_label)
}

/// Decodes `bytes` (without BOM) with `encoding` and assembles the result.
fn finish<'a>(
    encoding: &'static Encoding,
    bytes: &'a [u8],
    detection: Detection,
    declared_label: Option<String>,
) -> Decoded<'a> {
    let (text, had_errors) = encoding.decode_without_bom_handling(bytes);
    Decoded {
        text,
        encoding,
        detection,
        declared_label,
        had_errors,
    }
}

/// UTF-16 without BOM: the file starts with `<?` (or `<` plus another ASCII character)
/// encoded as two-byte units.
fn utf16_pattern(bytes: &[u8]) -> Option<&'static Encoding> {
    match bytes {
        [b'<', 0, c, 0, ..] if c.is_ascii() && *c != 0 => Some(UTF_16LE),
        [0, b'<', 0, c, ..] if c.is_ascii() && *c != 0 => Some(UTF_16BE),
        _ => None,
    }
}

/// The raw `encoding` label of the XML declaration at the start of `bytes`, if any.
fn declared_label(bytes: &[u8]) -> Option<&[u8]> {
    let head = &bytes[..bytes.len().min(DECLARATION_SEARCH_LIMIT)];
    let start = head.iter().position(|b| !b.is_ascii_whitespace())?;
    let decl = head[start..].strip_prefix(b"<?xml")?;
    if !decl.first()?.is_ascii_whitespace() {
        return None;
    }
    let decl = &decl[..find(decl, b"?>")?];
    let after_name = &decl[find(decl, b"encoding")? + b"encoding".len()..];
    let rest = after_name
        .trim_ascii_start()
        .strip_prefix(b"=")?
        .trim_ascii_start();
    let (&quote, rest) = rest.split_first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let label = &rest[..rest.iter().position(|&b| b == quote)?];
    (!label.trim_ascii().is_empty()).then_some(label.trim_ascii())
}

/// The position of the first occurrence of `needle` in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// The legacy encoding `chardetng` considers most likely for `bytes`.
fn guess(bytes: &[u8]) -> &'static Encoding {
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    detector.guess(None, Utf8Detection::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::{IBM866, KOI8_R, KOI8_U, WINDOWS_1251, WINDOWS_1252};

    /// License-free Russian sample text, long enough for the statistical detector.
    const RUSSIAN: &str = "Старый маяк стоял на краю скалистого берега. Каждый вечер \
        смотритель поднимался по винтовой лестнице, зажигал лампу и долго смотрел на \
        тёмное море, где проходили редкие корабли. Зимой ветер гудел в щелях, а летом \
        над водой кружили чайки, и тогда казалось, что время остановилось.";

    fn book(declaration: &str) -> String {
        format!("{declaration}<FictionBook><body><p>{RUSSIAN}</p></body></FictionBook>")
    }

    fn encode(encoding: &'static Encoding, text: &str) -> Vec<u8> {
        let (bytes, _, unmappable) = encoding.encode(text);
        assert!(
            !unmappable,
            "sample not representable in {}",
            encoding.name()
        );
        bytes.into_owned()
    }

    fn utf16(text: &str, little_endian: bool, bom: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if bom {
            out.extend_from_slice(if little_endian {
                &[0xff, 0xfe]
            } else {
                &[0xfe, 0xff]
            });
        }
        for unit in text.encode_utf16() {
            let bytes = if little_endian {
                unit.to_le_bytes()
            } else {
                unit.to_be_bytes()
            };
            out.extend_from_slice(&bytes);
        }
        out
    }

    /// [`decode`] for a temporary buffer.
    fn decode_owned(bytes: &[u8]) -> Decoded<'static> {
        let decoded = decode(bytes);
        Decoded {
            text: Cow::Owned(decoded.text.into_owned()),
            ..decoded
        }
    }

    fn assert_decoded(decoded: &Decoded<'_>, encoding: &'static Encoding, detection: Detection) {
        assert_eq!(
            (decoded.encoding.name(), decoded.detection),
            (encoding.name(), detection)
        );
        assert!(decoded.text.contains(RUSSIAN), "{}", decoded.text);
        assert!(!decoded.had_errors);
    }

    #[test]
    fn utf8_with_declaration() {
        let text = book(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        let decoded = decode(text.as_bytes());
        assert_decoded(&decoded, UTF_8, Detection::Declaration);
        assert_eq!(decoded.declared_label.as_deref(), Some("UTF-8"));
        assert!(
            matches!(decoded.text, Cow::Borrowed(_)),
            "no copy for valid UTF-8"
        );
    }

    #[test]
    fn utf8_with_bom() {
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice(book(r#"<?xml version="1.0"?>"#).as_bytes());
        let decoded = decode(&bytes);
        assert_decoded(&decoded, UTF_8, Detection::Bom);
        assert!(decoded.text.starts_with("<?xml"), "BOM must be removed");
    }

    #[test]
    fn utf16_with_bom() {
        let text = book(r#"<?xml version="1.0" encoding="UTF-16"?>"#);
        for little_endian in [true, false] {
            let decoded = decode_owned(&utf16(&text, little_endian, true));
            let expected = if little_endian { UTF_16LE } else { UTF_16BE };
            assert_decoded(&decoded, expected, Detection::Bom);
            assert!(decoded.text.starts_with("<?xml"));
        }
    }

    #[test]
    fn utf16_without_bom() {
        let text = book(r#"<?xml version="1.0" encoding="UTF-16"?>"#);
        assert_decoded(
            &decode_owned(&utf16(&text, true, false)),
            UTF_16LE,
            Detection::Utf16Pattern,
        );
        assert_decoded(
            &decode_owned(&utf16(&text, false, false)),
            UTF_16BE,
            Detection::Utf16Pattern,
        );
    }

    #[test]
    fn legacy_encodings_from_declaration() {
        for (label, encoding) in [
            ("windows-1251", WINDOWS_1251),
            ("cp1251", WINDOWS_1251),
            ("KOI8-R", KOI8_R),
            ("ibm866", IBM866),
        ] {
            let text = book(&format!(r#"<?xml version="1.0" encoding='{label}'?>"#));
            let decoded = decode_owned(&encode(encoding, &text));
            assert_decoded(&decoded, encoding, Detection::Declaration);
            assert_eq!(decoded.declared_label.as_deref(), Some(label));
        }
    }

    #[test]
    fn no_declaration_valid_utf8() {
        assert_decoded(&decode(book("").as_bytes()), UTF_8, Detection::ValidUtf8);
    }

    #[test]
    fn no_declaration_legacy_bytes_are_guessed() {
        let decoded = decode_owned(&encode(WINDOWS_1251, &book("")));
        assert_decoded(&decoded, WINDOWS_1251, Detection::Guessed);

        // chardetng reports the KOI8-U superset for KOI8-R text; both decode Russian
        // text identically.
        let decoded = decode_owned(&encode(KOI8_R, &book("")));
        assert!(
            [KOI8_R, KOI8_U].contains(&decoded.encoding),
            "{}",
            decoded.encoding.name()
        );
        assert_eq!(decoded.detection, Detection::Guessed);
        assert!(decoded.text.contains(RUSSIAN));
    }

    #[test]
    fn unknown_label_falls_back() {
        let text = book(r#"<?xml version="1.0" encoding="no-such-encoding"?>"#);
        let decoded = decode_owned(&encode(WINDOWS_1251, &text));
        assert_decoded(&decoded, WINDOWS_1251, Detection::Guessed);
        assert_eq!(decoded.declared_label.as_deref(), Some("no-such-encoding"));
        assert_decoded(&decode(text.as_bytes()), UTF_8, Detection::ValidUtf8);
    }

    #[test]
    fn replacement_and_utf16_labels_are_not_trusted() {
        for label in ["replacement", "iso-2022-kr", "utf-16", "utf-16be"] {
            let text = book(&format!(r#"<?xml version="1.0" encoding="{label}"?>"#));
            assert_decoded(&decode(text.as_bytes()), UTF_8, Detection::ValidUtf8);
            let decoded = decode_owned(&encode(WINDOWS_1251, &text));
            assert_decoded(&decoded, WINDOWS_1251, Detection::Guessed);
        }
    }

    #[test]
    fn declaration_says_utf8_but_bytes_are_cp1251() {
        let text = book(r#"<?xml version="1.0" encoding="utf-8"?>"#);
        let decoded = decode_owned(&encode(WINDOWS_1251, &text));
        assert_decoded(&decoded, WINDOWS_1251, Detection::Guessed);
        assert_eq!(decoded.declared_label.as_deref(), Some("utf-8"));
    }

    #[test]
    fn declaration_says_cp1251_but_bytes_are_utf8() {
        let text = book(r#"<?xml version="1.0" encoding="windows-1251"?>"#);
        assert_decoded(&decode(text.as_bytes()), UTF_8, Detection::ValidUtf8);
    }

    #[test]
    fn pure_ascii_follows_the_declaration() {
        let text = r#"<?xml version="1.0" encoding="windows-1251"?><p>plain</p>"#;
        let decoded = decode(text.as_bytes());
        assert_eq!(decoded.encoding, WINDOWS_1251);
        assert_eq!(decoded.detection, Detection::Declaration);
        assert_eq!(decoded.text, text);
    }

    #[test]
    fn malformed_utf8_from_bom_is_reported() {
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice(b"<p>caf\xe9</p>");
        let decoded = decode(&bytes);
        assert_eq!(decoded.encoding, UTF_8);
        assert!(decoded.had_errors);
        assert_eq!(decoded.text, "<p>caf\u{fffd}</p>");
    }

    #[test]
    fn western_legacy_text_is_guessed() {
        let text = "<p>Le café était fermé, mais la boulangère vendait déjà ses brioches \
                    près de l'église. Ça sentait le pain chaud et les crêpes.</p>";
        let decoded = decode_owned(&encode(WINDOWS_1252, text));
        assert_eq!(decoded.encoding, WINDOWS_1252);
        assert_eq!(decoded.text, text);
    }

    #[test]
    fn empty_and_tiny_inputs_do_not_panic() {
        for bytes in [
            &b""[..],
            b"<",
            b"<\0",
            b"\0<",
            b"\xff",
            b"\xfe\xff",
            b"<?xml",
            b"<?xml ?>",
        ] {
            let _ = decode(bytes);
        }
        assert_eq!(decode(b"").text, "");
    }

    #[test]
    fn declaration_label_parsing() {
        let label =
            |s: &str| declared_label(s.as_bytes()).map(|l| String::from_utf8_lossy(l).into_owned());
        assert_eq!(
            label(r#"<?xml version="1.0" encoding="cp1251"?>"#).as_deref(),
            Some("cp1251")
        );
        assert_eq!(
            label("<?xml version='1.0' encoding = 'koi8-r' ?>").as_deref(),
            Some("koi8-r")
        );
        assert_eq!(
            label("\r\n  <?xml encoding=\" utf-8 \"?>").as_deref(),
            Some("utf-8")
        );
        assert_eq!(
            label(r#"<?xml version="1.0"?><p encoding="cp1251"/>"#),
            None
        );
        assert_eq!(label(r#"<?xmlfoo encoding="cp1251"?>"#), None);
        assert_eq!(label(r#"<?xml encoding="cp1251"#), None);
        assert_eq!(label(r#"<?xml encoding=cp1251?>"#), None);
        assert_eq!(label(r#"<?xml encoding=""?>"#), None);
        assert_eq!(label(r#"<p/><?xml encoding="cp1251"?>"#), None);
        let far = format!(
            "<?xml version=\"1.0\"{} encoding=\"cp1251\"?>",
            " ".repeat(2000)
        );
        assert_eq!(label(&far), None, "declaration beyond the search limit");
    }

    #[test]
    fn roxmltree_ignores_the_declared_encoding_after_decoding() {
        let text = book(r#"<?xml version="1.0" encoding="windows-1251"?>"#);
        let bytes = encode(WINDOWS_1251, &text);
        let decoded = decode(&bytes);
        let doc = roxmltree::Document::parse(&decoded.text).unwrap();
        let p = doc.descendants().find(|n| n.has_tag_name("p")).unwrap();
        assert_eq!(p.text(), Some(RUSSIAN));
    }
}
