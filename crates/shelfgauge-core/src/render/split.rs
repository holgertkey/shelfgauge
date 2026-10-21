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
//!   `part_size` characters; a single larger unit is a part of its own. A part shorter
//!   than a quarter of `part_size` (the end of a body, or the rest before a large unit)
//!   joins the previous part of the same body (the next one if it is the first).
//!
//! In every mode a part also starts at each body, and a notes body is never split by
//! sections (a notes body has one section per note), only by size in
//! [`SplitMode::Size`].
//!
//! Finally, in every mode, a part longer than `max_part_size` characters (unless 0) is
//! cut into pieces of about equal size, titled "Title (1/3)", "Title (2/3)", ... (a book
//! without chapters would otherwise be one huge page): as few pieces as the limit
//! allows, each cut at the unit boundary nearest to its share of the text, so there is
//! no short remainder. A piece shorter than a quarter of the average still joins its
//! shorter neighbour (possible next to a unit larger than the limit).

use crate::config::{ReaderConfig, SplitMode};
use crate::format::model::{Block, BlockId, BlockKind, BodyKind, BookContent, Section};

/// The size counted for an image when splitting by size.
const IMAGE_CHARS: usize = 500;

/// Untitled parts with less text than this are merged into the next part.
pub const SMALL_PART_CHARS: usize = 1000;

/// A piece or a part by size shorter than `1 / SHORT_SHARE` of its target size joins a
/// neighbour.
const SHORT_SHARE: usize = 4;

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
/// use shelfgauge_core::config::{ReaderConfig, SplitMode};
/// use shelfgauge_core::format::model::{Block, Body, BookContent, Section};
/// use shelfgauge_core::render::split::split;
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

    match config.split {
        SplitMode::Section => cuts = absorb_small_parts(&units, &cuts),
        SplitMode::Size => {
            cuts = absorb_short_parts(&units, &cuts, config.part_size as usize / SHORT_SHARE);
        }
        SplitMode::None => {}
    }

    let mut parts = Vec::with_capacity(cuts.len());
    for (n, &start) in cuts.iter().enumerate() {
        let end = cuts.get(n + 1).copied().unwrap_or(units.len());
        let title = part_title(&units, start, end, n);
        // Cut a part longer than the limit into pieces of about equal size.
        let pieces = pieces(&units[start..end], config.max_part_size as usize);
        let count = pieces.len();
        for (k, &(from, to)) in pieces.iter().enumerate() {
            parts.push(Part {
                start: units[start + from].first,
                end: units[start + to - 1].last,
                title: if count > 1 {
                    format!("{title} ({}/{count})", k + 1)
                } else {
                    title.clone()
                },
            });
        }
    }
    parts
}

/// The title of the part made of `units[start..end]`, the `n`th part before limiting.
fn part_title(units: &[Unit], start: usize, end: usize, n: usize) -> String {
    let first = &units[start];
    let body_start = start == 0 || units[start - 1].body != first.body;
    let body_end = end == units.len() || units[end].body != first.body;
    if first.notes {
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
    }
}

/// Cuts `units` into pieces of at most `limit` characters (0: no limit) with about the
/// same size each; returns `(from, to)` index ranges.
///
/// There are as few pieces as the limit allows, and each cut is the unit boundary
/// nearest to an equal share of the text among those that keep the piece within the
/// limit and leave a rest that still fits into the remaining pieces. (Cutting greedily
/// at the average size left every piece a bit short, and the shortfall became an
/// extra, tiny last piece.) A single unit larger than the limit is a piece of its own;
/// a piece far shorter than the average (next to such a unit) joins its shorter
/// neighbour.
fn pieces(units: &[Unit], limit: usize) -> Vec<(usize, usize)> {
    let total: usize = units.iter().map(|u| u.chars).sum();
    if limit == 0 || total <= limit {
        return vec![(0, units.len())];
    }
    let len = units.len();
    // `prefix[i]`: the characters before unit `i`.
    let prefix: Vec<usize> = std::iter::once(0)
        .chain(units.iter().scan(0, |sum, unit| {
            *sum += unit.chars;
            Some(*sum)
        }))
        .collect();
    let size = |from: usize, to: usize| prefix[to] - prefix[from];
    // The end of the longest piece from `i` within the limit (at least one unit).
    let reach = |i: usize| {
        let end = prefix.partition_point(|&p| p <= prefix[i] + limit) - 1;
        end.max(i + 1)
    };
    // `need[i]`: the fewest pieces for `units[i..]` (filling each piece is optimal).
    let mut need = vec![0; len + 1];
    for i in (0..len).rev() {
        need[i] = 1 + need[reach(i)];
    }
    let count = need[0];
    let mut bounds = vec![0];
    for k in 1..count {
        let from = *bounds.last().expect("starts with 0");
        // Feasible cuts: within the limit, and the rest fits into `count - k` pieces
        // (`need` never grows to the right, so they form a range).
        let hi = reach(from);
        let lo = (from + 1..=hi)
            .find(|&b| need[b] <= count - k)
            .expect("the farthest cut always leaves a rest that fits");
        let ideal = total * k / count;
        let after = prefix[..=hi].partition_point(|&p| p < ideal).clamp(lo, hi);
        let cut =
            if after > lo && ideal.abs_diff(prefix[after - 1]) <= prefix[after].abs_diff(ideal) {
                after - 1
            } else {
                after
            };
        bounds.push(cut);
    }
    bounds.push(len);
    // Join pieces far shorter than the average to their shorter neighbour.
    let short = total / count / SHORT_SHARE;
    while bounds.len() > 2 {
        let Some(n) = (0..bounds.len() - 1).find(|&n| size(bounds[n], bounds[n + 1]) < short)
        else {
            break;
        };
        let join_previous = n + 2 == bounds.len()
            || (n > 0 && size(bounds[n - 1], bounds[n]) <= size(bounds[n + 1], bounds[n + 2]));
        bounds.remove(if join_previous { n } else { n + 1 });
    }
    bounds.windows(2).map(|w| (w[0], w[1])).collect()
}

/// [`SplitMode::Size`]: joins every part with fewer than `min` characters to the
/// previous part of the same body, or to the next one when it is the body's first.
fn absorb_short_parts(units: &[Unit], cuts: &[usize], min: usize) -> Vec<usize> {
    let mut cuts = cuts.to_vec();
    loop {
        let drop = (0..cuts.len()).find_map(|n| {
            let start = cuts[n];
            let end = cuts.get(n + 1).copied().unwrap_or(units.len());
            if units[start..end].iter().map(|u| u.chars).sum::<usize>() >= min {
                return None;
            }
            let body = units[start].body;
            if n > 0 && units[cuts[n - 1]].body == body {
                Some(n)
            } else if end < units.len() && units[end].body == body {
                Some(n + 1)
            } else {
                None
            }
        });
        match drop {
            Some(n) => {
                cuts.remove(n);
            }
            None => return cuts,
        }
    }
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
/// use shelfgauge_core::format::model::BlockId;
/// use shelfgauge_core::render::split::{Part, part_of};
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
            max_part_size: 0,
            ..ReaderConfig::default()
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

    /// A book without chapters: ten paragraphs of 1000 characters.
    fn flat_book() -> BookContent {
        let paragraphs = (0..10)
            .map(|i| p(&format!("{i}{}", "x".repeat(999))))
            .collect();
        let root = section("Flat", paragraphs, vec![]);
        BookContent::new(vec![Body::main(root)], Default::default())
    }

    fn limited(split: SplitMode, max_part_size: u32) -> ReaderConfig {
        ReaderConfig {
            max_part_size,
            ..config(split, 1, 50_000)
        }
    }

    fn part_chars(content: &BookContent, part: &Part) -> usize {
        content
            .blocks()
            .iter()
            .filter(|b| part.contains(b.id))
            .map(|b| b.plain_text().chars().count())
            .sum()
    }

    #[test]
    fn long_parts_are_cut_into_equal_pieces() {
        let content = flat_book();
        for split_mode in [SplitMode::None, SplitMode::Section, SplitMode::Size] {
            let parts = split(&content, &limited(split_mode, 3000));
            assert_valid(&content, &parts);
            // 10 004 characters, at most 3 000 per part: four pieces (not five), cut at
            // the paragraph boundaries nearest to 2 501, 5 002 and 7 503.
            let sizes: Vec<usize> = parts.iter().map(|p| part_chars(&content, p)).collect();
            assert!(
                sizes.iter().all(|&n| n <= 3000),
                "{split_mode:?}: {sizes:?}"
            );
            // The first piece also holds the title "Flat".
            assert_eq!(sizes, [2004, 3000, 2000, 3000], "{split_mode:?}");
            let titles: Vec<&str> = parts.iter().map(|p| p.title.as_str()).collect();
            assert_eq!(titles[0], "Flat (1/4)", "{split_mode:?}");
            assert_eq!(titles[3], "Flat (4/4)");
        }
    }

    #[test]
    fn short_parts_and_disabled_limit_are_left_alone() {
        let content = flat_book();
        let whole = split(&content, &limited(SplitMode::Section, 0));
        assert_eq!(whole.len(), 1, "0 disables the limit");
        assert_eq!(whole[0].title, "Flat");
        let roomy = split(&content, &limited(SplitMode::Section, 1_000_000));
        assert_eq!(roomy, whole);
        // Only the long parts of a book are cut.
        let parts = split(&book(), &limited(SplitMode::Section, 100));
        assert_valid(&book(), &parts);
        assert!(parts.iter().any(|p| p.title == "Chapter 3"), "{parts:?}");
        assert!(
            parts.iter().any(|p| p.title.starts_with("Part One (")),
            "{parts:?}"
        );
    }

    #[test]
    fn a_single_unit_larger_than_the_limit_stays_whole() {
        let content = flat_book();
        let parts = split(&content, &limited(SplitMode::Section, 10));
        assert_valid(&content, &parts);
        // Each paragraph alone, none cut; the short title joins the first one.
        assert_eq!(parts.len(), 10);
        assert_eq!(part_chars(&content, &parts[0]), 1004);
        assert!(parts[1..].iter().all(|p| part_chars(&content, p) == 1000));
    }

    /// An untitled main body with one paragraph per size.
    fn sized_book(sizes: &[usize]) -> BookContent {
        let paragraphs = sizes
            .iter()
            .enumerate()
            .map(|(i, &n)| {
                let text = format!("{i}:");
                p(&format!("{text}{}", "x".repeat(n - text.len())))
            })
            .collect();
        BookContent::new(
            vec![Body::main(section("", paragraphs, vec![]))],
            Default::default(),
        )
    }

    fn sizes(content: &BookContent, parts: &[Part]) -> Vec<usize> {
        parts.iter().map(|p| part_chars(content, p)).collect()
    }

    #[test]
    fn pieces_leave_no_short_remainder() {
        // Regression: 301 000 characters with a limit of 300 000 were cut greedily at
        // 150 500 into 150 000 + 150 000 + 1 000.
        let content = sized_book(&[1000; 301]);
        let parts = split(&content, &limited(SplitMode::None, 300_000));
        assert_valid(&content, &parts);
        assert_eq!(sizes(&content, &parts), [150_000, 151_000]);
        assert_eq!(parts[1].title, "Part 1 (2/2)");
    }

    #[test]
    fn pieces_are_as_few_as_the_limit_allows_and_never_short() {
        // Paragraph sizes from a fixed pseudo-random sequence (no `rand` dependency).
        let mut seed = 0x2545_f491_u64;
        let mut next = |max: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % max
        };
        for round in 0..200 {
            let count = 1 + next(400) as usize;
            let sizes_in: Vec<usize> = (0..count).map(|_| 10 + next(2000) as usize).collect();
            let total: usize = sizes_in.iter().sum();
            let limit = 2100 + next(60_000) as usize;
            let content = sized_book(&sizes_in);
            let parts = split(&content, &limited(SplitMode::None, limit as u32));
            assert_valid(&content, &parts);
            let got = sizes(&content, &parts);
            // Units are at most 2 010 characters, so the limit can always be kept.
            assert!(
                got.iter().all(|&n| n <= limit),
                "round {round}: {got:?} > {limit}"
            );
            // As few pieces as filling each one up to the limit gives.
            let mut least = 0;
            let mut fill = 0;
            for &n in &sizes_in {
                if fill == 0 || fill + n > limit {
                    least += 1;
                    fill = 0;
                }
                fill += n;
            }
            let least = if total <= limit { 1 } else { least };
            assert_eq!(got.len(), least, "round {round}: {got:?}");
            let average = total / got.len();
            assert!(
                got.len() == 1 || got.iter().all(|&n| n >= average / 4),
                "round {round}: a short piece in {got:?}"
            );
        }
    }

    #[test]
    fn a_short_piece_next_to_a_huge_unit_joins_a_neighbour() {
        // 990 | 20 | 990 with a limit of 1 000: the 20 joins the shorter neighbour (the
        // first), slightly over the limit, rather than being a page of its own.
        let content = sized_book(&[990, 20, 990]);
        let parts = split(&content, &limited(SplitMode::None, 1000));
        assert_valid(&content, &parts);
        assert_eq!(sizes(&content, &parts), [1010, 990]);
        // A unit larger than the limit stays one piece; small neighbours join it.
        let content = sized_book(&[100, 5000, 100]);
        let parts = split(&content, &limited(SplitMode::None, 1000));
        assert_valid(&content, &parts);
        assert_eq!(sizes(&content, &parts), [5200]);
    }

    #[test]
    fn size_mode_joins_short_parts() {
        // A short end of the body joins the previous part.
        let content = sized_book(&[1000, 1000, 1000, 1000, 200]);
        let parts = split(&content, &config(SplitMode::Size, 1, 2000));
        assert_valid(&content, &parts);
        assert_eq!(sizes(&content, &parts), [2000, 2200]);
        // A short rest before a unit larger than `part_size` joins the previous part.
        let content = sized_book(&[1000, 900, 100, 5000, 1000]);
        let parts = split(&content, &config(SplitMode::Size, 1, 2000));
        assert_valid(&content, &parts);
        assert_eq!(sizes(&content, &parts), [2000, 5000, 1000]);
        // A short first part joins the next one; a short whole body stays alone.
        let content = sized_book(&[100, 5000]);
        let parts = split(&content, &config(SplitMode::Size, 1, 2000));
        assert_eq!(sizes(&content, &parts), [5100]);
        let content = sized_book(&[100]);
        assert_eq!(split(&content, &config(SplitMode::Size, 1, 2000)).len(), 1);
    }

    #[test]
    fn empty_book_has_no_parts() {
        assert!(split(&BookContent::default(), &ReaderConfig::default()).is_empty());
    }
}
