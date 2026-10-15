//! Plain text (`.txt`).
//!
//! A text file has no markup and no metadata, so the parser works from statistics and
//! conventions:
//!
//! 1. The bytes are decoded with the shared detection (BOM, UTF-8, then the
//!    detector; see [`format::encoding`](crate::format::encoding)), line ends are
//!    normalized and control characters dropped.
//! 2. Lines become paragraphs, verse and rules in one of three styles decided per file
//!    (see `paragraphs`).
//! 3. Heading-like lines (`Глава 1`, `CHAPTER IV`, `ЧАСТЬ ПЕРВАЯ`, lone numerals) cut
//!    the text into sections (see `headings`) with the shared section builder; without
//!    them the book has no sections and is split by size for reading.
//! 4. Metadata comes from the file name only: the stem is the title, and
//!    `Author - Title` (exactly one ` - ` or ` — `) names the author, the last word
//!    taken as the last name. The language is not guessed; there is no cover.
//!
//! Not every `.txt` file is a book: a file with less than [`MIN_TEXT`] characters of
//! text (whitespace not counted), or with a well-known name ([`NOT_BOOK_NAMES`]:
//! `readme`, `license`, ...), is [not a book](crate::format::ParseErrorKind::NotABook).

use super::model::{Body, BookContent, BookMetadata, Person};
use super::sections::build_sections;
use super::{BookParser, ParseError};

mod headings;
mod paragraphs;
mod text;

pub use paragraphs::Style;

/// Minimum characters of text (whitespace not counted) of a book.
pub const MIN_TEXT: usize = 2_000;

/// File names (without the extension, lowercase) of text files that are not books.
pub const NOT_BOOK_NAMES: &[&str] = &[
    "readme",
    "read.me",
    "license",
    "licence",
    "copying",
    "changelog",
    "changes",
    "install",
    "authors",
    "todo",
    "file_id",
    "descript.ion",
    "index",
];

/// The plain text parser.
#[derive(Debug, Clone, Copy, Default)]
pub struct TxtParser;

impl BookParser for TxtParser {
    fn format(&self) -> &'static str {
        "txt"
    }

    fn extensions(&self) -> &'static [&'static str] {
        &["txt"]
    }

    /// Checks that the text is long enough to be a book; the title is left empty
    /// (the file name is not known here).
    fn parse_metadata(&self, data: &[u8]) -> Result<BookMetadata, ParseError> {
        check_length(data)?;
        Ok(BookMetadata::default())
    }

    /// Converts the text (see the [module docs](self)).
    fn parse_content(&self, data: &[u8]) -> Result<BookContent, ParseError> {
        let text = text::decode(data);
        let lines = text::lines(&text);
        let stats = paragraphs::stats(&lines);
        let headings = headings::find(&lines, stats.style);
        let (items, cuts) = paragraphs::build(&lines, stats, &headings);
        let main = build_sections(items, &cuts);
        Ok(BookContent::new(vec![Body::main(main)], Default::default()))
    }

    /// Checks the name and the length, and takes the title and author from the name.
    fn parse_metadata_named(&self, data: &[u8], name: &str) -> Result<BookMetadata, ParseError> {
        let stem = stem(name);
        if is_not_book_name(stem) {
            return Err(ParseError::not_a_book(format!(
                "{name:?} is not a book by its name"
            )));
        }
        check_length(data)?;
        let (authors, title) = from_stem(stem);
        Ok(BookMetadata {
            title,
            authors,
            ..BookMetadata::default()
        })
    }
}

/// The style the paragraphs of a text are read in (for reports and tests).
///
/// # Examples
///
/// ```
/// use shelfgauge::format::txt::{Style, style_of};
///
/// let wrapped = "A line of a hard-wrapped plain text book that goes on and on,\n";
/// let text = format!("{}\n", wrapped.repeat(3)).repeat(4);
/// assert_eq!(style_of(text.as_bytes()), Style::Blocks);
/// assert_eq!(style_of(b"One paragraph per line.\nAnother one."), Style::Lines);
/// ```
pub fn style_of(data: &[u8]) -> Style {
    let text = text::decode(data);
    paragraphs::stats(&text::lines(&text)).style
}

/// Not a book if the text is too short.
fn check_length(data: &[u8]) -> Result<(), ParseError> {
    // Every encoding needs at least one byte per character.
    if data.len() < MIN_TEXT || !text::has_text(&text::decode(data), MIN_TEXT) {
        return Err(ParseError::not_a_book(format!(
            "less than {MIN_TEXT} characters of text"
        )));
    }
    Ok(())
}

/// The file name without its last extension.
fn stem(name: &str) -> &str {
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem,
        _ => name,
    }
}

/// Whether a file stem is one of [`NOT_BOOK_NAMES`] (ignoring case).
fn is_not_book_name(stem: &str) -> bool {
    let stem = stem.trim().to_lowercase();
    NOT_BOOK_NAMES.contains(&stem.as_str())
}

/// Authors and title from a file stem: `Author - Title`, `Author — Title` or just
/// the title. Several authors are separated by commas.
fn from_stem(stem: &str) -> (Vec<Person>, String) {
    let title = stem.trim().to_owned();
    let separators = [" - ", " — ", " – "];
    let found: Vec<(usize, &str)> = separators
        .iter()
        .flat_map(|sep| stem.match_indices(sep))
        .collect();
    let [(at, separator)] = found.as_slice() else {
        return (Vec::new(), title);
    };
    let (author, rest) = (&stem[..*at], &stem[at + separator.len()..]);
    let authors: Vec<Person> = author
        .split([',', ';'])
        .filter_map(|name| super::epub::person(name, None))
        .collect();
    let rest = rest.trim();
    if authors.is_empty() || rest.is_empty() {
        return (Vec::new(), title);
    }
    (authors, rest.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name_of(person: &Person) -> String {
        [&person.first_name, &person.middle_name, &person.last_name]
            .into_iter()
            .flatten()
            .cloned()
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn titles_and_authors_from_the_name() {
        let cases: &[(&str, &[&str], &str)] = &[
            ("Война и мир", &[], "Война и мир"),
            ("Пушкин - Дубровский", &["Пушкин"], "Дубровский"),
            (
                "Александр Сергеевич Пушкин — Капитанская дочка",
                &["Александр|Сергеевич|Пушкин"],
                "Капитанская дочка",
            ),
            (
                "Толстой Лев - Анна Каренина",
                &["Толстой|Лев"],
                "Анна Каренина",
            ),
            (
                "Ильф, Петров - Двенадцать стульев",
                &["Ильф", "Петров"],
                "Двенадцать стульев",
            ),
            ("A - B - C", &[], "A - B - C"),
            ("Pre-war stories", &[], "Pre-war stories"),
            (" - Title", &[], "- Title"),
        ];
        for (stem, authors, title) in cases {
            let (found, found_title) = from_stem(stem);
            let found: Vec<String> = found.iter().map(name_of).collect();
            assert_eq!(
                (found, found_title.as_str()),
                (authors.iter().map(|s| s.to_string()).collect(), *title),
                "{stem:?}"
            );
        }
    }

    #[test]
    fn not_book_names() {
        for name in [
            "readme.txt",
            "README.TXT",
            "Read.Me.txt",
            "license.txt",
            "INDEX.txt",
            "file_id.txt",
        ] {
            assert!(is_not_book_name(stem(name)), "{name}");
        }
        for name in ["Война и мир.txt", "readme-not.txt", "indexes.txt", ".txt"] {
            assert!(!is_not_book_name(stem(name)), "{name}");
        }
    }

    #[test]
    fn short_texts_are_not_books() {
        let short = "Короткая заметка. ".repeat(50);
        let err = TxtParser.parse_metadata(short.as_bytes()).unwrap_err();
        assert!(err.is_not_a_book(), "{err}");
        let spaced = format!("{}{}", " ".repeat(5000), "x".repeat(100));
        assert!(
            TxtParser
                .parse_metadata(spaced.as_bytes())
                .unwrap_err()
                .is_not_a_book()
        );
        let long = "Длинный текст книги. ".repeat(200);
        assert!(TxtParser.parse_metadata(long.as_bytes()).is_ok());
        let err = TxtParser
            .parse_metadata_named(long.as_bytes(), "readme.txt")
            .unwrap_err();
        assert!(err.is_not_a_book());
        let metadata = TxtParser
            .parse_metadata_named(long.as_bytes(), "Автор - Книга.txt")
            .unwrap();
        assert_eq!(metadata.title, "Книга");
        assert_eq!(metadata.authors.len(), 1);
        assert_eq!(metadata.language, None);
        assert!(metadata.cover.is_none());
    }

    #[test]
    fn content_has_sections_at_headings() {
        let chapter = "It is a truth universally acknowledged, that a single man in possession\n\
                       of a good fortune, must be in want of a wife. However little known the\n\
                       feelings or views of such a man may be on his first entering a place.\n\n";
        let text = format!(
            "Title page\n\nBOOK ONE\n\nCHAPTER I\n\n{c}CHAPTER II\nThe Visit\n\n{c}BOOK TWO\n\nCHAPTER I\n\n{c}",
            c = chapter.repeat(2)
        );
        let content = TxtParser.parse_content(text.as_bytes()).unwrap();
        let root = &content.bodies()[0].section;
        assert_eq!(root.blocks[0].plain_text(), "Title page");
        let outline: Vec<(Option<String>, Vec<Option<String>>)> = root
            .children
            .iter()
            .map(|part| {
                (
                    part.title_text(),
                    part.children.iter().map(|c| c.title_text()).collect(),
                )
            })
            .collect();
        assert_eq!(outline.len(), 2, "{outline:?}");
        assert_eq!(outline[0].0.as_deref(), Some("BOOK ONE"));
        assert_eq!(outline[0].1.len(), 2);
        assert_eq!(
            outline[0].1[1].as_deref().map(|t| t.contains("The Visit")),
            Some(true)
        );
        assert_eq!(outline[1].1.len(), 1);
    }

    #[test]
    fn content_without_headings_has_no_sections() {
        let text = "A paragraph of text that is long enough to matter in the test.\n\n".repeat(50);
        let content = TxtParser.parse_content(text.as_bytes()).unwrap();
        let root = &content.bodies()[0].section;
        assert!(root.children.is_empty());
        assert_eq!(root.blocks.len(), 50);
        assert_eq!(content.bodies().len(), 1);
        assert!(content.images().is_empty());
    }

    #[test]
    fn garbage_does_not_panic() {
        for data in [
            &b""[..],
            b"\0\0\0",
            b"\xff\xfe",
            b"\r\r\r\n\n\x0c",
            &[0x80; 5000],
        ] {
            let _ = TxtParser.parse_content(data).unwrap();
            let _ = TxtParser.parse_metadata(data);
        }
    }
}
