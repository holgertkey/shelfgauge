//! Annotations: bookmarks and marks with notes, the reading position, and their
//! Markdown export.
//!
//! One entity, the [`Annotation`] (UI label "Mark"), covers bookmarks, notes and
//! quotes: the selected `text` is immutable (it is the quote and the fingerprint that
//! finds the place again when the book file changes), the `note` is editable and
//! optional. An annotation without text is a *bookmark* on the start of a block.
//!
//! Annotations belong to a book's *content key* (the identity of the content cache,
//! see [`content_key`](crate::render::load::content_key)), so they survive rescans,
//! moves and renames that keep the content. They live in their own database,
//! `.shelfgauge/userdata.sqlite` ([`store`]): the index can be rebuilt by rescanning,
//! user data cannot.
//!
//! - [`text`]: the rendered text of a book, which offsets count into;
//! - [`find`]: search within a book's text;
//! - [`anchor`]: anchors, quotes and the fingerprint search;
//! - [`store`]: the database;
//! - [`export`]: the one-way Markdown export;
//! - [`attach`]: re-attaching orphans and the cleanup after a scan.

pub mod attach;
pub mod export;
pub mod store;

// Shared with the reading device: these live in the core crate (`shelfgauge-core`).
pub use shelfgauge_core::annotations::{anchor, find, text};

use anchor::{Anchor, Quote};

/// Why an annotation was deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletedReason {
    /// Deleted in the UI; kept as a tombstone (for syncing in V2), never purged.
    User,
    /// The book was removed from the library; restored if it comes back, purged after
    /// `annotations.removed_book_retention_days`.
    BookRemoved,
}

impl DeletedReason {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::BookRemoved => "book_removed",
        }
    }

    /// The reason for a stored name.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "user" => Some(Self::User),
            "book_removed" => Some(Self::BookRemoved),
            _ => None,
        }
    }
}

/// The book an annotation was made in: its content key and a snapshot of its id and
/// metadata (used to re-attach orphans and to export them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookSnapshot {
    /// The content key.
    pub key: Vec<u8>,
    /// The book id in the index (a hint only: ids change on plain ↔ zip moves).
    pub id: i64,
    /// The title.
    pub title: String,
    /// The author display names joined with `, `.
    pub authors: String,
}

/// A stored annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    /// The UUID.
    pub id: String,
    /// The book's content key.
    pub book_key: Vec<u8>,
    /// The last known book id (a hint).
    pub book_id: i64,
    /// Where it is.
    pub anchor: Anchor,
    /// The text and its context.
    pub quote: Quote,
    /// The title of the section around the start, when it was created.
    pub section_title: Option<String>,
    /// The book title when it was created.
    pub book_title: String,
    /// The book's authors when it was created.
    pub book_authors: String,
    /// Set when re-attaching could not find the text in a changed book.
    pub anchor_lost: bool,
    /// The note, if any.
    pub note: Option<String>,
    /// Creation time (Unix seconds).
    pub created_at: i64,
    /// Last change (Unix seconds).
    pub updated_at: i64,
    /// Deletion time (Unix seconds); deleted annotations are hidden everywhere.
    pub deleted_at: Option<i64>,
    /// Why it was deleted.
    pub deleted_reason: Option<DeletedReason>,
    /// The profile it belongs to.
    pub profile: crate::profiles::ProfileId,
}

/// Code points of a mark's text shown as its label when it has no note.
const LABEL_CHARS: usize = 80;

impl Annotation {
    /// Whether this is a bookmark (no selected text).
    pub fn is_bookmark(&self) -> bool {
        self.quote.text.is_empty()
    }

    /// The note, if it is not blank.
    pub fn note(&self) -> Option<&str> {
        self.note.as_deref().filter(|n| !n.trim().is_empty())
    }

    /// The label in lists: the note's first line, else the start of the text; for a
    /// bookmark without a note, "`<section title> — <start of the block>`".
    pub fn label(&self) -> String {
        if let Some(note) = self.note() {
            return one_line(note.lines().find(|l| !l.trim().is_empty()).unwrap_or(""));
        }
        if self.is_bookmark() {
            let start = one_line(&words(&self.quote.suffix));
            return match (&self.section_title, start.is_empty()) {
                // A bookmark on the section's title: the title alone.
                (Some(section), false) if start == one_line(section) => section.clone(),
                (Some(section), false) => format!("{section} — {start}"),
                (Some(section), true) => section.clone(),
                (None, false) => start,
                (None, true) => "Bookmark".to_owned(),
            };
        }
        let text = one_line(&self.quote.text);
        if text::char_len(&text) > LABEL_CHARS {
            format!("{}…", text::first_chars(&text, LABEL_CHARS).trim_end())
        } else {
            text
        }
    }
}

/// Whitespace runs (including line breaks) collapsed to single spaces, trimmed.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A bookmark's fingerprint cut back to whole words, with "…" if it was cut.
fn words(suffix: &str) -> String {
    if text::char_len(suffix) < anchor::BOOKMARK_CHARS {
        return suffix.to_owned();
    }
    match suffix.trim_end().rsplit_once(char::is_whitespace) {
        Some((head, _)) if !head.trim().is_empty() => format!("{}…", head.trim_end()),
        _ => format!("{suffix}…"),
    }
}

/// The current time in Unix seconds.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::model::BlockId;

    pub(crate) fn sample(text: &str, note: Option<&str>) -> Annotation {
        let b = BlockId::new(3).unwrap();
        Annotation {
            id: "id".into(),
            book_key: vec![1],
            book_id: 1,
            anchor: Anchor::at_block(b),
            quote: Quote {
                text: text.into(),
                prefix: String::new(),
                suffix: "It was a dark and stormy night; the rain fell in torrents, except at"
                    .into(),
            },
            section_title: Some("Chapter 1".into()),
            book_title: "Book".into(),
            book_authors: "A. Author".into(),
            anchor_lost: false,
            note: note.map(str::to_owned),
            created_at: 0,
            updated_at: 0,
            deleted_at: None,
            deleted_reason: None,
            profile: crate::profiles::ProfileId::OWNER,
        }
    }

    #[test]
    fn labels() {
        assert_eq!(
            sample("", None).label(),
            "Chapter 1 — It was a dark and stormy night; the rain fell in torrents, except…"
        );
        assert_eq!(sample("", Some("\n  My place \nmore")).label(), "My place");
        assert_eq!(sample("", Some("   ")).label(), sample("", None).label());
        assert_eq!(sample("a\nquote", None).label(), "a quote");
        let long = "word ".repeat(40);
        let label = sample(&long, None).label();
        assert!(
            label.ends_with('…') && text::char_len(&label) <= 81,
            "{label}"
        );
        let mut bare = sample("", None);
        bare.section_title = None;
        bare.quote.suffix = "Short".into();
        assert_eq!(bare.label(), "Short");
        bare.quote.suffix.clear();
        assert_eq!(bare.label(), "Bookmark");
        let mut heading = sample("", None);
        heading.quote.suffix = "Chapter 1".into();
        assert_eq!(heading.label(), "Chapter 1");
        assert!(!sample("x", None).is_bookmark() && sample("", None).is_bookmark());
    }

    #[test]
    fn deleted_reasons_round_trip() {
        for reason in [DeletedReason::User, DeletedReason::BookRemoved] {
            assert_eq!(DeletedReason::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(DeletedReason::parse("x"), None);
    }
}
