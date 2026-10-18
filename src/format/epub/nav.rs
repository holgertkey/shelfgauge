//! The table of contents: the EPUB 3 navigation document (`nav epub:type="toc"`) or
//! the EPUB 2 NCX, flattened to entries with a depth.

use roxmltree::Node;

use super::package::{LocalName, Package};
use crate::format::xhtml::document::{OPS_NS, with_document};
use crate::format::xhtml::paths::resolve;
use crate::format::xml::normalize_space;
use crate::format::zipbook::ZipBook as Archive;

/// At most this many entries are read.
const MAX_ENTRIES: usize = 20_000;

/// Deeper entries are kept at this depth.
pub(super) const MAX_DEPTH: usize = 16;

/// One entry of the table of contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Entry {
    /// 1 for top-level entries.
    pub depth: usize,
    /// The label, whitespace-normalized.
    pub label: String,
    /// The entry path and fragment the entry points to.
    pub target: (String, Option<String>),
}

/// The entries of the navigation document, else of the NCX; empty if neither is usable.
pub(super) fn read(package: &Package, archive: &mut Archive<'_>) -> Vec<Entry> {
    let sources = [
        (
            package.nav_path(),
            nav as fn(Node<'_, '_>, &str) -> Vec<Entry>,
        ),
        (package.ncx_path(), ncx),
    ];
    for (path, parse) in sources {
        let Some(path) = path else { continue };
        let data = match archive.read(path) {
            Ok(Some(data)) => data,
            Ok(None) => continue,
            Err(e) => {
                tracing::debug!(error = ?e.to_string(), "skipping the table of contents");
                continue;
            }
        };
        match with_document(&data, |doc| parse(doc.root_element(), path)) {
            Ok(entries) if !entries.is_empty() => return entries,
            Ok(_) => {}
            Err(e) => tracing::debug!(
                path = ?path,
                error = ?e.to_string(),
                "skipping the table of contents"
            ),
        }
    }
    Vec::new()
}

/// The entries of a navigation document: the `toc` nav, else the first nav.
fn nav(root: Node<'_, '_>, base: &str) -> Vec<Entry> {
    let navs: Vec<Node<'_, '_>> = root
        .descendants()
        .filter(|n| n.has_tag_name_local("nav"))
        .collect();
    let toc = navs
        .iter()
        .find(|n| {
            n.attribute((OPS_NS, "type"))
                .is_some_and(|t| t.split_whitespace().any(|t| t == "toc"))
                || n.attribute("role") == Some("doc-toc")
        })
        .or(navs.first());
    let mut entries = Vec::new();
    if let Some(list) = toc.and_then(|n| n.children().find(|c| c.has_tag_name_local("ol"))) {
        nav_list(list, 1, base, &mut entries);
    }
    entries
}

fn nav_list(list: Node<'_, '_>, depth: usize, base: &str, out: &mut Vec<Entry>) {
    for item in list.children().filter(|n| n.has_tag_name_local("li")) {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let label_node = item
            .children()
            .find(|n| n.has_tag_name_local("a") || n.has_tag_name_local("span"));
        let at = out.len();
        let target = label_node
            .and_then(|n| n.attribute("href"))
            .and_then(|href| resolve(base, href));
        if let Some(label_node) = label_node {
            out.push(Entry {
                depth: depth.min(MAX_DEPTH),
                label: text(label_node),
                target: target.clone().unwrap_or_default(),
            });
        }
        if let Some(sub) = item.children().find(|n| n.has_tag_name_local("ol")) {
            nav_list(sub, depth + 1, base, out);
        }
        // A heading without a link (`span`) points where its first child points.
        if target.is_none() && at < out.len() {
            match out[at + 1..].first().map(|e| e.target.clone()) {
                Some(first) => out[at].target = first,
                None => {
                    out.remove(at);
                }
            }
        }
    }
}

/// The entries of an NCX `navMap`.
fn ncx(root: Node<'_, '_>, base: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    if let Some(map) = root.children().find(|n| n.has_tag_name_local("navMap")) {
        nav_points(map, 1, base, &mut entries);
    }
    entries
}

fn nav_points(parent: Node<'_, '_>, depth: usize, base: &str, out: &mut Vec<Entry>) {
    for point in parent
        .children()
        .filter(|n| n.has_tag_name_local("navPoint"))
    {
        if out.len() >= MAX_ENTRIES {
            return;
        }
        let label = point
            .children()
            .find(|n| n.has_tag_name_local("navLabel"))
            .map(text)
            .unwrap_or_default();
        let target = point
            .children()
            .find(|n| n.has_tag_name_local("content"))
            .and_then(|n| n.attribute("src"))
            .and_then(|src| resolve(base, src));
        if let Some(target) = target {
            out.push(Entry {
                depth: depth.min(MAX_DEPTH),
                label,
                target,
            });
        }
        nav_points(point, depth + 1, base, out);
    }
}

fn text(node: Node<'_, '_>) -> String {
    let raw: String = node
        .descendants()
        .filter(Node::is_text)
        .filter_map(|n| n.text())
        .collect();
    normalize_space(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(xml: &str, f: fn(Node<'_, '_>, &str) -> Vec<Entry>) -> Vec<(usize, String, String)> {
        with_document(xml.as_bytes(), |doc| {
            f(doc.root_element(), "OEBPS/nav.xhtml")
        })
        .unwrap()
        .into_iter()
        .map(|e| {
            let target = match e.target.1 {
                Some(fragment) => format!("{}#{fragment}", e.target.0),
                None => e.target.0,
            };
            (e.depth, e.label, target)
        })
        .collect()
    }

    #[test]
    fn nav_document_toc() {
        let xml = r#"<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops"><body>
<nav epub:type="landmarks"><ol><li><a href="x.xhtml">Landmark</a></li></ol></nav>
<nav epub:type="toc"><h1>Contents</h1><ol>
  <li><a href="text/ch1.xhtml">Chapter
     1</a></li>
  <li><span>Part Two</span><ol>
     <li><a href="text/ch2.xhtml#s1">Chapter 2</a></li>
  </ol></li>
  <li><span>Empty</span></li>
</ol></nav></body></html>"#;
        assert_eq!(
            parse(xml, nav),
            [
                (1, "Chapter 1".into(), "OEBPS/text/ch1.xhtml".into()),
                (1, "Part Two".into(), "OEBPS/text/ch2.xhtml#s1".into()),
                (2, "Chapter 2".into(), "OEBPS/text/ch2.xhtml#s1".into()),
            ]
        );
    }

    #[test]
    fn ncx_nav_map() {
        let xml = r#"<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/"><navMap>
<navPoint id="a" playOrder="1"><navLabel><text>One</text></navLabel><content src="ch1.html"/>
  <navPoint id="b" playOrder="2"><navLabel><text>One.A</text></navLabel><content src="ch1.html#a"/></navPoint>
</navPoint>
<navPoint id="c" playOrder="3"><navLabel><text>No target</text></navLabel></navPoint>
</navMap></ncx>"#;
        assert_eq!(
            parse(xml, ncx),
            [
                (1, "One".into(), "OEBPS/ch1.html".into()),
                (2, "One.A".into(), "OEBPS/ch1.html#a".into()),
            ]
        );
    }
}
