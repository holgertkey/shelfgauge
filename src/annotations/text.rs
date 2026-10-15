//! The text of a book as the reader page shows it, for anchoring annotations.
//!
//! An annotation's offsets count Unicode code points into a block's *rendered text*:
//! the `textContent` of the element `id="b<n>"` that [`render_part`] emits. The
//! renderer puts no whitespace between tags, so a container's text (epigraph, poem,
//! cite, ...) is the plain concatenation of its children's texts, a table's text the
//! concatenation of its cells, and an image contributes only its caption (the `alt`
//! text is an attribute). [`rendered_text`] reproduces that exactly; a test renders
//! books and compares.
//!
//! Selections are anchored in *leaf* blocks (blocks without nested blocks). The
//! *flat text* of a book joins the texts of all its leaf blocks with `\n`; quotes
//! spanning several blocks and the fingerprints used to find an annotation again are
//! substrings of it.
//!
//! [`render_part`]: crate::render::html::render_part

use std::collections::HashMap;

use crate::format::model::{Block, BlockId, BlockKind, BodyKind, BookContent, Inline, Section};

/// The separator between leaf blocks in the flat text and in multi-block quotes.
pub const BLOCK_SEPARATOR: char = '\n';

/// The rendered text of a block: what a browser returns as the `textContent` of the
/// block's element.
///
/// Line breaks are normalized like the HTML parser does (`\r\n` and `\r` become
/// `\n`) and NUL becomes U+FFFD.
///
/// # Examples
///
/// ```
/// use shelfgauge::annotations::text::rendered_text;
/// use shelfgauge::format::model::{Block, BlockKind, Inline};
///
/// let p = Block::new(BlockKind::Paragraph(vec![
///     Inline::Text("Hello, ".into()),
///     Inline::Emphasis(vec![Inline::Text("world".into())]),
/// ]));
/// assert_eq!(rendered_text(&p), "Hello, world");
/// let cite = Block::new(BlockKind::Cite(vec![Block::paragraph("a"), Block::paragraph("b")]));
/// assert_eq!(rendered_text(&cite), "ab");
/// ```
pub fn rendered_text(block: &Block) -> String {
    let mut out = String::new();
    block_text(block, &mut out);
    normalize(&out)
}

fn block_text(block: &Block, out: &mut String) {
    match &block.kind {
        BlockKind::Paragraph(inlines)
        | BlockKind::Subtitle(inlines)
        | BlockKind::Verse(inlines)
        | BlockKind::TextAuthor(inlines)
        | BlockKind::Preformatted(inlines) => inlines_text(inlines, out),
        BlockKind::Date(text) => out.push_str(text),
        BlockKind::EmptyLine | BlockKind::Rule => {}
        BlockKind::Image(image) => {
            // The renderer emits a caption only for a non-blank title.
            if let Some(title) = image.title.as_deref().filter(|t| !t.trim().is_empty()) {
                out.push_str(title);
            }
        }
        BlockKind::Table(table) => {
            for cell in table.rows.iter().flat_map(|row| &row.cells) {
                inlines_text(&cell.content, out);
            }
        }
        BlockKind::Epigraph(_)
        | BlockKind::Cite(_)
        | BlockKind::Annotation(_)
        | BlockKind::Poem { .. }
        | BlockKind::Stanza { .. }
        | BlockKind::List { .. }
        | BlockKind::ListItem(_) => {
            for child in child_blocks(block) {
                block_text(child, out);
            }
        }
    }
}

fn inlines_text(inlines: &[Inline], out: &mut String) {
    for inline in inlines {
        match inline {
            Inline::Text(text) => out.push_str(text),
            Inline::Emphasis(children)
            | Inline::Strong(children)
            | Inline::Strikethrough(children)
            | Inline::Subscript(children)
            | Inline::Superscript(children)
            | Inline::Code(children)
            | Inline::Link { children, .. } => inlines_text(children, out),
            // `<br>` and `<img>` have no text content.
            Inline::Image(_) | Inline::LineBreak => {}
        }
    }
}

/// The blocks nested directly in `block`, in numbering order.
fn child_blocks(block: &Block) -> impl Iterator<Item = &Block> {
    let (first, second): (&[Block], &[Block]) = match &block.kind {
        BlockKind::Epigraph(blocks)
        | BlockKind::Cite(blocks)
        | BlockKind::Annotation(blocks)
        | BlockKind::ListItem(blocks)
        | BlockKind::List { items: blocks, .. } => (blocks, &[]),
        BlockKind::Poem { title, blocks } | BlockKind::Stanza { title, blocks } => (title, blocks),
        _ => (&[], &[]),
    };
    first.iter().chain(second)
}

/// Line breaks and NUL as the HTML parser turns them into text.
fn normalize(text: &str) -> String {
    if !text.contains(['\r', '\0']) {
        return text.to_owned();
    }
    text.replace("\r\n", "\n")
        .replace(['\r'], "\n")
        .replace('\0', "\u{fffd}")
}

/// The number of code points in `text`.
pub fn char_len(text: &str) -> usize {
    text.chars().count()
}

/// The byte index of code point `n` in `text` (its length if `n` is past the end).
pub fn byte_index(text: &str, n: usize) -> usize {
    text.char_indices().nth(n).map_or(text.len(), |(i, _)| i)
}

/// The first `n` code points of `text`.
pub fn first_chars(text: &str, n: usize) -> &str {
    &text[..byte_index(text, n)]
}

/// The last `n` code points of `text`.
pub fn last_chars(text: &str, n: usize) -> &str {
    let len = char_len(text);
    &text[byte_index(text, len.saturating_sub(n))..]
}

/// What the reader needs to know about one block.
#[derive(Debug, Clone)]
struct BlockInfo {
    /// The rendered text.
    text: String,
    /// Whether the block has no nested blocks.
    leaf: bool,
    /// Whether the block is in a notes body.
    notes: bool,
    /// The innermost titled section around the block (index into `sections`).
    section: Option<usize>,
    /// For a leaf: the byte offset of its text in the flat text.
    flat_start: usize,
}

/// A titled section: its title and first block.
#[derive(Debug, Clone)]
struct SectionInfo {
    title: String,
    first: BlockId,
}

/// The rendered text of a whole book, by block; built once per loaded book.
#[derive(Debug, Clone)]
pub struct BookText {
    /// Indexed by block ordinal − 1.
    blocks: Vec<BlockInfo>,
    sections: Vec<SectionInfo>,
    /// The leaf blocks in order.
    leaves: Vec<BlockId>,
    /// The leaf texts joined with [`BLOCK_SEPARATOR`].
    flat: String,
    /// Code points of main-text leaves before each main-text leaf, and their total.
    main_before: HashMap<BlockId, u64>,
    main_total: u64,
    last_main: Option<BlockId>,
}

impl BookText {
    /// Collects the rendered text of every block of `content`.
    pub fn new(content: &BookContent) -> Self {
        let mut text = Self {
            blocks: Vec::new(),
            sections: Vec::new(),
            leaves: Vec::new(),
            flat: String::new(),
            main_before: HashMap::new(),
            main_total: 0,
            last_main: None,
        };
        for body in content.bodies() {
            let notes = body.kind == BodyKind::Notes;
            // The main body's own title is the book title, not a section; a notes
            // body's title (or "Notes") names its section.
            let section = if notes {
                body.section.first_block_id().map(|first| {
                    text.sections.push(SectionInfo {
                        title: body
                            .section
                            .title_text()
                            .unwrap_or_else(|| "Notes".to_owned()),
                        first,
                    });
                    text.sections.len() - 1
                })
            } else {
                None
            };
            text.section(&body.section, notes, section, true);
        }
        for (i, info) in text.blocks.iter_mut().enumerate() {
            if info.leaf {
                let id = BlockId::new(i as u32 + 1).expect("ordinals start at 1");
                if !text.flat.is_empty() || !text.leaves.is_empty() {
                    text.flat.push(BLOCK_SEPARATOR);
                }
                info.flat_start = text.flat.len();
                text.flat.push_str(&info.text);
                text.leaves.push(id);
                if !info.notes {
                    text.main_before.insert(id, text.main_total);
                    text.main_total += char_len(&info.text) as u64;
                    text.last_main = Some(id);
                }
            }
        }
        text
    }

    fn section(&mut self, section: &Section, notes: bool, around: Option<usize>, root: bool) {
        let mut current = around;
        if !root
            && let (Some(title), Some(first)) = (section.title_text(), section.first_block_id())
        {
            self.sections.push(SectionInfo { title, first });
            current = Some(self.sections.len() - 1);
        }
        for block in section.title.iter().chain(&section.blocks) {
            self.block(block, notes, current);
        }
        for child in &section.children {
            self.section(child, notes, current, false);
        }
    }

    fn block(&mut self, block: &Block, notes: bool, section: Option<usize>) {
        debug_assert_eq!(block.id.ordinal() as usize, self.blocks.len() + 1);
        let leaf = child_blocks(block).next().is_none();
        self.blocks.push(BlockInfo {
            text: rendered_text(block),
            leaf,
            notes,
            section,
            flat_start: 0,
        });
        for child in child_blocks(block) {
            self.block(child, notes, section);
        }
    }

    fn info(&self, id: BlockId) -> Option<&BlockInfo> {
        self.blocks.get((id.ordinal() as usize).checked_sub(1)?)
    }

    /// The number of blocks.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// The rendered text of block `id`.
    pub fn text(&self, id: BlockId) -> Option<&str> {
        self.info(id).map(|info| info.text.as_str())
    }

    /// Whether block `id` exists and has no nested blocks.
    pub fn is_leaf(&self, id: BlockId) -> bool {
        self.info(id).is_some_and(|info| info.leaf)
    }

    /// Whether block `id` is in a notes body.
    pub fn in_notes(&self, id: BlockId) -> bool {
        self.info(id).is_some_and(|info| info.notes)
    }

    /// The title of the innermost titled section around block `id` (a notes body's
    /// title for notes).
    pub fn section_title(&self, id: BlockId) -> Option<&str> {
        let index = self.info(id)?.section?;
        Some(self.sections[index].title.as_str())
    }

    /// The first block of the first section titled `title`.
    pub fn section_start(&self, title: &str) -> Option<BlockId> {
        self.sections
            .iter()
            .find(|s| s.title == title)
            .map(|s| s.first)
    }

    /// The leaf blocks in order.
    pub fn leaves(&self) -> &[BlockId] {
        &self.leaves
    }

    /// The flat text: all leaf texts joined with [`BLOCK_SEPARATOR`].
    pub fn flat(&self) -> &str {
        &self.flat
    }

    /// The byte offset in the flat text of code point `offset` of leaf `id`.
    pub fn flat_position(&self, id: BlockId, offset: usize) -> Option<usize> {
        let info = self.info(id).filter(|info| info.leaf)?;
        if offset > char_len(&info.text) {
            return None;
        }
        Some(info.flat_start + byte_index(&info.text, offset))
    }

    /// The leaf and code point offset of byte `position` of the flat text.
    ///
    /// A position on a separator belongs to the end of the leaf before it.
    pub fn block_at(&self, position: usize) -> Option<(BlockId, usize)> {
        let index = self
            .leaves
            .partition_point(|&id| self.info(id).is_some_and(|i| i.flat_start <= position));
        let id = *self.leaves.get(index.checked_sub(1)?)?;
        let info = self.info(id)?;
        let local = (position - info.flat_start).min(info.text.len());
        let local = (0..=local)
            .rev()
            .find(|&i| info.text.is_char_boundary(i))
            .unwrap_or(0);
        Some((id, char_len(&info.text[..local])))
    }

    /// Reading progress at block `id` in per mille: the share of the main text (notes
    /// excluded) before the block; 1000 at the last main-text block. `None` for a block
    /// in a notes body or an unknown block.
    pub fn progress(&self, id: BlockId) -> Option<u16> {
        let info = self.info(id)?;
        if info.notes {
            return None;
        }
        if Some(id) >= self.last_main && self.last_main.is_some() {
            return Some(1000);
        }
        // A container counts from its first leaf.
        let leaf = self.leaves.iter().find(|&&leaf| leaf >= id)?;
        let before = *self.main_before.get(leaf)?;
        if self.main_total == 0 {
            return Some(0);
        }
        Some(((before * 1000) / self.main_total).min(999) as u16)
    }

    /// The share of the main text (notes excluded) before block `id`, in units of
    /// `1 / scale`: like [`progress`](Self::progress) but finer and without its
    /// rounding rules (`scale` at the end of the main text is never reached by a
    /// block). `None` for a block in a notes body or an unknown block.
    pub fn share_before(&self, id: BlockId, scale: u64) -> Option<u64> {
        let info = self.info(id)?;
        if info.notes {
            return None;
        }
        // A container counts from its first leaf.
        let leaf = self.leaves.iter().find(|&&leaf| leaf >= id)?;
        let before = *self.main_before.get(leaf)?;
        Some((before * scale).checked_div(self.main_total).unwrap_or(0))
    }

    /// The main-text leaf block at `share / scale` of the main text: the last one that
    /// starts at or before that point (the first for 0, the last for `scale` or more).
    /// `None` if the book has no main text.
    pub fn block_at_share(&self, share: u64, scale: u64) -> Option<BlockId> {
        // Rounded up: the inverse of `share_before`, which rounds down.
        let position = (share.min(scale) * self.main_total).div_ceil(scale.max(1));
        self.leaves
            .iter()
            .rev()
            .copied()
            .find(|leaf| self.main_before.get(leaf).is_some_and(|&b| b <= position))
    }

    /// The last leaf block of the main text.
    pub fn last_main_block(&self) -> Option<BlockId> {
        self.last_main
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::model::{Body, ImageRef, Table, TableCell, TableRow};

    fn text(s: &str) -> Vec<Inline> {
        vec![Inline::Text(s.to_owned())]
    }

    #[test]
    fn rendered_text_of_every_kind() {
        let image = |title: Option<&str>| {
            Block::new(BlockKind::Image(ImageRef {
                id: "i".into(),
                alt: Some("alt".into()),
                title: title.map(str::to_owned),
            }))
        };
        assert_eq!(rendered_text(&image(None)), "");
        assert_eq!(rendered_text(&image(Some("  "))), "");
        assert_eq!(rendered_text(&image(Some(" Fig. 1 "))), " Fig. 1 ");
        let table = Block::new(BlockKind::Table(Table {
            rows: vec![TableRow {
                cells: vec![
                    TableCell {
                        content: text("a"),
                        ..TableCell::default()
                    },
                    TableCell {
                        content: text("b"),
                        ..TableCell::default()
                    },
                ],
            }],
        }));
        assert_eq!(rendered_text(&table), "ab");
        let poem = Block::new(BlockKind::Poem {
            title: vec![Block::paragraph("T")],
            blocks: vec![Block::new(BlockKind::Stanza {
                title: vec![],
                blocks: vec![
                    Block::new(BlockKind::Verse(text("v1"))),
                    Block::new(BlockKind::Verse(vec![
                        Inline::Text("v".into()),
                        Inline::Image(ImageRef::default()),
                        Inline::Link {
                            target: crate::format::model::LinkTarget::Anchor("n".into()),
                            note: true,
                            children: text("2"),
                        },
                    ])),
                ],
            })],
        });
        assert_eq!(rendered_text(&poem), "Tv1v2");
        let date = Block::new(BlockKind::Date("a\r\nb\rc\0".into()));
        assert_eq!(rendered_text(&date), "a\nb\nc\u{fffd}");
        let list = Block::new(BlockKind::List {
            ordered: true,
            start: 1,
            items: vec![
                Block::new(BlockKind::ListItem(vec![Block::paragraph("a")])),
                Block::new(BlockKind::ListItem(vec![Block::new(BlockKind::Paragraph(
                    vec![
                        Inline::Text("b".into()),
                        Inline::LineBreak,
                        Inline::Text("c".into()),
                    ],
                ))])),
            ],
        });
        assert_eq!(rendered_text(&list), "abc", "<br> has no text content");
        let pre = Block::new(BlockKind::Preformatted(text("\n  x\r\n")));
        assert_eq!(rendered_text(&pre), "\n  x\n");
        assert_eq!(rendered_text(&Block::new(BlockKind::Rule)), "");
    }

    fn book() -> BookContent {
        let chapter = |title: &str, paragraphs: &[&str]| Section {
            title: vec![Block::paragraph(title)],
            blocks: paragraphs.iter().map(|p| Block::paragraph(*p)).collect(),
            ..Section::default()
        };
        let main = Section {
            title: vec![Block::paragraph("Book")],
            blocks: vec![Block::new(BlockKind::Epigraph(vec![Block::paragraph(
                "Motto",
            )]))],
            children: vec![
                chapter("One", &["Alpha 😀 beta", "Gamma"]),
                chapter("Two", &["Delta"]),
            ],
            ..Section::default()
        };
        let notes = Section {
            children: vec![chapter("1", &["Note text"])],
            ..Section::default()
        };
        BookContent::new(
            vec![Body::main(main), Body::notes(notes)],
            Default::default(),
        )
    }

    fn b(n: u32) -> BlockId {
        BlockId::new(n).unwrap()
    }

    #[test]
    fn blocks_sections_and_flat_text() {
        let text = BookText::new(&book());
        // b1 Book, b2 epigraph, b3 Motto, b4 One, b5 Alpha, b6 Gamma, b7 Two, b8 Delta,
        // b9 "1" (note title), b10 Note text.
        assert_eq!(text.block_count(), 10);
        assert_eq!(text.text(b(2)), Some("Motto"));
        assert!(!text.is_leaf(b(2)));
        assert!(text.is_leaf(b(3)));
        assert_eq!(text.section_title(b(3)), None);
        assert_eq!(text.section_title(b(5)), Some("One"));
        assert_eq!(text.section_title(b(8)), Some("Two"));
        assert_eq!(text.section_title(b(10)), Some("1"));
        assert_eq!(text.section_start("Two"), Some(b(7)));
        assert!(text.in_notes(b(10)) && !text.in_notes(b(8)));
        assert_eq!(
            text.flat(),
            "Book\nMotto\nOne\nAlpha 😀 beta\nGamma\nTwo\nDelta\n1\nNote text"
        );
        assert_eq!(text.leaves().len(), 9);
        let pos = text.flat_position(b(5), 7).unwrap();
        assert_eq!(&text.flat()[pos..pos + 4], " bet");
        assert_eq!(text.block_at(pos), Some((b(5), 7)));
        // The separator after a block belongs to the block's end.
        let end = text.flat_position(b(5), 12).unwrap();
        assert_eq!(text.block_at(end), Some((b(5), 12)));
        assert_eq!(text.block_at(end + 1), Some((b(6), 0)));
        assert_eq!(text.flat_position(b(5), 13), None);
        assert_eq!(
            text.flat_position(b(2), 0),
            None,
            "containers are not anchors"
        );
        assert_eq!(text.text(b(11)), None);
    }

    #[test]
    fn shares_of_the_main_text_and_back() {
        let text = BookText::new(&book());
        // Main leaves and the code points before them: b1 0, b3 4, b4 9, b5 12, b6 24,
        // b7 29, b8 32; 37 in all.
        assert_eq!(text.share_before(b(1), 10_000), Some(0));
        assert_eq!(
            text.share_before(b(2), 37),
            Some(4),
            "a container: its first leaf"
        );
        assert_eq!(text.share_before(b(6), 37), Some(24));
        assert_eq!(text.share_before(b(6), 10_000), Some(24 * 10_000 / 37));
        assert_eq!(text.share_before(b(10), 10_000), None, "notes");
        assert_eq!(text.share_before(b(11), 10_000), None, "unknown");
        assert_eq!(text.block_at_share(0, 37), Some(b(1)));
        assert_eq!(text.block_at_share(23, 37), Some(b(5)));
        assert_eq!(text.block_at_share(24, 37), Some(b(6)));
        assert_eq!(
            text.block_at_share(37, 37),
            Some(b(8)),
            "the end: last main leaf"
        );
        assert_eq!(text.block_at_share(99, 37), Some(b(8)), "never the notes");
        // A share and back gives the same block.
        for n in [1, 3, 4, 5, 6, 7, 8] {
            let share = text.share_before(b(n), 10_000).unwrap();
            assert_eq!(text.block_at_share(share, 10_000), Some(b(n)), "b{n}");
        }
        assert_eq!(
            BookText::new(&BookContent::default()).block_at_share(5, 10),
            None
        );
    }

    #[test]
    fn progress_counts_main_text_only() {
        let text = BookText::new(&book());
        assert_eq!(text.progress(b(1)), Some(0));
        assert_eq!(
            text.progress(b(2)),
            text.progress(b(3)),
            "container = first leaf"
        );
        let mid = text.progress(b(6)).unwrap();
        assert!(mid > 0 && mid < 1000, "{mid}");
        assert_eq!(
            text.progress(b(8)),
            Some(1000),
            "last main block = finished"
        );
        assert_eq!(text.progress(b(10)), None, "notes are not progress");
        assert_eq!(text.last_main_block(), Some(b(8)));
    }

    #[test]
    fn char_helpers() {
        assert_eq!(char_len("a😀b"), 3);
        assert_eq!(first_chars("a😀b", 2), "a😀");
        assert_eq!(last_chars("a😀b", 2), "😀b");
        assert_eq!(last_chars("ab", 5), "ab");
        assert_eq!(byte_index("a😀b", 9), 6);
    }
}
