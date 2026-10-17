//! The FB2 (FictionBook 2) format.
//!
//! Parsing is defensive: element names are matched by local name (files with missing or
//! wrong namespaces are common), unknown elements keep their text, and no input makes
//! the parser panic. A document whose only problem is unbalanced tags (a stray close
//! tag, unclosed elements) is repaired and parsed again (see `format::repair`). Only input that
//! is not XML, or not FictionBook, is an error.
//!
//! A zipped FB2 (`.fbz`, or a `.fb2` file that is really a zip) is unzipped in memory
//! with the limits of the EPUB container (`format::zipbook`): the first `.fb2`
//! entry in central directory order is the book, or else the first entry.

use base64::Engine as _;
use base64::engine::general_purpose::{GeneralPurpose, PAD_INDIFFERENT};
use roxmltree::{Document, Node, ParsingOptions};

use super::model::{BookContent, BookMetadata, Image};
use super::xml::{MAX_XML_DEPTH, NODES_LIMIT, nesting_exceeds, normalize_space, sniff_image_mime};
use super::zipbook::ZipBook;
use super::{BookParser, ParseError, repair};

mod content;
pub mod encoding;
pub mod genres;
mod metadata;

/// Maximum element nesting converted structurally; deeper content is kept as plain text.
///
/// Together with [`MAX_XML_DEPTH`] this keeps parsing within
/// [`PARSER_STACK_SIZE`](crate::format::PARSER_STACK_SIZE).
const MAX_DEPTH: usize = 64;

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
        &["fb2", "fbz"]
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

/// The signature at the start of a zip file.
const ZIP_SIGNATURE: &[u8] = b"PK\x03\x04";

/// The FB2 document of a zipped FB2 (see the [module docs](self)).
fn unzip(data: &[u8]) -> Result<Vec<u8>, ParseError> {
    let mut zip = ZipBook::open(data, "zipped FB2")?;
    let name = zip
        .names()
        .iter()
        .find(|name| {
            name.rsplit_once('.')
                .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("fb2"))
        })
        .or_else(|| zip.names().first())
        .cloned()
        .ok_or_else(|| ParseError::new("the zipped FB2 is empty"))?;
    zip.read(&name)?
        .ok_or_else(|| ParseError::new("the zipped FB2 is empty"))
}

/// Decodes and parses `data` (unzipped first if it is a zip), then calls `f` with the
/// `<FictionBook>` root element.
fn with_root<T>(data: &[u8], f: impl FnOnce(Node<'_, '_>) -> T) -> Result<T, ParseError> {
    let unzipped;
    let data = if data.starts_with(ZIP_SIGNATURE) {
        unzipped = unzip(data)?;
        &unzipped[..]
    } else {
        data
    };
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
    let options = || ParsingOptions {
        allow_dtd: false,
        nodes_limit: NODES_LIMIT,
        ..ParsingOptions::default()
    };
    let invalid = |e: roxmltree::Error| ParseError::new(format!("invalid XML: {e}"));
    let repaired: String;
    let doc = match Document::parse_with_options(&decoded.text, options()) {
        Ok(doc) => doc,
        // Only the tag structure is wrong: retry with rebalanced tags.
        Err(
            e @ (roxmltree::Error::UnexpectedCloseTag(..)
            | roxmltree::Error::UnclosedRootNode
            | roxmltree::Error::UnexpectedEndOfStream),
        ) => {
            let Some(fixed) = repair::rebalance(&decoded.text) else {
                return Err(invalid(e));
            };
            repaired = fixed;
            if nesting_exceeds(&repaired, MAX_XML_DEPTH) {
                return Err(invalid(e));
            }
            let reason = e.to_string();
            let doc = Document::parse_with_options(&repaired, options()).map_err(|_| invalid(e))?;
            tracing::info!("repaired unbalanced tags ({reason})");
            doc
        }
        Err(e) => return Err(invalid(e)),
    };
    let root = doc.root_element();
    if root.tag_name().name() != "FictionBook" {
        return Err(ParseError::new(format!(
            "not an FB2 document: the root element is <{}>, expected <FictionBook>",
            root.tag_name().name()
        )));
    }
    Ok(f(root))
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
            .parse_content(b"<FictionBook>\n<body a=1></body></FictionBook>")
            .unwrap_err();
        assert!(err.message.starts_with("invalid XML:"), "{err}");
        assert!(err.message.contains("2:"), "position expected: {err}");
    }

    #[test]
    fn unbalanced_tags_are_repaired() {
        // A stray close tag (found in real books) and an unclosed document.
        let content = Fb2Parser
            .parse_content(
                b"<FictionBook><body><section><p>One</p></section></section>\
                  <section><p>Two</p></section></body></FictionBook>",
            )
            .unwrap();
        let text: Vec<String> = content.blocks().iter().map(|b| b.plain_text()).collect();
        assert_eq!(text, ["One", "Two"]);
        let metadata = Fb2Parser
            .parse_metadata(b"<FictionBook><description><title-info><book-title>T")
            .unwrap();
        assert_eq!(metadata.title, "T");
    }

    #[test]
    fn repair_keeps_the_depth_limit() {
        let xml = format!(
            "<FictionBook><body>{}</FictionBook>",
            "<section>".repeat(MAX_XML_DEPTH + 10)
        );
        assert!(Fb2Parser.parse_content(xml.as_bytes()).is_err());
    }

    #[test]
    fn zipped_fb2_is_unzipped() {
        use crate::format::zipbook::zip_bytes;
        let book = b"<FictionBook><description><title-info><book-title>Zipped</book-title>\
                     </title-info></description><body><section><p>Text</p></section></body>\
                     </FictionBook>";
        // The `.fb2` entry wins over earlier entries; without one the first entry.
        for entries in [
            &[("cover.jpg", &b"jpeg"[..]), ("Book.FB2", &book[..])][..],
            &[("book.xml", &book[..]), ("other.txt", &b"x"[..])][..],
        ] {
            let data = zip_bytes(entries);
            assert_eq!(Fb2Parser.parse_metadata(&data).unwrap().title, "Zipped");
            let text: Vec<String> = Fb2Parser
                .parse_content(&data)
                .unwrap()
                .blocks()
                .iter()
                .map(|b| b.plain_text())
                .collect();
            assert_eq!(text, ["Text"]);
        }
    }

    #[test]
    fn zipped_fb2_errors() {
        use crate::format::zipbook::zip_bytes;
        let err = Fb2Parser
            .parse_metadata(&zip_bytes(&[("notes.txt", b"plain text")]))
            .unwrap_err();
        assert!(err.message.starts_with("invalid XML"), "{err}");
        let err = Fb2Parser.parse_metadata(b"PK\x03\x04broken").unwrap_err();
        assert!(err.message.starts_with("not a zipped FB2"), "{err}");
        let empty = zip_bytes(&[]);
        assert!(Fb2Parser.parse_metadata(&empty).is_err());
        // A zip inside the zip is not unzipped again.
        let inner = zip_bytes(&[("book.fb2", b"<FictionBook/>")]);
        let err = Fb2Parser
            .parse_metadata(&zip_bytes(&[("book.fb2", &inner)]))
            .unwrap_err();
        assert!(err.message.starts_with("invalid XML"), "{err}");
        let bomb = zip_bytes(&[("book.fb2", &vec![b' '; 3 * 1024 * 1024])]);
        let err = Fb2Parser.parse_metadata(&bomb).unwrap_err();
        assert!(err.message.contains("compressed"), "{err}");
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
}
