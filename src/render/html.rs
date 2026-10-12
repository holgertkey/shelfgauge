//! Rendering book content as HTML: one part at a time, plus the table of contents.
//!
//! Every block becomes an element with `id="b<n>"` (its [`BlockId`]), whatever the
//! split setting, so deep links (`?at=b<n>`) and the browser's `#b<n>` scrolling work.
//! All text is escaped here; the anchors of the source format are never emitted as HTML
//! ids (they could collide with the page's own ids): internal links are resolved to
//! block IDs. External links are kept only for `http`, `https` and `mailto` URLs.

use std::collections::HashMap;
use std::fmt::Write;

use super::split::{Part, part_of};
use crate::format::model::{
    Align, Block, BlockId, BlockKind, BodyKind, BookContent, ImageRef, Inline, LinkTarget, Section,
};

/// Builds the URLs a rendered part links to.
pub trait ReaderLinks {
    /// The URL of block `id` in part `part` (0-based). The renderer uses `#b<n>` itself
    /// for blocks in the current part.
    fn block(&self, part: usize, id: BlockId) -> String;
    /// The URL of the image `image_id` of the book.
    fn image(&self, image_id: &str) -> String;
}

/// Escapes text for HTML content and attribute values.
///
/// # Examples
///
/// ```
/// use shelfgauge::render::html::escape;
///
/// assert_eq!(escape(r#"<a href="x">&'"#), "&lt;a href=&quot;x&quot;&gt;&amp;&#39;");
/// ```
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    escape_into(text, &mut out);
    out
}

fn escape_into(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// Source-format anchors (section and block ids) resolved to block IDs.
fn anchors(content: &BookContent) -> HashMap<&str, BlockId> {
    fn section<'a>(s: &'a Section, map: &mut HashMap<&'a str, BlockId>) {
        if let (Some(anchor), Some(id)) = (&s.anchor, s.first_block_id()) {
            map.entry(anchor.as_str()).or_insert(id);
        }
        for block in s.title.iter().chain(&s.blocks) {
            block.walk(|b| {
                if let Some(anchor) = &b.anchor {
                    map.entry(anchor.as_str()).or_insert(b.id);
                }
            });
        }
        for child in &s.children {
            section(child, map);
        }
    }
    let mut map = HashMap::new();
    for body in content.bodies() {
        section(&body.section, &mut map);
    }
    map
}

/// The first and last block IDs of a section and everything in it.
fn section_range(section: &Section) -> Option<(BlockId, BlockId)> {
    let first = section.first_block_id()?;
    let mut last = first;
    fn walk(s: &Section, last: &mut BlockId) {
        for block in s.title.iter().chain(&s.blocks) {
            *last = (*last).max(block.last_id());
        }
        for child in &s.children {
            walk(child, last);
        }
    }
    walk(section, &mut last);
    Some((first, last))
}

/// Renders part `index` of `parts`.
///
/// # Examples
///
/// ```
/// use shelfgauge::config::ReaderConfig;
/// use shelfgauge::format::model::{Block, BlockId, Body, BookContent, Section};
/// use shelfgauge::render::html::{ReaderLinks, render_part};
/// use shelfgauge::render::split::split;
///
/// struct Links;
/// impl ReaderLinks for Links {
///     fn block(&self, part: usize, id: BlockId) -> String { format!("?part={part}#{id}") }
///     fn image(&self, id: &str) -> String { format!("/img/{id}") }
/// }
///
/// let root = Section { blocks: vec![Block::paragraph("1 < 2")], ..Section::default() };
/// let content = BookContent::new(vec![Body::main(root)], Default::default());
/// let parts = split(&content, &ReaderConfig::default());
/// let html = render_part(&content, &parts, 0, &Links);
/// assert!(html.contains(r#"<p id="b1">1 &lt; 2</p>"#), "{html}");
/// ```
pub fn render_part(
    content: &BookContent,
    parts: &[Part],
    index: usize,
    links: &dyn ReaderLinks,
) -> String {
    let Some(part) = parts.get(index) else {
        return String::new();
    };
    let mut renderer = Renderer {
        anchors: anchors(content),
        parts,
        index,
        part,
        links,
        out: String::new(),
    };
    for body in content.bodies() {
        let Some((first, last)) = section_range(&body.section) else {
            continue;
        };
        if last < part.start || first > part.end {
            continue;
        }
        let class = match body.kind {
            BodyKind::Main => "body",
            BodyKind::Notes => "body notes",
        };
        let _ = write!(renderer.out, "<div class=\"{class}\">");
        renderer.section(&body.section, 0);
        renderer.out.push_str("</div>");
    }
    renderer.out
}

struct Renderer<'a> {
    anchors: HashMap<&'a str, BlockId>,
    parts: &'a [Part],
    index: usize,
    part: &'a Part,
    links: &'a dyn ReaderLinks,
    out: String,
}

impl Renderer<'_> {
    fn section(&mut self, section: &Section, depth: u32) {
        let Some((first, last)) = section_range(section) else {
            return;
        };
        if last < self.part.start || first > self.part.end {
            return;
        }
        let wrap = depth > 0;
        if wrap {
            self.out.push_str("<section>");
        }
        let title: Vec<&Block> = section
            .title
            .iter()
            .filter(|b| self.part.contains(b.id))
            .collect();
        if !title.is_empty() {
            let level = (depth + 1).min(6);
            let _ = write!(
                self.out,
                "<div class=\"title title-{level}\" role=\"heading\" aria-level=\"{level}\">"
            );
            for block in title {
                self.block(block, "title-line");
            }
            self.out.push_str("</div>");
        }
        for block in &section.blocks {
            if self.part.contains(block.id) {
                self.block(block, "");
            }
        }
        for child in &section.children {
            self.section(child, depth + 1);
        }
        if wrap {
            self.out.push_str("</section>");
        }
    }

    /// Opens an element with the block's id and an optional class.
    fn open(&mut self, tag: &str, id: BlockId, class: &str) {
        if class.is_empty() {
            let _ = write!(self.out, "<{tag} id=\"{id}\">");
        } else {
            let _ = write!(self.out, "<{tag} id=\"{id}\" class=\"{class}\">");
        }
    }

    fn blocks(&mut self, blocks: &[Block], class: &str) {
        for block in blocks {
            self.block(block, class);
        }
    }

    /// Renders a block; `class` is given to paragraphs (e.g. title lines).
    fn block(&mut self, block: &Block, class: &str) {
        let id = block.id;
        match &block.kind {
            BlockKind::Paragraph(inlines) => self.text_block("p", id, class, inlines),
            BlockKind::Subtitle(inlines) => self.text_block("p", id, "subtitle", inlines),
            BlockKind::Verse(inlines) => self.text_block("p", id, "verse", inlines),
            BlockKind::TextAuthor(inlines) => self.text_block("p", id, "text-author", inlines),
            BlockKind::Date(text) => {
                self.open("p", id, "date");
                escape_into(text, &mut self.out);
                self.out.push_str("</p>");
            }
            BlockKind::EmptyLine => {
                self.open("div", id, "empty-line");
                self.out.push_str("</div>");
            }
            BlockKind::Image(image) => {
                self.open("figure", id, "");
                self.image(image, "");
                if let Some(title) = image.title.as_deref().filter(|t| !t.trim().is_empty()) {
                    self.out.push_str("<figcaption>");
                    escape_into(title, &mut self.out);
                    self.out.push_str("</figcaption>");
                }
                self.out.push_str("</figure>");
            }
            BlockKind::Epigraph(blocks) => {
                self.open("blockquote", id, "epigraph");
                self.blocks(blocks, "");
                self.out.push_str("</blockquote>");
            }
            BlockKind::Cite(blocks) => {
                self.open("blockquote", id, "cite");
                self.blocks(blocks, "");
                self.out.push_str("</blockquote>");
            }
            BlockKind::Annotation(blocks) => {
                self.open("div", id, "annotation");
                self.blocks(blocks, "");
                self.out.push_str("</div>");
            }
            BlockKind::Poem { title, blocks } | BlockKind::Stanza { title, blocks } => {
                let kind = if matches!(block.kind, BlockKind::Poem { .. }) {
                    "poem"
                } else {
                    "stanza"
                };
                self.open("div", id, kind);
                if !title.is_empty() {
                    let _ = write!(self.out, "<div class=\"{kind}-title\">");
                    self.blocks(title, "");
                    self.out.push_str("</div>");
                }
                self.blocks(blocks, "");
                self.out.push_str("</div>");
            }
            BlockKind::Table(table) => {
                self.open("table", id, "");
                for row in &table.rows {
                    self.out.push_str("<tr>");
                    for cell in &row.cells {
                        let tag = if cell.header { "th" } else { "td" };
                        let _ = write!(self.out, "<{tag}");
                        if cell.colspan > 1 {
                            let _ = write!(self.out, " colspan=\"{}\"", cell.colspan);
                        }
                        if cell.rowspan > 1 {
                            let _ = write!(self.out, " rowspan=\"{}\"", cell.rowspan);
                        }
                        // Classes, not `style`: the CSP allows no inline styles.
                        match cell.align {
                            Some(Align::Left) => self.out.push_str(" class=\"align-left\""),
                            Some(Align::Center) => self.out.push_str(" class=\"align-center\""),
                            Some(Align::Right) => self.out.push_str(" class=\"align-right\""),
                            None => {}
                        }
                        self.out.push('>');
                        self.inlines(&cell.content);
                        let _ = write!(self.out, "</{tag}>");
                    }
                    self.out.push_str("</tr>");
                }
                self.out.push_str("</table>");
            }
        }
    }

    fn text_block(&mut self, tag: &str, id: BlockId, class: &str, inlines: &[Inline]) {
        self.open(tag, id, class);
        self.inlines(inlines);
        let _ = write!(self.out, "</{tag}>");
    }

    fn image(&mut self, image: &ImageRef, class: &str) {
        let src = self.links.image(&image.id);
        self.out.push_str("<img src=\"");
        escape_into(&src, &mut self.out);
        self.out.push_str("\" alt=\"");
        escape_into(image.alt.as_deref().unwrap_or(""), &mut self.out);
        self.out.push('"');
        if !class.is_empty() {
            let _ = write!(self.out, " class=\"{class}\"");
        }
        self.out.push_str(" loading=\"lazy\">");
    }

    fn inlines(&mut self, inlines: &[Inline]) {
        for inline in inlines {
            self.inline(inline);
        }
    }

    fn wrap(&mut self, tag: &str, children: &[Inline]) {
        let _ = write!(self.out, "<{tag}>");
        self.inlines(children);
        let _ = write!(self.out, "</{tag}>");
    }

    fn inline(&mut self, inline: &Inline) {
        match inline {
            Inline::Text(text) => escape_into(text, &mut self.out),
            Inline::Emphasis(children) => self.wrap("em", children),
            Inline::Strong(children) => self.wrap("strong", children),
            Inline::Strikethrough(children) => self.wrap("s", children),
            Inline::Subscript(children) => self.wrap("sub", children),
            Inline::Superscript(children) => self.wrap("sup", children),
            Inline::Code(children) => self.wrap("code", children),
            Inline::Image(image) => self.image(image, "inline"),
            Inline::Link {
                target,
                note,
                children,
            } => {
                let href = match target {
                    LinkTarget::Anchor(anchor) => self.anchors.get(anchor.as_str()).map(|&id| {
                        if self.part.contains(id) {
                            format!("#{id}")
                        } else {
                            let part = part_of(self.parts, id).unwrap_or(self.index);
                            self.links.block(part, id)
                        }
                    }),
                    LinkTarget::Url(url) => safe_url(url).map(str::to_owned),
                };
                match href {
                    Some(href) => {
                        self.out.push_str("<a href=\"");
                        escape_into(&href, &mut self.out);
                        self.out.push('"');
                        if *note {
                            self.out.push_str(" class=\"note-ref\"");
                        }
                        if matches!(target, LinkTarget::Url(_)) {
                            self.out.push_str(" rel=\"noopener noreferrer nofollow\"");
                        }
                        self.out.push('>');
                        self.inlines(children);
                        self.out.push_str("</a>");
                    }
                    // A link that leads nowhere is rendered as its text.
                    None => self.inlines(children),
                }
            }
        }
    }
}

/// The URL if it is an `http`, `https` or `mailto` URL.
fn safe_url(url: &str) -> Option<&str> {
    let url = url.trim();
    let (scheme, _) = url.split_once(':')?;
    ["http", "https", "mailto"]
        .iter()
        .any(|s| scheme.eq_ignore_ascii_case(s))
        .then_some(url)
}

/// Renders the table of contents: the titled sections as nested lists linking to
/// their first block; entries in the current part are marked `class="active"`.
pub fn render_toc(
    content: &BookContent,
    parts: &[Part],
    current: usize,
    links: &dyn ReaderLinks,
) -> String {
    struct Entry {
        depth: usize,
        title: String,
        id: BlockId,
    }
    fn collect(section: &Section, depth: usize, out: &mut Vec<Entry>) {
        let mut child_depth = depth;
        if let (Some(title), Some(id)) = (section.title_text(), section.first_block_id()) {
            out.push(Entry { depth, title, id });
            child_depth = depth + 1;
        }
        for child in &section.children {
            collect(child, child_depth, out);
        }
    }
    let mut entries = Vec::new();
    for body in content.bodies() {
        match body.kind {
            BodyKind::Main => {
                // The book title (the main body's title) is not a TOC entry.
                for child in &body.section.children {
                    collect(child, 0, &mut entries);
                }
            }
            BodyKind::Notes => {
                if let Some(id) = body.section.first_block_id() {
                    entries.push(Entry {
                        depth: 0,
                        title: body
                            .section
                            .title_text()
                            .unwrap_or_else(|| "Notes".to_owned()),
                        id,
                    });
                }
            }
        }
    }

    let mut out = String::from("<nav class=\"toc\" aria-label=\"Contents\">");
    let mut open = 0usize; // currently open <ul> levels
    for entry in &entries {
        let depth = entry.depth.min(open); // never skip a level
        if depth + 1 > open {
            out.push_str("<ul>");
            open += 1;
        } else {
            out.push_str("</li>");
            while open > depth + 1 {
                out.push_str("</ul></li>");
                open -= 1;
            }
        }
        let part = part_of(parts, entry.id).unwrap_or(0);
        let href = links.block(part, entry.id);
        out.push_str("<li><a href=\"");
        escape_into(&href, &mut out);
        out.push('"');
        if part == current {
            out.push_str(" class=\"active\"");
        }
        out.push('>');
        escape_into(&entry.title, &mut out);
        out.push_str("</a>");
    }
    if open > 0 {
        out.push_str("</li>");
        for _ in 1..open {
            out.push_str("</ul></li>");
        }
        out.push_str("</ul>");
    }
    out.push_str("</nav>");
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::{ReaderConfig, SplitMode};
    use crate::format::model::{Body, Table, TableCell, TableRow};
    use crate::render::split::split;

    struct Links;

    impl ReaderLinks for Links {
        fn block(&self, part: usize, id: BlockId) -> String {
            format!("/read?part={}#{id}", part + 1)
        }
        fn image(&self, image_id: &str) -> String {
            format!("/image/{image_id}")
        }
    }

    fn ids(html: &str) -> Vec<u32> {
        html.split(" id=\"b")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap().parse().unwrap())
            .collect()
    }

    fn configs() -> Vec<ReaderConfig> {
        vec![
            ReaderConfig {
                split: SplitMode::None,
                ..ReaderConfig::default()
            },
            ReaderConfig {
                split: SplitMode::Section,
                section_depth: 2,
                ..ReaderConfig::default()
            },
            ReaderConfig {
                split: SplitMode::Size,
                part_size: 30,
                ..ReaderConfig::default()
            },
        ]
    }

    #[test]
    fn every_block_is_rendered_once_with_its_id_in_every_mode() {
        let content = crate::render::split::tests::book();
        let all: Vec<u32> = (1..=content.block_count() as u32).collect();
        for config in configs() {
            let parts = split(&content, &config);
            let mut rendered = Vec::new();
            for i in 0..parts.len() {
                rendered.extend(ids(&render_part(&content, &parts, i, &Links)));
            }
            assert_eq!(rendered, all, "{config:?}");
        }
    }

    fn text(s: &str) -> Vec<Inline> {
        vec![Inline::Text(s.to_owned())]
    }

    fn content_with(blocks: Vec<Block>, children: Vec<Section>) -> BookContent {
        let root = Section {
            blocks,
            children,
            ..Section::default()
        };
        BookContent::new(vec![Body::main(root)], Default::default())
    }

    fn render_all(content: &BookContent) -> String {
        let parts = split(
            content,
            &ReaderConfig {
                split: SplitMode::None,
                ..ReaderConfig::default()
            },
        );
        render_part(content, &parts, 0, &Links)
    }

    #[test]
    fn text_and_attributes_are_escaped() {
        let image = ImageRef {
            id: "a\"b".to_owned(),
            alt: Some("<x>".to_owned()),
            title: Some("&caption".to_owned()),
        };
        let content = content_with(
            vec![
                Block::paragraph("<script>alert('x')</script> & \"q\""),
                Block::new(BlockKind::Image(image)),
                Block::new(BlockKind::Date("<1830>".to_owned())),
            ],
            vec![],
        );
        let html = render_all(&content);
        assert!(!html.contains("<script>"), "{html}");
        assert!(
            html.contains("&lt;script&gt;alert(&#39;x&#39;)&lt;/script&gt; &amp; &quot;q&quot;")
        );
        assert!(
            html.contains(r#"<img src="/image/a&quot;b" alt="&lt;x&gt;""#),
            "{html}"
        );
        assert!(html.contains("<figcaption>&amp;caption</figcaption>"));
        assert!(html.contains("&lt;1830&gt;"));
    }

    #[test]
    fn links_are_resolved_or_dropped() {
        let link = |target: LinkTarget, note: bool| Inline::Link {
            target,
            note,
            children: text("x"),
        };
        let mut target = Block::paragraph("Target");
        target.anchor = Some("sidebar".to_owned());
        let content = content_with(
            vec![
                Block::new(BlockKind::Paragraph(vec![
                    link(LinkTarget::Anchor("sidebar".into()), true),
                    link(LinkTarget::Anchor("missing".into()), false),
                    link(LinkTarget::Url("javascript:alert(1)".into()), false),
                    link(LinkTarget::Url(" JavaScript:alert(1)".into()), false),
                    link(LinkTarget::Url("data:text/html,x".into()), false),
                    link(
                        LinkTarget::Url("https://example.org/?a=1&b=2".into()),
                        false,
                    ),
                    link(LinkTarget::Url("mailto:a@example.org".into()), false),
                ])),
                target,
            ],
            vec![],
        );
        let html = render_all(&content);
        assert!(
            html.contains(r##"<a href="#b2" class="note-ref">x</a>"##),
            "{html}"
        );
        assert!(
            !html.contains("id=\"sidebar\""),
            "source anchors never become ids"
        );
        assert!(!html.to_ascii_lowercase().contains("javascript:"), "{html}");
        assert!(!html.contains("data:"), "{html}");
        assert!(html.contains(
            r#"<a href="https://example.org/?a=1&amp;b=2" rel="noopener noreferrer nofollow">"#
        ));
        assert!(html.contains("href=\"mailto:a@example.org\""));
        assert_eq!(html.matches("<a ").count(), 3, "{html}");
    }

    #[test]
    fn links_to_other_parts_use_the_part_url() {
        let chapter = |title: &str, block: Block| Section {
            title: vec![Block::paragraph(title)],
            blocks: vec![block],
            ..Section::default()
        };
        let mut note = Block::paragraph("Note text");
        note.anchor = Some("n1".to_owned());
        let link = Block::new(BlockKind::Paragraph(vec![Inline::Link {
            target: LinkTarget::Anchor("n1".into()),
            note: true,
            children: text("[1]"),
        }]));
        let content = content_with(vec![], vec![chapter("One", link), chapter("Two", note)]);
        let parts = split(
            &content,
            &ReaderConfig {
                split: SplitMode::Section,
                ..ReaderConfig::default()
            },
        );
        assert_eq!(parts.len(), 2);
        let html = render_part(&content, &parts, 0, &Links);
        assert!(
            html.contains(r#"<a href="/read?part=2#b4" class="note-ref">[1]</a>"#),
            "{html}"
        );
    }

    #[test]
    fn tables_use_classes_for_alignment() {
        let cell = |header, align| TableCell {
            header,
            align,
            colspan: 2,
            content: text("c"),
            ..TableCell::default()
        };
        let table = Table {
            rows: vec![TableRow {
                cells: vec![cell(true, Some(Align::Center)), cell(false, None)],
            }],
        };
        let html = render_all(&content_with(
            vec![Block::new(BlockKind::Table(table))],
            vec![],
        ));
        assert!(html.contains(r#"<table id="b1"><tr><th colspan="2" class="align-center">c</th><td colspan="2">c</td></tr></table>"#), "{html}");
        assert!(!html.contains("style"));
    }

    #[test]
    fn toc_nests_titled_sections_and_marks_the_current_part() {
        let content = crate::render::split::tests::book();
        let parts = split(
            &content,
            &ReaderConfig {
                split: SplitMode::Section,
                section_depth: 2,
                ..ReaderConfig::default()
            },
        );
        let toc = render_toc(&content, &parts, 1, &Links);
        let titles: Vec<&str> = toc
            .split("\">")
            .skip(1)
            .filter_map(|s| s.split("</a>").next())
            .filter(|s| !s.contains('<'))
            .collect();
        assert_eq!(
            titles,
            [
                "Part One",
                "Chapter 1",
                "Chapter 2",
                "Section 2.1",
                "Chapter 3",
                "Notes"
            ]
        );
        assert_eq!(toc.matches("<ul>").count(), toc.matches("</ul>").count());
        assert_eq!(toc.matches("<li>").count(), toc.matches("</li>").count());
        let active: BTreeSet<&str> = toc
            .split("class=\"active\">")
            .skip(1)
            .filter_map(|s| s.split("</a>").next())
            .collect();
        assert_eq!(active, BTreeSet::from(["Chapter 2", "Section 2.1"]));
    }
}
