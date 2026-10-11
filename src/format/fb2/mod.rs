//! The FB2 (FictionBook 2) format.
//!
//! Parsing is defensive: element names are matched by local name (files with missing or
//! wrong namespaces are common), unknown elements keep their text, and no input makes
//! the parser panic. Only input that is not XML, or not FictionBook, is an error.

use base64::Engine as _;
use base64::engine::general_purpose::{GeneralPurpose, PAD_INDIFFERENT};
use roxmltree::{Document, Node, ParsingOptions};

use super::model::{BookContent, BookMetadata, Image};
use super::{BookParser, ParseError};

mod content;
pub mod encoding;
mod metadata;

/// Maximum number of XML nodes in one file; protects against memory exhaustion.
///
/// A large real book (10 MB of text) has a few hundred thousand nodes.
const NODES_LIMIT: u32 = 4_000_000;

/// Maximum element nesting converted structurally; deeper content is kept as plain text.
const MAX_DEPTH: usize = 64;

/// Maximum element nesting accepted at all.
///
/// `roxmltree`'s tokenizer recurses once per nesting level without a limit (about 10 KB
/// of stack per level in debug builds), and a stack overflow aborts the whole process
/// (it cannot be caught), so deeper documents are rejected before parsing. Real FB2
/// files nest a few dozen levels at most. Together with [`MAX_DEPTH`] this keeps parsing
/// within [`PARSER_STACK_SIZE`](crate::format::PARSER_STACK_SIZE).
const MAX_XML_DEPTH: usize = 256;

/// Lenient base64: padding optional, non-zero trailing bits accepted.
const BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    PAD_INDIFFERENT.with_decode_allow_trailing_bits(true),
);

/// The FB2 parser.
#[derive(Debug, Clone, Copy, Default)]
pub struct Fb2Parser;

impl BookParser for Fb2Parser {
    fn format(&self) -> &'static str {
        "fb2"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["fb2"]
    }

    /// Parses `<description>/<title-info>` and the cover image; bodies are not converted.
    ///
    /// A missing title is returned as an empty string (the scanner falls back to the
    /// file name); missing authors as an empty list.
    fn parse_metadata(&self, data: &[u8]) -> Result<BookMetadata, ParseError> {
        with_root(data, metadata::parse)
    }

    /// Parses all bodies (the first is the main text, the others notes) and images.
    fn parse_content(&self, data: &[u8]) -> Result<BookContent, ParseError> {
        with_root(data, content::parse)
    }
}

/// Decodes and parses `data`, then calls `f` with the `<FictionBook>` root element.
fn with_root<T>(data: &[u8], f: impl FnOnce(Node<'_, '_>) -> T) -> Result<T, ParseError> {
    let decoded = encoding::decode(data);
    if decoded.had_errors {
        tracing::warn!(
            encoding = decoded.encoding.name(),
            "malformed byte sequences replaced while decoding"
        );
    }
    if nesting_exceeds(&decoded.text, MAX_XML_DEPTH) {
        return Err(ParseError::new(format!(
            "elements are nested more than {MAX_XML_DEPTH} levels deep"
        )));
    }
    let options = ParsingOptions {
        allow_dtd: false,
        nodes_limit: NODES_LIMIT,
        ..ParsingOptions::default()
    };
    let doc = Document::parse_with_options(&decoded.text, options)
        .map_err(|e| ParseError::new(format!("invalid XML: {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() != "FictionBook" {
        return Err(ParseError::new(format!(
            "not an FB2 document: the root element is <{}>, expected <FictionBook>",
            root.tag_name().name()
        )));
    }
    Ok(f(root))
}

/// Whether element nesting in `xml` exceeds `limit`, by a quick scan of the markup.
///
/// Comments, CDATA sections, processing instructions, declarations and quoted attribute
/// values are skipped; malformed markup is left to the XML parser to report.
fn nesting_exceeds(xml: &str, limit: usize) -> bool {
    let bytes = xml.as_bytes();
    let skip_past = |from: usize, end: &[u8]| {
        bytes[from..]
            .windows(end.len())
            .position(|w| w == end)
            .map_or(bytes.len(), |p| from + p + end.len())
    };
    let mut depth = 0usize;
    let mut i = 0;
    while let Some(offset) = bytes[i..].iter().position(|&b| b == b'<') {
        i += offset;
        let rest = &bytes[i..];
        if rest.starts_with(b"<!--") {
            i = skip_past(i + 4, b"-->");
        } else if rest.starts_with(b"<![CDATA[") {
            i = skip_past(i + 9, b"]]>");
        } else if rest.starts_with(b"<?") {
            i = skip_past(i + 2, b"?>");
        } else if rest.starts_with(b"<!") || rest.starts_with(b"</") {
            depth = depth.saturating_sub(usize::from(rest.starts_with(b"</")));
            i = skip_past(i + 2, b">");
        } else {
            // A start tag: find its end outside quoted attribute values.
            let mut quote = None;
            let mut j = i + 1;
            while j < bytes.len() {
                match (quote, bytes[j]) {
                    (None, b'"' | b'\'') => quote = Some(bytes[j]),
                    (Some(q), b) if b == q => quote = None,
                    (None, b'>') => break,
                    _ => {}
                }
                j += 1;
            }
            if j < bytes.len() && bytes[j - 1] != b'/' {
                depth += 1;
                if depth > limit {
                    return true;
                }
            }
            i = j;
        }
        if i >= bytes.len() {
            break;
        }
    }
    false
}

/// Whether `node` is an element with the local name `name` (namespace ignored).
fn is(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name() == name
}

/// The child elements of `node` with the local name `name`.
fn children<'a, 'input>(
    node: Node<'a, 'input>,
    name: &'static str,
) -> impl Iterator<Item = Node<'a, 'input>> {
    node.children().filter(move |child| is(*child, name))
}

/// The first child element of `node` with the local name `name`.
fn child<'a, 'input>(node: Node<'a, 'input>, name: &'static str) -> Option<Node<'a, 'input>> {
    children(node, name).next()
}

/// The value of the attribute with local name `name` in any namespace (FB2 uses both
/// `xlink:href` and `l:href`, and broken files use a bare `href`).
fn attr<'a>(node: Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.attributes()
        .find(|attribute| attribute.name() == name)
        .map(|attribute| attribute.value())
}

/// The trimmed, whitespace-normalized value of a non-empty attribute.
fn attr_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    attr(node, name)
        .map(normalize_space)
        .filter(|s| !s.is_empty())
}

/// All text of `node` and its descendants, whitespace-normalized and trimmed.
fn text_content(node: Node<'_, '_>) -> String {
    let mut text = String::new();
    for descendant in node.descendants().filter(Node::is_text) {
        text.push_str(descendant.text().unwrap_or_default());
        text.push(' ');
    }
    normalize_space(&text)
}

/// Collapses runs of XML whitespace into single spaces and trims the result.
fn normalize_space(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split(is_xml_space).filter(|w| !w.is_empty()) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// XML whitespace (space, tab, CR, LF); other Unicode spaces such as NBSP are content.
fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// The `<binary>` elements of the document.
fn binaries<'a, 'input>(root: Node<'a, 'input>) -> impl Iterator<Item = Node<'a, 'input>> {
    children(root, "binary")
}

/// Decodes a `<binary>` element; `None` (logged) if its base64 data is malformed.
fn decode_binary(node: Node<'_, '_>) -> Option<Image> {
    let mut encoded = Vec::new();
    for text in node.children().filter(Node::is_text) {
        let bytes = text.text().unwrap_or_default().bytes();
        encoded.extend(bytes.filter(|b| !b.is_ascii_whitespace()));
    }
    let data = match BASE64.decode(&encoded) {
        Ok(data) if !data.is_empty() => data,
        Ok(_) => return None,
        Err(e) => {
            tracing::debug!(id = attr(node, "id"), "skipping malformed binary: {e}");
            return None;
        }
    };
    let mime = sniff_image_mime(&data)
        .map(str::to_owned)
        .or_else(|| declared_mime(attr(node, "content-type")?))
        .unwrap_or_else(|| "application/octet-stream".to_owned());
    Some(Image { mime, data })
}

/// The MIME type of common image formats, from their magic bytes.
fn sniff_image_mime(data: &[u8]) -> Option<&'static str> {
    match data {
        [0xff, 0xd8, 0xff, ..] => Some("image/jpeg"),
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', ..] => Some("image/png"),
        [b'G', b'I', b'F', b'8', b'7' | b'9', b'a', ..] => Some("image/gif"),
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => Some("image/webp"),
        [b'B', b'M', ..] => Some("image/bmp"),
        _ => None,
    }
}

/// A normalized `content-type` attribute, if it names an image type.
fn declared_mime(content_type: &str) -> Option<String> {
    let mime = content_type.trim().to_ascii_lowercase();
    match mime.as_str() {
        "image/jpg" | "image/pjpeg" => Some("image/jpeg".to_owned()),
        _ if mime.starts_with("image/") && !mime.contains(char::is_whitespace) => Some(mime),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const PNG_1X1: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

    fn binary(xml: &str) -> Option<Image> {
        let doc = Document::parse(xml).unwrap();
        decode_binary(doc.root_element())
    }

    #[test]
    fn non_xml_and_non_fb2_input_are_errors() {
        let err = Fb2Parser.parse_metadata(b"not xml at all").unwrap_err();
        assert!(err.message.starts_with("invalid XML:"), "{err}");
        let err = Fb2Parser
            .parse_content(b"<html><body/></html>")
            .unwrap_err();
        assert!(err.message.contains("<html>"), "{err}");
        let err = Fb2Parser
            .parse_metadata(b"<FictionBook><body>")
            .unwrap_err();
        assert!(err.message.starts_with("invalid XML:"), "{err}");
        let err = Fb2Parser
            .parse_content(b"<FictionBook>\n<body></section></FictionBook>")
            .unwrap_err();
        assert!(err.message.contains("2:"), "position expected: {err}");
    }

    #[test]
    fn dtd_is_rejected() {
        let xml = b"<?xml version=\"1.0\"?><!DOCTYPE FictionBook [<!ENTITY a \"x\">]>\
                    <FictionBook><body/></FictionBook>";
        assert!(Fb2Parser.parse_content(xml).is_err());
    }

    #[test]
    fn namespace_prefixes_are_ignored() {
        let xml = br#"<fb:FictionBook xmlns:fb="http://www.gribuser.ru/xml/fictionbook/2.0">
            <fb:description><fb:title-info><fb:book-title>T</fb:book-title></fb:title-info></fb:description>
            </fb:FictionBook>"#;
        assert_eq!(Fb2Parser.parse_metadata(xml).unwrap().title, "T");
    }

    #[test]
    fn nesting_scan() {
        let nested = |n: usize| format!("<r>{}x{}</r>", "<s a='>'>".repeat(n), "</s>".repeat(n));
        assert!(!nesting_exceeds(&nested(9), 10));
        assert!(nesting_exceeds(&nested(10), 10));
        // Empty elements, closed siblings, comments, CDATA and PIs do not nest.
        let flat = format!(
            "<r>{}</r>",
            "<p/><p></p><!-- <a><b><c> --><![CDATA[<a><b><c>]]><?pi <a><b> ?>".repeat(50)
        );
        assert!(!nesting_exceeds(&flat, 3));
        for tricky in ["", "<", "<a", "<a '", "<!--", "</", "<a/", "<<<>>>"] {
            let _ = nesting_exceeds(tricky, 1);
        }
    }

    #[test]
    fn deepest_accepted_nesting_fits_the_parser_stack() {
        // Exercise the section, block and inline conversion paths at the deepest nesting
        // that passes the pre-scan.
        let depth = MAX_XML_DEPTH - 6;
        let cases = [
            ("", "section", "<p>deepest</p>"),
            ("<section>", "cite", "<p>deepest</p>"),
            ("<section><p>", "emphasis", "deepest"),
        ];
        for (prefix, tag, inner) in cases {
            let suffix: String = prefix
                .split_inclusive('>')
                .rev()
                .map(|open| open.replacen('<', "</", 1))
                .collect();
            let xml = format!(
                "<FictionBook><body>{prefix}{}{inner}{}{suffix}</body></FictionBook>",
                format!("<{tag}>").repeat(depth),
                format!("</{tag}>").repeat(depth),
            );
            let content = crate::format::run_with_parser_stack(move || {
                Fb2Parser.parse_content(xml.as_bytes())
            })
            .unwrap_or_else(|e| panic!("<{tag}>: {e}"));
            let mut text = String::new();
            content.for_each_block(|b| {
                if b.kind.name() == "paragraph" {
                    text.push_str(&b.plain_text());
                }
            });
            assert_eq!(text, "deepest", "<{tag}>");
        }
    }

    #[test]
    fn too_deep_nesting_is_an_error_not_a_crash() {
        let depth = 100_000;
        let xml = format!(
            "<FictionBook><body>{}x{}</body></FictionBook>",
            "<section>".repeat(depth),
            "</section>".repeat(depth)
        );
        let err = Fb2Parser.parse_content(xml.as_bytes()).unwrap_err();
        assert!(err.message.contains("nested more than"), "{err}");
        assert!(Fb2Parser.parse_metadata(xml.as_bytes()).is_err());
    }

    #[test]
    fn whitespace_normalization() {
        assert_eq!(normalize_space("  a \n\t b\r\n"), "a b");
        assert_eq!(normalize_space("a\u{a0}b"), "a\u{a0}b");
        assert_eq!(normalize_space(" \n "), "");
    }

    #[test]
    fn binaries_with_line_breaks_decode_and_sniff_type() {
        let wrapped: String = PNG_1X1
            .as_bytes()
            .chunks(20)
            .map(|c| format!("{}\r\n", std::str::from_utf8(c).unwrap()))
            .collect();
        let image = binary(&format!(
            r#"<binary id="c" content-type="image/jpeg">{wrapped}</binary>"#
        ))
        .unwrap();
        assert_eq!(image.mime, "image/png", "magic bytes win over content-type");
        assert!(image.data.starts_with(b"\x89PNG"));
    }

    #[test]
    fn binaries_without_padding_or_with_unknown_format() {
        let unpadded = PNG_1X1.trim_end_matches('=');
        assert!(binary(&format!("<binary>{unpadded}</binary>")).is_some());
        let image = binary(r#"<binary content-type="image/jpg">AAECAw==</binary>"#).unwrap();
        assert_eq!(image.mime, "image/jpeg");
        let image = binary(r#"<binary content-type="text/plain">AAECAw==</binary>"#).unwrap();
        assert_eq!(image.mime, "application/octet-stream");
    }

    #[test]
    fn malformed_or_empty_binaries_are_skipped() {
        assert_eq!(binary("<binary>!!!not base64!!!</binary>"), None);
        assert_eq!(binary("<binary>A</binary>"), None);
        assert_eq!(binary("<binary/>"), None);
    }

    #[test]
    fn mime_sniffing() {
        assert_eq!(sniff_image_mime(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(b"GIF89a..."), Some("image/gif"));
        assert_eq!(
            sniff_image_mime(b"RIFF\0\0\0\0WEBPVP8 "),
            Some("image/webp")
        );
        assert_eq!(sniff_image_mime(b"BM.."), Some("image/bmp"));
        assert_eq!(sniff_image_mime(b"\x89PN"), None);
    }
}
