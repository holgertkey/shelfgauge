//! The format-independent book model produced by every [`BookParser`](super::BookParser).
//!
//! Everything downstream of the parsers (storage, rendering, the web UI) works only
//! with [`BookMetadata`] and [`BookContent`], never with format-specific details.
//!
//! # Block numbering
//!
//! Every [`Block`] of a book has a [`BlockId`]: its 1-based ordinal in document order
//! (`b1`, `b2`, ...). Block IDs are the contract for deep links (`?at=b1234`) and, in V2,
//! for reading-position anchors, so the rule is fixed:
//!
//! - Bodies are numbered in order (the main body first, then notes bodies), then within
//!   each [`Section`]: its `title` lines, its `blocks`, then its `children` sections,
//!   recursively.
//! - A container block (epigraph, cite, annotation, poem, stanza) is numbered *before*
//!   its contents; a poem's or stanza's `title` lines come before its `blocks`.
//! - Every block counts: title lines, empty lines, images, verses, text-author lines.
//! - Not blocks (never numbered): sections and bodies themselves (a TOC entry links to
//!   the section's [`Section::first_block_id`]), inline content including inline images,
//!   and table rows and cells (a table is one block).
//! - IDs never depend on how the book is split into parts for reading.
//! - Annotation blocks in [`BookMetadata`] are numbered separately, starting at `b1`;
//!   they are never mixed with content IDs.
//!
//! Numbering happens in [`BookContent::new`] and [`Annotation::new`], so parsers cannot
//! forget it. Any change to the numbering or to the model requires bumping
//! [`PARSER_VERSION`].

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Version of the parsed-content model and of block numbering.
///
/// Cached [`BookContent`] records the version it was produced with; bumping this
/// constant invalidates every cache. Bump it whenever the model, a parser's output or
/// the block numbering rule changes (the golden numbering test in this module failing is
/// a signal to do so).
pub const PARSER_VERSION: u32 = 1;

/// A block's ordinal in document order, shown as `b<n>` (see the [module docs](self)).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct BlockId(u32);

impl BlockId {
    /// The placeholder of a block that has not been numbered yet.
    pub const UNASSIGNED: Self = Self(0);

    /// Creates the ID with the given ordinal; `None` for 0.
    pub fn new(ordinal: u32) -> Option<Self> {
        (ordinal > 0).then_some(Self(ordinal))
    }

    /// The 1-based ordinal (0 for [`BlockId::UNASSIGNED`]).
    pub fn ordinal(self) -> u32 {
        self.0
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "b{}", self.0)
    }
}

/// The error returned when parsing a string that is not a canonical [`BlockId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid block id {0:?} (expected `b` followed by a number, e.g. `b12`)")]
pub struct ParseBlockIdError(String);

impl FromStr for BlockId {
    type Err = ParseBlockIdError;

    /// Parses the canonical form only: `b` followed by a positive decimal number without
    /// leading zeros.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::format::model::BlockId;
    ///
    /// let id: BlockId = "b12".parse().unwrap();
    /// assert_eq!(id.ordinal(), 12);
    /// assert_eq!(id.to_string(), "b12");
    /// assert!("b012".parse::<BlockId>().is_err());
    /// ```
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || ParseBlockIdError(s.to_owned());
        let digits = s.strip_prefix('b').ok_or_else(err)?;
        if digits.is_empty()
            || digits.starts_with('0')
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(err());
        }
        digits.parse().ok().and_then(Self::new).ok_or_else(err)
    }
}

/// Bibliographic metadata of a book.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookMetadata {
    /// The book title; empty if the source has none (the scanner then uses the file
    /// name).
    pub title: String,
    /// Authors in source order.
    pub authors: Vec<Person>,
    /// Series the book belongs to, in source order (FB2 allows several).
    pub series: Vec<Series>,
    /// Genre codes as given by the source (e.g. FB2 `sf_fantasy`).
    pub genres: Vec<String>,
    /// The annotation (blurb), as numbered blocks.
    pub annotation: Annotation,
    /// The language code of the book text, if known (e.g. `ru`).
    pub language: Option<String>,
    /// The cover image, if the book has one.
    pub cover: Option<Image>,
}

/// A person's name as given by the source; every part is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Person {
    /// First (given) name.
    pub first_name: Option<String>,
    /// Middle name or patronymic.
    pub middle_name: Option<String>,
    /// Last name (surname).
    pub last_name: Option<String>,
    /// Nickname or pen name.
    pub nickname: Option<String>,
}

/// Membership of a book in a series.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Series {
    /// The series name.
    pub name: String,
    /// The book's number in the series, verbatim from the source.
    ///
    /// Kept as text because real files contain values like `1.5` or `2-3`; use
    /// [`Series::numeric_index`] for sorting.
    pub index: Option<String>,
}

impl Series {
    /// The index as a number, if it parses as one (e.g. `"3"` → `3.0`, `"1.5"` → `1.5`).
    pub fn numeric_index(&self) -> Option<f64> {
        let value: f64 = self.index.as_deref()?.trim().parse().ok()?;
        value.is_finite().then_some(value)
    }
}

/// Binary image data with its MIME type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Image {
    /// MIME type, e.g. `image/jpeg`.
    pub mime: String,
    /// The decoded image bytes.
    pub data: Vec<u8>,
}

/// An annotation: a list of blocks numbered on their own, starting at `b1`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Annotation(Vec<Block>);

impl Annotation {
    /// Creates an annotation from blocks and assigns their IDs.
    pub fn new(mut blocks: Vec<Block>) -> Self {
        let mut next = 0;
        for block in &mut blocks {
            number_block(block, &mut next);
        }
        Self(blocks)
    }

    /// The numbered blocks.
    pub fn blocks(&self) -> &[Block] {
        &self.0
    }

    /// Whether the annotation has no blocks.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The text without markup; blocks are separated by newlines.
    pub fn plain_text(&self) -> String {
        blocks_text(&self.0)
    }
}

/// The parsed text of a book: bodies of sections plus the images they reference.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookContent {
    bodies: Vec<Body>,
    images: BTreeMap<String, Image>,
}

impl BookContent {
    /// Creates the content and assigns every block its [`BlockId`] (see the
    /// [module docs](self) for the rule).
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::format::model::*;
    ///
    /// let section = Section {
    ///     title: vec![Block::paragraph("Chapter 1")],
    ///     blocks: vec![Block::paragraph("It was a dark night.")],
    ///     ..Section::default()
    /// };
    /// let content = BookContent::new(vec![Body::main(section)], Default::default());
    /// let ids: Vec<String> = content.blocks().iter().map(|b| b.id.to_string()).collect();
    /// assert_eq!(ids, ["b1", "b2"]);
    /// ```
    pub fn new(mut bodies: Vec<Body>, images: BTreeMap<String, Image>) -> Self {
        let mut next = 0;
        for body in &mut bodies {
            walk_section_mut(&mut body.section, &mut |block| {
                next += 1;
                block.id = BlockId(next);
            });
        }
        Self { bodies, images }
    }

    /// The bodies in document order: the main body first, then notes bodies.
    pub fn bodies(&self) -> &[Body] {
        &self.bodies
    }

    /// Images by their source ID, referenced from [`ImageRef::id`].
    pub fn images(&self) -> &BTreeMap<String, Image> {
        &self.images
    }

    /// Calls `f` for every block in numbering order.
    pub fn for_each_block<'a>(&'a self, mut f: impl FnMut(&'a Block)) {
        for body in &self.bodies {
            walk_section(&body.section, &mut f);
        }
    }

    /// All blocks in numbering order (containers before their contents).
    pub fn blocks(&self) -> Vec<&Block> {
        let mut blocks = Vec::new();
        self.for_each_block(|block| blocks.push(block));
        blocks
    }

    /// The number of blocks (equal to the highest block ID).
    pub fn block_count(&self) -> usize {
        let mut count = 0;
        self.for_each_block(|_| count += 1);
        count
    }
}

/// One body of a book: the main text, or notes/comments referenced from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Body {
    /// What this body contains.
    pub kind: BodyKind,
    /// The root section: the body's own title and epigraphs, with the top-level sections
    /// as `children`. `section_depth = 1` in the reader config refers to these children.
    pub section: Section,
}

impl Body {
    /// A main-text body.
    pub fn main(section: Section) -> Self {
        Self {
            kind: BodyKind::Main,
            section,
        }
    }

    /// A notes body.
    pub fn notes(section: Section) -> Self {
        Self {
            kind: BodyKind::Notes,
            section,
        }
    }
}

/// The kind of a [`Body`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BodyKind {
    /// The main text.
    Main,
    /// Footnotes, endnotes or comments that the main text links to.
    Notes,
}

/// A section of a book: title, content blocks, then nested sections.
///
/// The document order is always `title`, `blocks`, `children`. Parsers must wrap content
/// that appears after nested sections in the source into an untitled child section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    /// The source ID, used as the target of internal links (e.g. a note's `id`).
    pub anchor: Option<String>,
    /// Title lines ([`BlockKind::Paragraph`] or [`BlockKind::EmptyLine`]); empty if
    /// untitled.
    pub title: Vec<Block>,
    /// Content blocks: epigraphs, annotation, image, paragraphs, poems, ...
    pub blocks: Vec<Block>,
    /// Nested sections.
    pub children: Vec<Section>,
}

impl Section {
    /// The title as plain text, with lines joined by spaces; `None` if untitled.
    pub fn title_text(&self) -> Option<String> {
        let text = self
            .title
            .iter()
            .map(Block::plain_text)
            .filter(|line| !line.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        (!text.is_empty()).then_some(text)
    }

    /// The ID of the first block in this section or its descendants, if any.
    pub fn first_block_id(&self) -> Option<BlockId> {
        self.title
            .first()
            .or_else(|| self.blocks.first())
            .map(|block| block.id)
            .or_else(|| self.children.iter().find_map(Section::first_block_id))
    }
}

/// A numbered unit of book content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Block {
    /// The block's ID; [`BlockId::UNASSIGNED`] until numbered by [`BookContent::new`] or
    /// [`Annotation::new`].
    pub id: BlockId,
    /// The source ID of this block, used as the target of internal links.
    pub anchor: Option<String>,
    /// What the block is.
    pub kind: BlockKind,
}

impl Block {
    /// Creates an unnumbered block without an anchor.
    pub fn new(kind: BlockKind) -> Self {
        Self {
            id: BlockId::UNASSIGNED,
            anchor: None,
            kind,
        }
    }

    /// Creates an unnumbered paragraph with plain text.
    pub fn paragraph(text: impl Into<String>) -> Self {
        Self::new(BlockKind::Paragraph(vec![Inline::Text(text.into())]))
    }

    /// The text without markup; nested blocks are separated by newlines.
    pub fn plain_text(&self) -> String {
        let mut out = String::new();
        match &self.kind {
            BlockKind::Paragraph(inlines)
            | BlockKind::Subtitle(inlines)
            | BlockKind::Verse(inlines)
            | BlockKind::TextAuthor(inlines) => inlines_text(inlines, &mut out),
            BlockKind::Date(text) => out.push_str(text),
            BlockKind::EmptyLine | BlockKind::Image(_) => {}
            BlockKind::Epigraph(blocks)
            | BlockKind::Cite(blocks)
            | BlockKind::Annotation(blocks) => {
                out = blocks_text(blocks);
            }
            BlockKind::Poem { title, blocks } | BlockKind::Stanza { title, blocks } => {
                out = blocks_text(title.iter().chain(blocks));
            }
            BlockKind::Table(table) => {
                let rows = table.rows.iter().map(|row| {
                    let mut line = String::new();
                    for (i, cell) in row.cells.iter().enumerate() {
                        if i > 0 {
                            line.push('\t');
                        }
                        inlines_text(&cell.content, &mut line);
                    }
                    line
                });
                out = rows.collect::<Vec<_>>().join("\n");
            }
        }
        out
    }
}

/// The kinds of blocks.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// A paragraph of text.
    Paragraph(Vec<Inline>),
    /// A subtitle inside a section or stanza.
    Subtitle(Vec<Inline>),
    /// An intentional empty line.
    EmptyLine,
    /// A block-level image.
    Image(ImageRef),
    /// One line of a poem.
    Verse(Vec<Inline>),
    /// The author line of an epigraph, cite or poem.
    TextAuthor(Vec<Inline>),
    /// The date of a poem.
    Date(String),
    /// An epigraph: paragraphs, poems, cites, empty lines and text-author lines.
    Epigraph(Vec<Block>),
    /// A quotation: paragraphs, poems, subtitles, tables, empty lines and text-author
    /// lines.
    Cite(Vec<Block>),
    /// A section-level annotation.
    Annotation(Vec<Block>),
    /// A poem.
    Poem {
        /// Title lines.
        title: Vec<Block>,
        /// Epigraphs, stanzas, text-author lines and the date.
        blocks: Vec<Block>,
    },
    /// A stanza of a poem.
    Stanza {
        /// Title lines.
        title: Vec<Block>,
        /// Subtitle and verses.
        blocks: Vec<Block>,
    },
    /// A table (one block; rows and cells are not numbered).
    Table(Table),
}

impl BlockKind {
    /// A short lowercase name of the kind, e.g. `paragraph` or `text_author`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Paragraph(_) => "paragraph",
            Self::Subtitle(_) => "subtitle",
            Self::EmptyLine => "empty_line",
            Self::Image(_) => "image",
            Self::Verse(_) => "verse",
            Self::TextAuthor(_) => "text_author",
            Self::Date(_) => "date",
            Self::Epigraph(_) => "epigraph",
            Self::Cite(_) => "cite",
            Self::Annotation(_) => "annotation",
            Self::Poem { .. } => "poem",
            Self::Stanza { .. } => "stanza",
            Self::Table(_) => "table",
        }
    }
}

/// A reference to an image in [`BookContent::images`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    /// The image ID (without a leading `#`).
    pub id: String,
    /// Alternative text.
    pub alt: Option<String>,
    /// Image title (caption).
    pub title: Option<String>,
}

/// A table.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    /// Rows in order.
    pub rows: Vec<TableRow>,
}

/// A table row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableRow {
    /// Cells in order.
    pub cells: Vec<TableCell>,
}

/// A table cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableCell {
    /// Whether this is a header cell.
    pub header: bool,
    /// Number of columns the cell spans (at least 1).
    pub colspan: u32,
    /// Number of rows the cell spans (at least 1).
    pub rowspan: u32,
    /// Horizontal alignment, if specified.
    pub align: Option<Align>,
    /// Cell content.
    pub content: Vec<Inline>,
}

impl Default for TableCell {
    fn default() -> Self {
        Self {
            header: false,
            colspan: 1,
            rowspan: 1,
            align: None,
            content: Vec::new(),
        }
    }
}

/// Horizontal alignment of a table cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    /// Left-aligned.
    Left,
    /// Centered.
    Center,
    /// Right-aligned.
    Right,
}

/// Inline content of a paragraph-like block.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Inline {
    /// Plain text.
    Text(String),
    /// Emphasized (usually italic) text.
    Emphasis(Vec<Inline>),
    /// Strong (usually bold) text.
    Strong(Vec<Inline>),
    /// Struck-through text.
    Strikethrough(Vec<Inline>),
    /// Subscript.
    Subscript(Vec<Inline>),
    /// Superscript.
    Superscript(Vec<Inline>),
    /// Code (monospace) text.
    Code(Vec<Inline>),
    /// A link.
    Link {
        /// Where the link points.
        target: LinkTarget,
        /// Whether this is a note reference (FB2 `type="note"`).
        note: bool,
        /// The link text.
        children: Vec<Inline>,
    },
    /// An inline image (not a block, not numbered).
    Image(ImageRef),
}

/// The target of a [`Inline::Link`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkTarget {
    /// A section or block in the same book, by its source anchor (without `#`).
    Anchor(String),
    /// An external URL.
    Url(String),
}

/// Assigns the next ordinals to `block` and its nested blocks, in numbering order.
fn number_block(block: &mut Block, next: &mut u32) {
    walk_block_mut(block, &mut |block| {
        *next += 1;
        block.id = BlockId(*next);
    });
}

// The traversal exists twice (shared and mutable borrows), so the child order of a
// container is defined once per borrow kind right next to each other below. The
// invariant test checks that numbering and iteration agree.

/// The child block lists of a container block, in numbering order.
fn children(kind: &BlockKind) -> [&[Block]; 2] {
    match kind {
        BlockKind::Epigraph(blocks) | BlockKind::Cite(blocks) | BlockKind::Annotation(blocks) => {
            [blocks, &[]]
        }
        BlockKind::Poem { title, blocks } | BlockKind::Stanza { title, blocks } => [title, blocks],
        _ => [&[], &[]],
    }
}

/// Mutable variant of [`children`]; must return the lists in the same order.
fn children_mut(kind: &mut BlockKind) -> [&mut [Block]; 2] {
    match kind {
        BlockKind::Epigraph(blocks) | BlockKind::Cite(blocks) | BlockKind::Annotation(blocks) => {
            [blocks, &mut []]
        }
        BlockKind::Poem { title, blocks } | BlockKind::Stanza { title, blocks } => [title, blocks],
        _ => [&mut [], &mut []],
    }
}

/// Calls `f` for every block of `section` in numbering order.
fn walk_section<'a>(section: &'a Section, f: &mut impl FnMut(&'a Block)) {
    for block in section.title.iter().chain(&section.blocks) {
        walk_block(block, f);
    }
    for child in &section.children {
        walk_section(child, f);
    }
}

/// Calls `f` for `block`, then for its nested blocks.
fn walk_block<'a>(block: &'a Block, f: &mut impl FnMut(&'a Block)) {
    f(block);
    for list in children(&block.kind) {
        for child in list {
            walk_block(child, f);
        }
    }
}

/// Mutable variant of [`walk_section`].
fn walk_section_mut(section: &mut Section, f: &mut impl FnMut(&mut Block)) {
    for block in section.title.iter_mut().chain(&mut section.blocks) {
        walk_block_mut(block, f);
    }
    for child in &mut section.children {
        walk_section_mut(child, f);
    }
}

/// Mutable variant of [`walk_block`].
fn walk_block_mut(block: &mut Block, f: &mut impl FnMut(&mut Block)) {
    f(block);
    for list in children_mut(&mut block.kind) {
        for child in list {
            walk_block_mut(child, f);
        }
    }
}

/// Plain text of inline content, appended to `out`.
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
            Inline::Image(_) => {}
        }
    }
}

/// Plain text of blocks, one non-empty block per line.
fn blocks_text<'a>(blocks: impl IntoIterator<Item = &'a Block>) -> String {
    blocks
        .into_iter()
        .map(Block::plain_text)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(s: &str) -> Vec<Inline> {
        vec![Inline::Text(s.to_owned())]
    }

    fn b(kind: BlockKind) -> Block {
        Block::new(kind)
    }

    fn p(s: &str) -> Block {
        Block::paragraph(s)
    }

    /// A book using every block kind, nested containers and a notes body.
    fn sample() -> BookContent {
        let poem = BlockKind::Poem {
            title: vec![p("Poem")],
            blocks: vec![
                b(BlockKind::Epigraph(vec![p("Poem epigraph")])),
                b(BlockKind::Stanza {
                    title: vec![p("Stanza")],
                    blocks: vec![
                        b(BlockKind::Subtitle(text("Stanza subtitle"))),
                        b(BlockKind::Verse(text("First line"))),
                        b(BlockKind::Verse(text("Second line"))),
                    ],
                }),
                b(BlockKind::TextAuthor(text("Poet"))),
                b(BlockKind::Date("1830".to_owned())),
            ],
        };
        let table = Table {
            rows: vec![TableRow {
                cells: vec![
                    TableCell {
                        header: true,
                        content: text("A"),
                        ..TableCell::default()
                    },
                    TableCell {
                        content: text("B"),
                        ..TableCell::default()
                    },
                ],
            }],
        };
        let linked = BlockKind::Paragraph(vec![
            Inline::Text("See ".to_owned()),
            Inline::Link {
                target: LinkTarget::Anchor("n1".to_owned()),
                note: true,
                children: text("[1]"),
            },
            Inline::Image(ImageRef {
                id: "inline.png".to_owned(),
                ..ImageRef::default()
            }),
        ]);
        let chapter1 = Section {
            title: vec![p("Chapter 1"), b(BlockKind::EmptyLine), p("The Beginning")],
            blocks: vec![
                b(BlockKind::Annotation(vec![p("Chapter annotation")])),
                b(BlockKind::Image(ImageRef {
                    id: "pic.jpg".to_owned(),
                    ..ImageRef::default()
                })),
                b(linked),
                b(BlockKind::Subtitle(text("Subtitle"))),
                b(poem),
                b(BlockKind::Cite(vec![
                    p("Quoted"),
                    b(BlockKind::Table(table)),
                    b(BlockKind::TextAuthor(text("Quoted author"))),
                ])),
            ],
            children: vec![Section {
                blocks: vec![p("Nested paragraph")],
                ..Section::default()
            }],
            ..Section::default()
        };
        let chapter2 = Section {
            children: vec![Section {
                title: vec![p("Chapter 2.1")],
                ..Section::default()
            }],
            ..Section::default()
        };
        let main = Section {
            title: vec![p("Book Title")],
            blocks: vec![b(BlockKind::Epigraph(vec![
                p("Book epigraph"),
                b(BlockKind::TextAuthor(text("Someone"))),
            ]))],
            children: vec![chapter1, chapter2],
            ..Section::default()
        };
        let notes = Section {
            title: vec![p("Notes")],
            children: vec![Section {
                anchor: Some("n1".to_owned()),
                title: vec![p("1")],
                blocks: vec![p("A note.")],
                ..Section::default()
            }],
            ..Section::default()
        };
        let mut images = BTreeMap::new();
        images.insert(
            "pic.jpg".to_owned(),
            Image {
                mime: "image/jpeg".to_owned(),
                data: vec![0xff, 0xd8, 0xff],
            },
        );
        BookContent::new(vec![Body::main(main), Body::notes(notes)], images)
    }

    /// Golden test of the numbering contract. If this fails after a change, the block
    /// IDs of existing books changed: bump `PARSER_VERSION`.
    #[test]
    fn numbering_follows_the_documented_order() {
        let content = sample();
        let actual: Vec<(String, &str, String)> = content
            .blocks()
            .iter()
            .map(|block| (block.id.to_string(), block.kind.name(), block.plain_text()))
            .collect();
        let expected = [
            ("b1", "paragraph", "Book Title"),
            ("b2", "epigraph", "Book epigraph\nSomeone"),
            ("b3", "paragraph", "Book epigraph"),
            ("b4", "text_author", "Someone"),
            ("b5", "paragraph", "Chapter 1"),
            ("b6", "empty_line", ""),
            ("b7", "paragraph", "The Beginning"),
            ("b8", "annotation", "Chapter annotation"),
            ("b9", "paragraph", "Chapter annotation"),
            ("b10", "image", ""),
            ("b11", "paragraph", "See [1]"),
            ("b12", "subtitle", "Subtitle"),
            (
                "b13",
                "poem",
                "Poem\nPoem epigraph\nStanza\nStanza subtitle\nFirst line\nSecond line\nPoet\n1830",
            ),
            ("b14", "paragraph", "Poem"),
            ("b15", "epigraph", "Poem epigraph"),
            ("b16", "paragraph", "Poem epigraph"),
            (
                "b17",
                "stanza",
                "Stanza\nStanza subtitle\nFirst line\nSecond line",
            ),
            ("b18", "paragraph", "Stanza"),
            ("b19", "subtitle", "Stanza subtitle"),
            ("b20", "verse", "First line"),
            ("b21", "verse", "Second line"),
            ("b22", "text_author", "Poet"),
            ("b23", "date", "1830"),
            ("b24", "cite", "Quoted\nA\tB\nQuoted author"),
            ("b25", "paragraph", "Quoted"),
            ("b26", "table", "A\tB"),
            ("b27", "text_author", "Quoted author"),
            ("b28", "paragraph", "Nested paragraph"),
            ("b29", "paragraph", "Chapter 2.1"),
            ("b30", "paragraph", "Notes"),
            ("b31", "paragraph", "1"),
            ("b32", "paragraph", "A note."),
        ];
        let expected: Vec<(String, &str, String)> = expected
            .iter()
            .map(|(id, kind, text)| (id.to_string(), *kind, text.to_string()))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn ids_are_consecutive_and_all_assigned() {
        let content = sample();
        let ordinals: Vec<u32> = content.blocks().iter().map(|b| b.id.ordinal()).collect();
        let expected: Vec<u32> = (1..=ordinals.len() as u32).collect();
        assert_eq!(ordinals, expected);
        assert_eq!(content.block_count(), ordinals.len());
    }

    #[test]
    fn numbering_is_deterministic_and_idempotent() {
        assert_eq!(sample(), sample());
        // Re-numbering already numbered content yields the same IDs.
        let content = sample();
        let renumbered = BookContent::new(content.bodies().to_vec(), content.images().clone());
        assert_eq!(renumbered, content);
    }

    #[test]
    fn annotation_is_numbered_on_its_own() {
        let annotation = Annotation::new(vec![
            p("First."),
            b(BlockKind::Cite(vec![p("Quote.")])),
            b(BlockKind::EmptyLine),
        ]);
        let ids: Vec<(u32, &str)> = annotation
            .blocks()
            .iter()
            .flat_map(|block| {
                let mut all = vec![(block.id.ordinal(), block.kind.name())];
                if let BlockKind::Cite(children) = &block.kind {
                    all.extend(children.iter().map(|c| (c.id.ordinal(), c.kind.name())));
                }
                all
            })
            .collect();
        assert_eq!(
            ids,
            [
                (1, "paragraph"),
                (2, "cite"),
                (3, "paragraph"),
                (4, "empty_line")
            ]
        );
        assert_eq!(annotation.plain_text(), "First.\nQuote.");
        assert!(Annotation::default().is_empty());
    }

    #[test]
    fn section_title_and_first_block() {
        let content = sample();
        let main = &content.bodies()[0].section;
        assert_eq!(main.title_text().as_deref(), Some("Book Title"));
        let chapter1 = &main.children[0];
        assert_eq!(
            chapter1.title_text().as_deref(),
            Some("Chapter 1 The Beginning")
        );
        assert_eq!(chapter1.first_block_id(), BlockId::new(5));
        // Untitled, blockless section: the first block of its descendants.
        let chapter2 = &main.children[1];
        assert_eq!(chapter2.title_text(), None);
        assert_eq!(chapter2.first_block_id(), BlockId::new(29));
        assert_eq!(Section::default().first_block_id(), None);
        assert_eq!(content.bodies()[1].kind, BodyKind::Notes);
    }

    #[test]
    fn block_id_display_and_parse_round_trip() {
        for n in [1, 9, 10, 1234, u32::MAX] {
            let id = BlockId::new(n).unwrap();
            assert_eq!(id.to_string(), format!("b{n}"));
            assert_eq!(id.to_string().parse::<BlockId>(), Ok(id));
        }
        assert_eq!(BlockId::new(0), None);
    }

    #[test]
    fn block_id_parse_accepts_only_the_canonical_form() {
        for bad in [
            "",
            "b",
            "b0",
            "b01",
            "B1",
            "1",
            "b-1",
            "b+1",
            "b 1",
            " b1",
            "b1 ",
            "b1a",
            "bb1",
            "b4294967296",
        ] {
            assert!(bad.parse::<BlockId>().is_err(), "{bad:?}");
        }
        let err = "x".parse::<BlockId>().unwrap_err().to_string();
        assert!(err.contains("\"x\""), "{err}");
    }

    #[test]
    fn content_serde_round_trip() {
        let content = sample();
        let json = serde_json::to_string(&content).unwrap();
        let back: BookContent = serde_json::from_str(&json).unwrap();
        assert_eq!(back, content);
    }

    /// Regression: the content cache uses postcard, which is not self-describing, so
    /// the model must not use `skip_serializing_if` (an omitted field shifts every
    /// following one).
    #[test]
    fn content_postcard_round_trip() {
        let mut content = sample();
        content.bodies[0].section.anchor = Some("top".to_owned());
        let bytes = postcard::to_stdvec(&content).unwrap();
        let back: BookContent = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back, content);
    }

    #[test]
    fn metadata_serde_round_trip() {
        let metadata = BookMetadata {
            title: "Война и мир".to_owned(),
            authors: vec![Person {
                first_name: Some("Лев".to_owned()),
                middle_name: Some("Николаевич".to_owned()),
                last_name: Some("Толстой".to_owned()),
                nickname: None,
            }],
            series: vec![Series {
                name: "Epic".to_owned(),
                index: Some("1.5".to_owned()),
            }],
            genres: vec!["prose_classic".to_owned()],
            annotation: Annotation::new(vec![p("About the book.")]),
            language: Some("ru".to_owned()),
            cover: Some(Image {
                mime: "image/png".to_owned(),
                data: vec![0x89, b'P', b'N', b'G'],
            }),
        };
        let json = serde_json::to_string(&metadata).unwrap();
        let back: BookMetadata = serde_json::from_str(&json).unwrap();
        assert_eq!(back, metadata);
    }

    #[test]
    fn series_numeric_index() {
        let series = |index: Option<&str>| Series {
            name: "S".to_owned(),
            index: index.map(str::to_owned),
        };
        assert_eq!(series(Some("3")).numeric_index(), Some(3.0));
        assert_eq!(series(Some(" 1.5 ")).numeric_index(), Some(1.5));
        assert_eq!(series(Some("2-3")).numeric_index(), None);
        assert_eq!(series(Some("NaN")).numeric_index(), None);
        assert_eq!(series(Some("inf")).numeric_index(), None);
        assert_eq!(series(None).numeric_index(), None);
    }

    #[test]
    fn plain_text_skips_markup_and_images() {
        let block = b(BlockKind::Paragraph(vec![
            Inline::Strong(vec![Inline::Emphasis(text("bold italic"))]),
            Inline::Text(" and ".to_owned()),
            Inline::Link {
                target: LinkTarget::Url("https://example.org".to_owned()),
                note: false,
                children: vec![Inline::Code(text("code"))],
            },
            Inline::Image(ImageRef::default()),
            Inline::Superscript(text("2")),
        ]));
        assert_eq!(block.plain_text(), "bold italic and code2");
    }
}
