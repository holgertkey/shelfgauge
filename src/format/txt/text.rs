//! Plain text bytes → clean lines.

use crate::format::encoding::decode_text;

/// One line of the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Line<'a> {
    /// Leading whitespace in columns (a tab counts as [`TAB_WIDTH`]).
    pub indent: usize,
    /// The text without leading and trailing whitespace; empty for a blank line.
    pub text: &'a str,
    /// The length in characters, indentation included.
    pub len: usize,
}

impl Line<'_> {
    /// Whether the line is empty or whitespace only.
    pub fn is_blank(&self) -> bool {
        self.text.is_empty()
    }
}

/// Columns a tab counts for in the indentation.
const TAB_WIDTH: usize = 4;

/// Decodes `data` and cleans the text: line ends become `\n`, a form feed becomes a
/// blank line, and other control characters (except tabs) and stray BOMs are dropped.
pub(super) fn decode(data: &[u8]) -> String {
    let decoded = decode_text(data);
    if decoded.had_errors {
        tracing::warn!(
            encoding = decoded.encoding.name(),
            "malformed byte sequences replaced while decoding"
        );
    }
    clean(&decoded.text)
}

/// See [`decode`].
fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                chars.next_if_eq(&'\n');
                out.push('\n');
            }
            '\n' | '\t' => out.push(c),
            '\u{c}' => out.push_str("\n\n"),
            '\u{feff}' => {}
            c if c.is_control() && (c as u32) < 0xa0 => {}
            c => out.push(c),
        }
    }
    out
}

/// Splits clean text into lines.
pub(super) fn lines(text: &str) -> Vec<Line<'_>> {
    text.split('\n')
        .map(|raw| {
            let raw = raw.trim_end();
            let mut indent = 0;
            for c in raw.chars() {
                match c {
                    '\t' => indent += TAB_WIDTH,
                    c if c.is_whitespace() => indent += 1,
                    _ => break,
                }
            }
            let text = raw.trim_start();
            Line {
                indent: if text.is_empty() { 0 } else { indent },
                text,
                len: indent + text.chars().count(),
            }
        })
        .collect()
}

/// `text` with runs of whitespace as single spaces.
pub(super) fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Whether `text` has at least `min` characters that are not whitespace; stops
/// counting at `min`.
pub(super) fn has_text(text: &str, min: usize) -> bool {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .take(min)
        .count()
        >= min
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_ends_and_controls_are_cleaned() {
        assert_eq!(clean("a\r\nb\rc\nd"), "a\nb\nc\nd");
        assert_eq!(clean("one\u{c}two"), "one\n\ntwo");
        assert_eq!(clean("x\0y\u{1}z\u{7f}\u{85}\u{feff}\tw"), "xyz\tw");
        assert_eq!(clean("\u{14}Глава 1\u{15}"), "Глава 1");
    }

    #[test]
    fn lines_have_indent_and_length() {
        let lines = lines("  ab \n\tc\n   \nd");
        assert_eq!(
            lines,
            [
                Line {
                    indent: 2,
                    text: "ab",
                    len: 4
                },
                Line {
                    indent: 4,
                    text: "c",
                    len: 5
                },
                Line {
                    indent: 0,
                    text: "",
                    len: 0
                },
                Line {
                    indent: 0,
                    text: "d",
                    len: 1
                },
            ]
        );
        assert!(lines[2].is_blank());
    }

    #[test]
    fn spaces_collapse_and_text_counts() {
        assert_eq!(collapse_spaces(" a  b\t c "), "a b c");
        assert!(has_text("a b c", 3));
        assert!(!has_text("a b ", 3));
    }

    #[test]
    fn decoding_uses_the_shared_detection() {
        let (bytes, _, _) = encoding_rs::IBM866.encode(
            "Старый маяк стоял на краю скалистого берега. Каждый вечер смотритель \
             поднимался по винтовой лестнице, зажигал лампу и долго смотрел на море.\r\n",
        );
        let text = decode(&bytes);
        assert!(text.starts_with("Старый маяк"), "{text}");
        assert!(text.ends_with("море.\n"));
    }
}
