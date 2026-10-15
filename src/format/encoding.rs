//! Decoding the raw bytes of a book to UTF-8 text.
//!
//! The steps every format shares, in this order:
//!
//! 1. A byte order mark (UTF-8, UTF-16 LE/BE) wins; it is removed from the text.
//! 2. (Mostly) valid UTF-8 → UTF-8.
//! 3. Otherwise the legacy encoding guessed by `chardetng` (which never guesses UTF-8
//!    here). Cyrillic code pages are recognized: windows-1251, IBM866 and KOI8 (reported
//!    as KOI8-U, which decodes Russian text like KOI8-R).
//!
//! [`decode_text`] runs them for plain text, with one more step between 1 and 2: UTF-16
//! without a BOM, recognized by its line breaks and spaces. Formats with an in-band
//! declaration (the XML declaration of FB2, see
//! [`fb2::encoding`](super::fb2::encoding); `<meta charset>` of HTML) use it between steps 1 and 2, but do not trust it blindly:
//!
//! - declared UTF-8 but not (mostly) valid UTF-8 → treated as undeclared;
//! - declared another encoding but the bytes are (mostly) valid UTF-8 *and* contain
//!   non-ASCII bytes → UTF-8 (valid multi-byte UTF-8 is practically never
//!   legacy-encoded text; saved pages re-encoded to UTF-8 often keep their old label);
//! - a label that is unknown or names an encoding that cannot be declared in ASCII
//!   (UTF-16) → treated as undeclared.
//!
//! "Mostly valid UTF-8" means valid multi-byte characters outnumber invalid sequences
//! at least [`MOSTLY_UTF8_RATIO`] to 1: a UTF-8 book with a few stray bytes is decoded
//! as UTF-8 with replacements instead of being misread entirely as a legacy encoding.
//! Legacy-encoded Cyrillic text consists almost entirely of invalid sequences.
//!
//! Decoding never fails: malformed sequences become U+FFFD and are reported in
//! [`Decoded::had_errors`] so the caller can log or reject the book.

use std::borrow::Cow;

use chardetng::{EncodingDetector, Iso2022JpDetection, Utf8Detection};
use encoding_rs::{Encoding, UTF_8, UTF_16BE, UTF_16LE};

/// Minimum ratio of valid multi-byte UTF-8 characters to invalid sequences for bytes
/// to be decoded as UTF-8 despite errors.
pub const MOSTLY_UTF8_RATIO: usize = 10;

/// The decoded text of a book and how its encoding was determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded<'a> {
    /// The text, without a BOM. An XML declaration is left as it was.
    pub text: Cow<'a, str>,
    /// The encoding used for decoding.
    pub encoding: &'static Encoding,
    /// How [`Decoded::encoding`] was chosen.
    pub detection: Detection,
    /// The `encoding` label of an XML declaration, if there was one (even if unused).
    pub declared_label: Option<String>,
    /// Whether malformed input was replaced with U+FFFD.
    pub had_errors: bool,
}

/// How the encoding of a file was determined.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detection {
    /// From a byte order mark.
    Bom,
    /// UTF-16 without BOM, recognized by its first characters (FB2) or by its line
    /// breaks and spaces (plain text).
    Utf16Pattern,
    /// From an in-band declaration (the XML declaration, `<meta charset>`).
    Declaration,
    /// The bytes are valid UTF-8 (no usable declaration, or one that was contradicted).
    ValidUtf8,
    /// The bytes are mostly valid UTF-8 and invalid sequences were replaced (no usable
    /// declaration, or one that was contradicted).
    MostlyUtf8,
    /// Guessed by the statistical detector.
    Guessed,
}

/// Decodes plain text: BOM, then UTF-16 without BOM, then (mostly) valid UTF-8, then
/// the detector (see the [module docs](self)).
///
/// # Examples
///
/// ```
/// use shelfgauge::format::encoding::{decode_text, Detection};
///
/// let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(
///     "Старый маяк стоял на краю скалистого берега. Каждый вечер смотритель \
///      поднимался по винтовой лестнице и зажигал лампу.",
/// );
/// let decoded = decode_text(&bytes);
/// assert_eq!(decoded.encoding, encoding_rs::WINDOWS_1251);
/// assert_eq!(decoded.detection, Detection::Guessed);
/// assert!(decoded.text.starts_with("Старый маяк"));
/// ```
pub fn decode_text(bytes: &[u8]) -> Decoded<'_> {
    if let Some(decoded) = decode_bom(bytes) {
        return decoded;
    }
    if let Some(encoding) = utf16_text(bytes) {
        return finish(encoding, bytes, Detection::Utf16Pattern, None);
    }
    decode_declared(bytes, None)
}

/// How many leading bytes [`utf16_text`] looks at.
const UTF16_SAMPLE: usize = 64 * 1024;

/// How many line break and space units [`utf16_text`] needs at least.
const UTF16_MIN_HITS: usize = 8;

/// UTF-16 plain text without a BOM, LE or BE.
///
/// Text in UTF-8 or a legacy encoding has no zero bytes, while UTF-16 text has a line
/// break or space unit (`0A 00`, `0D 00`, `20 00` in LE) every paragraph at least, even
/// in CJK text. The reverse units (U+0A00, U+0D00, U+2000) are practically never used,
/// so the byte order is the one whose units occur and whose reverse does not. In the
/// first [`UTF16_SAMPLE`] bytes:
///
/// - break and space units are at least 1 in 256 units and at least [`UTF16_MIN_HITS`]
///   (a book has many more; stray zero bytes in other text do not add up to that), and
///   the other order has at most an eighth of them;
/// - the units form valid UTF-16 (no lone surrogates) with few control characters.
pub(crate) fn utf16_text(bytes: &[u8]) -> Option<&'static Encoding> {
    let sample = &bytes[..bytes.len().min(UTF16_SAMPLE) & !1];
    let units = sample.len() / 2;
    let (mut le, mut be) = (0usize, 0usize);
    for pair in sample.as_chunks::<2>().0 {
        match pair {
            [b'\n' | b'\r' | b' ', 0] => le += 1,
            [0, b'\n' | b'\r' | b' '] => be += 1,
            _ => {}
        }
    }
    let (encoding, hits, other) = if le >= be {
        (UTF_16LE, le, be)
    } else {
        (UTF_16BE, be, le)
    };
    if hits < UTF16_MIN_HITS || hits < units / 256 || other > hits / 8 {
        return None;
    }
    let units = sample.as_chunks::<2>().0.iter().map(|&pair| {
        if encoding == UTF_16LE {
            u16::from_le_bytes(pair)
        } else {
            u16::from_be_bytes(pair)
        }
    });
    let mut controls = 0usize;
    let mut chars = char::decode_utf16(units).peekable();
    while let Some(c) = chars.next() {
        match c {
            // A pair cut by the end of the sample.
            Err(_) if chars.peek().is_none() && sample.len() < bytes.len() => {}
            Err(_) => return None,
            Ok('\t' | '\n' | '\r' | '\u{c}') => {}
            Ok(c) if c.is_control() => controls += 1,
            Ok(_) => {}
        }
    }
    (controls <= sample.len() / 2 / 100).then_some(encoding)
}

/// Decodes `bytes` (without a BOM) that declare the encoding `declared_label`, or
/// nothing, checking the label against the bytes (see the [module docs](self)).
pub(crate) fn decode_declared(bytes: &[u8], declared_label: Option<String>) -> Decoded<'_> {
    let declared = declared_label
        .as_deref()
        .and_then(|label| Encoding::for_label_no_replacement(label.as_bytes()))
        .filter(|encoding| encoding.is_ascii_compatible());
    let utf8 = Utf8Quality::of(bytes);
    let (encoding, detection) = match (declared, utf8) {
        (Some(e), Utf8Quality::Invalid) if e == UTF_8 => (guess(bytes), Detection::Guessed),
        (Some(e), _) if e == UTF_8 => (UTF_8, Detection::Declaration),
        (Some(_), Utf8Quality::Valid) if !bytes.is_ascii() => (UTF_8, Detection::ValidUtf8),
        (Some(_), Utf8Quality::Mostly) => (UTF_8, Detection::MostlyUtf8),
        (Some(encoding), _) => (encoding, Detection::Declaration),
        (None, Utf8Quality::Valid) => (UTF_8, Detection::ValidUtf8),
        (None, Utf8Quality::Mostly) => (UTF_8, Detection::MostlyUtf8),
        (None, Utf8Quality::Invalid) => (guess(bytes), Detection::Guessed),
    };
    finish(encoding, bytes, detection, declared_label)
}

/// Step 1: decodes `bytes` by their byte order mark, if they have one.
pub(crate) fn decode_bom(bytes: &[u8]) -> Option<Decoded<'_>> {
    let (encoding, bom_len) = Encoding::for_bom(bytes)?;
    Some(finish(encoding, &bytes[bom_len..], Detection::Bom, None))
}

/// How well bytes match UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Utf8Quality {
    /// Entirely valid UTF-8.
    Valid,
    /// Valid multi-byte characters outnumber invalid sequences by [`MOSTLY_UTF8_RATIO`].
    Mostly,
    /// Not UTF-8.
    Invalid,
}

impl Utf8Quality {
    pub(crate) fn of(bytes: &[u8]) -> Self {
        let mut multibyte = 0usize;
        let mut invalid = 0usize;
        for chunk in bytes.utf8_chunks() {
            multibyte += chunk.valid().chars().filter(|c| !c.is_ascii()).count();
            invalid += usize::from(!chunk.invalid().is_empty());
        }
        if invalid == 0 {
            Self::Valid
        } else if multibyte >= MOSTLY_UTF8_RATIO * invalid {
            Self::Mostly
        } else {
            Self::Invalid
        }
    }
}

/// Decodes `bytes` (without BOM) with `encoding` and assembles the result.
pub(crate) fn finish<'a>(
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

/// The legacy encoding `chardetng` considers most likely for `bytes`.
pub(crate) fn guess(bytes: &[u8]) -> &'static Encoding {
    let mut detector = EncodingDetector::new(Iso2022JpDetection::Deny);
    detector.feed(bytes, true);
    detector.guess(None, Utf8Detection::Deny)
}

#[cfg(test)]
mod tests {
    use super::*;
    use encoding_rs::{IBM866, KOI8_R, KOI8_U, WINDOWS_1251, WINDOWS_1252};

    /// License-free Russian sample text, long enough for the statistical detector.
    pub(crate) const RUSSIAN: &str = "Старый маяк стоял на краю скалистого берега. Каждый вечер \
        смотритель поднимался по винтовой лестнице, зажигал лампу и долго смотрел на \
        тёмное море, где проходили редкие корабли. Зимой ветер гудел в щелях, а летом \
        над водой кружили чайки, и тогда казалось, что время остановилось.";

    fn encode(encoding: &'static Encoding, text: &str) -> Vec<u8> {
        let (bytes, _, unmappable) = encoding.encode(text);
        assert!(!unmappable, "not representable in {}", encoding.name());
        bytes.into_owned()
    }

    #[test]
    fn text_with_bom() {
        let mut bytes = vec![0xef, 0xbb, 0xbf];
        bytes.extend_from_slice(RUSSIAN.as_bytes());
        let decoded = decode_text(&bytes);
        assert_eq!(
            (decoded.encoding, decoded.detection),
            (UTF_8, Detection::Bom)
        );
        assert_eq!(decoded.text, RUSSIAN);
        for (bom, encoding) in [([0xff, 0xfe], UTF_16LE), ([0xfe, 0xff], UTF_16BE)] {
            let mut bytes = bom.to_vec();
            for unit in "Привет".encode_utf16() {
                let pair = if encoding == UTF_16LE {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                bytes.extend_from_slice(&pair);
            }
            let decoded = decode_text(&bytes);
            assert_eq!(decoded.encoding, encoding);
            assert_eq!(decoded.text, "Привет");
        }
    }

    #[test]
    fn text_in_utf8() {
        let decoded = decode_text(RUSSIAN.as_bytes());
        assert_eq!(decoded.detection, Detection::ValidUtf8);
        assert!(matches!(decoded.text, Cow::Borrowed(_)));
        let mut bytes = RUSSIAN.as_bytes().to_vec();
        bytes.insert(1, 0xc8);
        let decoded = decode_text(&bytes);
        assert_eq!(
            (decoded.encoding, decoded.detection),
            (UTF_8, Detection::MostlyUtf8)
        );
        assert!(decoded.had_errors);
    }

    #[test]
    fn cyrillic_code_pages_are_guessed() {
        for encoding in [WINDOWS_1251, IBM866, KOI8_R] {
            let bytes = encode(encoding, RUSSIAN);
            let decoded = decode_text(&bytes);
            assert_eq!(decoded.detection, Detection::Guessed);
            // chardetng reports the KOI8-U superset for KOI8-R text.
            let expected: &[&Encoding] = if encoding == KOI8_R {
                &[KOI8_R, KOI8_U]
            } else {
                &[encoding]
            };
            assert!(
                expected.contains(&decoded.encoding),
                "{}",
                decoded.encoding.name()
            );
            assert_eq!(decoded.text, RUSSIAN);
        }
        let text = "Le café était fermé, mais la boulangère vendait déjà ses brioches.";
        assert_eq!(decode_text(&encode(WINDOWS_1252, text)).text, text);
    }

    #[test]
    fn an_xml_declaration_is_not_used_for_text() {
        let text = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>{RUSSIAN}");
        let bytes = encode(WINDOWS_1251, &text);
        let decoded = decode_text(&bytes);
        assert_eq!(decoded.encoding, WINDOWS_1251);
        assert_eq!(decoded.declared_label, None);
    }

    fn utf16(text: &str, encoding: &'static Encoding) -> Vec<u8> {
        text.encode_utf16()
            .flat_map(|unit| {
                if encoding == UTF_16LE {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                }
            })
            .collect()
    }

    #[test]
    fn utf16_without_bom_is_recognized() {
        let german = "Der alte Leuchtturm stand am Rand der felsigen Küste.\r\n\
            Jeden Abend stieg der Wärter die Wendeltreppe hinauf.\r\n";
        // Paragraphs indented with U+3000 (bytes `00 30` in LE) and no ASCII spaces.
        let japanese = "\u{3000}古い灯台は岩だらけの海岸の端に立っていた。\n\
            \u{3000}毎晩、灯台守はらせん階段を上ってランプに火をともした。\n\
            \u{3000}一隻の船も通らない暗い海を長い間眺めていた。\n"
            .repeat(5);
        let emoji = "Короткая строка 🙂\n".repeat(1000);
        for text in [RUSSIAN, german, &japanese, &emoji] {
            for encoding in [UTF_16LE, UTF_16BE] {
                let bytes = utf16(text, encoding);
                let decoded = decode_text(&bytes);
                assert_eq!(
                    (decoded.encoding, decoded.detection),
                    (encoding, Detection::Utf16Pattern),
                    "{text:.20}"
                );
                assert_eq!(decoded.text, text);
                assert!(!decoded.had_errors);
            }
        }
    }

    #[test]
    fn utf16_sample_may_cut_a_surrogate_pair() {
        // The high surrogate is the last unit of the sample.
        let mut text = "a\n".repeat(UTF16_SAMPLE / 4 - 1);
        text.push('a');
        text.push('🙂');
        text.push('\n');
        let bytes = utf16(&text, UTF_16LE);
        assert!(bytes.len() > UTF16_SAMPLE);
        assert_eq!(utf16_text(&bytes), Some(UTF_16LE));
    }

    #[test]
    fn other_texts_are_not_utf16() {
        let mut stray_zeros = encode(WINDOWS_1251, RUSSIAN);
        stray_zeros.extend_from_slice(b"\n\0 \0");
        let binary: Vec<u8> = (0..1000u32)
            .flat_map(|i| [b' ', 0, (i % 7) as u8, 0])
            .collect();
        let ascii = utf16(&"ab\n".repeat(UTF16_MIN_HITS), UTF_16LE);
        let few = utf16(&"ab\n".repeat(UTF16_MIN_HITS - 1), UTF_16LE);
        let mut lone_surrogate = ascii.clone();
        lone_surrogate.extend_from_slice(&[0x00, 0xd8, b'x', 0]);
        for bytes in [
            RUSSIAN.as_bytes(),
            &encode(WINDOWS_1251, RUSSIAN),
            &stray_zeros,
            &binary,
            &lone_surrogate,
            &few,
            b"\n\0",
            b"\n",
        ] {
            assert_eq!(utf16_text(bytes), None, "{bytes:?}");
        }
        assert_eq!(utf16_text(&ascii), Some(UTF_16LE));
    }

    #[test]
    fn utf8_quality() {
        assert_eq!(Utf8Quality::of(b"ascii"), Utf8Quality::Valid);
        let mut mostly = "аааааааааа".as_bytes().to_vec();
        mostly.push(0xff);
        assert_eq!(Utf8Quality::of(&mostly), Utf8Quality::Mostly);
        mostly.extend_from_slice(b" \xff");
        assert_eq!(Utf8Quality::of(&mostly), Utf8Quality::Invalid);
        assert_eq!(Utf8Quality::of(b"\xff\xfe plain"), Utf8Quality::Invalid);
        assert_eq!(
            Utf8Quality::of(&encode(WINDOWS_1251, RUSSIAN)),
            Utf8Quality::Invalid
        );
        assert_eq!(
            Utf8Quality::of(&encode(KOI8_R, RUSSIAN)),
            Utf8Quality::Invalid
        );
    }

    #[test]
    fn empty_and_tiny_inputs_do_not_panic() {
        for bytes in [&b""[..], b"\0", b"\xff", b"\xfe\xff", b"\xef\xbb"] {
            let _ = decode_text(bytes);
        }
        assert_eq!(decode_text(b"").text, "");
    }
}
