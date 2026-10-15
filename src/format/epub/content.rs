//! Spine documents → [`BookContent`].
//!
//! 1. Every linear spine document is converted to a flat list of *items*: top-level
//!    blocks and headings (`h1`–`h6`). Wrapper elements (`div`, `section`, `body`, ...)
//!    are not blocks; their content joins the flow, and a run of inline content outside
//!    a paragraph becomes one paragraph.
//! 2. Footnotes (`aside` and other elements typed `footnote`, `endnote`, `rearnote`, or
//!    with the role `doc-footnote` / `doc-endnote`), note collections (`endnotes`,
//!    `footnotes`, `rearnotes`, `doc-endnotes`) and non-linear spine documents are
//!    moved to a notes body, one section per note.
//! 3. The main body is cut into sections at the entries of the table of contents (see
//!    [`build_sections`]); without a usable table of contents at the headings, and
//!    without headings at each spine document.
//! 4. Element ids become anchors: each id is the key `path#id` (a document itself is
//!    `path`), attached to the first block completed at or after the element, and
//!    links to keys are rewritten to the anchor of that block. A link whose target is
//!    not part of the text is kept as plain text.
//!
//! Images are loaded from the container when first referenced; anything that is not
//! a known raster type (SVG included) is left out.

use std::collections::{BTreeMap, HashMap, HashSet};

use roxmltree::Node;

use super::archive::Archive;
use super::drm::Protection;
use super::nav::{self, Entry};
use super::package::Package;
use super::paths::{has_scheme, resolve};
use super::xml::{OPS_NS, with_document};
use crate::format::ParseError;
use crate::format::model::{
    Align, Block, BlockKind, Body, BookContent, Image, ImageRef, Inline, LinkTarget, Section,
    Table, TableCell, TableRow,
};
use crate::format::xml::{
    extend_merged, normalize_space, push_text, sniff_image_mime, trim_end, trim_start,
};

/// Maximum element nesting converted structurally; deeper content is kept as plain text.
const MAX_DEPTH: usize = 64;

/// Converts the spine of `package` (see the [module docs](self)).
pub(super) fn parse(
    package: &Package,
    archive: &mut Archive<'_>,
    protection: &Protection,
) -> Result<BookContent, ParseError> {
    let toc = nav::read(package, archive);
    let mut converter = Converter::new(Some(archive), &protection.encrypted);
    let mut items = Vec::new();
    let mut doc_starts = Vec::new();
    let mut linear_documents = 0;
    for item in &package.spine {
        let Some(data) = converter.read(&item.path)? else {
            tracing::debug!("skipping the missing spine document {:?}", item.path);
            continue;
        };
        converter.doc = item.path.clone();
        converter.pending.push(item.path.clone());
        let mut doc_items = Vec::new();
        with_document(&data, |doc| {
            let body = doc
                .root_element()
                .children()
                .find(|n| is(*n, "body"))
                .unwrap_or(doc.root_element());
            if item.linear {
                converter.document(body, &mut doc_items);
            } else {
                converter.nonlinear(body);
            }
        })
        .map_err(|e| ParseError::new(format!("{}: {e}", item.path)))?;
        if item.linear {
            linear_documents += 1;
            if !doc_items.is_empty() {
                doc_starts.push(items.len());
            }
            items.extend(doc_items);
        }
    }
    if linear_documents == 0 {
        return Err(ParseError::new("the EPUB has no readable spine documents"));
    }

    // Table of contents entries → item indexes.
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        item.block().walk(|b| {
            if let Some(anchor) = &b.anchor {
                index_of.entry(anchor.as_str()).or_insert(i);
            }
        });
    }
    let resolve_entry = |entry: &Entry| {
        let key = key(&entry.target.0, entry.target.1.as_deref());
        let canonical = converter.aliases.get(&key)?;
        index_of.get(canonical.as_str()).copied()
    };
    let mut cuts: Vec<Cut> = Vec::new();
    for entry in &toc {
        let Some(index) = resolve_entry(entry) else {
            continue;
        };
        // Entries that go back in the text are dropped.
        if cuts.last().is_some_and(|last| index < last.index) {
            continue;
        }
        cuts.push(Cut {
            depth: entry.depth,
            label: Some(entry.label.clone()).filter(|l| !l.is_empty()),
            index,
        });
    }
    if cuts.is_empty() {
        cuts = heading_cuts(&items);
    }
    if cuts.is_empty() {
        cuts = doc_starts
            .iter()
            .map(|&index| Cut {
                depth: 1,
                label: None,
                index,
            })
            .collect();
    }
    let main = build_sections(items, &cuts);

    let mut bodies = vec![Body::main(main)];
    if !converter.notes.is_empty() {
        bodies.push(Body::notes(Section {
            title: converter.notes_title.take().into_iter().collect(),
            children: std::mem::take(&mut converter.notes),
            ..Section::default()
        }));
    }
    converter.resolve_links(&mut bodies);
    // Images of left-out documents (a cover page) are not kept.
    let mut used = HashSet::new();
    for body in &mut bodies {
        for_each_block_mut(&mut body.section, &mut |block| {
            if let BlockKind::Image(image) = &block.kind {
                used.insert(image.id.clone());
            }
            for inlines in inline_lists_mut(&mut block.kind) {
                collect_images(inlines, &mut used);
            }
        });
    }
    let mut images = std::mem::take(&mut converter.images);
    images.retain(|id, _| used.contains(id));
    Ok(BookContent::new(bodies, images))
}

/// The blocks of an HTML fragment (a book description): no images, links only to
/// URLs.
pub(super) fn fragment_blocks(root: Node<'_, '_>) -> Vec<Block> {
    let encrypted = HashSet::new();
    let mut converter = Converter::new(None, &encrypted);
    let mut items = Vec::new();
    converter.flow(root, 0, false, &mut items);
    let mut wrapper = vec![Body::main(Section {
        blocks: items.into_iter().map(Item::into_block).collect(),
        ..Section::default()
    })];
    converter.resolve_links(&mut wrapper);
    std::mem::take(&mut wrapper[0].section.blocks)
}

/// A top-level unit of the main text.
#[derive(Debug)]
enum Item {
    Block(Block),
    /// A heading (a paragraph block) with its level 1–6.
    Heading(u8, Block),
}

impl Item {
    fn block(&self) -> &Block {
        match self {
            Item::Block(block) | Item::Heading(_, block) => block,
        }
    }

    /// The block as a title line: a heading stays a paragraph.
    fn into_paragraph(self) -> Block {
        match self {
            Item::Block(block) | Item::Heading(_, block) => block,
        }
    }

    /// The block; a heading becomes a subtitle.
    fn into_block(self) -> Block {
        match self {
            Item::Block(block) => block,
            Item::Heading(_, block) => as_subtitle(block),
        }
    }
}

fn as_subtitle(block: Block) -> Block {
    match block.kind {
        BlockKind::Paragraph(inlines) => Block {
            kind: BlockKind::Subtitle(inlines),
            ..block
        },
        _ => block,
    }
}

/// Where a section starts: an item index, a depth (1 = top level) and a label.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cut {
    depth: usize,
    label: Option<String>,
    index: usize,
}

/// Cuts at the headings: the depth is the rank of the heading level among the levels
/// used. A heading right after a heading of a higher rank is not a cut: it continues
/// that title.
fn heading_cuts(items: &[Item]) -> Vec<Cut> {
    let mut levels: Vec<u8> = items
        .iter()
        .filter_map(|item| match item {
            Item::Heading(level, _) => Some(*level),
            Item::Block(_) => None,
        })
        .collect();
    levels.sort_unstable();
    levels.dedup();
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item {
            Item::Heading(level, _)
                if index
                    .checked_sub(1)
                    .and_then(|i| match &items[i] {
                        Item::Heading(previous, _) => Some(previous < level),
                        Item::Block(_) => None,
                    })
                    .unwrap_or(false) =>
            {
                None
            }
            Item::Heading(level, _) => Some(Cut {
                depth: levels.iter().position(|l| l == level).unwrap_or(0) + 1,
                label: None,
                index,
            }),
            Item::Block(_) => None,
        })
        .collect()
}

/// Builds the main body's root section from the items and the cuts (sorted by index).
///
/// A cut opens a section at its depth, closing open sections of the same or a greater
/// depth. Its title comes from the headings at the cut, possibly after images and
/// empty lines (which then follow the title). The headings there form *title units*:
/// a heading with the following headings of a lower rank (`h1` "Chapter 1" + `h2` "The
/// Beginning" is one two-line title). Several cuts at one index get the units whose
/// text equals their labels; without such a match the innermost cuts get the units (a
/// "Part One" entry without a heading of its own, followed by "Chapter 1" with one).
/// Without headings, a paragraph whose text equals the label is the title. A section
/// without a title keeps the cut's label as its [`Section::nav_title`].
/// Items before the first cut belong to the root; other headings become subtitles.
fn build_sections(items: Vec<Item>, cuts: &[Cut]) -> Section {
    let targeted: HashSet<usize> = cuts.iter().map(|c| c.index).collect();
    let mut items: Vec<Option<Item>> = items.into_iter().map(Some).collect();
    let mut stack: Vec<(usize, Section)> = vec![(0, Section::default())];
    let mut pos = 0;
    let mut next = 0;
    while pos < items.len() {
        if let Some(first) = cuts.get(next)
            && first.index <= pos
        {
            let count = cuts[next..]
                .iter()
                .take_while(|c| c.index == first.index)
                .count();
            let group = &cuts[next..next + count];
            next += count;
            open_group(&mut items, &mut pos, group, &targeted, &mut stack);
            continue;
        }
        // Titles never reach past the next cut, so it is still ahead.
        let end = cuts
            .get(next)
            .map_or(items.len(), |c| c.index.min(items.len()));
        let top = &mut stack.last_mut().expect("the root is never closed").1;
        for slot in &mut items[pos..end] {
            if let Some(item) = slot.take() {
                top.blocks.push(item.into_block());
            }
        }
        pos = end;
    }
    close(&mut stack, 1);
    stack.pop().map(|(_, root)| root).unwrap_or_default()
}

/// Opens the sections of the cuts at `pos` (see [`build_sections`]).
fn open_group(
    items: &mut [Option<Item>],
    pos: &mut usize,
    group: &[Cut],
    targeted: &HashSet<usize>,
    stack: &mut Vec<(usize, Section)>,
) {
    let headings = headings_at(items, *pos, targeted);
    let units = title_units(items, &headings);
    let assigned = assign_units(items, &units, group);
    let mut leading = Vec::new();
    if let Some(&first) = headings.first() {
        // Leading images and empty lines follow the first title.
        leading = (*pos..first)
            .filter_map(|i| items[i].take().map(Item::into_block))
            .collect();
        *pos = first;
    }
    let mut unit_end = *pos;
    for (k, cut) in group.iter().enumerate() {
        close(stack, cut.depth);
        let mut section = Section::default();
        match assigned[k] {
            Some(u) => {
                for &h in &units[u] {
                    if let Some(item) = items[h].take() {
                        section.title.push(item.into_paragraph());
                    }
                    unit_end = unit_end.max(h + 1);
                }
                section.blocks.append(&mut leading);
            }
            None if k + 1 == group.len() && headings.is_empty() => {
                match label_paragraph(items, *pos, cut, targeted) {
                    Some(p) => {
                        section.blocks = (*pos..p)
                            .filter_map(|i| items[i].take().map(Item::into_block))
                            .collect();
                        if let Some(item) = items[p].take() {
                            section.title.push(item.into_paragraph());
                        }
                        *pos = p + 1;
                        unit_end = *pos;
                    }
                    None => section.nav_title = cut.label.clone(),
                }
            }
            None => section.nav_title = cut.label.clone(),
        }
        stack.push((cut.depth, section));
    }
    // Headings of units no cut took stay in the text as subtitles of the last section.
    let top = &mut stack.last_mut().expect("a cut was opened").1;
    top.blocks.append(&mut leading);
    for slot in &mut items[*pos..unit_end] {
        if let Some(item) = slot.take() {
            top.blocks.push(item.into_block());
        }
    }
    *pos = unit_end.max(*pos);
}

/// A paragraph at `pos` (after images and empty lines) whose text is the cut's label:
/// a chapter title written as a paragraph (`<p class="chapter">CHAPTER II.</p>`).
fn label_paragraph(
    items: &[Option<Item>],
    pos: usize,
    cut: &Cut,
    targeted: &HashSet<usize>,
) -> Option<usize> {
    let label = comparable(cut.label.as_deref()?);
    let mut i = pos;
    while let Some(Some(Item::Block(block))) = items.get(i) {
        if i > pos && targeted.contains(&i) {
            return None;
        }
        match &block.kind {
            BlockKind::Image(_) | BlockKind::EmptyLine => i += 1,
            BlockKind::Paragraph(_)
                if !label.is_empty() && comparable(&block.plain_text()) == label =>
            {
                return Some(i);
            }
            _ => return None,
        }
    }
    None
}

/// Splits consecutive headings into title units: each heading with the following
/// headings of a lower rank (a greater level number).
fn title_units(items: &[Option<Item>], headings: &[usize]) -> Vec<Vec<usize>> {
    let level = |i: usize| match &items[i] {
        Some(Item::Heading(level, _)) => *level,
        _ => 0,
    };
    let mut units: Vec<Vec<usize>> = Vec::new();
    for &h in headings {
        match units.last_mut() {
            Some(unit) if level(h) > level(unit[0]) => unit.push(h),
            _ => units.push(vec![h]),
        }
    }
    units
}

/// Which unit each cut of a group takes as its title (`None`: no title).
///
/// Units whose text equals a cut's label go to that cut (in order); without any such
/// match the units go to the innermost (last) cuts.
fn assign_units(items: &[Option<Item>], units: &[Vec<usize>], group: &[Cut]) -> Vec<Option<usize>> {
    let unit_text = |unit: &[usize]| {
        let text: Vec<String> = unit
            .iter()
            .filter_map(|&h| items[h].as_ref().map(|item| item.block().plain_text()))
            .collect();
        comparable(&text.join(" "))
    };
    let texts: Vec<String> = units.iter().map(|u| unit_text(u)).collect();
    let mut assigned = vec![None; group.len()];
    let mut next_unit = 0;
    for (k, cut) in group.iter().enumerate() {
        let Some(label) = cut.label.as_deref().map(comparable) else {
            continue;
        };
        if let Some(offset) = texts[next_unit..]
            .iter()
            .position(|text| *text == label || text.starts_with(&format!("{label} ")))
        {
            assigned[k] = Some(next_unit + offset);
            next_unit += offset + 1;
        }
    }
    if assigned.iter().all(Option::is_none) {
        let titled = units.len().min(group.len());
        let first = group.len() - titled;
        for (k, slot) in assigned.iter_mut().enumerate().skip(first) {
            *slot = Some(k - first);
        }
    }
    assigned
}

/// Text for comparing titles with labels: lowercase words of letters and digits.
fn comparable(text: &str) -> String {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// The consecutive headings at `pos`, after images and empty lines; none reaches an
/// index another cut points to.
fn headings_at(items: &[Option<Item>], pos: usize, targeted: &HashSet<usize>) -> Vec<usize> {
    let mut found = Vec::new();
    let mut i = pos;
    while let Some(Some(item)) = items.get(i) {
        if i > pos && targeted.contains(&i) {
            break;
        }
        match item {
            Item::Heading(..) => found.push(i),
            Item::Block(block)
                if found.is_empty()
                    && matches!(block.kind, BlockKind::Image(_) | BlockKind::EmptyLine) => {}
            Item::Block(_) => break,
        }
        i += 1;
    }
    found
}

/// Closes the open sections with a depth of at least `depth` (never the root).
fn close(stack: &mut Vec<(usize, Section)>, depth: usize) {
    while stack.len() > 1 && stack.last().is_some_and(|(d, _)| *d >= depth) {
        let (_, section) = stack.pop().expect("checked length");
        stack
            .last_mut()
            .expect("checked length")
            .1
            .children
            .push(section);
    }
}

/// The anchor key of a document (`path`) or an element in it (`path#id`).
fn key(path: &str, fragment: Option<&str>) -> String {
    match fragment {
        Some(fragment) => format!("{path}#{fragment}"),
        None => path.to_owned(),
    }
}

fn is(node: Node<'_, '_>, name: &str) -> bool {
    node.is_element() && node.tag_name().name().eq_ignore_ascii_case(name)
}

/// The `epub:type` tokens and the `role` of an element.
fn types<'a>(node: Node<'a, '_>) -> impl Iterator<Item = &'a str> {
    node.attribute((OPS_NS, "type"))
        .unwrap_or_default()
        .split_whitespace()
        .chain(
            node.attribute("role")
                .unwrap_or_default()
                .split_whitespace(),
        )
}

fn has_type(node: Node<'_, '_>, wanted: &[&str]) -> bool {
    types(node).any(|t| wanted.contains(&t))
}

/// A single note: `aside` typed as a note, or any element typed as a footnote or
/// endnote.
fn is_note(node: Node<'_, '_>) -> bool {
    has_type(
        node,
        &[
            "footnote",
            "endnote",
            "rearnote",
            "doc-footnote",
            "doc-endnote",
        ],
    ) || (is(node, "aside") && has_type(node, &["note"]))
}

/// A collection of notes.
fn is_notes_container(node: Node<'_, '_>) -> bool {
    has_type(
        node,
        &["footnotes", "endnotes", "rearnotes", "doc-endnotes"],
    )
}

/// How an element is converted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Left out with its content.
    Skip,
    /// Inline content.
    Inline,
    /// A block (or a wrapper of blocks).
    Block,
}

fn role(node: Node<'_, '_>) -> Role {
    if node.is_text() {
        return Role::Inline;
    }
    if !node.is_element() || node.has_attribute("hidden") {
        return Role::Skip;
    }
    let name = node.tag_name().name().to_ascii_lowercase();
    match name.as_str() {
        "script" | "style" | "head" | "title" | "meta" | "link" | "template" | "audio"
        | "video" | "iframe" | "object" | "embed" | "canvas" | "map" | "input" | "button"
        | "select" | "textarea" | "noscript" | "rp" => Role::Skip,
        "nav"
            if has_type(
                node,
                &["toc", "landmarks", "page-list", "doc-toc", "doc-pagelist"],
            ) =>
        {
            Role::Skip
        }
        "p" | "div" | "section" | "article" | "main" | "header" | "footer" | "body" | "aside"
        | "nav" | "hgroup" | "figure" | "figcaption" | "blockquote" | "ul" | "ol" | "li" | "dl"
        | "dt" | "dd" | "pre" | "hr" | "table" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
        | "center" | "address" | "details" | "summary" | "fieldset" | "legend" | "form" | "svg"
        | "html" => Role::Block,
        "img" if !is_inline_image(node) => Role::Block,
        _ if is_note(node) || is_notes_container(node) => Role::Block,
        // An inline element around blocks (invalid, but seen) is a wrapper.
        _ if node.children().any(|c| role(c) == Role::Block) => Role::Block,
        _ => Role::Inline,
    }
}

/// An image inside a line of text (with text around it in the same parent).
fn is_inline_image(node: Node<'_, '_>) -> bool {
    node.parent().is_some_and(|parent| {
        parent
            .children()
            .any(|c| c.is_text() && !normalize_space(c.text().unwrap_or_default()).is_empty())
    })
}

struct Converter<'z, 'a> {
    archive: Option<&'z mut Archive<'a>>,
    encrypted: &'z HashSet<String>,
    /// The entry path of the current document.
    doc: String,
    /// Anchor keys waiting for the next completed block.
    pending: Vec<String>,
    /// Every key → the anchor of the block it belongs to.
    aliases: HashMap<String, String>,
    images: BTreeMap<String, Image>,
    /// Image paths that could not be used.
    bad_images: HashSet<String>,
    notes: Vec<Section>,
    notes_title: Option<Block>,
}

impl<'z, 'a> Converter<'z, 'a> {
    fn new(archive: Option<&'z mut Archive<'a>>, encrypted: &'z HashSet<String>) -> Self {
        Self {
            archive,
            encrypted,
            doc: String::new(),
            pending: Vec::new(),
            aliases: HashMap::new(),
            images: BTreeMap::new(),
            bad_images: HashSet::new(),
            notes: Vec::new(),
            notes_title: None,
        }
    }

    fn read(&mut self, path: &str) -> Result<Option<Vec<u8>>, ParseError> {
        match &mut self.archive {
            Some(archive) => archive.read(path),
            None => Ok(None),
        }
    }

    /// Records the id of `node` as pending.
    fn record_id(&mut self, node: Node<'_, '_>) {
        if let Some(id) = node
            .attribute("id")
            .map(str::trim)
            .filter(|id| !id.is_empty())
            && !self.doc.is_empty()
        {
            self.pending.push(key(&self.doc, Some(id)));
        }
    }

    /// A completed block: it takes the pending keys.
    fn finish(&mut self, kind: BlockKind) -> Block {
        let mut block = Block::new(kind);
        if let Some(first) = self.pending.first().cloned() {
            for key in self.pending.drain(..) {
                self.aliases.entry(key).or_insert_with(|| first.clone());
            }
            block.anchor = Some(first);
        }
        block
    }

    /// Converts a linear document's body into items (notes are set aside).
    fn document(&mut self, body: Node<'_, '_>, out: &mut Vec<Item>) {
        if is_notes_container(body) {
            self.notes_container(body, 0);
        } else {
            self.flow(body, 0, true, out);
        }
    }

    /// Converts a non-linear document into one notes section; a document with nothing
    /// but images (a cover page) is left out.
    fn nonlinear(&mut self, body: Node<'_, '_>) {
        let saved = std::mem::take(&mut self.pending);
        self.pending.push(self.doc.clone());
        let mut items = Vec::new();
        self.document(body, &mut items);
        self.pending = saved;
        let has_text = items
            .iter()
            .any(|item| !item.block().plain_text().trim().is_empty());
        if !has_text {
            return;
        }
        let mut section = Section::default();
        let mut items = items.into_iter().peekable();
        if let Some(Item::Heading(..)) = items.peek()
            && let Some(Item::Heading(_, heading)) = items.next()
        {
            section.title.push(heading);
        }
        section.blocks = items.map(Item::into_block).collect();
        self.notes.push(section);
    }

    /// Converts the children of `parent` at block level: runs of inline content become
    /// paragraphs. With `top`, headings are kept as heading items.
    fn flow(&mut self, parent: Node<'_, '_>, depth: usize, top: bool, out: &mut Vec<Item>) {
        let mut run: Vec<Node<'_, '_>> = Vec::new();
        for child in parent.children() {
            match role(child) {
                Role::Skip => {}
                Role::Inline => run.push(child),
                Role::Block => {
                    self.flush(&mut run, depth, out);
                    self.block(child, depth, top, out);
                }
            }
        }
        self.flush(&mut run, depth, out);
    }

    /// The blocks of the children of `parent` (headings as subtitles).
    fn blocks(&mut self, parent: Node<'_, '_>, depth: usize) -> Vec<Block> {
        let mut items = Vec::new();
        self.flow(parent, depth, false, &mut items);
        items.into_iter().map(Item::into_block).collect()
    }

    /// A run of inline nodes as one paragraph (nothing if it has no content).
    fn flush(&mut self, run: &mut Vec<Node<'_, '_>>, depth: usize, out: &mut Vec<Item>) {
        if run.is_empty() {
            return;
        }
        let mut inlines = Vec::new();
        for node in run.drain(..) {
            self.inline(node, depth, &mut inlines);
        }
        trim_start(&mut inlines);
        trim_end(&mut inlines);
        if !inlines.is_empty() {
            out.push(Item::Block(self.finish(BlockKind::Paragraph(inlines))));
        }
    }

    fn block(&mut self, node: Node<'_, '_>, depth: usize, top: bool, out: &mut Vec<Item>) {
        if is_note(node) {
            self.note(node, None, depth);
            return;
        }
        if is_notes_container(node) {
            self.notes_container(node, depth);
            return;
        }
        if depth > MAX_DEPTH {
            self.record_id(node);
            let text = normalize_space(&text_of(node));
            if !text.is_empty() {
                let kind = BlockKind::Paragraph(vec![Inline::Text(text)]);
                out.push(Item::Block(self.finish(kind)));
            }
            return;
        }
        self.record_id(node);
        let next = depth + 1;
        let name = node.tag_name().name().to_ascii_lowercase();
        let kind = match name.as_str() {
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                let inlines = self.inlines(node, next);
                if inlines.is_empty() {
                    return;
                }
                let block = self.finish(BlockKind::Paragraph(inlines));
                out.push(if top {
                    Item::Heading(name.as_bytes()[1] - b'0', block)
                } else {
                    Item::Block(as_subtitle(block))
                });
                return;
            }
            "p" | "figcaption" | "summary" | "legend" | "dt" if !has_block_child(node) => {
                let inlines = self.inlines(node, next);
                if !inlines.is_empty() && only_images(&inlines) {
                    // A paragraph holding just an image is a picture, not a line.
                    for inline in inlines {
                        if let Inline::Image(image) = inline {
                            out.push(Item::Block(self.finish(BlockKind::Image(image))));
                        }
                    }
                    return;
                }
                if inlines.is_empty() || is_blank(&inlines) {
                    if node.children().next().is_none() && name != "p" {
                        return;
                    }
                    BlockKind::EmptyLine
                } else if name == "dt" {
                    BlockKind::Paragraph(vec![Inline::Strong(inlines)])
                } else {
                    BlockKind::Paragraph(inlines)
                }
            }
            "blockquote" => {
                let blocks = self.blocks(node, next);
                if blocks.is_empty() {
                    return;
                }
                BlockKind::Cite(blocks)
            }
            "ul" | "ol" => {
                let mut items = Vec::new();
                let mut stray = Vec::new();
                for child in node.children() {
                    if is(child, "li") {
                        self.flush_stray(&mut stray, next, &mut items);
                        self.record_id(child);
                        let blocks = self.blocks(child, next + 1);
                        items.push(self.finish(BlockKind::ListItem(blocks)));
                    } else if role(child) != Role::Skip {
                        stray.push(child);
                    }
                }
                self.flush_stray(&mut stray, next, &mut items);
                if items.is_empty() {
                    return;
                }
                let ordered = name == "ol";
                let start = node
                    .attribute("start")
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(1);
                BlockKind::List {
                    ordered,
                    start,
                    items,
                }
            }
            "pre" => {
                let mut inlines = Vec::new();
                for child in node.children() {
                    self.pre_inline(child, next, &mut inlines);
                }
                if inlines.is_empty() {
                    return;
                }
                BlockKind::Preformatted(inlines)
            }
            "hr" => {
                out.push(Item::Block(self.finish(BlockKind::Rule)));
                // A repaired document may have content inside `<hr>`.
                self.flow(node, next, top, out);
                return;
            }
            "table" => BlockKind::Table(self.table(node, next)),
            "img" => {
                if let Some(image) = self.image_ref(node, "src") {
                    out.push(Item::Block(self.finish(BlockKind::Image(image))));
                }
                self.flow(node, next, top, out);
                return;
            }
            "svg" => {
                // A cover page wraps its image in SVG; the SVG itself is never used.
                if let Some(image) = node
                    .descendants()
                    .filter(|n| is(*n, "image"))
                    .find_map(|n| self.image_ref(n, "href"))
                {
                    out.push(Item::Block(self.finish(BlockKind::Image(image))));
                }
                return;
            }
            _ => {
                // Wrappers (and inline elements around blocks).
                if has_type(node, &["pagebreak", "doc-pagebreak"]) {
                    return;
                }
                self.flow(node, next, top, out);
                return;
            }
        };
        out.push(Item::Block(self.finish(kind)));
    }

    /// Content of a list outside `li` elements, as an item of its own.
    fn flush_stray(&mut self, stray: &mut Vec<Node<'_, '_>>, depth: usize, items: &mut Vec<Block>) {
        if stray.is_empty() {
            return;
        }
        let mut out = Vec::new();
        let mut run = Vec::new();
        for node in stray.drain(..) {
            match role(node) {
                Role::Inline => run.push(node),
                Role::Block => {
                    self.flush(&mut run, depth, &mut out);
                    self.block(node, depth, false, &mut out);
                }
                Role::Skip => {}
            }
        }
        self.flush(&mut run, depth, &mut out);
        if !out.is_empty() {
            let blocks = out.into_iter().map(Item::into_block).collect();
            items.push(self.finish(BlockKind::ListItem(blocks)));
        }
    }

    /// One note as a notes section; `number` (an item of an ordered note list) becomes
    /// its title.
    fn note(&mut self, node: Node<'_, '_>, number: Option<u32>, depth: usize) {
        let saved = std::mem::take(&mut self.pending);
        self.record_id(node);
        let mut section = Section::default();
        if let Some(number) = number {
            let kind = BlockKind::Paragraph(vec![Inline::Text(number.to_string())]);
            section.title.push(self.finish(kind));
        }
        section.blocks = self.blocks(node, depth + 1);
        // Ids of an empty note go nowhere.
        self.pending = saved;
        if !section.blocks.is_empty() {
            self.notes.push(section);
        }
    }

    /// A collection of notes: list items and notes become notes sections, the first
    /// heading names the notes body, other content is a section of its own.
    fn notes_container(&mut self, node: Node<'_, '_>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let saved = std::mem::take(&mut self.pending);
        self.record_id(node);
        for child in node.children() {
            if role(child) == Role::Skip {
                continue;
            }
            let name = child.tag_name().name().to_ascii_lowercase();
            if matches!(name.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6")
                && self.notes_title.is_none()
            {
                self.record_id(child);
                let inlines = self.inlines(child, depth + 1);
                if !inlines.is_empty() {
                    self.notes_title = Some(self.finish(BlockKind::Paragraph(inlines)));
                }
            } else if name == "ol" || name == "ul" {
                let mut number = child
                    .attribute("start")
                    .and_then(|s| s.trim().parse::<u32>().ok())
                    .unwrap_or(1);
                for item in child.children().filter(|c| is(*c, "li")) {
                    let shown = (name == "ol").then_some(number);
                    self.note(item, shown, depth + 1);
                    number = number.saturating_add(1);
                }
            } else if is_note(child) {
                self.note(child, None, depth + 1);
            } else if matches!(name.as_str(), "div" | "section" | "article" | "aside") {
                self.notes_container(child, depth + 1);
            } else {
                let mut items = Vec::new();
                match role(child) {
                    Role::Inline => self.flush(&mut vec![child], depth + 1, &mut items),
                    _ => self.block(child, depth + 1, false, &mut items),
                }
                if !items.is_empty() {
                    self.notes.push(Section {
                        blocks: items.into_iter().map(Item::into_block).collect(),
                        ..Section::default()
                    });
                }
            }
        }
        self.pending = saved;
    }

    fn table(&mut self, node: Node<'_, '_>, depth: usize) -> Table {
        let mut rows = Vec::new();
        let mut row_nodes = Vec::new();
        for child in node.children() {
            if is(child, "tr") {
                row_nodes.push(child);
            } else if is(child, "thead") || is(child, "tbody") || is(child, "tfoot") {
                row_nodes.extend(child.children().filter(|c| is(*c, "tr")));
            }
        }
        for tr in row_nodes {
            self.record_id(tr);
            let cells = tr
                .children()
                .filter(|c| is(*c, "td") || is(*c, "th"))
                .map(|cell| {
                    self.record_id(cell);
                    let mut content = Vec::new();
                    for (i, part) in cell.children().enumerate() {
                        // Block children of a cell are separated by line breaks.
                        if i > 0 && role(part) == Role::Block {
                            content.push(Inline::LineBreak);
                        }
                        self.inline(part, depth + 1, &mut content);
                    }
                    trim_start(&mut content);
                    trim_end(&mut content);
                    TableCell {
                        header: is(cell, "th"),
                        colspan: span(cell, "colspan"),
                        rowspan: span(cell, "rowspan"),
                        align: match cell.attribute("align").map(str::trim) {
                            Some(a) if a.eq_ignore_ascii_case("left") => Some(Align::Left),
                            Some(a) if a.eq_ignore_ascii_case("center") => Some(Align::Center),
                            Some(a) if a.eq_ignore_ascii_case("right") => Some(Align::Right),
                            _ => None,
                        },
                        content,
                    }
                })
                .collect();
            rows.push(TableRow { cells });
        }
        Table { rows }
    }

    /// The inline content of `node`, trimmed.
    fn inlines(&mut self, node: Node<'_, '_>, depth: usize) -> Vec<Inline> {
        let mut out = Vec::new();
        for child in node.children() {
            self.inline(child, depth, &mut out);
        }
        trim_start(&mut out);
        trim_end(&mut out);
        out
    }

    /// Converts one node at inline level; block elements inside are flattened.
    fn inline(&mut self, node: Node<'_, '_>, depth: usize, out: &mut Vec<Inline>) {
        if node.is_text() {
            push_text(out, node.text().unwrap_or_default());
            return;
        }
        if !node.is_element() || role(node) == Role::Skip {
            return;
        }
        if depth > MAX_DEPTH {
            push_text(out, &text_of(node));
            return;
        }
        self.record_id(node);
        if has_type(node, &["pagebreak", "doc-pagebreak"]) {
            return;
        }
        let name = node.tag_name().name().to_ascii_lowercase();
        let wrap = |this: &mut Self, make: fn(Vec<Inline>) -> Inline, out: &mut Vec<Inline>| {
            let mut children = Vec::new();
            for child in node.children() {
                this.inline(child, depth + 1, &mut children);
            }
            if !children.is_empty() {
                out.push(make(children));
            }
        };
        match name.as_str() {
            "em" | "i" | "cite" | "dfn" | "var" => wrap(self, Inline::Emphasis, out),
            "strong" | "b" => wrap(self, Inline::Strong, out),
            "s" | "strike" | "del" => wrap(self, Inline::Strikethrough, out),
            "sub" => wrap(self, Inline::Subscript, out),
            "sup" => wrap(self, Inline::Superscript, out),
            "code" | "kbd" | "samp" | "tt" => wrap(self, Inline::Code, out),
            "br" => {
                out.push(Inline::LineBreak);
                // A repaired document may have content inside `<br>`.
                for child in node.children() {
                    self.inline(child, depth + 1, out);
                }
            }
            "img" => {
                if let Some(image) = self.image_ref(node, "src") {
                    out.push(Inline::Image(image));
                }
            }
            "a" => {
                let mut children = Vec::new();
                for child in node.children() {
                    self.inline(child, depth + 1, &mut children);
                }
                let href = node.attribute("href").map(str::trim).unwrap_or_default();
                let target = if href.is_empty() {
                    None
                } else if has_scheme(href) {
                    Some(LinkTarget::Url(href.to_owned()))
                } else {
                    resolve(&self.doc, href)
                        .map(|(path, fragment)| LinkTarget::Anchor(key(&path, fragment.as_deref())))
                };
                match target {
                    Some(target) if !children.is_empty() => out.push(Inline::Link {
                        target,
                        note: has_type(node, &["noteref", "doc-noteref"]),
                        children,
                    }),
                    _ => extend_merged(out, children),
                }
            }
            _ => {
                // Spans and other inline elements, and blocks inside inline content.
                for child in node.children() {
                    self.inline(child, depth + 1, out);
                }
            }
        }
    }

    /// Inline content of a `<pre>`: whitespace and line breaks kept.
    fn pre_inline(&mut self, node: Node<'_, '_>, depth: usize, out: &mut Vec<Inline>) {
        if node.is_text() {
            let text = node.text().unwrap_or_default();
            if let Some(Inline::Text(previous)) = out.last_mut() {
                previous.push_str(text);
            } else if !text.is_empty() {
                out.push(Inline::Text(text.to_owned()));
            }
            return;
        }
        if !node.is_element() || role(node) == Role::Skip {
            return;
        }
        self.record_id(node);
        if is(node, "br") {
            push_raw(out, "\n");
            return;
        }
        let mut children = Vec::new();
        if depth > MAX_DEPTH {
            push_raw(&mut children, &text_of(node));
        } else {
            for child in node.children() {
                self.pre_inline(child, depth + 1, &mut children);
            }
        }
        if children.is_empty() {
            return;
        }
        let name = node.tag_name().name().to_ascii_lowercase();
        match name.as_str() {
            "em" | "i" => out.push(Inline::Emphasis(children)),
            "strong" | "b" => out.push(Inline::Strong(children)),
            _ => {
                for child in children {
                    match child {
                        Inline::Text(text) => push_raw(out, &text),
                        other => out.push(other),
                    }
                }
            }
        }
    }

    /// The image an element refers to by `attribute` (any namespace), if it is a
    /// usable raster image.
    fn image_ref(&mut self, node: Node<'_, '_>, attribute: &str) -> Option<ImageRef> {
        let href = node
            .attributes()
            .find(|a| a.name() == attribute)
            .map(|a| a.value().trim())?;
        let (path, _) = resolve(&self.doc, href)?;
        if !self.images.contains_key(&path) {
            if self.bad_images.contains(&path) || self.encrypted.contains(&path) {
                return None;
            }
            let image = match self.read(&path) {
                Ok(Some(data)) => sniff_image_mime(&data).map(|mime| Image {
                    mime: mime.to_owned(),
                    data,
                }),
                Ok(None) => None,
                Err(e) => {
                    tracing::debug!("skipping an image: {e}");
                    None
                }
            };
            match image {
                Some(image) => {
                    self.images.insert(path.clone(), image);
                }
                None => {
                    self.bad_images.insert(path);
                    return None;
                }
            }
        }
        let text = |name: &str| {
            node.attribute(name)
                .map(normalize_space)
                .filter(|t| !t.is_empty())
        };
        Some(ImageRef {
            id: path,
            alt: text("alt"),
            title: text("title"),
        })
    }

    /// Rewrites link targets to block anchors, marks links into the notes body as
    /// note references, turns links that lead nowhere into text, and clears anchors
    /// no link points to.
    fn resolve_links(&self, bodies: &mut [Body]) {
        let mut notes = HashSet::new();
        for body in bodies.iter().skip(1) {
            collect_anchors(&body.section, &mut notes);
        }
        let mut used = HashSet::new();
        for body in bodies.iter_mut() {
            for_each_block_mut(&mut body.section, &mut |block| {
                for inlines in inline_lists_mut(&mut block.kind) {
                    rewrite(inlines, &self.aliases, &notes, &mut used);
                }
            });
        }
        for body in bodies.iter_mut() {
            for_each_block_mut(&mut body.section, &mut |block| {
                if block.anchor.as_ref().is_some_and(|a| !used.contains(a)) {
                    block.anchor = None;
                }
            });
        }
    }
}

/// Appends text to the last text inline without any whitespace handling.
fn push_raw(out: &mut Vec<Inline>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(Inline::Text(previous)) = out.last_mut() {
        previous.push_str(text);
    } else {
        out.push(Inline::Text(text.to_owned()));
    }
}

fn rewrite(
    inlines: &mut Vec<Inline>,
    aliases: &HashMap<String, String>,
    notes: &HashSet<String>,
    used: &mut HashSet<String>,
) {
    let old = std::mem::take(inlines);
    for inline in old {
        match inline {
            Inline::Link {
                target: LinkTarget::Anchor(key),
                note,
                mut children,
            } => {
                rewrite(&mut children, aliases, notes, used);
                match aliases.get(&key) {
                    Some(anchor) => {
                        used.insert(anchor.clone());
                        inlines.push(Inline::Link {
                            note: note || notes.contains(anchor),
                            target: LinkTarget::Anchor(anchor.clone()),
                            children,
                        });
                    }
                    None => extend_merged(inlines, children),
                }
            }
            mut other => {
                if let Some(children) = formatting_children(&mut other) {
                    rewrite(children, aliases, notes, used);
                }
                // A note reference is raised by the reader already: `<sup>` around it
                // would raise it twice.
                if let Inline::Superscript(children) = &mut other
                    && let [Inline::Link { note: true, .. }] = children.as_slice()
                {
                    inlines.append(children);
                    continue;
                }
                inlines.push(other);
            }
        }
    }
}

fn formatting_children(inline: &mut Inline) -> Option<&mut Vec<Inline>> {
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

/// The inline lists of a block (not of its nested blocks).
fn inline_lists_mut(kind: &mut BlockKind) -> Vec<&mut Vec<Inline>> {
    match kind {
        BlockKind::Paragraph(inlines)
        | BlockKind::Subtitle(inlines)
        | BlockKind::Verse(inlines)
        | BlockKind::TextAuthor(inlines)
        | BlockKind::Preformatted(inlines) => vec![inlines],
        BlockKind::Table(table) => table
            .rows
            .iter_mut()
            .flat_map(|row| row.cells.iter_mut().map(|cell| &mut cell.content))
            .collect(),
        _ => Vec::new(),
    }
}

/// The nested block lists of a block.
fn children_mut(kind: &mut BlockKind) -> Vec<&mut Vec<Block>> {
    match kind {
        BlockKind::Epigraph(blocks)
        | BlockKind::Cite(blocks)
        | BlockKind::Annotation(blocks)
        | BlockKind::ListItem(blocks)
        | BlockKind::List { items: blocks, .. } => vec![blocks],
        BlockKind::Poem { title, blocks } | BlockKind::Stanza { title, blocks } => {
            vec![title, blocks]
        }
        _ => Vec::new(),
    }
}

fn for_each_block_mut(section: &mut Section, f: &mut impl FnMut(&mut Block)) {
    fn block(b: &mut Block, f: &mut impl FnMut(&mut Block)) {
        f(b);
        for list in children_mut(&mut b.kind) {
            for child in list {
                block(child, f);
            }
        }
    }
    for b in section.title.iter_mut().chain(&mut section.blocks) {
        block(b, f);
    }
    for child in &mut section.children {
        for_each_block_mut(child, f);
    }
}

fn collect_anchors(section: &Section, out: &mut HashSet<String>) {
    for b in section.title.iter().chain(&section.blocks) {
        b.walk(|b| {
            if let Some(anchor) = &b.anchor {
                out.insert(anchor.clone());
            }
        });
    }
    for child in &section.children {
        collect_anchors(child, out);
    }
}

fn collect_images(inlines: &mut [Inline], used: &mut HashSet<String>) {
    for inline in inlines {
        if let Inline::Image(image) = inline {
            used.insert(image.id.clone());
        } else if let Some(children) = formatting_children(inline) {
            collect_images(children, used);
        }
    }
}

fn has_block_child(node: Node<'_, '_>) -> bool {
    node.children().any(|c| role(c) == Role::Block)
}

/// Whether inline content is only whitespace (no-break spaces included) and line
/// breaks.
fn is_blank(inlines: &[Inline]) -> bool {
    inlines.iter().all(|inline| match inline {
        Inline::Text(text) => text.chars().all(char::is_whitespace),
        Inline::LineBreak => true,
        Inline::Image(_) => false,
        other => {
            let mut other = other.clone();
            formatting_children(&mut other).is_none_or(|c| is_blank(c))
        }
    })
}

/// Whether inline content is images and whitespace only.
fn only_images(inlines: &[Inline]) -> bool {
    inlines.iter().any(|i| matches!(i, Inline::Image(_)))
        && inlines.iter().all(|inline| match inline {
            Inline::Image(_) | Inline::LineBreak => true,
            Inline::Text(text) => text.chars().all(char::is_whitespace),
            _ => false,
        })
}

/// All text of `node`.
fn text_of(node: Node<'_, '_>) -> String {
    node.descendants()
        .filter(Node::is_text)
        .filter_map(|n| n.text())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A `colspan`/`rowspan` value, clamped to 1..=1000 (1 if missing or invalid).
fn span(node: Node<'_, '_>, name: &str) -> u32 {
    node.attribute(name)
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map_or(1, |n| n.clamp(1, 1000))
}
