//! The bookmark, quote and note lists of the reader's sidebar: their items and labels.
//!
//! The instance builds them from its stored annotations, the hub's reader app from
//! sync records; both render them with the same template
//! (`templates/partials/annotation_lists.html` in the repository).

use super::anchor::{BOOKMARK_CHARS, Quote};
use super::text;

/// The longest label of a quote, in code points (then cut with "…").
const LABEL_CHARS: usize = 80;

/// An annotation in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListItem {
    /// The annotation's id (a UUID).
    pub id: String,
    /// Where it is in the book.
    pub href: String,
    /// The text of the link ([`label`]).
    pub label: String,
    /// The note, empty without one.
    pub note: String,
    /// The quoted text; empty for a bookmark.
    pub text: String,
    /// Whether this is a bookmark (no quoted text, not a note).
    pub bookmark: bool,
    /// Whether this is a note on a place (no quoted text): listed after the quotes.
    pub place_note: bool,
    /// Whether its place was not found in the book (the book has changed).
    pub lost: bool,
    /// The anchor: start block id, e.g. `b12`.
    pub start_block: String,
    /// The anchor: code points into the start block's rendered text.
    pub start_offset: u32,
    /// The anchor: end block id.
    pub end_block: String,
    /// The anchor: code points into the end block's rendered text.
    pub end_offset: u32,
    /// The text before the quote.
    pub prefix: String,
    /// The text after the quote.
    pub suffix: String,
    /// Where the note form posts.
    pub note_action: String,
    /// Where the delete form posts.
    pub delete_action: String,
    /// Where it starts, in ten-thousandths of the main text (bookmarks in the reader:
    /// the ticks on the status line); `None` when unknown or in the notes.
    pub progress: Option<u64>,
}

/// A book's bookmarks, quotes and notes on places, in book order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lists {
    /// The bookmarks.
    pub bookmarks: Vec<ListItem>,
    /// The quotes (marks).
    pub marks: Vec<ListItem>,
    /// The notes on places (shown after the quotes).
    pub notes: Vec<ListItem>,
    /// Where the forms return to.
    pub back: String,
}

impl Lists {
    /// Sorts `items` into bookmarks, quotes and notes, keeping their order.
    pub fn new(items: impl IntoIterator<Item = ListItem>, back: String) -> Self {
        let mut lists = Self {
            back,
            ..Self::default()
        };
        for item in items {
            if item.place_note {
                lists.notes.push(item);
            } else if item.bookmark {
                lists.bookmarks.push(item);
            } else {
                lists.marks.push(item);
            }
        }
        lists
    }

    /// Whether there are neither quotes nor notes (the Quotes tab is empty).
    pub fn no_quotes_or_notes(&self) -> bool {
        self.marks.is_empty() && self.notes.is_empty()
    }
}

/// The label of an annotation in lists: the note's first line, else the start of the
/// quoted text; for a bookmark without a note, the text from its place (the line it
/// points to), else the section's title. A note on a place is labelled by its place
/// (`label(None, ..)`): its note is shown under it.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::annotations::anchor::Quote;
/// use shelfgauge_core::annotations::lists::label;
///
/// let bookmark = Quote { text: String::new(), prefix: String::new(), suffix: "It was".into() };
/// assert_eq!(label(None, &bookmark, Some("Chapter 1")), "It was");
/// let empty = Quote { suffix: String::new(), ..bookmark.clone() };
/// assert_eq!(label(None, &empty, Some("Chapter 1")), "Chapter 1");
/// let quote = Quote { text: "a  dark\nnight".into(), ..bookmark.clone() };
/// assert_eq!(label(Some("\n mine \n"), &quote, None), "mine");
/// assert_eq!(label(None, &quote, None), "a dark night");
/// ```
pub fn label(note: Option<&str>, quote: &Quote, section_title: Option<&str>) -> String {
    if let Some(note) = note.filter(|n| !n.trim().is_empty()) {
        return one_line(note.lines().find(|l| !l.trim().is_empty()).unwrap_or(""));
    }
    if quote.text.is_empty() {
        let start = one_line(&words(&quote.suffix));
        return match (section_title, start.is_empty()) {
            (_, false) => start,
            (Some(section), true) => section.to_owned(),
            (None, true) => "Bookmark".to_owned(),
        };
    }
    let text = one_line(&quote.text);
    if text::char_len(&text) > LABEL_CHARS {
        format!("{}…", text::first_chars(&text, LABEL_CHARS).trim_end())
    } else {
        text
    }
}

/// Whitespace runs (including line breaks) collapsed to single spaces, trimmed.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A bookmark's fingerprint cut back to whole words, with "…" if it was cut.
fn words(suffix: &str) -> String {
    if text::char_len(suffix) < BOOKMARK_CHARS {
        return suffix.to_owned();
    }
    match suffix.trim_end().rsplit_once(char::is_whitespace) {
        Some((head, _)) if !head.trim().is_empty() => format!("{}…", head.trim_end()),
        _ => format!("{suffix}…"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, bookmark: bool, place_note: bool) -> ListItem {
        ListItem {
            id: id.into(),
            href: String::new(),
            label: String::new(),
            note: String::new(),
            text: String::new(),
            bookmark,
            place_note,
            lost: false,
            start_block: "b1".into(),
            start_offset: 0,
            end_block: "b1".into(),
            end_offset: 0,
            prefix: String::new(),
            suffix: String::new(),
            note_action: String::new(),
            delete_action: String::new(),
            progress: None,
        }
    }

    #[test]
    fn items_go_to_three_lists_in_order() {
        let lists = Lists::new(
            [
                item("n1", false, true),
                item("b1", true, false),
                item("q1", false, false),
                item("n2", false, true),
                item("q2", false, false),
            ],
            "/back".into(),
        );
        let ids = |list: &[ListItem]| list.iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&lists.bookmarks), ["b1"]);
        assert_eq!(ids(&lists.marks), ["q1", "q2"]);
        assert_eq!(ids(&lists.notes), ["n1", "n2"]);
        assert_eq!(lists.back, "/back");
        assert!(!lists.no_quotes_or_notes());
        let only_notes = Lists::new([item("n", false, true)], String::new());
        assert!(only_notes.marks.is_empty() && !only_notes.no_quotes_or_notes());
        assert!(Lists::new([item("b", true, false)], String::new()).no_quotes_or_notes());
    }
}
