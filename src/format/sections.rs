//! Building a section tree from a flat list of items and the places where sections
//! start ("cuts").
//!
//! Formats without a section structure of their own (EPUB, plain text) produce a flat
//! list of [`Item`]s (blocks and headings) and a list of [`Cut`]s (from a table of
//! contents, or from the headings with [`heading_cuts`]); [`build_sections`] turns
//! them into the main body's section tree with the same rules for every format.

use std::collections::HashSet;

use crate::format::model::{Block, BlockKind, Section};

/// A top-level unit of the main text.
#[derive(Debug)]
pub(crate) enum Item {
    /// A block of the text.
    Block(Block),
    /// A heading (a paragraph block) with its level 1–6.
    Heading(u8, Block),
}

impl Item {
    /// The block of the item.
    pub(crate) fn block(&self) -> &Block {
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
    pub(crate) fn into_block(self) -> Block {
        match self {
            Item::Block(block) => block,
            Item::Heading(_, block) => as_subtitle(block),
        }
    }
}

/// A paragraph as a subtitle; other blocks unchanged.
pub(crate) fn as_subtitle(block: Block) -> Block {
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
pub(crate) struct Cut {
    /// The section depth, 1 = top level.
    pub(crate) depth: usize,
    /// The table of contents label, if any.
    pub(crate) label: Option<String>,
    /// The index of the item the section starts at.
    pub(crate) index: usize,
}

/// Cuts at the headings: the depth is the rank of the heading level among the levels
/// used. A heading right after a heading of a higher rank is not a cut: it continues
/// that title.
pub(crate) fn heading_cuts(items: &[Item]) -> Vec<Cut> {
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
pub(crate) fn build_sections(items: Vec<Item>, cuts: &[Cut]) -> Section {
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
