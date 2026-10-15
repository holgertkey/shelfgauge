//! Heading lines of plain text.
//!
//! A line is a heading candidate if it is short ([`MAX_HEADING_LEN`]), follows a blank
//! line (or starts the text) and reads, case-insensitively and without decoration
//! (`* ЧАСТЬ ПЕРВАЯ *`):
//!
//! - `Часть`, `Книга`, `Том`, `Part`, `Book`, `Volume` and a number (rank 1);
//! - `Глава`, `Chapter` and a number (rank 2);
//! - `Пролог`, `Эпилог`, `Предисловие`, `Послесловие`, `Prologue`, `Epilogue`,
//!   `Preface`, `Afterword`, alone or with a number (rank 2);
//! - a number word and one of these keywords (`FIRST EPILOGUE`, `Первая часть`);
//! - a lone Roman or Arabic numeral (rank 2), only if at least [`MIN_NUMERALS`] lines
//!   are such numerals.
//!
//! A number is Arabic, Roman or written as words (`ONE`, `первая`, `двадцать первая`).
//! After it, the line may go on only after punctuation (`Глава 1. Name`,
//! `BOOK ONE: 1805`). The candidate must be followed by a blank line, or by one short
//! line (the chapter's name, which joins the title) and a blank line; in the *lines*
//! style it may also be followed directly by text.
//!
//! A candidate without any text before the next candidate of the same or a higher rank
//! is a line of a table of contents, not a heading (repeated until nothing changes, so
//! a contents list of parts and chapters goes away entirely).

use super::paragraphs::Style;
use super::text::Line;

/// Longest heading line in characters.
pub(super) const MAX_HEADING_LEN: usize = 80;

/// Lone numerals are headings only if there are at least this many.
pub(super) const MIN_NUMERALS: usize = 3;

/// A heading found in the lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Heading {
    /// The index of the heading line.
    pub line: usize,
    /// 1 for parts, 2 for chapters.
    pub rank: u8,
    /// The heading text without decoration.
    pub text: String,
    /// The line index of the chapter's name, if one follows the heading.
    pub name: Option<usize>,
}

/// What kind of heading line `text` is: its rank and whether it is a lone numeral.
fn classify(text: &str) -> Option<(u8, bool)> {
    let text = undecorated(text);
    if text.is_empty() || text.chars().count() > MAX_HEADING_LEN {
        return None;
    }
    let bare = text.trim_end_matches(['.', ':']);
    if (bare.len() <= 4 && bare.chars().all(|c| c.is_ascii_digit())) || is_roman(bare) {
        return Some((2, true));
    }
    let (word, rest) = first_word(text);
    if let Some(keyword) = Keyword::of(&word) {
        let titled = numbered(rest) || (keyword == Keyword::Other && ends_title(rest));
        return titled.then_some((keyword.rank(), false));
    }
    // An ordinal first: `FIRST EPILOGUE`, `Первая часть`.
    let (second, after) = first_word(rest.trim_start());
    let keyword = Keyword::of(&second)?;
    (rest.starts_with(char::is_whitespace) && is_number_word(&word) && ends_title(after))
        .then_some((keyword.rank(), false))
}

/// The first word of `text` (letters only), lowercase, and the rest.
fn first_word(text: &str) -> (String, &str) {
    let end = text
        .find(|c: char| !c.is_alphabetic())
        .unwrap_or(text.len());
    (text[..end].to_lowercase(), &text[end..])
}

/// A word that starts a heading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keyword {
    /// `Часть`, `Книга`, `Том`, `Part`, `Book`, `Volume`: rank 1, numbered.
    Part,
    /// `Глава`, `Chapter`: rank 2, numbered.
    Chapter,
    /// Prologues, epilogues, prefaces, afterwords: rank 2, number optional.
    Other,
}

impl Keyword {
    fn of(word: &str) -> Option<Self> {
        match word {
            "часть" | "книга" | "том" | "part" | "book" | "volume" => Some(Self::Part),
            "глава" | "chapter" => Some(Self::Chapter),
            "пролог"
            | "эпилог"
            | "предисловие"
            | "послесловие"
            | "prologue"
            | "epilogue"
            | "preface"
            | "afterword" => Some(Self::Other),
            _ => None,
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Part => 1,
            Self::Chapter | Self::Other => 2,
        }
    }
}

/// Whether `rest` may end a heading: nothing, or punctuation and a name.
fn ends_title(rest: &str) -> bool {
    let rest = rest.trim_start();
    rest.is_empty() || rest.starts_with(SEPARATORS)
}

/// Punctuation after which a heading line may go on with a name.
const SEPARATORS: &[char] = &['.', ':', '—', '–', '-', ')'];

/// Whether `rest` (after a keyword) is a number, possibly followed by punctuation and
/// more text.
fn numbered(rest: &str) -> bool {
    if !rest.starts_with(char::is_whitespace) {
        return false;
    }
    let mut words = rest.split_whitespace().peekable();
    if words.peek().is_some_and(|w| w.eq_ignore_ascii_case("the")) {
        words.next();
    }
    let mut count = 0;
    for word in words {
        let core = word.trim_end_matches(['.', ':', ',', ')']);
        if count < 3 && !core.is_empty() && is_number(core) {
            count += 1;
            if core.len() != word.len() {
                return true;
            }
            continue;
        }
        return count > 0 && word.starts_with(SEPARATORS);
    }
    count > 0
}

/// An Arabic or Roman numeral or a number word (`twenty-one` included).
fn is_number(word: &str) -> bool {
    if word.chars().all(|c| c.is_ascii_digit()) {
        return word.len() <= 4;
    }
    if is_roman(word) {
        return true;
    }
    let word = word.to_lowercase();
    word.split('-').all(is_number_word)
}

/// A well-formed Roman numeral up to 3999, all uppercase or all lowercase.
fn is_roman(word: &str) -> bool {
    if word.is_empty()
        || word.len() > 15
        || !(word.chars().all(|c| c.is_ascii_uppercase())
            || word.chars().all(|c| c.is_ascii_lowercase()))
    {
        return false;
    }
    let digit = |c: char| match c.to_ascii_uppercase() {
        'I' => Some(1),
        'V' => Some(5),
        'X' => Some(10),
        'L' => Some(50),
        'C' => Some(100),
        'D' => Some(500),
        'M' => Some(1000),
        _ => None,
    };
    let Some(values) = word.chars().map(digit).collect::<Option<Vec<u32>>>() else {
        return false;
    };
    let mut value = 0;
    for (i, &v) in values.iter().enumerate() {
        if values.get(i + 1).is_some_and(|&next| next > v) {
            value -= v as i64;
        } else {
            value += v as i64;
        }
    }
    (1..4000).contains(&value) && to_roman(value as u32) == word.to_ascii_uppercase()
}

/// The canonical Roman numeral of `n`.
fn to_roman(mut n: u32) -> String {
    const PARTS: &[(u32, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for &(value, letters) in PARTS {
        while n >= value {
            out.push_str(letters);
            n -= value;
        }
    }
    out
}

/// English and Russian number words, cardinal and ordinal, lowercase.
fn is_number_word(word: &str) -> bool {
    const ENGLISH: &[&str] = &[
        "one",
        "two",
        "three",
        "four",
        "five",
        "six",
        "seven",
        "eight",
        "nine",
        "ten",
        "eleven",
        "twelve",
        "thirteen",
        "fourteen",
        "fifteen",
        "sixteen",
        "seventeen",
        "eighteen",
        "nineteen",
        "twenty",
        "thirty",
        "forty",
        "fifty",
        "first",
        "second",
        "third",
        "fourth",
        "fifth",
        "sixth",
        "seventh",
        "eighth",
        "ninth",
        "tenth",
        "eleventh",
        "twelfth",
        "thirteenth",
        "fourteenth",
        "fifteenth",
        "sixteenth",
        "seventeenth",
        "eighteenth",
        "nineteenth",
        "twentieth",
        "thirtieth",
        "fortieth",
        "fiftieth",
        "last",
        "final",
    ];
    const RUSSIAN: &[&str] = &[
        "один",
        "одна",
        "два",
        "две",
        "три",
        "четыре",
        "пять",
        "шесть",
        "семь",
        "восемь",
        "девять",
        "десять",
        "сорок",
        "третий",
        "третья",
        "третье",
    ];
    /// Ordinal stems; an ending of up to three letters follows (`первая`, `пятый`).
    const RUSSIAN_STEMS: &[&str] = &[
        "перв",
        "втор",
        "четверт",
        "пят",
        "шест",
        "седьм",
        "восьм",
        "девят",
        "десят",
        "одиннадцат",
        "двенадцат",
        "тринадцат",
        "четырнадцат",
        "пятнадцат",
        "шестнадцат",
        "семнадцат",
        "восемнадцат",
        "девятнадцат",
        "двадцат",
        "тридцат",
        "сороков",
        "последн",
    ];
    ENGLISH.contains(&word)
        || RUSSIAN.contains(&word)
        || RUSSIAN_STEMS.iter().any(|stem| {
            word.strip_prefix(stem)
                .is_some_and(|ending| ending.chars().count() <= 3)
        })
}

/// `text` without surrounding decoration (`*`, `=`, `#`, `~`, `_` and spaces).
pub(super) fn undecorated(text: &str) -> &str {
    text.trim_matches(|c: char| c.is_whitespace() || matches!(c, '*' | '=' | '#' | '~' | '_'))
}

/// Finds the headings among `lines` (see the [module docs](self)).
pub(super) fn find(lines: &[Line<'_>], style: Style) -> Vec<Heading> {
    let blank_or_end = |i: usize| lines.get(i).is_none_or(Line::is_blank);
    let kind = |i: usize| lines.get(i).and_then(|l| classify(l.text));
    let numerals = lines
        .iter()
        .filter(|l| classify(l.text).is_some_and(|(_, numeral)| numeral))
        .count();
    let mut found = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some((rank, numeral)) = kind(i) else {
            continue;
        };
        if (numeral && numerals < MIN_NUMERALS) || (i > 0 && !lines[i - 1].is_blank()) {
            continue;
        }
        let name = if blank_or_end(i + 1) {
            None
        } else if kind(i + 1).is_none()
            && lines[i + 1].text.chars().count() <= MAX_HEADING_LEN
            && blank_or_end(i + 2)
        {
            Some(i + 1)
        } else if style == Style::Lines && kind(i + 1).is_none() {
            None
        } else {
            continue;
        };
        found.push(Heading {
            line: i,
            rank,
            text: undecorated(line.text).to_owned(),
            name,
        });
    }
    drop_contents_lines(lines, found)
}

/// Removes candidates without text before the next candidate of the same or a higher
/// rank (table of contents lines), until nothing changes.
fn drop_contents_lines(lines: &[Line<'_>], mut headings: Vec<Heading>) -> Vec<Heading> {
    let mut dropped = std::collections::HashSet::new();
    loop {
        let mut keep = vec![true; headings.len()];
        for (k, heading) in headings.iter().enumerate() {
            let end = headings.get(k + 1).map_or(lines.len(), |next| next.line);
            let start = heading.name.unwrap_or(heading.line) + 1;
            let has_text = lines[start..end]
                .iter()
                .enumerate()
                .any(|(offset, line)| !line.is_blank() && !dropped.contains(&(start + offset)));
            let next_rank = headings.get(k + 1).map(|next| next.rank);
            if !has_text && next_rank.is_some_and(|rank| rank <= heading.rank)
                || !has_text && next_rank.is_none()
            {
                keep[k] = false;
            }
        }
        if keep.iter().all(|&k| k) {
            return headings;
        }
        let mut k = 0;
        headings.retain(|heading| {
            let kept = keep[k];
            k += 1;
            if !kept {
                dropped.insert(heading.line);
                dropped.extend(heading.name);
            }
            kept
        });
    }
}

#[cfg(test)]
mod tests {
    use super::super::text::lines;
    use super::*;

    #[test]
    fn heading_lines() {
        for (text, rank) in [
            ("Глава 1", 2),
            ("ГЛАВА 1.", 2),
            ("Глава первая", 2),
            ("Глава двадцать первая", 2),
            ("Глава 1. Никогда не разговаривайте с неизвестными", 2),
            ("* ЧАСТЬ ПЕРВАЯ *", 1),
            ("* ТОМ ПЕРВЫЙ *", 1),
            ("Книга третья", 1),
            ("CHAPTER I.", 2),
            ("Chapter XLII", 2),
            ("Chapter the First", 2),
            ("chapter twenty-one", 2),
            ("BOOK ONE: 1805", 1),
            ("PART ONE", 1),
            ("Volume II", 1),
            ("Пролог", 2),
            ("ЭПИЛОГ", 2),
            ("Epilogue: Ten Years Later", 2),
            ("Prologue 2", 2),
            ("XIV", 2),
            ("12.", 2),
            ("FIRST EPILOGUE: 1813 - 20", 2),
            ("Первая часть", 1),
            ("SECOND PART", 1),
        ] {
            assert_eq!(classify(text).map(|(r, _)| r), Some(rank), "{text:?}");
        }
    }

    #[test]
    fn lines_that_are_not_headings() {
        for text in [
            "Глава семьи вошла в комнату.",
            "Chapter 1 was the hardest to write",
            "Part of the problem",
            "Book",
            "Глава",
            "Prologues are long",
            "First part of the day",
            "Second chapter begins",
            "Hello Book",
            "Mix",
            "Il",
            "CIVIL",
            "IIII",
            "12345",
            "Volume one of the series was published in 1869 and the rest followed at a steady pace",
        ] {
            assert_eq!(classify(text), None, "{text:?}");
        }
    }

    fn find_in(text: &str, style: Style) -> Vec<(usize, u8, String, Option<usize>)> {
        find(&lines(text), style)
            .into_iter()
            .map(|h| (h.line, h.rank, h.text, h.name))
            .collect()
    }

    #[test]
    fn headings_stand_apart_and_take_a_name() {
        let text = "Intro text.\n\nChapter 1\nThe Beginning\n\nText one.\n\nCHAPTER 2\n\nText two.\nChapter 3\n\nNot a heading above.";
        assert_eq!(
            find_in(text, Style::Blocks),
            [
                (2, 2, "Chapter 1".to_owned(), Some(3)),
                (7, 2, "CHAPTER 2".to_owned(), None)
            ]
        );
    }

    #[test]
    fn in_the_lines_style_text_may_follow_at_once() {
        let text = "\nГлава 1\nПервый абзац главы, длинный и подробный, совсем не похожий на название главы, а на текст.\nВторой.\n";
        assert_eq!(
            find_in(text, Style::Lines),
            [(1, 2, "Глава 1".to_owned(), None)]
        );
        assert!(find_in(text, Style::Blocks).is_empty());
    }

    #[test]
    fn lone_numerals_need_company() {
        assert!(find_in("I\n\nText.\n\nII\n\nText.", Style::Blocks).is_empty());
        let found = find_in("I\n\nText.\n\nII\n\nText.\n\nIII\n\nText.", Style::Blocks);
        assert_eq!(found.len(), 3);
    }

    #[test]
    fn contents_lists_are_not_headings() {
        let text = "Contents\n\nBOOK ONE\n\nCHAPTER I\n\nCHAPTER II\n\nBOOK TWO\n\nCHAPTER I\n\n\
                    BOOK ONE\n\nCHAPTER I\n\nText.\n\nCHAPTER II\n\nText.\n\nBOOK TWO\n\nCHAPTER I\n\nText.";
        let found: Vec<(usize, String)> = find_in(text, Style::Blocks)
            .into_iter()
            .map(|(line, _, text, _)| (line, text))
            .collect();
        assert_eq!(
            found,
            [
                (12, "BOOK ONE".to_owned()),
                (14, "CHAPTER I".to_owned()),
                (18, "CHAPTER II".to_owned()),
                (22, "BOOK TWO".to_owned()),
                (24, "CHAPTER I".to_owned()),
            ]
        );
    }

    #[test]
    fn a_part_right_before_a_chapter_stays() {
        let found = find_in("ЧАСТЬ ПЕРВАЯ\n\nГлава 1\n\nТекст.", Style::Blocks);
        assert_eq!(found.len(), 2);
    }
}
