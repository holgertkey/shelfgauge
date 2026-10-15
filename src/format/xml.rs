//! Helpers shared by the XML-based formats (FB2, EPUB).

use super::model::Inline;

/// Maximum number of XML nodes in one document; protects against memory exhaustion.
///
/// A large real book (10 MB of text) has a few hundred thousand nodes.
pub(crate) const NODES_LIMIT: u32 = 4_000_000;

/// Maximum element nesting accepted at all.
///
/// `roxmltree`'s tokenizer recurses once per nesting level without a limit (about 10 KB
/// of stack per level in debug builds), and a stack overflow aborts the whole process
/// (it cannot be caught), so deeper documents are rejected before parsing. Real books
/// nest a few dozen levels at most. Together with the conversion depth limits of the
/// parsers this keeps parsing within
/// [`PARSER_STACK_SIZE`](crate::format::PARSER_STACK_SIZE).
pub(crate) const MAX_XML_DEPTH: usize = 256;

/// Whether element nesting in `xml` exceeds `limit`, by a quick scan of the markup.
///
/// Comments, CDATA sections, processing instructions, declarations and quoted attribute
/// values are skipped; malformed markup is left to the XML parser to report.
pub(crate) fn nesting_exceeds(xml: &str, limit: usize) -> bool {
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

/// Collapses runs of XML whitespace into single spaces and trims the result.
pub(crate) fn normalize_space(text: &str) -> String {
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
pub(crate) fn is_xml_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n')
}

/// The MIME type of common image formats, from their magic bytes.
pub(crate) fn sniff_image_mime(data: &[u8]) -> Option<&'static str> {
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

/// Appends text with XML whitespace runs collapsed, merging with a preceding text.
pub(crate) fn push_text(out: &mut Vec<Inline>, text: &str) {
    let mut collapsed = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.chars() {
        if matches!(c, ' ' | '\t' | '\r' | '\n') {
            if !in_space {
                collapsed.push(' ');
            }
            in_space = true;
        } else {
            collapsed.push(c);
            in_space = false;
        }
    }
    if collapsed.is_empty() {
        return;
    }
    if let Some(Inline::Text(previous)) = out.last_mut() {
        if previous.ends_with(' ') && collapsed.starts_with(' ') {
            collapsed.remove(0);
        }
        previous.push_str(&collapsed);
    } else {
        out.push(Inline::Text(collapsed));
    }
}

/// Appends inlines, merging a leading text with a preceding text.
pub(crate) fn extend_merged(out: &mut Vec<Inline>, inlines: Vec<Inline>) {
    for inline in inlines {
        match inline {
            Inline::Text(text) => push_text(out, &text),
            other => out.push(other),
        }
    }
}

/// The child list of a formatting inline, if it is one.
fn inline_children(inline: &mut Inline) -> Option<&mut Vec<Inline>> {
    match inline {
        Inline::Emphasis(children)
        | Inline::Strong(children)
        | Inline::Strikethrough(children)
        | Inline::Subscript(children)
        | Inline::Superscript(children)
        | Inline::Code(children)
        | Inline::Link { children, .. } => Some(children),
        Inline::Text(_) | Inline::Image(_) | Inline::LineBreak => None,
    }
}

/// Removes leading whitespace and line breaks of the inline content.
pub(crate) fn trim_start(inlines: &mut Vec<Inline>) {
    while let Some(first) = inlines.first_mut() {
        match first {
            Inline::Text(text) => {
                let trimmed = text.trim_start_matches(' ');
                if trimmed.is_empty() {
                    inlines.remove(0);
                    continue;
                }
                *text = trimmed.to_owned();
            }
            Inline::LineBreak => {
                inlines.remove(0);
                continue;
            }
            Inline::Image(_) => {}
            other => {
                if let Some(children) = inline_children(other) {
                    trim_start(children);
                    if children.is_empty() {
                        inlines.remove(0);
                        continue;
                    }
                }
            }
        }
        return;
    }
}

/// Removes trailing whitespace and line breaks of the inline content.
pub(crate) fn trim_end(inlines: &mut Vec<Inline>) {
    while let Some(last) = inlines.last_mut() {
        match last {
            Inline::Text(text) => {
                let trimmed_len = text.trim_end_matches(' ').len();
                if trimmed_len == 0 {
                    inlines.pop();
                    continue;
                }
                text.truncate(trimmed_len);
            }
            Inline::LineBreak => {
                inlines.pop();
                continue;
            }
            Inline::Image(_) => {}
            other => {
                if let Some(children) = inline_children(other) {
                    trim_end(children);
                    if children.is_empty() {
                        inlines.pop();
                        continue;
                    }
                }
            }
        }
        return;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn whitespace_normalization() {
        assert_eq!(normalize_space("  a \n\t b\r\n"), "a b");
        assert_eq!(normalize_space("a\u{a0}b"), "a\u{a0}b");
        assert_eq!(normalize_space(" \n "), "");
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
