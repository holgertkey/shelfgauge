//! Loading XML documents of books (EPUB container, package, navigation and XHTML
//! documents; HTML pages serialized as XHTML).
//!
//! XHTML content documents are XML, but real books use what only HTML parsers accept,
//! so the text goes through a pre-pass before `roxmltree` sees it:
//!
//! - the document type declaration is removed (`roxmltree` rejects DTDs, and the XHTML
//!   DTDs are never loaded anyway);
//! - named character references of XHTML 1.x (`&nbsp;`, `&mdash;`, ...) become numeric
//!   ones; an unknown reference or a bare `&` is kept as text;
//! - numeric references to characters XML does not allow (`&#0;`, surrogates) and such
//!   characters themselves (C0 controls other than tab, CR and LF) are replaced or
//!   dropped;
//! - an undeclared namespace prefix (often `epub:`) gets a declaration on the root
//!   element.
//!
//! Comments and CDATA sections are copied unchanged. A document whose only remaining
//! problem is its tag structure is repaired like FB2 (see [`repair`]).

use roxmltree::{Document, ParsingOptions};

use super::entities;
use crate::format::ParseError;
use crate::format::fb2::encoding;
use crate::format::repair;
use crate::format::xml::{MAX_XML_DEPTH, NODES_LIMIT, nesting_exceeds};

/// The namespace of `epub:type` and other EPUB attributes.
pub(crate) const OPS_NS: &str = "http://www.idpf.org/2007/ops";

/// Well-known prefixes declared when a document uses them without a declaration.
const KNOWN_PREFIXES: &[(&str, &str)] =
    &[("epub", OPS_NS), ("xlink", "http://www.w3.org/1999/xlink")];

/// How many undeclared prefixes are declared before giving up.
const MAX_PREFIX_RETRIES: usize = 8;

/// Decodes `data`, runs the pre-pass and parses it, then calls `f` with the document.
pub(crate) fn with_document<T>(
    data: &[u8],
    f: impl FnOnce(&Document<'_>) -> T,
) -> Result<T, ParseError> {
    let decoded = encoding::decode(data);
    let mut text = prepare(&decoded.text);
    for (prefix, uri) in KNOWN_PREFIXES {
        if uses_prefix(&text, prefix) {
            text = declare_prefix(&text, prefix, uri);
        }
    }
    if nesting_exceeds(&text, MAX_XML_DEPTH) {
        return Err(ParseError::new(format!(
            "elements are nested more than {MAX_XML_DEPTH} levels deep"
        )));
    }
    let invalid = |e: &roxmltree::Error| ParseError::new(format!("invalid XML: {e}"));
    let mut f = Some(f);
    let mut repaired = false;
    for _ in 0..=MAX_PREFIX_RETRIES {
        let error = match parse_and_apply(&text, &mut f) {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        match &error {
            roxmltree::Error::UnknownNamespace(prefix, _) => {
                let uri = format!("urn:x-shelfgauge:undeclared:{prefix}");
                let declared = declare_prefix(&text, prefix, &uri);
                if declared == text {
                    return Err(invalid(&error));
                }
                text = declared;
            }
            roxmltree::Error::UnexpectedCloseTag(..)
            | roxmltree::Error::UnclosedRootNode
            | roxmltree::Error::UnexpectedEndOfStream
                if !repaired =>
            {
                let Some(fixed) = repair::rebalance(&text) else {
                    return Err(invalid(&error));
                };
                if nesting_exceeds(&fixed, MAX_XML_DEPTH) {
                    return Err(invalid(&error));
                }
                tracing::info!("repaired unbalanced tags ({error})");
                text = fixed;
                repaired = true;
            }
            _ => return Err(invalid(&error)),
        }
    }
    Err(ParseError::new("invalid XML: too many undeclared prefixes"))
}

/// Parses `text` and calls `f` (taken from the option) with the document.
fn parse_and_apply<T>(
    text: &str,
    f: &mut Option<impl FnOnce(&Document<'_>) -> T>,
) -> Result<T, roxmltree::Error> {
    let options = ParsingOptions {
        allow_dtd: false,
        nodes_limit: NODES_LIMIT,
        ..ParsingOptions::default()
    };
    let doc = Document::parse_with_options(text, options)?;
    let f = f.take().expect("called once: the document parsed");
    Ok(f(&doc))
}

/// The pre-pass described in the [module docs](self).
pub(crate) fn prepare(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 64);
    let mut rest = text;
    let mut seen_root = false;
    while let Some(pos) = rest.find(['<', '&']) {
        push_chars(&mut out, &rest[..pos]);
        rest = &rest[pos..];
        if rest.starts_with('&') {
            rest = reference(&mut out, rest);
            continue;
        }
        let verbatim = |end: &str, from: usize| {
            rest[from..]
                .find(end)
                .map_or(rest.len(), |p| from + p + end.len())
        };
        let len = if rest.starts_with("<!--") {
            verbatim("-->", 4)
        } else if rest.starts_with("<![CDATA[") {
            verbatim("]]>", 9)
        } else if rest.starts_with("<?") {
            verbatim("?>", 2)
        } else if !seen_root && starts_with_ignore_case(rest, "<!DOCTYPE") {
            // Removed, internal subset included.
            rest = &rest[doctype_len(rest)..];
            continue;
        } else {
            if !rest.starts_with("<!") {
                seen_root = true;
            }
            // A tag: copied, but references in attribute values are handled too.
            out.push('<');
            rest = &rest[1..];
            continue;
        };
        push_chars(&mut out, &rest[..len]);
        rest = &rest[len..];
    }
    push_chars(&mut out, rest);
    out
}

/// Appends `text` without the characters XML does not allow.
fn push_chars(out: &mut String, text: &str) {
    if text.chars().all(is_xml_char) {
        out.push_str(text);
    } else {
        out.extend(text.chars().filter(|&c| is_xml_char(c)));
    }
}

/// Whether XML 1.0 allows `c` in a document.
fn is_xml_char(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\r' | '\u{20}'..='\u{d7ff}' | '\u{e000}'..='\u{fffd}')
        || c >= '\u{10000}'
}

/// Converts the reference at the start of `rest` (which starts with `&`) and returns
/// the input after it.
fn reference<'a>(out: &mut String, rest: &'a str) -> &'a str {
    let body = &rest[1..];
    let end = body
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
        .unwrap_or(body.len());
    let name = &body[..end];
    if !body[end..].starts_with(';') || name.is_empty() {
        out.push_str("&amp;");
        return body;
    }
    let after = &body[end + 1..];
    if let Some(number) = name.strip_prefix('#') {
        let value = match number.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok(),
            None => number.parse().ok(),
        };
        match value {
            Some(value) if char::from_u32(value).is_some_and(is_xml_char) => {
                out.push_str(&rest[..end + 2]);
            }
            Some(_) => out.push_str("&#xFFFD;"),
            None => {
                out.push_str("&amp;");
                return body;
            }
        }
        return after;
    }
    if matches!(name, "amp" | "lt" | "gt" | "quot" | "apos") {
        out.push_str(&rest[..end + 2]);
    } else if let Some(c) = entities::lookup(name) {
        out.push_str(&format!("&#x{:X};", u32::from(c)));
    } else {
        // Unknown: kept as text.
        out.push_str("&amp;");
        return body;
    }
    after
}

/// The length of the document type declaration at the start of `rest`.
fn doctype_len(rest: &str) -> usize {
    let mut quote = None;
    let mut subset = false;
    for (i, c) in rest.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '[') => subset = true,
            (None, ']') => subset = false,
            (None, '>') if !subset => return i + 1,
            _ => {}
        }
    }
    rest.len()
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.len() >= prefix.len()
        && text.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Whether `text` uses `prefix:` in a tag or attribute name without declaring it.
fn uses_prefix(text: &str, prefix: &str) -> bool {
    let declaration = format!("xmlns:{prefix}");
    if text.contains(&declaration) {
        return false;
    }
    let attribute = format!(" {prefix}:");
    let element = format!("<{prefix}:");
    text.contains(&attribute) || text.contains(&element)
}

/// Adds `xmlns:prefix="uri"` to the root element's start tag (unchanged if there is no
/// root element).
fn declare_prefix(text: &str, prefix: &str, uri: &str) -> String {
    let Some(at) = root_name_end(text) else {
        return text.to_owned();
    };
    let mut out = String::with_capacity(text.len() + uri.len() + 16);
    out.push_str(&text[..at]);
    out.push_str(&format!(" xmlns:{prefix}=\"{uri}\""));
    out.push_str(&text[at..]);
    out
}

/// The byte offset just after the root element's name.
fn root_name_end(text: &str) -> Option<usize> {
    let mut rest = text;
    let mut offset = 0;
    loop {
        let pos = rest.find('<')?;
        let tag = &rest[pos..];
        let skip = if tag.starts_with("<!--") {
            tag.find("-->").map(|p| p + 3)
        } else if tag.starts_with("<?") {
            tag.find("?>").map(|p| p + 2)
        } else if tag.starts_with("<!") {
            Some(doctype_len(tag))
        } else {
            let name_len = tag[1..]
                .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
                .unwrap_or(tag.len() - 1);
            return Some(offset + pos + 1 + name_len);
        }?;
        offset += pos + skip;
        rest = &rest[pos + skip..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctype_is_removed() {
        let xhtml = r#"<?xml version="1.0"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html><body><p>a</p></body></html>"#;
        let out = prepare(xhtml);
        assert!(!out.contains("DOCTYPE"), "{out}");
        assert!(out.contains("<html><body>"));
        let subset = r#"<!doctype html [ <!ENTITY a "x>y"> ]><html/>"#;
        assert_eq!(prepare(subset), "<html/>");
    }

    #[test]
    fn references_are_made_xml() {
        assert_eq!(
            prepare("<p a=\"x&nbsp;y\">a&nbsp;b&mdash;&amp;&lt;&#65;&#x42;</p>"),
            "<p a=\"x&#xA0;y\">a&#xA0;b&#x2014;&amp;&lt;&#65;&#x42;</p>"
        );
        assert_eq!(
            prepare("Q&A & co &foo; &;"),
            "Q&amp;A &amp; co &amp;foo; &amp;;"
        );
        assert_eq!(
            prepare("a&#0;b&#xD800;c&#x110000;"),
            "a&#xFFFD;b&#xFFFD;c&#xFFFD;"
        );
        assert_eq!(prepare("&#zz;"), "&amp;#zz;");
    }

    #[test]
    fn invalid_characters_are_dropped() {
        assert_eq!(prepare("a\u{1}b\u{b}c\td\u{fffe}"), "abc\td");
    }

    #[test]
    fn comments_and_cdata_are_verbatim() {
        let text = "<r><!-- &nbsp; --><![CDATA[&nbsp; <x>]]>&nbsp;</r>";
        assert_eq!(
            prepare(text),
            "<r><!-- &nbsp; --><![CDATA[&nbsp; <x>]]>&#xA0;</r>"
        );
    }

    #[test]
    fn undeclared_prefixes_are_declared() {
        let xhtml = br#"<html><body><aside epub:type="footnote" foo:bar="x"><p>n</p></aside></body></html>"#;
        let ns = with_document(xhtml, |doc| {
            let aside = doc.descendants().find(|n| n.has_tag_name("aside")).unwrap();
            aside.attribute((OPS_NS, "type")).map(str::to_owned)
        })
        .unwrap();
        assert_eq!(ns.as_deref(), Some("footnote"));
    }

    #[test]
    fn entities_and_doctype_parse() {
        let xhtml = br#"<?xml version="1.0" encoding="utf-8"?>
<!DOCTYPE html PUBLIC "-//W3C//DTD XHTML 1.1//EN" "http://www.w3.org/TR/xhtml11/DTD/xhtml11.dtd">
<html xmlns="http://www.w3.org/1999/xhtml"><body><p>a&nbsp;b &copy; R&D</p></body></html>"#;
        let text = with_document(xhtml, |doc| {
            doc.descendants()
                .find(|n| n.has_tag_name("p"))
                .and_then(|p| p.text())
                .map(str::to_owned)
        })
        .unwrap();
        assert_eq!(text.as_deref(), Some("a\u{a0}b © R&D"));
    }

    #[test]
    fn unbalanced_tags_are_repaired() {
        let text = with_document(b"<html><body><p>a<br>b</p></body></html>", |doc| {
            doc.root_element()
                .descendants()
                .filter(|n| n.is_text())
                .filter_map(|n| n.text())
                .collect::<String>()
        })
        .unwrap();
        assert_eq!(text, "ab");
        assert!(with_document(b"not xml <", |_| ()).is_err());
    }
}
