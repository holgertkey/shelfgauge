//! The bookmark, quote and note lists of the reader's sidebar: their items and labels.
//!
//! The instance builds them from its stored annotations, the hub's reader app from
//! sync records; both render them with the same template
//! (`templates/partials/annotation_lists.html` in the repository).

use super::anchor::{BOOKMARK_CHARS, Quote};
use super::export::{self, ExportHead, ExportItem, ExportKind};
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
    /// The text of the link ([`label`]); for a bookmark, [`Lists::new`] makes it
    /// `Bookmark <number> · <percent>%` ([`bookmark_label`]).
    pub label: String,
    /// A bookmark's place: the start of the line it points to, else the section's
    /// title (its second line in the list; set by [`Lists::new`]); empty for other
    /// items.
    pub place: String,
    /// The note, empty without one.
    pub note: String,
    /// The quoted text; empty for a bookmark.
    pub text: String,
    /// The section title at its start (the export groups quotes by it).
    pub section_title: Option<String>,
    /// Whether this is a bookmark (no quoted text, not a note).
    pub bookmark: bool,
    /// Whether this is a note on a place (no quoted text): listed after the bookmarks.
    pub place_note: bool,
    /// When it was made (Unix seconds): bookmarks are numbered and listed in this
    /// order.
    pub created_at: i64,
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

/// A book's bookmarks (in the order they were made), quotes and notes on places (in
/// book order).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lists {
    /// The bookmarks, numbered in the order they were made.
    pub bookmarks: Vec<ListItem>,
    /// The quotes (marks).
    pub marks: Vec<ListItem>,
    /// The notes on places (shown in the Bookmarks tab, after the bookmarks).
    pub notes: Vec<ListItem>,
    /// Where the forms return to.
    pub back: String,
    /// The book, for the export and the copied quotes.
    pub book: ExportHead,
}

impl Lists {
    /// Sorts `items` (in book order) into bookmarks, quotes and notes. Quotes and notes
    /// keep their order; bookmarks are put in the order they were made and labelled
    /// [`bookmark_label`], with [`ListItem::place`] from their place (never from a
    /// note).
    pub fn new(items: impl IntoIterator<Item = ListItem>, back: String, book: ExportHead) -> Self {
        let mut lists = Self {
            back,
            book,
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
        // Stable: bookmarks made in the same second stay in book order.
        lists.bookmarks.sort_by_key(|b| b.created_at);
        for (i, bookmark) in lists.bookmarks.iter_mut().enumerate() {
            // Its place, never its note (a bookmark of 0.24 can have one).
            let quote = Quote {
                text: String::new(),
                prefix: String::new(),
                suffix: bookmark.suffix.clone(),
            };
            bookmark.place = label(None, &quote, bookmark.section_title.as_deref());
            bookmark.label = bookmark_label(i + 1, bookmark.progress);
        }
        lists
    }

    /// Whether there are neither quotes nor notes (nothing to export or copy).
    pub fn no_quotes_or_notes(&self) -> bool {
        self.marks.is_empty() && self.notes.is_empty()
    }

    /// Whether there are neither bookmarks nor notes (the Bookmarks tab is empty).
    pub fn no_bookmarks_or_notes(&self) -> bool {
        self.bookmarks.is_empty() && self.notes.is_empty()
    }

    /// The Quotes tab's export: the quotes and notes in the
    /// [`unlinked`](export::unlinked) form.
    pub fn export_text(&self) -> String {
        let quotes = self.marks.iter().map(|m| ExportItem {
            kind: ExportKind::Quote,
            text: &m.text,
            note: Some(m.note.as_str()),
            section_title: m.section_title.as_deref(),
            label: String::new(),
            href: String::new(),
            lost: m.lost,
        });
        let notes = self.notes.iter().map(|n| ExportItem {
            kind: ExportKind::PlaceNote,
            text: "",
            note: Some(n.note.as_str()),
            section_title: n.section_title.as_deref(),
            label: n.label.clone(),
            href: String::new(),
            lost: n.lost,
        });
        export::unlinked(&self.book, &quotes.chain(notes).collect::<Vec<_>>())
    }

    /// The file name of [`Lists::export_text`].
    pub fn export_file_name(&self) -> String {
        export::quotes_file_name(&self.book)
    }

    /// The start of a selection's export ([`unlinked_head`](export::unlinked_head)).
    pub fn export_head(&self) -> String {
        export::unlinked_head(&self.book)
    }

    /// A quote's record in a selection's export ([`quote_record`](export::quote_record)).
    pub fn export_record(&self, item: &ListItem) -> String {
        export::quote_record(&item.text, Some(&item.note))
    }

    /// The heading of a quote's section in a selection's export
    /// ([`section_heading`](export::section_heading)); `None` outside a section.
    pub fn export_section(&self, item: &ListItem) -> Option<String> {
        item.section_title.as_deref().map(export::section_heading)
    }

    /// One quote for a message ([`copy_text`](export::copy_text)).
    pub fn copy_text(&self, item: &ListItem) -> String {
        export::copy_text(&self.book, &item.text, Some(&item.note))
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

/// The label of the `number`th bookmark (from 1, in the order they were made), with
/// its place in whole percent of the main text when `progress` (ten-thousandths) is
/// known.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::annotations::lists::bookmark_label;
///
/// assert_eq!(bookmark_label(1, Some(512)), "Bookmark 1 · 5%");
/// assert_eq!(bookmark_label(2, None), "Bookmark 2");
/// ```
pub fn bookmark_label(number: usize, progress: Option<u64>) -> String {
    match progress {
        Some(progress) => format!("Bookmark {number} · {}%", progress.min(10_000) / 100),
        None => format!("Bookmark {number}"),
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
            place: String::new(),
            note: String::new(),
            text: String::new(),
            section_title: None,
            bookmark,
            place_note,
            created_at: 0,
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
            ExportHead::default(),
        );
        let ids = |list: &[ListItem]| list.iter().map(|i| i.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&lists.bookmarks), ["b1"]);
        assert_eq!(ids(&lists.marks), ["q1", "q2"]);
        assert_eq!(ids(&lists.notes), ["n1", "n2"]);
        assert_eq!(lists.back, "/back");
        assert!(!lists.no_quotes_or_notes());
        let only_notes = Lists::new(
            [item("n", false, true)],
            String::new(),
            ExportHead::default(),
        );
        assert!(only_notes.marks.is_empty() && !only_notes.no_quotes_or_notes());
        assert!(only_notes.bookmarks.is_empty() && !only_notes.no_bookmarks_or_notes());
        let only_bookmarks = Lists::new(
            [item("b", true, false)],
            String::new(),
            ExportHead::default(),
        );
        assert!(only_bookmarks.no_quotes_or_notes() && !only_bookmarks.no_bookmarks_or_notes());
        assert!(Lists::default().no_bookmarks_or_notes());
    }

    #[test]
    fn bookmarks_are_numbered_in_the_order_they_were_made() {
        let bookmark = |id: &str, created_at: i64, progress: Option<u64>, suffix: &str| ListItem {
            created_at,
            progress,
            suffix: suffix.into(),
            ..item(id, true, false)
        };
        // In book order, as the callers pass them; "late" and "same" were made in the
        // same second (they keep book order).
        let mut note = item("n", false, true);
        note.label = "Note's place".into();
        note.created_at = 1;
        let lists = Lists::new(
            [
                bookmark("early-in-book", 30, Some(512), "It was"),
                note,
                bookmark("late", 20, None, "Later"),
                bookmark("same", 20, Some(9_999), "Last line"),
                bookmark("first", 10, Some(10_000), "The end"),
            ],
            String::new(),
            ExportHead::default(),
        );
        let shown = lists
            .bookmarks
            .iter()
            .map(|b| (b.id.as_str(), b.label.as_str(), b.place.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            shown,
            [
                ("first", "Bookmark 1 · 100%", "The end"),
                ("late", "Bookmark 2", "Later"),
                ("same", "Bookmark 3 · 99%", "Last line"),
                ("early-in-book", "Bookmark 4 · 5%", "It was"),
            ]
        );
        assert_eq!(
            lists.notes[0].label, "Note's place",
            "notes keep their label"
        );
        assert!(lists.notes[0].place.is_empty());

        // A bookmark of 0.24 with a note: the place is its line, never the note; with
        // no text there, the section's title.
        let mut named = bookmark("named", 1, None, "The line");
        named.note = "My name for it".into();
        named.label = "My name for it".into();
        let mut heading = bookmark("heading", 2, None, "");
        heading.section_title = Some("Chapter 3".into());
        let lists = Lists::new([named, heading], String::new(), ExportHead::default());
        assert_eq!(lists.bookmarks[0].place, "The line");
        assert_eq!(lists.bookmarks[1].place, "Chapter 3");
    }

    #[test]
    fn export_and_copy_from_the_lists() {
        let mut quote = item("q", false, false);
        quote.text = "Quoted text.".into();
        quote.note = "Mine".into();
        quote.section_title = Some("Chapter 1".into());
        let mut bare = item("q2", false, false);
        bare.text = "Another.".into();
        bare.section_title = Some("Chapter 1".into());
        let mut note = item("n", false, true);
        note.note = "A thought".into();
        note.label = "The place".into();
        let mut bookmark = item("b", true, false);
        bookmark.label = "A bookmark".into();
        let book = ExportHead {
            title: "T".into(),
            authors: "A".into(),
        };
        let lists = Lists::new([bookmark, quote, note, bare], String::new(), book);
        assert_eq!(
            lists.export_text(),
            "# T\n\nA\n\n## Quotes\n\n### Chapter 1\n\nQuoted text.\n\nNote: Mine\n\n\
             ***\n\nAnother.\n\n## Notes\n\nThe place\n\nNote: A thought\n"
        );
        assert_eq!(lists.export_file_name(), "A — T — quotes.md");
        assert_eq!(
            lists.copy_text(&lists.marks[0]),
            "Quoted text.\n\nNote: Mine\n\n— A, T"
        );
        assert_eq!(lists.copy_text(&lists.marks[1]), "Another.\n\n— A, T");
    }
}
