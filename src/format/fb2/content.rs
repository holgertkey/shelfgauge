//! `<body>` → [`BookContent`].
//!
//! Every element is converted by its local name regardless of where it appears, so
//! slightly wrong structures still produce sensible output. Unknown elements keep their
//! text: as a paragraph at block level, flattened at inline level.

use std::collections::BTreeMap;

use roxmltree::Node;

use super::{
    MAX_DEPTH, attr, attr_text, binaries, child, children, decode_binary, is, normalize_space,
    text_content,
};
use crate::format::model::{
    Align, Block, BlockKind, Body, BookContent, ImageRef, Inline, LinkTarget, Section, Table,
    TableCell, TableRow,
};
use crate::format::xml::{extend_merged, push_text, trim_end, trim_start};

/// Converts all bodies and binaries; the first body is the main text, the rest notes.
pub(super) fn parse(root: Node<'_, '_>) -> BookContent {
    let bodies = children(root, "body")
        .enumerate()
        .map(|(i, body)| {
            let section = section(body, 0);
            if i == 0 {
                Body::main(section)
            } else {
                Body::notes(section)
            }
        })
        .collect();
    let mut images = BTreeMap::new();
    for binary in binaries(root) {
        let Some(id) = attr(binary, "id")
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        if images.contains_key(id) {
            continue;
        }
        if let Some(image) = decode_binary(binary) {
            images.insert(id.to_owned(), image);
        }
    }
    BookContent::new(bodies, images)
}

/// Converts a `<body>` or `<section>`.
///
/// Content after a nested section is wrapped into an untitled child section, keeping
/// the model's `title`, `blocks`, `children` document order. The first `<title>` is the
/// section title if only images precede it (`<body>` is `image?, title?, ...`).
fn section(node: Node<'_, '_>, depth: usize) -> Section {
    let mut section = Section {
        anchor: attr_text(node, "id"),
        ..Section::default()
    };
    if depth > MAX_DEPTH {
        section.blocks = flattened(node).into_iter().collect();
        return section;
    }
    let mut trailing = Section::default();
    for child in node.children() {
        if is(child, "section") {
            push_nonempty(&mut section.children, std::mem::take(&mut trailing));
            section.children.push(self::section(child, depth + 1));
        } else if is(child, "title")
            && section.title.is_empty()
            && section.children.is_empty()
            && section
                .blocks
                .iter()
                .all(|b| matches!(b.kind, BlockKind::Image(_)))
        {
            section.title = title_lines(child, depth + 1);
        } else if section.children.is_empty() {
            block(child, depth + 1, &mut section.blocks);
        } else {
            block(child, depth + 1, &mut trailing.blocks);
        }
    }
    push_nonempty(&mut section.children, trailing);
    section
}

fn push_nonempty(children: &mut Vec<Section>, section: Section) {
    if !section.blocks.is_empty() || !section.children.is_empty() {
        children.push(section);
    }
}

/// The lines of a `<title>`: paragraphs and empty lines.
fn title_lines(node: Node<'_, '_>, depth: usize) -> Vec<Block> {
    let mut lines = Vec::new();
    for child in node.children() {
        if is(child, "empty-line") {
            lines.push(Block::new(BlockKind::EmptyLine));
        } else if child.is_element() || has_text(child) {
            let inlines = if child.is_element() {
                inlines(child, depth + 1)
            } else {
                vec![Inline::Text(normalize_space(
                    child.text().unwrap_or_default(),
                ))]
            };
            lines.push(with_anchor(BlockKind::Paragraph(inlines), child));
        }
    }
    lines
}

/// The blocks of an `<annotation>` element (used for the book annotation).
pub(super) fn annotation_blocks(node: Node<'_, '_>) -> Vec<Block> {
    blocks(node, 1)
}

/// Converts the children of `node` to blocks.
fn blocks(node: Node<'_, '_>, depth: usize) -> Vec<Block> {
    let mut out = Vec::new();
    for child in node.children() {
        block(child, depth, &mut out);
    }
    out
}

/// Converts one node at block level, appending zero or more blocks to `out`.
fn block(node: Node<'_, '_>, depth: usize, out: &mut Vec<Block>) {
    if node.is_text() {
        if has_text(node) {
            let text = normalize_space(node.text().unwrap_or_default());
            out.push(Block::new(BlockKind::Paragraph(vec![Inline::Text(text)])));
        }
        return;
    }
    if !node.is_element() {
        return;
    }
    if depth > MAX_DEPTH {
        out.extend(flattened(node));
        return;
    }
    let next = depth + 1;
    let kind = match node.tag_name().name() {
        "p" => BlockKind::Paragraph(inlines(node, next)),
        "subtitle" => BlockKind::Subtitle(inlines(node, next)),
        "v" => BlockKind::Verse(inlines(node, next)),
        "text-author" => BlockKind::TextAuthor(inlines(node, next)),
        "date" => BlockKind::Date(text_content(node)),
        "empty-line" => BlockKind::EmptyLine,
        "image" => match image_ref(node) {
            Some(image) => BlockKind::Image(image),
            None => return,
        },
        "epigraph" => BlockKind::Epigraph(blocks(node, next)),
        "cite" => BlockKind::Cite(blocks(node, next)),
        "annotation" => BlockKind::Annotation(blocks(node, next)),
        "poem" | "stanza" => {
            let title = child(node, "title").map_or_else(Vec::new, |t| title_lines(t, next));
            let mut content = Vec::new();
            for child in node.children().filter(|c| !is(*c, "title")) {
                block(child, next, &mut content);
            }
            if is(node, "poem") {
                BlockKind::Poem {
                    title,
                    blocks: content,
                }
            } else {
                BlockKind::Stanza {
                    title,
                    blocks: content,
                }
            }
        }
        "table" => BlockKind::Table(table(node, next)),
        "title" => {
            // A title where none is expected: keep its lines as subtitles.
            for line in title_lines(node, next) {
                out.push(match line.kind {
                    BlockKind::Paragraph(inlines) => Block {
                        kind: BlockKind::Subtitle(inlines),
                        ..line
                    },
                    _ => line,
                });
            }
            return;
        }
        "section" | "body" => {
            // A section where none is expected: keep its content inline.
            let section = section(node, next);
            out.extend(section_into_blocks(section));
            return;
        }
        "strong" | "emphasis" | "style" | "a" | "strikethrough" | "sub" | "sup" | "code" => {
            BlockKind::Paragraph(inlines_of(std::iter::once(node), next))
        }
        name => {
            tracing::debug!("unknown FB2 element <{name}> at block level");
            if node.children().any(is_block_element) {
                for child in node.children() {
                    block(child, next, out);
                }
                return;
            }
            if text_content(node).is_empty() {
                return;
            }
            BlockKind::Paragraph(inlines(node, next))
        }
    };
    out.push(with_anchor(kind, node));
}

/// The blocks of a misplaced section, its title lines as subtitles.
fn section_into_blocks(section: Section) -> Vec<Block> {
    let mut out: Vec<Block> = section
        .title
        .into_iter()
        .map(|line| match line.kind {
            BlockKind::Paragraph(inlines) => Block {
                kind: BlockKind::Subtitle(inlines),
                ..line
            },
            _ => line,
        })
        .collect();
    out.extend(section.blocks);
    for child in section.children {
        out.extend(section_into_blocks(child));
    }
    out
}

/// Elements that are converted as blocks (used to decide how to unwrap unknown ones).
fn is_block_element(node: Node<'_, '_>) -> bool {
    node.is_element()
        && matches!(
            node.tag_name().name(),
            "p" | "subtitle"
                | "v"
                | "text-author"
                | "date"
                | "empty-line"
                | "image"
                | "epigraph"
                | "cite"
                | "annotation"
                | "poem"
                | "stanza"
                | "table"
                | "title"
                | "section"
        )
}

/// Too deeply nested content, kept as one plain paragraph.
fn flattened(node: Node<'_, '_>) -> Option<Block> {
    let text = text_content(node);
    (!text.is_empty()).then(|| Block::new(BlockKind::Paragraph(vec![Inline::Text(text)])))
}

fn with_anchor(kind: BlockKind, node: Node<'_, '_>) -> Block {
    Block {
        anchor: attr_text(node, "id"),
        ..Block::new(kind)
    }
}

fn has_text(node: Node<'_, '_>) -> bool {
    node.is_text() && !normalize_space(node.text().unwrap_or_default()).is_empty()
}

/// An `<image>` reference; `None` without a local `#id` link.
fn image_ref(node: Node<'_, '_>) -> Option<ImageRef> {
    let id = attr(node, "href")?.trim().strip_prefix('#')?.trim();
    if id.is_empty() {
        tracing::debug!("skipping <image> without a local link");
        return None;
    }
    Some(ImageRef {
        id: id.to_owned(),
        alt: attr_text(node, "alt"),
        title: attr_text(node, "title"),
    })
}

/// A `<table>`: `<tr>` rows of `<th>`/`<td>` cells.
fn table(node: Node<'_, '_>, depth: usize) -> Table {
    let rows = children(node, "tr")
        .map(|tr| TableRow {
            cells: tr
                .children()
                .filter(|c| is(*c, "td") || is(*c, "th"))
                .map(|cell| TableCell {
                    header: is(cell, "th"),
                    colspan: span(cell, "colspan"),
                    rowspan: span(cell, "rowspan"),
                    align: match attr(cell, "align").map(str::trim) {
                        Some("left") => Some(Align::Left),
                        Some("center") => Some(Align::Center),
                        Some("right") => Some(Align::Right),
                        _ => None,
                    },
                    content: inlines(cell, depth + 1),
                })
                .collect(),
        })
        .collect();
    Table { rows }
}

/// A `colspan`/`rowspan` value, clamped to 1..=1000 (1 if missing or invalid).
fn span(node: Node<'_, '_>, name: &str) -> u32 {
    attr(node, name)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(1, |n| n.clamp(1, 1000))
}

/// The inline content of `node`, with whitespace normalized and trimmed at the edges.
fn inlines(node: Node<'_, '_>, depth: usize) -> Vec<Inline> {
    inlines_of(node.children(), depth)
}

fn inlines_of<'a, 'input: 'a>(
    nodes: impl Iterator<Item = Node<'a, 'input>>,
    depth: usize,
) -> Vec<Inline> {
    let mut out = Vec::new();
    for node in nodes {
        push_inline(node, depth, &mut out);
    }
    trim_start(&mut out);
    trim_end(&mut out);
    out
}

/// Converts one node at inline level.
fn push_inline(node: Node<'_, '_>, depth: usize, out: &mut Vec<Inline>) {
    if node.is_text() {
        push_text(out, node.text().unwrap_or_default());
        return;
    }
    if !node.is_element() {
        return;
    }
    if depth > MAX_DEPTH {
        push_text(out, &text_content(node));
        return;
    }
    let wrap = |make: fn(Vec<Inline>) -> Inline, out: &mut Vec<Inline>| {
        let mut children = Vec::new();
        for child in node.children() {
            push_inline(child, depth + 1, &mut children);
        }
        if !children.is_empty() {
            out.push(make(children));
        }
    };
    match node.tag_name().name() {
        "strong" => wrap(Inline::Strong, out),
        "emphasis" => wrap(Inline::Emphasis, out),
        "strikethrough" => wrap(Inline::Strikethrough, out),
        "sub" => wrap(Inline::Subscript, out),
        "sup" => wrap(Inline::Superscript, out),
        "code" => wrap(Inline::Code, out),
        "image" => {
            if let Some(image) = image_ref(node) {
                out.push(Inline::Image(image));
            }
        }
        "a" => {
            let mut children = Vec::new();
            for child in node.children() {
                push_inline(child, depth + 1, &mut children);
            }
            let href = attr(node, "href").map(str::trim).unwrap_or_default();
            let target = match href.strip_prefix('#') {
                Some(anchor) if !anchor.is_empty() => Some(LinkTarget::Anchor(anchor.to_owned())),
                Some(_) => None,
                None if !href.is_empty() => Some(LinkTarget::Url(href.to_owned())),
                None => None,
            };
            match target {
                Some(target) if !children.is_empty() => out.push(Inline::Link {
                    target,
                    note: attr(node, "type").map(str::trim) == Some("note"),
                    children,
                }),
                _ => extend_merged(out, children),
            }
        }
        name => {
            if name != "style" {
                tracing::debug!("unknown FB2 element <{name}> at inline level");
            }
            for child in node.children() {
                push_inline(child, depth + 1, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::BookParser;
    use crate::format::fb2::Fb2Parser;
    use crate::format::model::BodyKind;

    fn content(bodies: &str) -> BookContent {
        let xml = format!(
            r#"<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0" xmlns:l="http://www.w3.org/1999/xlink">{bodies}</FictionBook>"#
        );
        Fb2Parser.parse_content(xml.as_bytes()).unwrap()
    }

    fn main_section(body: &str) -> Section {
        content(&format!("<body>{body}</body>")).bodies()[0]
            .section
            .clone()
    }

    fn text(s: &str) -> Inline {
        Inline::Text(s.to_owned())
    }

    fn kinds(blocks: &[Block]) -> Vec<&'static str> {
        blocks.iter().map(|b| b.kind.name()).collect()
    }

    #[test]
    fn sections_titles_and_paragraphs() {
        let body = main_section(
            "<title><p>Book</p></title>\
             <section id='c1'><title><p>Chapter 1</p><empty-line/><p>Start</p></title>\
               <p>One.</p><p>Two.</p></section>\
             <section><p>Untitled.</p></section>",
        );
        assert_eq!(body.title_text().as_deref(), Some("Book"));
        assert_eq!(body.children.len(), 2);
        let c1 = &body.children[0];
        assert_eq!(c1.anchor.as_deref(), Some("c1"));
        assert_eq!(kinds(&c1.title), ["paragraph", "empty_line", "paragraph"]);
        assert_eq!(c1.title_text().as_deref(), Some("Chapter 1 Start"));
        assert_eq!(kinds(&c1.blocks), ["paragraph", "paragraph"]);
        assert_eq!(body.children[1].title_text(), None);
    }

    /// Regression: `<body>` may start with an image before its title (FB2 schema:
    /// `image?, title?, epigraph*, section+`); the title used to become a subtitle.
    #[test]
    fn image_before_title_keeps_the_title() {
        let body = main_section(
            "<image l:href='#frontispiece'/><title><p>Book</p></title><section><p>x</p></section>",
        );
        assert_eq!(body.title_text().as_deref(), Some("Book"));
        assert_eq!(kinds(&body.blocks), ["image"]);
        // Anything else before the title makes it a subtitle.
        let body = main_section("<p>x</p><title><p>Late</p></title>");
        assert_eq!(body.title_text(), None);
        assert_eq!(kinds(&body.blocks), ["paragraph", "subtitle"]);
    }

    #[test]
    fn content_after_subsections_becomes_an_untitled_child() {
        let body = main_section(
            "<section><p>Intro.</p><section><p>A</p></section><p>Stray.</p>\
             <section><p>B</p></section><p>End.</p></section>",
        );
        let section = &body.children[0];
        assert_eq!(section.blocks[0].plain_text(), "Intro.");
        let texts: Vec<String> = section
            .children
            .iter()
            .map(|c| c.blocks[0].plain_text())
            .collect();
        assert_eq!(texts, ["A", "Stray.", "B", "End."]);
        assert!(section.children[1].title.is_empty());
    }

    #[test]
    fn whitespace_is_normalized_inside_paragraphs() {
        let body = main_section(
            "<section><p>\n   Hello,   <strong> bold </strong>\n <emphasis>world</emphasis>!  \n</p></section>",
        );
        let BlockKind::Paragraph(inlines) = &body.children[0].blocks[0].kind else {
            panic!()
        };
        assert_eq!(
            inlines,
            &[
                text("Hello, "),
                Inline::Strong(vec![text(" bold ")]),
                text(" "),
                Inline::Emphasis(vec![text("world")]),
                text("!"),
            ]
        );
    }

    #[test]
    fn inline_markup_links_and_images() {
        let body = main_section(
            "<section><p><strikethrough>s</strikethrough><sub>1</sub><sup>2</sup><code>c</code>\
             <style name='x'>styled</style>\
             <a l:href='#n1' type='note'>[1]</a><a l:href='https://example.org'>web</a>\
             <a l:href='#'>empty anchor</a><a>no href</a><image l:href='#i.png'/></p></section>",
        );
        let BlockKind::Paragraph(inlines) = &body.children[0].blocks[0].kind else {
            panic!()
        };
        assert_eq!(
            inlines,
            &[
                Inline::Strikethrough(vec![text("s")]),
                Inline::Subscript(vec![text("1")]),
                Inline::Superscript(vec![text("2")]),
                Inline::Code(vec![text("c")]),
                text("styled"),
                Inline::Link {
                    target: LinkTarget::Anchor("n1".to_owned()),
                    note: true,
                    children: vec![text("[1]")],
                },
                Inline::Link {
                    target: LinkTarget::Url("https://example.org".to_owned()),
                    note: false,
                    children: vec![text("web")],
                },
                text("empty anchorno href"),
                Inline::Image(ImageRef {
                    id: "i.png".to_owned(),
                    ..ImageRef::default()
                }),
            ]
        );
    }

    #[test]
    fn poems_epigraphs_cites_and_tables() {
        let body = main_section(
            "<section><epigraph><p>E</p><text-author>A</text-author></epigraph>\
             <image l:href='#pic' alt='Alt'/><annotation><p>Ann</p></annotation>\
             <subtitle>Sub</subtitle>\
             <poem><title><p>Poem</p></title><epigraph><p>PE</p></epigraph>\
               <stanza><title><p>I</p></title><subtitle>s</subtitle><v>line 1</v><v>line 2</v></stanza>\
               <text-author>Poet</text-author><date value='1830-01-01'>1830</date></poem>\
             <cite id='q'><p>Q</p><text-author>QA</text-author></cite>\
             <table><tr><th colspan='2' align='center'>H</th></tr>\
               <tr><td rowspan='x'>a</td><td align='right' colspan='99999'>b</td></tr></table>\
             <empty-line/></section>",
        );
        let blocks = &body.children[0].blocks;
        assert_eq!(
            kinds(blocks),
            [
                "epigraph",
                "image",
                "annotation",
                "subtitle",
                "poem",
                "cite",
                "table",
                "empty_line"
            ]
        );
        let BlockKind::Image(image) = &blocks[1].kind else {
            panic!()
        };
        assert_eq!(
            (image.id.as_str(), image.alt.as_deref()),
            ("pic", Some("Alt"))
        );
        let BlockKind::Poem {
            title,
            blocks: poem,
        } = &blocks[4].kind
        else {
            panic!()
        };
        assert_eq!(kinds(title), ["paragraph"]);
        assert_eq!(kinds(poem), ["epigraph", "stanza", "text_author", "date"]);
        let BlockKind::Stanza {
            title,
            blocks: stanza,
        } = &poem[1].kind
        else {
            panic!()
        };
        assert_eq!(title[0].plain_text(), "I");
        assert_eq!(kinds(stanza), ["subtitle", "verse", "verse"]);
        assert_eq!(poem[3].plain_text(), "1830");
        assert_eq!(blocks[5].anchor.as_deref(), Some("q"));
        let BlockKind::Table(table) = &blocks[6].kind else {
            panic!()
        };
        let header = &table.rows[0].cells[0];
        assert!(header.header);
        assert_eq!((header.colspan, header.align), (2, Some(Align::Center)));
        let cells = &table.rows[1].cells;
        assert_eq!((cells[0].rowspan, cells[0].header), (1, false));
        assert_eq!(
            (cells[1].colspan, cells[1].align),
            (1000, Some(Align::Right))
        );
    }

    #[test]
    fn notes_bodies_and_images() {
        let png = super::super::tests::PNG_1X1;
        let content = content(&format!(
            "<body><section><p>See<a l:href='#n1' type='note'>1</a></p></section></body>\
             <body name='notes'><title><p>Notes</p></title>\
               <section id='n1'><title><p>1</p></title><p>The note.</p></section></body>\
             <binary id='pic' content-type='image/png'>{png}</binary>\
             <binary id='pic'>AAAA</binary><binary id='bad'>%%%</binary><binary>{png}</binary>"
        ));
        assert_eq!(content.bodies().len(), 2);
        assert_eq!(content.bodies()[0].kind, BodyKind::Main);
        assert_eq!(content.bodies()[1].kind, BodyKind::Notes);
        let note = &content.bodies()[1].section.children[0];
        assert_eq!(note.anchor.as_deref(), Some("n1"));
        assert_eq!(note.blocks[0].plain_text(), "The note.");
        let images: Vec<&str> = content.images().keys().map(String::as_str).collect();
        assert_eq!(
            images,
            ["pic"],
            "first binary per id wins, invalid ones skipped"
        );
        assert_eq!(content.images()["pic"].mime, "image/png");
    }

    #[test]
    fn unknown_elements_keep_their_text() {
        let body = main_section(
            "<section><div><p>Inside div.</p></div><custom>Custom <strong>text</strong></custom>\
             <empty/>Stray text<strong>Bold line</strong></section>",
        );
        let texts: Vec<String> = body.children[0]
            .blocks
            .iter()
            .map(Block::plain_text)
            .collect();
        assert_eq!(
            texts,
            ["Inside div.", "Custom text", "Stray text", "Bold line"]
        );
    }

    #[test]
    fn misplaced_titles_and_sections_are_kept() {
        let body = main_section(
            "<section><p>P</p><title><p>Late title</p></title>\
             <cite><section><title><p>Inner</p></title><p>Q</p></section></cite></section>",
        );
        let blocks = &body.children[0].blocks;
        assert_eq!(kinds(blocks), ["paragraph", "subtitle", "cite"]);
        let BlockKind::Cite(cite) = &blocks[2].kind else {
            panic!()
        };
        assert_eq!(kinds(cite), ["subtitle", "paragraph"]);
    }

    #[test]
    fn images_without_local_links_are_skipped() {
        let body = main_section(
            "<section><image/><image l:href='http://x/y.png'/><image l:href='#'/><p>x</p></section>",
        );
        assert_eq!(kinds(&body.children[0].blocks), ["paragraph"]);
    }

    #[test]
    fn deep_nesting_is_flattened_not_overflowing() {
        // Deeper than MAX_DEPTH but within MAX_XML_DEPTH.
        let depth = 200;
        let open = "<section><p>level</p>".repeat(depth);
        let close = "</section>".repeat(depth);
        let content = crate::format::run_with_parser_stack(move || {
            content(&format!("<body>{open}<p>deepest</p>{close}</body>"))
        });
        let mut text = String::new();
        content.for_each_block(|b| text.push_str(&b.plain_text()));
        assert!(text.ends_with("deepest"), "deep text must be kept");
        assert!(
            content.block_count() < depth,
            "beyond MAX_DEPTH content is flattened"
        );
    }

    #[test]
    fn empty_and_bodiless_documents() {
        assert!(content("").bodies().is_empty());
        let body = main_section("");
        assert_eq!(body, Section::default());
    }
}
