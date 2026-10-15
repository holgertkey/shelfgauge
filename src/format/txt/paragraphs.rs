//! Lines → paragraphs, verse, rules and headings.
//!
//! Each file gets one **style**, decided from statistics of its non-blank lines:
//!
//! - [`Style::Indented`] (lib.ru): a paragraph starts at each indented line (2+ spaces
//!   or a tab) and after each blank line. Chosen when at least [`MIN_INDENT_BREAKS`]
//!   lines, and at least 1 in [`INDENT_BREAK_SHARE`] non-blank lines, are *indent
//!   breaks*: a line indented deeper than the non-blank line before it.
//! - [`Style::Blocks`] (Project Gutenberg): blank lines separate blocks, and the lines
//!   of a hard-wrapped block are joined with spaces. Chosen when the wrap width (the
//!   95th percentile of line lengths) is [`MIN_WRAP_WIDTH`] to [`MAX_WRAP_WIDTH`], at
//!   least half of the lines belong to blocks of two or more lines, and blocks have
//!   [`MAX_BLOCK_LINES`] lines on average at most (blank lines do separate them). A block is
//!   hard-wrapped if all its lines but the last reach [`WRAPPED_SHARE`] of the wrap
//!   width.
//! - [`Style::Lines`]: otherwise every non-blank line is a paragraph.
//!
//! In every style a run of two or more short lines that are not prose (at most half of
//! them, the last not counted, end a sentence) is **verse** (or a letter's signature,
//! an address): one paragraph with line breaks. Short means up to
//! [`SHORT_LINE`] characters in the lines style and below [`VERSE_SHARE`] of the
//! wrap width otherwise; in the indented style verse lines must also be indented
//! deeper than paragraphs.
//!
//! A line of three or more `*`, `-`, `_` or `=` (spaces between allowed: `* * *`) is a
//! [`BlockKind::Rule`]. Whitespace inside a line becomes single spaces. When lines are
//! joined, a line ending in a letter and a hyphen is joined to a next line starting
//! with a lowercase letter without a space (the hyphen is kept: no guesses about
//! word division).
//!
//! A paragraph longer than [`MAX_PARAGRAPH`] characters (a file without line breaks)
//! is split at sentence ends, so that the reader can still cut it into parts.

use std::collections::HashMap;

use super::headings::Heading;
use super::text::{Line, collapse_spaces};
use crate::format::model::{Block, BlockKind, Inline};
use crate::format::sections::{Cut, Item};

/// How the lines of a plain text file form paragraphs; decided per file from
/// statistics of its lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// A new paragraph at each indented line and after each blank line (lib.ru).
    Indented,
    /// Blank-line separated blocks whose hard-wrapped lines are joined (Project
    /// Gutenberg).
    Blocks,
    /// One paragraph per line.
    Lines,
}

/// Minimum number of indent breaks for [`Style::Indented`].
pub(super) const MIN_INDENT_BREAKS: usize = 3;

/// At least one in this many non-blank lines must be an indent break for
/// [`Style::Indented`].
pub(super) const INDENT_BREAK_SHARE: usize = 20;

/// Smallest wrap width of [`Style::Blocks`].
pub(super) const MIN_WRAP_WIDTH: usize = 50;

/// Largest wrap width of [`Style::Blocks`].
pub(super) const MAX_WRAP_WIDTH: usize = 120;

/// Most lines per block on average in [`Style::Blocks`].
pub(super) const MAX_BLOCK_LINES: usize = 30;

/// A hard-wrapped line reaches this share of the wrap width.
pub(super) const WRAPPED_SHARE: f64 = 0.6;

/// A verse line is shorter than this share of the wrap width.
pub(super) const VERSE_SHARE: f64 = 0.75;

/// Longest verse line in the lines style, in characters.
pub(super) const SHORT_LINE: usize = 60;

/// Longest paragraph in characters.
pub(super) const MAX_PARAGRAPH: usize = 100_000;

/// Statistics that decide the style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Stats {
    /// The style.
    pub style: Style,
    /// The 95th percentile of the lengths of non-blank lines.
    pub wrap_width: usize,
    /// The most common indentation of indent breaks (0 if none).
    pub paragraph_indent: usize,
}

/// Decides the style of `lines`.
pub(super) fn stats(lines: &[Line<'_>]) -> Stats {
    let nonblank: Vec<&Line<'_>> = lines.iter().filter(|l| !l.is_blank()).collect();
    let mut lengths: Vec<usize> = nonblank.iter().map(|l| l.len).collect();
    lengths.sort_unstable();
    let wrap_width = lengths
        .get((lengths.len() * 95 / 100).min(lengths.len().saturating_sub(1)))
        .copied()
        .unwrap_or(0);

    let mut indents: HashMap<usize, usize> = HashMap::new();
    let mut breaks = 0;
    let mut in_blocks = 0;
    let mut blocks = 0;
    for (i, line) in lines.iter().enumerate() {
        if line.is_blank() {
            continue;
        }
        let previous = i.checked_sub(1).map(|p| &lines[p]);
        let next = lines.get(i + 1);
        if line.indent >= 2 && previous.is_some_and(|p| !p.is_blank() && p.indent < line.indent) {
            breaks += 1;
            *indents.entry(line.indent).or_default() += 1;
        }
        if previous.is_some_and(|p| !p.is_blank()) || next.is_some_and(|n| !n.is_blank()) {
            in_blocks += 1;
        }
        if previous.is_none_or(Line::is_blank) {
            blocks += 1;
        }
    }
    let paragraph_indent = indents
        .iter()
        .max_by_key(|&(indent, count)| (*count, std::cmp::Reverse(*indent)))
        .map_or(0, |(indent, _)| *indent);
    let style = if breaks >= MIN_INDENT_BREAKS && breaks * INDENT_BREAK_SHARE >= nonblank.len() {
        Style::Indented
    } else if (MIN_WRAP_WIDTH..=MAX_WRAP_WIDTH).contains(&wrap_width)
        && in_blocks * 2 >= nonblank.len()
        && blocks * MAX_BLOCK_LINES >= nonblank.len()
    {
        Style::Blocks
    } else {
        Style::Lines
    };
    Stats {
        style,
        wrap_width,
        paragraph_indent,
    }
}

/// Builds the items of the text and the cuts at its headings.
pub(super) fn build(
    lines: &[Line<'_>],
    stats: Stats,
    headings: &[Heading],
) -> (Vec<Item>, Vec<Cut>) {
    let mut ranks: Vec<u8> = headings.iter().map(|h| h.rank).collect();
    ranks.sort_unstable();
    ranks.dedup();
    let by_line: HashMap<usize, &Heading> = headings.iter().map(|h| (h.line, h)).collect();
    let mut builder = Builder {
        stats,
        items: Vec::new(),
        cuts: Vec::new(),
        block: Vec::new(),
        singles: Vec::new(),
    };
    let mut i = 0;
    while i < lines.len() {
        let line = &lines[i];
        i += 1;
        if line.is_blank() {
            builder.flush();
            continue;
        }
        if let Some(heading) = by_line.get(&(i - 1)) {
            builder.flush();
            builder.cuts.push(Cut {
                depth: ranks.iter().position(|&r| r == heading.rank).unwrap_or(0) + 1,
                label: None,
                index: builder.items.len(),
            });
            builder
                .items
                .push(Item::Heading(heading.rank, Block::paragraph(&heading.text)));
            if let Some(name) = heading.name {
                let text = collapse_spaces(lines[name].text);
                builder
                    .items
                    .push(Item::Heading(heading.rank + 1, Block::paragraph(text)));
                i = name + 1;
            }
            continue;
        }
        if is_rule(line.text) {
            builder.flush();
            builder.items.push(Item::Block(Block::new(BlockKind::Rule)));
            continue;
        }
        match stats.style {
            Style::Lines => builder.singles.push(line.clone()),
            Style::Blocks => builder.block.push(line.clone()),
            Style::Indented => {
                if line.indent >= 2 {
                    builder.flush_block();
                }
                builder.block.push(line.clone());
            }
        }
    }
    builder.flush();
    (builder.items, builder.cuts)
}

struct Builder<'a> {
    stats: Stats,
    items: Vec<Item>,
    cuts: Vec<Cut>,
    /// Lines of the current block or paragraph.
    block: Vec<Line<'a>>,
    /// Consecutive one-line paragraphs, which may be verse.
    singles: Vec<Line<'a>>,
}

impl Builder<'_> {
    /// Ends the current block and the run of single lines.
    fn flush(&mut self) {
        self.flush_block();
        self.flush_singles();
    }

    /// Ends the current block: prose, or single lines.
    fn flush_block(&mut self) {
        let block = std::mem::take(&mut self.block);
        match block.len() {
            0 => {}
            1 => self.singles.extend(block),
            _ if self.stats.style == Style::Indented || self.wrapped(&block) => {
                self.flush_singles();
                let texts: Vec<&str> = block.iter().map(|l| l.text).collect();
                self.paragraph(&join(&texts));
            }
            _ => {
                self.singles.extend(block);
                self.flush_singles();
            }
        }
    }

    /// Whether all lines of a block but the last reach the wrap width.
    fn wrapped(&self, block: &[Line<'_>]) -> bool {
        let min = (self.stats.wrap_width as f64 * WRAPPED_SHARE) as usize;
        block[..block.len() - 1].iter().all(|l| l.len >= min)
    }

    /// Whether a line may be part of verse.
    fn short(&self, line: &Line<'_>) -> bool {
        match self.stats.style {
            Style::Lines => line.text.chars().count() <= SHORT_LINE,
            Style::Blocks => (line.len as f64) < self.stats.wrap_width as f64 * VERSE_SHARE,
            Style::Indented => {
                line.indent > self.stats.paragraph_indent
                    && (line.len as f64) < self.stats.wrap_width as f64 * VERSE_SHARE
            }
        }
    }

    /// Turns the run of single lines into paragraphs and verse.
    fn flush_singles(&mut self) {
        let singles = std::mem::take(&mut self.singles);
        let mut i = 0;
        while i < singles.len() {
            let run = singles[i..]
                .iter()
                .take_while(|line| self.short(line))
                .count();
            if run >= 2 && is_verse(&singles[i..i + run]) {
                self.verse(&singles[i..i + run]);
                i += run;
            } else {
                self.paragraph(&collapse_spaces(singles[i].text));
                i += 1;
            }
        }
    }

    fn paragraph(&mut self, text: &str) {
        for piece in split_long(text) {
            self.items.push(Item::Block(Block::paragraph(piece)));
        }
    }

    fn verse(&mut self, lines: &[Line<'_>]) {
        let mut inlines = Vec::new();
        for line in lines {
            if !inlines.is_empty() {
                inlines.push(Inline::LineBreak);
            }
            inlines.push(Inline::Text(collapse_spaces(line.text)));
        }
        self.items
            .push(Item::Block(Block::new(BlockKind::Paragraph(inlines))));
    }
}

/// At most half of the lines (the last not counted) end a sentence.
fn is_verse(lines: &[Line<'_>]) -> bool {
    let counted = &lines[..lines.len() - 1];
    let ends = counted.iter().filter(|l| ends_sentence(l.text)).count();
    ends * 2 <= counted.len()
}

/// Whether `text` ends with sentence punctuation, possibly before closing quotes.
fn ends_sentence(text: &str) -> bool {
    text.trim_end_matches(['"', '»', '”', '’', '\'', ')', ']'])
        .ends_with(['.', '!', '?', '…'])
}

/// A line of three or more `*`, `-`, `_` or `=`, spaces between allowed.
pub(super) fn is_rule(text: &str) -> bool {
    let mut marks = text.chars().filter(|c| !c.is_whitespace());
    let Some(first) = marks.next() else {
        return false;
    };
    matches!(first, '*' | '-' | '_' | '=')
        && marks.clone().all(|c| c == first)
        && marks.count() >= 2
}

/// Joins the lines of a paragraph (see the [module docs](self)).
fn join(lines: &[&str]) -> String {
    let mut out = String::new();
    for line in lines {
        let starts_lower = line.chars().next().is_some_and(char::is_lowercase);
        let hyphenated = out
            .strip_suffix('-')
            .and_then(|rest| rest.chars().next_back())
            .is_some_and(char::is_alphabetic);
        if !out.is_empty() && !(hyphenated && starts_lower) {
            out.push(' ');
        }
        out.push_str(line);
    }
    collapse_spaces(&out)
}

/// Splits a paragraph longer than [`MAX_PARAGRAPH`] characters after sentence ends (or
/// at spaces, or anywhere, if a sentence is too long).
fn split_long(text: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.chars().count() > MAX_PARAGRAPH {
        let limit = rest
            .char_indices()
            .nth(MAX_PARAGRAPH)
            .map_or(rest.len(), |(i, _)| i);
        let head = &rest[..limit];
        let cut = head
            .match_indices(". ")
            .chain(head.match_indices("! "))
            .chain(head.match_indices("? "))
            .map(|(i, _)| i + 1)
            .max()
            .filter(|&i| i > limit / 2)
            .or_else(|| head.rfind(' ').filter(|&i| i > 0))
            .unwrap_or(limit);
        pieces.push(rest[..cut].trim_end().to_owned());
        rest = rest[cut..].trim_start();
    }
    if !rest.is_empty() || pieces.is_empty() {
        pieces.push(rest.to_owned());
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::super::headings;
    use super::super::text::lines;
    use super::*;

    /// A hard-wrapped line of about 70 characters.
    const WRAPPED: &str = "It is a truth universally acknowledged, that a single man in posses-";

    fn style_of(text: &str) -> Style {
        stats(&lines(text)).style
    }

    /// The paragraphs of `text`: verse lines joined with ` / `, rules as `---`.
    fn paragraphs(text: &str) -> Vec<String> {
        let lines = lines(text);
        let stats = stats(&lines);
        let headings = headings::find(&lines, stats.style);
        build(&lines, stats, &headings)
            .0
            .into_iter()
            .map(|item| match item {
                Item::Heading(rank, block) => format!("#{rank} {}", block.plain_text()),
                Item::Block(block) => match &block.kind {
                    BlockKind::Rule => "---".to_owned(),
                    BlockKind::Paragraph(inlines) => inlines
                        .iter()
                        .map(|i| match i {
                            Inline::Text(t) => t.clone(),
                            _ => " / ".to_owned(),
                        })
                        .collect(),
                    _ => unreachable!(),
                },
            })
            .collect()
    }

    fn indented_book() -> String {
        let paragraph = "     Несколько  лет  тому  назад  в  одном  из своих  поместий жил старинный\n\
                         русской барин, Кирила Петрович Троекуров. Его богатство, знатный род и связи\n\
                         давали ему большой вес.\n";
        format!(
            "ГЛАВА 1.\n\n{}          Мороз и солнце; день чудесный!\n          Еще ты дремлешь, друг прелестный —\n          Пора, красавица, проснись.\n{}     - Да.\n     - Нет.\n",
            paragraph.repeat(3),
            paragraph.repeat(2)
        )
    }

    #[test]
    fn indented_style() {
        let text = indented_book();
        assert_eq!(style_of(&text), Style::Indented);
        let paragraphs = paragraphs(&text);
        assert_eq!(paragraphs[0], "#2 ГЛАВА 1.");
        assert_eq!(
            paragraphs[1],
            "Несколько лет тому назад в одном из своих поместий жил старинный русской \
             барин, Кирила Петрович Троекуров. Его богатство, знатный род и связи давали \
             ему большой вес."
        );
        assert_eq!(
            paragraphs[4],
            "Мороз и солнце; день чудесный! / Еще ты дремлешь, друг прелестный — / Пора, \
             красавица, проснись."
        );
        assert_eq!(
            &paragraphs[7..],
            ["- Да.", "- Нет."],
            "dialogue is not verse"
        );
    }

    fn blocks_book() -> String {
        let block = format!(
            "{WRAPPED}\nsion of a good fortune, must be in want of a wife. However little\nknown the feelings.\n\n"
        );
        format!(
            "PRIDE AND PREJUDICE\nBy Jane Austen\n\n{}* * *\n\n    Dear Sir,\n    I remain yours\n    sincerely, W. C.\n\n{}",
            block.repeat(3),
            block
        )
    }

    #[test]
    fn blocks_style() {
        let text = blocks_book();
        assert_eq!(style_of(&text), Style::Blocks);
        let paragraphs = paragraphs(&text);
        assert_eq!(paragraphs[0], "PRIDE AND PREJUDICE / By Jane Austen");
        assert_eq!(
            paragraphs[1],
            "It is a truth universally acknowledged, that a single man in posses-sion of \
             a good fortune, must be in want of a wife. However little known the feelings."
        );
        assert_eq!(paragraphs[4], "---");
        assert_eq!(
            paragraphs[5],
            "Dear Sir, / I remain yours / sincerely, W. C."
        );
        assert_eq!(paragraphs.len(), 7);
    }

    #[test]
    fn lines_style() {
        let long = "Однажды весною, в час небывало жаркого заката, в Москве, на Патриарших \
                    прудах, появились два гражданина, и это было очень странно.";
        let text = format!(
            " Глава 1. Никогда не разговаривайте с неизвестными\n\n {long}\n - Дайте нарзану, - попросил Берлиоз.\n - Нарзану нету, - ответила женщина.\n {long}\n ...Так кто ж ты, наконец?\n - Я - часть той силы,\n что вечно хочет\n зла и вечно совершает благо.\n{}",
            format!(" {long}\n").repeat(5)
        );
        assert_eq!(style_of(&text), Style::Lines);
        let paragraphs = paragraphs(&text);
        assert_eq!(
            paragraphs[0],
            "#2 Глава 1. Никогда не разговаривайте с неизвестными"
        );
        assert_eq!(paragraphs[2], "- Дайте нарзану, - попросил Берлиоз.");
        assert_eq!(paragraphs[3], "- Нарзану нету, - ответила женщина.");
        assert_eq!(
            paragraphs[5],
            "...Так кто ж ты, наконец? / - Я - часть той силы, / что вечно хочет / зла и \
             вечно совершает благо."
        );
        assert_eq!(paragraphs.len(), 11);
    }

    #[test]
    fn hard_wrapped_text_without_blank_lines_or_indents_is_lines() {
        let text = format!("{WRAPPED}\n").repeat(100);
        assert_eq!(style_of(&text), Style::Lines);
    }

    #[test]
    fn rules() {
        for rule in [
            "***",
            "* * *",
            "*  *  *  *",
            "---",
            "------------",
            "___",
            "= = =",
        ] {
            assert!(is_rule(rule), "{rule:?}");
        }
        for text in ["**", "- -", "-*-", "*a*", "", "--- x"] {
            assert!(!is_rule(text), "{text:?}");
        }
    }

    #[test]
    fn joining_keeps_hyphens_without_guessing() {
        assert_eq!(join(&["well-", "known fact"]), "well-known fact");
        assert_eq!(join(&["сказал он -", "и ушел"]), "сказал он - и ушел");
        assert_eq!(join(&["Anglo-", "Saxon"]), "Anglo- Saxon");
        assert_eq!(join(&["a  b", "c"]), "a b c");
    }

    #[test]
    fn long_paragraphs_are_split_at_sentence_ends() {
        let sentence = "Слово за словом шла длинная фраза без конца. ";
        let text = sentence.repeat(MAX_PARAGRAPH / sentence.chars().count() * 2 + 3);
        let pieces = split_long(text.trim());
        assert!(pieces.len() >= 2);
        assert!(pieces.iter().all(|p| p.chars().count() <= MAX_PARAGRAPH));
        assert!(pieces[..pieces.len() - 1].iter().all(|p| p.ends_with('.')));
        assert_eq!(pieces.join(" "), text.trim());

        let word = "x".repeat(MAX_PARAGRAPH + 10);
        let pieces = split_long(&word);
        assert_eq!(pieces.len(), 2);
        assert_eq!(pieces[0].len(), MAX_PARAGRAPH);
        assert_eq!(split_long(""), [""]);
    }
}
