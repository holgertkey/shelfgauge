//! Splitting a book into parts for reading.
//!
//! A part is a contiguous range of block IDs. Parts are cut only between *units*: a
//! unit is one entry of a section's `title` or `blocks` together with everything nested
//! in it, so a poem, epigraph or cite is never split. Together the parts cover every
//! block of the book exactly once, in order. Block IDs never depend on the split: only
//! the ranges do.
//!
//! The rules per [`SplitMode`]:
//!
//! - [`SplitMode::None`]: the whole book is one part.
//! - [`SplitMode::Section`]: in the main body, a part starts at every section of depth
//!   `1..=section_depth` (the body's root section has depth 0, its children depth 1).
//!   A part is merged into the next part of the same body if it contains only section
//!   titles (e.g. "Part One" directly followed by its first chapter), or if no titled
//!   section starts in it and it has less than [`SMALL_PART_CHARS`] characters of text
//!   (front matter such as a copyright notice or an epigraph).
//! - [`SplitMode::Size`]: a part is cut before a unit that would make it longer than
//!   `part_size` characters; a single larger unit is a part of its own.
//!
//! In every mode a part also starts at each body, and a notes body is never split by
//! sections (a notes body has one section per note), only by size in
//! [`SplitMode::Size`].

use crate::config::{ReaderConfig, SplitMode};
use crate::format::model::{Block, BlockId, BlockKind, BodyKind, BookContent, Section};

/// The size counted for an image when splitting by size.
const IMAGE_CHARS: usize = 500;

/// Untitled parts with less text than this are merged into the next part.
pub const SMALL_PART_CHARS: usize = 1000;

/// One part of a book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    /// The first block.
    pub start: BlockId,
    /// The last block (inclusive).
    pub end: BlockId,
    /// A title for navigation: the first titled section starting in the part, else the
    /// innermost titled section around its start, else the book title (first part) or
    /// "Part N".
    pub title: String,
}

impl Part {
    /// Whether block `id` is in this part.
    pub fn contains(&self, id: BlockId) -> bool {
        self.start <= id && id <= self.end
    }
}

/// A unit: a top-level entry of a section's title or blocks, with its contents.
#[derive(Debug)]
struct Unit {
    first: BlockId,
    last: BlockId,
    /// Characters for size splitting (images count as [`IMAGE_CHARS`]).
    chars: usize,
    /// Characters of text only.
    text_chars: usize,
    body: usize,
    notes: bool,
    title_line: bool,
    /// The sections (depth ≥ 1, outermost first) that start with this unit.
    starts: Vec<(u32, Option<String>)>,
    /// The innermost titled section (depth ≥ 1) around the unit.
    context: Option<String>,
    /// The title of the body's root section.
    root_title: Option<String>,
}

impl Unit {
    /// The first titled section starting with this unit.
    fn started_title(&self) -> Option<&str> {
        self.starts.iter().find_map(|(_, title)| title.as_deref())
    }
}

/// Splits `content` into parts according to `config`.
///
/// A book without blocks has no parts.
///
/// # Examples
///
/// ```
/// use shelfgauge::config::{ReaderConfig, SplitMode};
/// use shelfgauge::format::model::{Block, Body, BookContent, Section};
/// use shelfgauge::render::split::split;
///
/// let chapter = |t: &str| Section {
///     title: vec![Block::paragraph(t)],
///     blocks: vec![Block::paragraph("Text.")],
///     ..Section::default()
/// };
/// let root = Section { children: vec![chapter("One"), chapter("Two")], ..Section::default() };
/// let content = BookContent::new(vec![Body::main(root)], Default::default());
/// let config = ReaderConfig { split: SplitMode::Section, ..ReaderConfig::default() };
/// let parts = split(&content, &config);
/// assert_eq!(parts.len(), 2);
/// assert_eq!(parts[1].title, "Two");
/// assert_eq!((parts[1].start.to_string(), parts[1].end.to_string()), ("b3".into(), "b4".into()));
/// ```
pub fn split(content: &BookContent, config: &ReaderConfig) -> Vec<Part> {
    let mut units = Vec::new();
    for (body_index, body) in content.bodies().iter().enumerate() {
        let mut collector = Collector {
            units: &mut units,
            body: body_index,
            notes: body.kind == BodyKind::Notes,
            pending: Vec::new(),
            titles: Vec::new(),
            root_title: body.section.title_text(),
        };
        collector.section(&body.section, 0);
    }

    // Indexes of the units that start a part.
    let mut cuts = Vec::new();
    let mut size = 0usize;
    for (i, unit) in units.iter().enumerate() {
        let cut = i == 0
            || unit.body != units[i - 1].body
            || match config.split {
                SplitMode::None => false,
                SplitMode::Section => {
                    !unit.notes
                        && unit
                            .starts
                            .iter()
                            .any(|(depth, _)| *depth <= config.section_depth)
                }
                SplitMode::Size => size > 0 && size + unit.chars > config.part_size as usize,
            };
        if cut {
            cuts.push(i);
            size = 0;
        }
        size += unit.chars;
    }

    if config.split == SplitMode::Section {
        cuts = absorb_small_parts(&units, &cuts);
    }

    cuts.iter()
        .enumerate()
        .map(|(n, &start)| {
            let end = cuts.get(n + 1).copied().unwrap_or(units.len());
            let first = &units[start];
            let body_start = start == 0 || units[start - 1].body != first.body;
            let body_end = end == units.len() || units[end].body != first.body;
            let title = if first.notes {
                // A notes body is named by its title, not by its first note.
                first
                    .root_title
                    .clone()
                    .unwrap_or_else(|| "Notes".to_owned())
            } else if body_start && body_end && first.root_title.is_some() {
                first.root_title.clone().unwrap_or_default()
            } else {
                units[start..end]
                    .iter()
                    .find_map(|u| u.started_title().map(str::to_owned))
                    .or_else(|| first.context.clone())
                    .or_else(|| body_start.then(|| first.root_title.clone()).flatten())
                    .unwrap_or_else(|| format!("Part {}", n + 1))
            };
            Part {
                start: first.first,
                end: units[end - 1].last,
                title,
            }
        })
        .collect()
}

/// Drops every cut that would end a part made only of section titles, or an untitled
/// part with little text, so such a part joins the next part of the same body.
fn absorb_small_parts(units: &[Unit], cuts: &[usize]) -> Vec<usize> {
    let mut result: Vec<usize> = Vec::with_capacity(cuts.len());
    let mut absorbing = false;
    for (n, &start) in cuts.iter().enumerate() {
        if !absorbing {
            result.push(start);
        }
        let end = cuts.get(n + 1).copied().unwrap_or(units.len());
        let part_start = *result.last().expect("the first cut is always kept");
        let part = &units[part_start..end];
        // A run of titles keeps absorbing even after absorbed front matter.
        let small = units[start..end].iter().all(|u| u.title_line)
            || (part.iter().all(|u| u.started_title().is_none())
                && part.iter().map(|u| u.text_chars).sum::<usize>() < SMALL_PART_CHARS);
        absorbing = small && end < units.len() && units[end].body == units[part_start].body;
    }
    result
}

/// The index of the part containing block `id`.
///
/// # Examples
///
/// ```
/// use shelfgauge::format::model::BlockId;
/// use shelfgauge::render::split::{Part, part_of};
///
/// let id = |n| BlockId::new(n).unwrap();
/// let parts = [
///     Part { start: id(1), end: id(4), title: "A".into() },
///     Part { start: id(5), end: id(9), title: "B".into() },
/// ];
/// assert_eq!(part_of(&parts, id(5)), Some(1));
/// assert_eq!(part_of(&parts, id(10)), None);
/// ```
pub fn part_of(parts: &[Part], id: BlockId) -> Option<usize> {
    let i = parts.partition_point(|part| part.end < id);
    parts.get(i).filter(|part| part.contains(id)).map(|_| i)
}

struct Collector<'a> {
    units: &'a mut Vec<Unit>,
    body: usize,
    notes: bool,
    /// Sections started but without a unit yet.
    pending: Vec<(u32, Option<String>)>,
    /// Titles of the enclosing sections of depth ≥ 1 (`None` for untitled ones).
    titles: Vec<Option<String>>,
    root_title: Option<String>,
}

impl Collector<'_> {
    fn section(&mut self, section: &Section, depth: u32) {
        let title = section.title_text();
        if depth > 0 {
            self.pending.push((depth, title.clone()));
            self.titles.push(title);
        }
        for block in &section.title {
            self.unit(block, true);
        }
        for block in &section.blocks {
            self.unit(block, false);
        }
        for child in &section.children {
            self.section(child, depth + 1);
        }
        if depth > 0 {
            self.titles.pop();
        }
    }

    fn unit(&mut self, block: &Block, title_line: bool) {
        let (mut chars, mut text_chars) = (0, 0);
        block.walk(|b| match &b.kind {
            BlockKind::Image(_) => chars += IMAGE_CHARS,
            // Containers are counted through their contents.
            BlockKind::Epigraph(_)
            | BlockKind::Cite(_)
            | BlockKind::Annotation(_)
            | BlockKind::Poem { .. }
            | BlockKind::Stanza { .. } => {}
            _ => {
                let n = b.plain_text().chars().count();
                text_chars += n;
                chars += n.max(1);
            }
        });
        self.units.push(Unit {
            first: block.id,
            last: block.last_id(),
            chars,
            text_chars,
            body: self.body,
            notes: self.notes,
            title_line,
            starts: std::mem::take(&mut self.pending),
            context: self.titles.iter().rev().find_map(Clone::clone),
            root_title: self.root_title.clone(),
        });
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::format::model::{Body, Inline};

    fn p(text: &str) -> Block {
        Block::paragraph(text)
    }

    fn section(title: &str, blocks: Vec<Block>, children: Vec<Section>) -> Section {
        Section {
            title: if title.is_empty() {
                Vec::new()
            } else {
                vec![p(title)]
            },
            blocks,
            children,
            ..Section::default()
        }
    }

    /// Main body: book title, epigraph, an untitled section, Part One (title only) with
    /// chapters 1 and 2 (chapter 2 with a poem and a subsection), chapter 3; a notes body
    /// with 3 notes.
    pub(crate) fn book() -> BookContent {
        let poem = Block::new(BlockKind::Poem {
            title: vec![p("Poem")],
            blocks: vec![
                Block::new(BlockKind::Verse(vec![Inline::Text("Line one".into())])),
                Block::new(BlockKind::Verse(vec![Inline::Text("Line two".into())])),
            ],
        });
        let epigraph = Block::new(BlockKind::Epigraph(vec![p("Epigraph")]));
        let chapter1 = section(
            "Chapter 1",
            vec![p(&"a".repeat(40)), p(&"b".repeat(40))],
            vec![],
        );
        let sub = section("Section 2.1", vec![p(&"d".repeat(30))], vec![]);
        let chapter2 = section("Chapter 2", vec![p(&"c".repeat(30)), poem], vec![sub]);
        let part_one = section("Part One", vec![], vec![chapter1, chapter2]);
        let chapter3 = section("Chapter 3", vec![p(&"e".repeat(10))], vec![]);
        let copyright = section("", vec![p("Copyright")], vec![]);
        let root = section("Book", vec![epigraph], vec![copyright, part_one, chapter3]);
        let notes = section(
            "Notes",
            vec![],
            (1..=3)
                .map(|n| section(&n.to_string(), vec![p(&format!("Note {n}"))], vec![]))
                .collect(),
        );
        BookContent::new(
            vec![Body::main(root), Body::notes(notes)],
            Default::default(),
        )
    }

    fn config(split: SplitMode, section_depth: u32, part_size: u32) -> ReaderConfig {
        ReaderConfig {
            split,
            section_depth,
            part_size,
        }
    }

    fn ranges(parts: &[Part]) -> Vec<(u32, u32)> {
        parts
            .iter()
            .map(|p| (p.start.ordinal(), p.end.ordinal()))
            .collect()
    }

    /// Parts are contiguous, cover `1..=N`, never cut a container, and `part_of`
    /// finds every block.
    fn assert_valid(content: &BookContent, parts: &[Part]) {
        let n = content.block_count() as u32;
        assert!(!parts.is_empty());
        assert_eq!(parts[0].start.ordinal(), 1);
        assert_eq!(parts.last().unwrap().end.ordinal(), n);
        for pair in parts.windows(2) {
            assert_eq!(
                pair[0].end.ordinal() + 1,
                pair[1].start.ordinal(),
                "{parts:?}"
            );
        }
        content.for_each_block(|block| {
            let i = part_of(parts, block.id).unwrap();
            assert!(
                parts[i].contains(block.last_id()),
                "{} is cut: {parts:?}",
                block.id
            );
        });
    }

    #[test]
    fn every_mode_covers_every_block_once() {
        let content = book();
        for cfg in [
            config(SplitMode::None, 1, 1),
            config(SplitMode::Section, 1, 1),
            config(SplitMode::Section, 2, 1),
            config(SplitMode::Section, 9, 1),
            config(SplitMode::Size, 1, 1),
            config(SplitMode::Size, 1, 50),
            config(SplitMode::Size, 1, 100_000),
        ] {
            let parts = split(&content, &cfg);
            assert_valid(&content, &parts);
        }
    }

    #[test]
    fn none_is_one_part_per_body() {
        let content = book();
        let parts = split(&content, &config(SplitMode::None, 1, 1));
        // b1 Book, b2-b3 epigraph, ... the notes body starts at its title.
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].title, "Notes");
    }

    #[test]
    fn sections_at_depth_one() {
        let content = book();
        let parts = split(&content, &config(SplitMode::Section, 1, 1));
        let titles: Vec<&str> = parts.iter().map(|p| p.title.as_str()).collect();
        // The book title, epigraph and the untitled copyright section are too small for
        // a part of their own: they join Part One (with everything below it); then
        // Chapter 3; the notes body as one part.
        assert_eq!(titles, ["Part One", "Chapter 3", "Notes"]);
        assert_eq!(ranges(&parts)[0].0, 1);
    }

    #[test]
    fn sections_at_depth_two_merge_title_only_parts() {
        let content = book();
        let parts = split(&content, &config(SplitMode::Section, 2, 1));
        let titles: Vec<&str> = parts.iter().map(|p| p.title.as_str()).collect();
        // The front matter and "Part One" (only a title) are merged with Chapter 1.
        // Section 2.1 (depth 3) stays inside Chapter 2.
        assert_eq!(titles, ["Part One", "Chapter 2", "Chapter 3", "Notes"]);
        let text: Vec<String> = content
            .blocks()
            .iter()
            .filter(|b| parts[0].contains(b.id) && b.kind.name() == "paragraph")
            .map(|b| b.plain_text())
            .filter(|t| t.len() < 20)
            .collect();
        assert_eq!(
            text,
            ["Book", "Epigraph", "Copyright", "Part One", "Chapter 1"]
        );
    }

    #[test]
    fn deeper_depth_than_the_book_splits_every_section() {
        let content = book();
        let parts = split(&content, &config(SplitMode::Section, 9, 1));
        let titles: Vec<&str> = parts.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(
            titles,
            ["Part One", "Chapter 2", "Section 2.1", "Chapter 3", "Notes"]
        );
    }

    #[test]
    fn size_cuts_between_units_and_keeps_containers_whole() {
        let content = book();
        let parts = split(&content, &config(SplitMode::Size, 1, 1));
        // With a limit of 1 character, every unit is its own part, and the poem
        // (b12..=b15 or similar) is one unit.
        let poem = content
            .blocks()
            .into_iter()
            .find(|b| b.kind.name() == "poem")
            .unwrap();
        let i = part_of(&parts, poem.id).unwrap();
        assert_eq!((parts[i].start, parts[i].end), (poem.id, poem.last_id()));

        let big = split(&content, &config(SplitMode::Size, 1, 100_000));
        assert_eq!(big.len(), 2, "one part per body");
        let medium = split(&content, &config(SplitMode::Size, 1, 50));
        assert!(medium.len() > 2 && medium.len() < parts.len());
    }

    #[test]
    fn block_ids_do_not_depend_on_the_split() {
        let content = book();
        let all: Vec<u32> = content.blocks().iter().map(|b| b.id.ordinal()).collect();
        for cfg in [
            config(SplitMode::None, 1, 1),
            config(SplitMode::Section, 2, 1),
            config(SplitMode::Size, 1, 20),
        ] {
            let parts = split(&content, &cfg);
            let covered: Vec<u32> = parts
                .iter()
                .flat_map(|p| p.start.ordinal()..=p.end.ordinal())
                .collect();
            assert_eq!(covered, all);
        }
    }

    #[test]
    fn none_parts_are_named_after_the_body() {
        let parts = split(&book(), &config(SplitMode::None, 1, 1));
        let titles: Vec<&str> = parts.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["Book", "Notes"]);
    }

    /// Regression: untitled chapters must still be split (only small untitled front
    /// matter is merged).
    #[test]
    fn untitled_chapters_are_parts_of_their_own() {
        let chapter = |n: usize| section("", vec![p(&format!("{n}{}", "x".repeat(2000)))], vec![]);
        let copyright = section("", vec![p("© 2026")], vec![]);
        let root = section(
            "Book",
            vec![],
            vec![copyright, chapter(1), chapter(2), chapter(3)],
        );
        let content = BookContent::new(vec![Body::main(root)], Default::default());
        let parts = split(&content, &config(SplitMode::Section, 1, 1));
        assert_valid(&content, &parts);
        assert_eq!(parts.len(), 3, "{parts:?}");
        let titles: Vec<&str> = parts.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["Book", "Part 2", "Part 3"]);
        assert_eq!(
            ranges(&parts)[0],
            (1, 3),
            "title and copyright join chapter 1"
        );
    }

    #[test]
    fn empty_book_has_no_parts() {
        assert!(split(&BookContent::default(), &ReaderConfig::default()).is_empty());
    }
}
