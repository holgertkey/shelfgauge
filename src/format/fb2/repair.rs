//! Repairing unbalanced tags, for files that are XML except for their tag structure.
//!
//! Real FB2 files sometimes have a stray close tag (e.g. a `</section>` without an
//! opening tag) or end without closing their elements. [`rebalance`] rewrites only the
//! tag structure, deterministically:
//!
//! - a close tag that matches an open element closes the elements opened after it
//!   first (`<a><b></a>` → `<a><b></b></a>`);
//! - a close tag that matches no open element is dropped;
//! - elements still open at the end are closed, innermost first;
//! - an unterminated tag at the end of the input is dropped.
//!
//! Comments, CDATA sections, processing instructions and declarations are copied
//! unchanged, and `>` inside quoted attribute values does not end a tag. Names are
//! compared exactly (namespace prefix included).

/// The rebalanced document, or `None` if the tag structure needed no change.
pub(super) fn rebalance(xml: &str) -> Option<String> {
    let bytes = xml.as_bytes();
    let mut out = String::with_capacity(xml.len() + 64);
    let mut open: Vec<&str> = Vec::new();
    let mut changed = false;
    let mut i = 0;
    let close_all = |open: &mut Vec<&str>, out: &mut String, down_to: usize| {
        while open.len() > down_to {
            let name = open.pop().expect("checked length");
            out.push_str("</");
            out.push_str(name);
            out.push('>');
        }
    };

    while i < bytes.len() {
        let Some(offset) = bytes[i..].iter().position(|&b| b == b'<') else {
            out.push_str(&xml[i..]);
            break;
        };
        out.push_str(&xml[i..i + offset]);
        i += offset;
        let rest = &bytes[i..];

        let verbatim_until = |end: &[u8], from: usize| {
            bytes[from..]
                .windows(end.len())
                .position(|w| w == end)
                .map(|p| from + p + end.len())
        };
        let special = if rest.starts_with(b"<!--") {
            Some(verbatim_until(b"-->", i + 4))
        } else if rest.starts_with(b"<![CDATA[") {
            Some(verbatim_until(b"]]>", i + 9))
        } else if rest.starts_with(b"<?") {
            Some(verbatim_until(b"?>", i + 2))
        } else if rest.starts_with(b"<!") {
            Some(verbatim_until(b">", i + 2))
        } else {
            None
        };
        if let Some(end) = special {
            match end {
                Some(end) => {
                    out.push_str(&xml[i..end]);
                    i = end;
                }
                None => {
                    // Unterminated at the end of the input: drop it.
                    changed = true;
                    i = bytes.len();
                }
            }
            continue;
        }

        // A tag: find its end outside quoted attribute values.
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
        if j >= bytes.len() {
            changed = true; // an unterminated tag at the end
            break;
        }
        let tag = &xml[i..=j];
        i = j + 1;

        if let Some(inner) = tag.strip_prefix("</") {
            let name = inner
                .trim_end_matches('>')
                .split(|c: char| c.is_whitespace())
                .next()
                .unwrap_or("");
            match open.iter().rposition(|open| *open == name) {
                Some(index) => {
                    if index + 1 < open.len() {
                        changed = true;
                        close_all(&mut open, &mut out, index + 1);
                    }
                    open.pop();
                    out.push_str(tag);
                }
                None => changed = true, // a stray close tag
            }
        } else {
            out.push_str(tag);
            let self_closing = tag[..tag.len() - 1].trim_end().ends_with('/');
            let name_end = tag[1..]
                .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
                .map_or(tag.len(), |p| p + 1);
            let name = &tag[1..name_end];
            if !self_closing && !name.is_empty() {
                open.push(name);
            }
        }
    }
    if !open.is_empty() {
        changed = true;
        close_all(&mut open, &mut out, 0);
    }
    changed.then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balanced_documents_are_left_alone() {
        for xml in [
            "<a><b/><c x='1'>t</c></a>",
            "<?xml version='1.0'?><a><!-- </b> --><![CDATA[</c>]]></a>",
            "text only",
            "",
        ] {
            assert_eq!(rebalance(xml), None, "{xml}");
        }
    }

    #[test]
    fn stray_close_tags_are_dropped() {
        assert_eq!(
            rebalance(
                "<body><section><p>1</p></section></section><section><p>2</p></section></body>"
            )
            .unwrap(),
            "<body><section><p>1</p></section><section><p>2</p></section></body>"
        );
    }

    #[test]
    fn close_tags_close_inner_elements_first() {
        assert_eq!(
            rebalance("<a><b><i>x</a>").unwrap(),
            "<a><b><i>x</i></b></a>"
        );
    }

    #[test]
    fn unclosed_elements_are_closed_at_the_end() {
        assert_eq!(rebalance("<a><b>text").unwrap(), "<a><b>text</b></a>");
        assert_eq!(rebalance("<a><b>text<p cla").unwrap(), "<a><b>text</b></a>");
        assert_eq!(rebalance("<a><!-- unterminated").unwrap(), "<a></a>");
    }

    #[test]
    fn markup_in_comments_cdata_and_attributes_is_not_structure() {
        let xml = r#"<a title="x > </a> y"><!-- </a> --><![CDATA[</a>]]><?pi </a>?></section></a>"#;
        assert_eq!(
            rebalance(xml).unwrap(),
            r#"<a title="x > </a> y"><!-- </a> --><![CDATA[</a>]]><?pi </a>?></a>"#
        );
    }

    #[test]
    fn names_are_compared_exactly() {
        assert_eq!(rebalance("<l:a><a>x</l:a>").unwrap(), "<l:a><a>x</a></l:a>");
        assert_eq!(rebalance("<a>x</A>").unwrap(), "<a>x</a>");
    }

    #[test]
    fn rebalancing_is_idempotent() {
        let once = rebalance("<a><b></a></c><d>").unwrap();
        assert_eq!(rebalance(&once), None, "{once}");
    }
}
