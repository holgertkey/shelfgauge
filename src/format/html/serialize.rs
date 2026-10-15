//! The HTML tree → well-formed XHTML for the shared converter.
//!
//! - Only the `<body>` is written. Elements and attributes keep their (lowercase)
//!   names; names that are not XML names or have a prefix (Word's `<o:p>`) are dropped,
//!   an element keeping its content. Only the `epub:` and `xlink:` attribute prefixes
//!   are kept; `<a name>` becomes `id` (older tables of contents link to it).
//! - Left out with their content: scripts, styles, forms and their controls, embedded
//!   objects, SVG and MathML, page numbers of printed editions (`class="pagenum"`, as
//!   in Project Gutenberg books), and site chrome — a `<nav>`, `<header>` or `<footer>`
//!   whose text is mostly link text ([`CHROME_LINK_SHARE`]).
//! - Elements nested deeper than [`FLATTEN_DEPTH`] are written as their text, so the
//!   XML stage never reaches its nesting limit.
//! - A `<pre>` holding a whole book (more than [`BIG_PRE_LINES`] lines, or most of the
//!   page's text) is read like a plain text file (paragraphs, verse, headings; see
//!   [`format::txt`](crate::format::txt)), its `<h*>` elements kept as headings. The
//!   outermost such `<pre>` is converted (lib.ru nests a `<pre>` in a `<pre>`). Other
//!   `<pre>` stay preformatted.
//! - Text is escaped (`&`, `<`, `>`; also `"` in attributes).

use super::dom::{Data, Dom, Id};
use crate::format::model::{BlockKind, Inline};
use crate::format::sections::Item;
use crate::format::txt;

/// Elements below this depth are written as text.
pub(super) const FLATTEN_DEPTH: usize = 128;

/// A `<pre>` with more lines than this is read as plain text.
pub(super) const BIG_PRE_LINES: usize = 50;

/// Site chrome: a `nav`, `header` or `footer` with more link text than this share.
pub(super) const CHROME_LINK_SHARE: f64 = 0.5;

/// Elements left out with their content.
const SKIPPED: &[&str] = &[
    "head", "script", "style", "noscript", "iframe", "template", "form", "input", "button",
    "select", "option", "optgroup", "textarea", "label", "object", "embed", "applet", "canvas",
    "audio", "video", "map", "svg", "math", "frameset", "frame", "dialog",
];

/// Characters of text (whitespace not counted) and of link text below `id`.
pub(super) fn text_stats(dom: &Dom, id: Id) -> (usize, usize) {
    let (mut text, mut links) = (0, 0);
    let mut stack = vec![(id, false)];
    while let Some((node, in_link)) = stack.pop() {
        match &dom.nodes[node].data {
            Data::Text(t) => {
                let count = t.chars().filter(|c| !c.is_whitespace()).count();
                text += count;
                if in_link {
                    links += count;
                }
            }
            Data::Element { .. } => {
                let name = dom.name(node).unwrap_or_default();
                if node != id && SKIPPED.contains(&name) {
                    continue;
                }
                let link = in_link || (name == "a" && dom.attr(node, "href").is_some());
                stack.extend(dom.children(node).map(|c| (c, link)));
            }
            _ => stack.extend(dom.children(node).map(|c| (c, in_link))),
        }
    }
    (text, links)
}

/// Writes the body of `dom` as an XHTML document.
pub(super) fn to_xhtml(dom: &Dom, body: Id) -> String {
    let (page_text, _) = text_stats(dom, body);
    let mut writer = Writer {
        dom,
        page_text,
        out: String::from("<html><body>"),
    };
    writer.children(body);
    writer.out.push_str("</body></html>");
    writer.out
}

/// One step of the iterative walk.
enum Step {
    Node(Id, usize),
    Close(String),
}

struct Writer<'a> {
    dom: &'a Dom,
    page_text: usize,
    out: String,
}

impl Writer<'_> {
    fn children(&mut self, parent: Id) {
        let mut stack: Vec<Step> = self
            .dom
            .children(parent)
            .map(|c| Step::Node(c, 2))
            .collect();
        stack.reverse();
        while let Some(step) = stack.pop() {
            let (id, depth) = match step {
                Step::Close(name) => {
                    self.out.push_str("</");
                    self.out.push_str(&name);
                    self.out.push('>');
                    continue;
                }
                Step::Node(id, depth) => (id, depth),
            };
            let name = match &self.dom.nodes[id].data {
                Data::Text(text) => {
                    escape(&mut self.out, text, false);
                    continue;
                }
                Data::Element { .. } => self.dom.name(id).unwrap_or_default().to_owned(),
                _ => continue,
            };
            if SKIPPED.contains(&name.as_str())
                || is_page_number(self.dom, id)
                || self.is_chrome(id, &name)
            {
                continue;
            }
            if depth > FLATTEN_DEPTH {
                self.text_of(id);
                continue;
            }
            if name == "pre" && self.is_big_pre(id) {
                self.big_pre(id);
                continue;
            }
            let children: Vec<Id> = self.dom.children(id).collect();
            if is_xml_name(&name) && !name.contains(':') {
                self.open(id, &name, children.is_empty());
                if !children.is_empty() {
                    stack.push(Step::Close(name));
                }
            }
            for &child in children.iter().rev() {
                stack.push(Step::Node(child, depth + 1));
            }
        }
    }

    /// The start tag (self-closing if `empty`).
    fn open(&mut self, id: Id, name: &str, empty: bool) {
        self.out.push('<');
        self.out.push_str(name);
        let Data::Element { attrs, .. } = &self.dom.nodes[id].data else {
            return;
        };
        let mut has_id = false;
        for attr in attrs {
            let local: &str = &attr.name.local;
            let keep = is_xml_name(local)
                && !local.starts_with("xmlns")
                && match local.split_once(':') {
                    Some((prefix, rest)) => {
                        matches!(prefix, "epub" | "xlink")
                            && is_xml_name(rest)
                            && !rest.contains(':')
                    }
                    None => true,
                };
            if !keep {
                continue;
            }
            has_id |= local == "id";
            self.attribute(local, &attr.value);
        }
        if !has_id
            && name == "a"
            && let Some(anchor) = self.dom.attr(id, "name")
        {
            self.attribute("id", anchor);
        }
        self.out.push_str(if empty { "/>" } else { ">" });
    }

    fn attribute(&mut self, name: &str, value: &str) {
        self.out.push(' ');
        self.out.push_str(name);
        self.out.push_str("=\"");
        escape(&mut self.out, value, true);
        self.out.push('"');
    }

    /// Writes the text below `id`.
    fn text_of(&mut self, id: Id) {
        let texts: Vec<Id> = self
            .dom
            .descendants(id)
            .filter(|&n| matches!(self.dom.nodes[n].data, Data::Text(_)))
            .collect();
        for node in texts {
            if let Data::Text(text) = &self.dom.nodes[node].data {
                escape(&mut self.out, text, false);
                self.out.push(' ');
            }
        }
    }

    /// Site chrome: a `nav`, `header` or `footer` that is mostly links.
    fn is_chrome(&self, id: Id, name: &str) -> bool {
        if !matches!(name, "nav" | "header" | "footer") {
            return false;
        }
        let (text, links) = text_stats(self.dom, id);
        text > 0 && links as f64 > text as f64 * CHROME_LINK_SHARE
    }

    fn is_big_pre(&self, id: Id) -> bool {
        let (text, _) = text_stats(self.dom, id);
        let lines: usize = self
            .dom
            .descendants(id)
            .map(|n| match &self.dom.nodes[n].data {
                Data::Text(t) => t.matches('\n').count(),
                _ => 0,
            })
            .sum();
        lines > BIG_PRE_LINES || text * 2 > self.page_text
    }

    /// Writes a `<pre>` holding a book as paragraphs and headings.
    fn big_pre(&mut self, id: Id) {
        let mut text = String::new();
        let mut forced = Vec::new();
        let mut stack: Vec<Id> = self.dom.children(id).collect();
        stack.reverse();
        while let Some(node) = stack.pop() {
            match &self.dom.nodes[node].data {
                Data::Text(t) => text.push_str(&txt::clean(t)),
                Data::Element { .. } => {
                    let name = self.dom.name(node).unwrap_or_default();
                    if SKIPPED.contains(&name) {
                        continue;
                    }
                    if name == "br" {
                        text.push('\n');
                    } else if matches!(name, "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
                        let mut heading = String::new();
                        for n in self.dom.descendants(node) {
                            if let Data::Text(t) = &self.dom.nodes[n].data {
                                heading.push_str(t);
                                heading.push(' ');
                            }
                        }
                        let heading = heading.split_whitespace().collect::<Vec<_>>().join(" ");
                        if !heading.is_empty() {
                            text.push_str("\n\n");
                            forced.push((text.matches('\n').count(), heading.clone()));
                            text.push_str(&heading);
                            text.push_str("\n\n");
                        }
                        continue;
                    }
                    let start = stack.len();
                    stack.extend(self.dom.children(node));
                    stack[start..].reverse();
                }
                _ => {}
            }
        }
        for item in txt::items_with_headings(&text, &forced) {
            match item {
                Item::Heading(rank, block) => {
                    let level = (rank + 1).min(6);
                    self.out.push_str(&format!("<h{level}>"));
                    escape(&mut self.out, &block.plain_text(), false);
                    self.out.push_str(&format!("</h{level}>"));
                }
                Item::Block(block) => match &block.kind {
                    BlockKind::Rule => self.out.push_str("<hr/>"),
                    BlockKind::Paragraph(inlines) => {
                        self.out.push_str("<p>");
                        for inline in inlines {
                            match inline {
                                Inline::LineBreak => self.out.push_str("<br/>"),
                                Inline::Text(text) => escape(&mut self.out, text, false),
                                _ => {}
                            }
                        }
                        self.out.push_str("</p>");
                    }
                    _ => {}
                },
            }
        }
    }
}

/// A page number of the printed edition (`class="pagenum"`).
fn is_page_number(dom: &Dom, id: Id) -> bool {
    dom.attr(id, "class").is_some_and(|class| {
        class
            .split_whitespace()
            .any(|c| c.eq_ignore_ascii_case("pagenum"))
    })
}

/// Appends `text` escaped for XML content (and attribute values with `attribute`).
fn escape(out: &mut String, text: &str, attribute: bool) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// Whether `name` is an XML name (ASCII letters, digits, `_`, `-`, `.`, `:`; not
/// starting with a digit, `-` or `.`) without a prefix colon in elements.
fn is_xml_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':'))
        && !name.starts_with(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn xhtml(html: &str) -> String {
        let dom = Dom::parse(html).unwrap();
        let body = dom.find("body").unwrap();
        to_xhtml(&dom, body)
    }

    #[test]
    fn tags_are_balanced_and_text_escaped() {
        assert_eq!(
            xhtml("<P CLASS=x>a & b <BR> c<p>d < e"),
            "<html><body><p class=\"x\">a &amp; b <br/> c</p><p>d &lt; e</p></body></html>"
        );
    }

    #[test]
    fn bad_names_are_unwrapped_and_bad_attributes_dropped() {
        assert_eq!(
            xhtml(
                "<p><o:p>word</o:p><span a<b=1 xmlns:x=y epub:type=noteref data-x=\"q&quot;\" f:g=h>s</span></p>"
            ),
            "<html><body><p>word<span epub:type=\"noteref\" data-x=\"q&quot;\">s</span></p></body></html>"
        );
    }

    #[test]
    fn named_anchors_become_ids() {
        assert_eq!(
            xhtml("<a name=ch1></a><a id=x name=y>t</a>"),
            "<html><body><a name=\"ch1\" id=\"ch1\"/><a id=\"x\" name=\"y\">t</a></body></html>"
        );
    }

    #[test]
    fn scripts_forms_and_link_chrome_are_left_out() {
        let page = "<header><a href=/>Home</a> <a href=/b>Books</a></header>\
                    <header><h2>Chapter 1</h2></header>\
                    <script>alert(1)</script><form><select><option>x</select></form>\
                    <p>Text</p><nav><a href=/n>Next</a></nav><footer>Plain footer</footer>";
        assert_eq!(
            xhtml(page),
            "<html><body><header><h2>Chapter 1</h2></header><p>Text</p>\
             <footer>Plain footer</footer></body></html>"
        );
        assert_eq!(
            xhtml("<h2><span class=\"pagenum\"><a id=p9>{ix}</a></span>PREFACE.</h2>"),
            "<html><body><h2>PREFACE.</h2></body></html>"
        );
    }

    #[test]
    fn deep_nesting_is_flattened() {
        let html = format!("{}deep{}", "<div>".repeat(400), "</div>".repeat(400));
        let out = xhtml(&html);
        assert!(
            out.matches("<div>").count() <= FLATTEN_DEPTH,
            "{}",
            out.len()
        );
        assert!(out.contains("deep"));
    }

    #[test]
    fn a_big_pre_is_read_as_text() {
        let paragraph = "     Несколько лет тому назад в одном из своих поместий жил старинный\n\
                         русской барин, Кирила Петрович Троекуров.\n";
        let html = format!(
            "<pre><div><form><select><option>Fine HTML</select></form></div><pre>\n\
             <ul><a name=0></a><h2>Пушкин. Дубровский</h2></ul>\n\
             <ul><a name=1></a><h2>ГЛАВА I.</h2></ul>\n{}</pre></pre>\
             <pre>small\ncode</pre>",
            paragraph.repeat(30)
        );
        let out = xhtml(&html);
        assert!(out.starts_with("<html><body><h3>Пушкин. Дубровский</h3><h3>ГЛАВА I.</h3><p>Несколько лет тому назад"), "{out}");
        assert!(!out.contains("Fine HTML"));
        assert!(
            out.ends_with("<pre>small\ncode</pre></body></html>"),
            "{out}"
        );
    }
}
