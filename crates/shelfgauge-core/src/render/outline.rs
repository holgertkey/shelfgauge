//! Figures of the reader page that depend on the whole book: reading progress, chapter
//! ticks, the running head and the language tag.
//!
//! The instance computes them for each page it serves; a publication's manifest
//! carries them, so the reading device never needs the whole book for them.

use serde::{Deserialize, Serialize};

use super::split::Part;
use crate::annotations::text::BookText;
use crate::format::model::{BlockId, BodyKind, BookContent, Section};

/// Positions in the book are shares of the main text in units of `1 / SHARE_SCALE` (the
/// status line's track, `?progress=`).
pub const SHARE_SCALE: u64 = 10_000;

/// Chapter ticks: titled sections down to this depth (1 = top level).
const CHAPTER_DEPTH: u32 = 2;

/// The reading progress (in `1 / SHARE_SCALE` of the main text) at the start of a part
/// and at the start of the next one (`SHARE_SCALE` for the last part of the main text);
/// the reader interpolates between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartProgress {
    /// The share before the part's first block.
    pub start: u64,
    /// The share before the next part's first block, never below `start`.
    pub end: u64,
}

/// The progress over part `index` of `parts`; `None` for a part that starts in the notes
/// (or a part that does not exist).
pub fn part_progress(text: &BookText, parts: &[Part], index: usize) -> Option<PartProgress> {
    let start = text.share_before(parts.get(index)?.start, SHARE_SCALE)?;
    let end = parts
        .get(index + 1)
        .and_then(|next| text.share_before(next.start, SHARE_SCALE))
        .unwrap_or(SHARE_SCALE);
    Some(PartProgress {
        start,
        end: end.max(start),
    })
}

/// A chapter tick on the status line's track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChapterTick {
    /// Where the chapter starts, in `1 / SHARE_SCALE` of the main text.
    pub at: u64,
    /// The chapter title on one line.
    pub title: String,
}

/// The chapter ticks: the titled sections of the main bodies down to depth 2 (sections
/// in top-level sections), in book order.
pub fn chapter_ticks(content: &BookContent, text: &BookText) -> Vec<ChapterTick> {
    fn walk(section: &Section, depth: u32, text: &BookText, ticks: &mut Vec<ChapterTick>) {
        if depth > CHAPTER_DEPTH {
            return;
        }
        if depth > 0
            && let (Some(title), Some(first)) = (section.title_text(), section.first_block_id())
            && let Some(at) = text.share_before(first, SHARE_SCALE)
        {
            ticks.push(ChapterTick { at, title });
        }
        for child in &section.children {
            walk(child, depth + 1, text, ticks);
        }
    }
    let mut ticks = Vec::new();
    for body in content.bodies() {
        if body.kind != BodyKind::Notes {
            walk(&body.section, 0, text, &mut ticks);
        }
    }
    ticks
}

/// The section title in effect where a part starts: the last title at or above
/// `start` in book order (not the book's own title, the main body's), its lines joined
/// with ". " as the reader joins them (`reader.js`, `titleText`). `None` before the first one.
pub fn title_before(content: &BookContent, start: BlockId) -> Option<String> {
    fn walk(section: &Section, start: BlockId, found: &mut Option<String>) {
        match section.title.first() {
            Some(first) if first.id > start => return,
            Some(_) => {
                let lines: Vec<String> = section
                    .title
                    .iter()
                    .map(|line| {
                        line.plain_text()
                            .split_whitespace()
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .filter(|line| !line.is_empty())
                    .collect();
                if !lines.is_empty() {
                    *found = Some(lines.join(". "));
                }
            }
            None => {}
        }
        for child in &section.children {
            walk(child, start, found);
        }
    }
    let mut found = None;
    for body in content.bodies() {
        if body.kind == BodyKind::Main {
            for child in &body.section.children {
                walk(child, start, &mut found);
            }
        } else {
            walk(&body.section, start, &mut found);
        }
    }
    found
}

/// The book's language as an HTML `lang` value: a BCP 47 tag such as `ru` or `en-GB`
/// (`_` is accepted for `-`); anything else (`Русский`, `ru, en`) is dropped.
///
/// Browsers hyphenate and reader views pick a voice by it, so a wrong value is worse
/// than none.
pub fn html_lang(language: Option<&str>) -> Option<String> {
    let tag = language?.trim().replace('_', "-");
    let mut subtags = tag.split('-');
    let primary = subtags.next()?;
    let valid = (2..=3).contains(&primary.len())
        && primary.bytes().all(|b| b.is_ascii_alphabetic())
        && subtags
            .all(|s| (1..=8).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric()));
    valid.then(|| {
        let rest = &tag[primary.len()..];
        format!("{}{rest}", primary.to_ascii_lowercase())
    })
}

#[cfg(test)]
mod tests {
    use super::{html_lang, title_before};
    use crate::format::model::{Block, Body, BookContent, Section};

    fn section(title: &[&str], blocks: &[&str], children: Vec<Section>) -> Section {
        Section {
            title: title.iter().map(|line| Block::paragraph(*line)).collect(),
            blocks: blocks.iter().map(|text| Block::paragraph(*text)).collect(),
            children,
            ..Section::default()
        }
    }

    #[test]
    fn the_running_head_starts_with_the_title_in_effect() {
        let chapter1 = section(&["Chapter 1", "The  Start"], &["a", "b"], vec![]);
        let sub = section(&["2.1"], &["d"], vec![]);
        let chapter2 = section(&["Chapter 2"], &["c"], vec![sub]);
        let root = section(&["Book"], &["intro"], vec![chapter1, chapter2]);
        let notes = section(&["Notes"], &[], vec![section(&["1"], &["Note"], vec![])]);
        let content = BookContent::new(
            vec![Body::main(root), Body::notes(notes)],
            Default::default(),
        );
        let main = &content.bodies()[0].section;
        let notes = &content.bodies()[1].section;
        let head = |id| title_before(&content, id);
        // Before the first chapter: no title (the book's own one does not count).
        assert_eq!(head(main.title[0].id), None);
        assert_eq!(head(main.blocks[0].id), None);
        // A part starting at a title, or inside its section; lines joined with ". ".
        let chapter1 = &main.children[0];
        assert_eq!(
            head(chapter1.title[0].id).as_deref(),
            Some("Chapter 1. The Start")
        );
        assert_eq!(
            head(chapter1.blocks[1].id).as_deref(),
            Some("Chapter 1. The Start")
        );
        // The innermost title above: a subsection's.
        assert_eq!(
            head(main.children[1].children[0].blocks[0].id).as_deref(),
            Some("2.1")
        );
        // The notes have their own titles.
        assert_eq!(head(notes.title[0].id).as_deref(), Some("Notes"));
        assert_eq!(head(notes.children[0].blocks[0].id).as_deref(), Some("1"));
    }

    #[test]
    fn book_languages_become_html_lang_tags() {
        for (language, expected) in [
            ("ru", Some("ru")),
            (" RU ", Some("ru")),
            ("en-GB", Some("en-GB")),
            ("en_US", Some("en-US")),
            ("rus", Some("rus")),
            ("sr-Latn-RS", Some("sr-Latn-RS")),
            ("", None),
            ("r", None),
            ("Русский", None),
            ("russian", None),
            ("ru, en", None),
            ("ru-", None),
            ("ru\"><script>", None),
        ] {
            assert_eq!(
                html_lang(Some(language)).as_deref(),
                expected,
                "{language:?}"
            );
        }
        assert_eq!(html_lang(None), None);
    }
}
