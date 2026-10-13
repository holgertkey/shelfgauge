//! Anchors: where an annotation is in a book, and how to find it again.
//!
//! An [`Anchor`] is a range between two code point positions in leaf blocks. Its
//! [`Quote`] (the selected text with a little context) is derived from the book on the
//! server, never taken from the client, so it always matches the rendered text and can
//! be searched for when the book file changes ([`find`]).

use super::text::{self, BLOCK_SEPARATOR, BookText, char_len};
use crate::format::model::BlockId;

/// Code points of context kept before and after a selection.
pub const CONTEXT_CHARS: usize = 32;

/// Code points of a block's start kept as a bookmark's fingerprint and label.
pub const BOOKMARK_CHARS: usize = 64;

/// The shortest quote that is searched for without its context when the full
/// fingerprint is not found.
const MIN_BARE_QUOTE_CHARS: usize = 16;

/// A position range in a book: from `start_offset` in `start_block` to `end_offset` in
/// `end_block`, in code points of the blocks' rendered text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// The leaf block where the range starts.
    pub start_block: BlockId,
    /// The code point offset in `start_block`.
    pub start_offset: u32,
    /// The leaf block where the range ends.
    pub end_block: BlockId,
    /// The code point offset in `end_block` (exclusive end).
    pub end_offset: u32,
}

impl Anchor {
    /// A point anchor at the start of `block` (a bookmark).
    pub fn at_block(block: BlockId) -> Self {
        Self {
            start_block: block,
            start_offset: 0,
            end_block: block,
            end_offset: 0,
        }
    }
}

/// The text at an anchor and its context: the annotation's immutable fingerprint.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Quote {
    /// The selected text (blocks separated by `\n`); empty for a bookmark.
    pub text: String,
    /// Up to [`CONTEXT_CHARS`] code points before the text (empty for a bookmark).
    pub prefix: String,
    /// Up to [`CONTEXT_CHARS`] code points after the text; for a bookmark, the first
    /// [`BOOKMARK_CHARS`] code points of the block.
    pub suffix: String,
}

/// Why an anchor is not valid for a book.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AnchorError {
    /// A block does not exist or contains other blocks.
    #[error("block {0} is not a text block of this book")]
    NoSuchBlock(BlockId),
    /// An offset is past the end of its block.
    #[error("offset {offset} is outside block {block}")]
    BadOffset {
        /// The block.
        block: BlockId,
        /// The offset.
        offset: u32,
    },
    /// The range ends before it starts.
    #[error("the selection ends before it starts")]
    Reversed,
    /// The range selects nothing.
    #[error("the selection is empty")]
    Empty,
}

/// The quote at a selection `anchor`, after checking it against the book.
///
/// # Examples
///
/// ```
/// use shelfgauge::annotations::anchor::{Anchor, quote};
/// use shelfgauge::annotations::text::BookText;
/// use shelfgauge::format::model::{Block, BlockId, Body, BookContent, Section};
///
/// let root = Section {
///     blocks: vec![Block::paragraph("Call me Ishmael."), Block::paragraph("Some years ago")],
///     ..Section::default()
/// };
/// let book = BookText::new(&BookContent::new(vec![Body::main(root)], Default::default()));
/// let b = |n| BlockId::new(n).unwrap();
/// let q = quote(&book, &Anchor { start_block: b(1), start_offset: 8, end_block: b(2), end_offset: 4 })
///     .unwrap();
/// assert_eq!(q.text, "Ishmael.\nSome");
/// assert_eq!(q.prefix, "Call me ");
/// assert_eq!(q.suffix, " years ago");
/// ```
pub fn quote(book: &BookText, anchor: &Anchor) -> Result<Quote, AnchorError> {
    let start = position(book, anchor.start_block, anchor.start_offset)?;
    let end = position(book, anchor.end_block, anchor.end_offset)?;
    if end < start {
        return Err(AnchorError::Reversed);
    }
    let flat = book.flat();
    let selected = &flat[start..end];
    if selected.chars().all(|c| c == BLOCK_SEPARATOR) {
        return Err(AnchorError::Empty);
    }
    Ok(Quote {
        text: selected.to_owned(),
        prefix: text::last_chars(&flat[..start], CONTEXT_CHARS).to_owned(),
        suffix: text::first_chars(&flat[end..], CONTEXT_CHARS).to_owned(),
    })
}

/// The quote of a bookmark on `block`: no text, the start of the block as `suffix`.
pub fn bookmark_quote(book: &BookText, block: BlockId) -> Result<Quote, AnchorError> {
    let text = book
        .text(block)
        .filter(|_| book.is_leaf(block))
        .ok_or(AnchorError::NoSuchBlock(block))?;
    Ok(Quote {
        text: String::new(),
        prefix: String::new(),
        suffix: text::first_chars(text, BOOKMARK_CHARS).to_owned(),
    })
}

fn position(book: &BookText, block: BlockId, offset: u32) -> Result<usize, AnchorError> {
    if !book.is_leaf(block) {
        return Err(AnchorError::NoSuchBlock(block));
    }
    book.flat_position(block, offset as usize)
        .ok_or(AnchorError::BadOffset { block, offset })
}

/// Finds an annotation's quote in a (possibly changed) book, preferring the occurrence
/// nearest to block `near`.
///
/// A selection is searched for as `prefix + text + suffix`, and if that is not found
/// and the text is long enough, as the text alone. A bookmark (empty text) is found as
/// a leaf block that starts with its `suffix`. Returns `None` if nothing matches.
pub fn find(book: &BookText, quote: &Quote, near: BlockId) -> Option<Anchor> {
    if quote.text.is_empty() {
        if quote.suffix.is_empty() {
            return None;
        }
        return book
            .leaves()
            .iter()
            .copied()
            .filter(|&id| book.text(id).is_some_and(|t| t.starts_with(&quote.suffix)))
            .min_by_key(|id| distance(*id, near))
            .map(Anchor::at_block);
    }
    let flat = book.flat();
    let full = format!("{}{}{}", quote.prefix, quote.text, quote.suffix);
    let candidates = |needle: &str, skip: usize| -> Vec<usize> {
        flat.match_indices(needle).map(|(i, _)| i + skip).collect()
    };
    let mut starts = candidates(&full, quote.prefix.len());
    if starts.is_empty() && char_len(&quote.text) >= MIN_BARE_QUOTE_CHARS {
        starts = candidates(&quote.text, 0);
    }
    starts
        .into_iter()
        .filter_map(|start| {
            let (start_block, start_offset) = book.block_at(start)?;
            let (end_block, end_offset) = book.block_at(start + quote.text.len())?;
            Some(Anchor {
                start_block,
                start_offset: start_offset as u32,
                end_block,
                end_offset: end_offset as u32,
            })
        })
        .min_by_key(|anchor| distance(anchor.start_block, near))
}

fn distance(a: BlockId, b: BlockId) -> u32 {
    a.ordinal().abs_diff(b.ordinal())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::model::{Block, BlockKind, Body, BookContent, Section};

    fn b(n: u32) -> BlockId {
        BlockId::new(n).unwrap()
    }

    fn book(paragraphs: &[&str]) -> BookText {
        let root = Section {
            blocks: paragraphs.iter().map(|p| Block::paragraph(*p)).collect(),
            ..Section::default()
        };
        BookText::new(&BookContent::new(
            vec![Body::main(root)],
            Default::default(),
        ))
    }

    fn anchor(sb: u32, so: u32, eb: u32, eo: u32) -> Anchor {
        Anchor {
            start_block: b(sb),
            start_offset: so,
            end_block: b(eb),
            end_offset: eo,
        }
    }

    #[test]
    fn quotes_within_and_across_blocks() {
        let text = book(&["Ёлка 😀 зелёная", "второй", "третий"]);
        let q = quote(&text, &anchor(1, 5, 1, 6)).unwrap();
        assert_eq!(q.text, "😀");
        assert_eq!(q.prefix, "Ёлка ");
        assert_eq!(q.suffix, " зелёная\nвторой\nтретий");
        let q = quote(&text, &anchor(1, 7, 3, 3)).unwrap();
        assert_eq!(q.text, "зелёная\nвторой\nтре");
        assert_eq!(q.suffix, "тий");
        // Selecting to the end of a block.
        let q = quote(&text, &anchor(2, 0, 2, 6)).unwrap();
        assert_eq!(q.text, "второй");
    }

    #[test]
    fn invalid_anchors_are_rejected() {
        let text = book(&["abc", "def"]);
        assert_eq!(
            quote(&text, &anchor(1, 0, 3, 1)),
            Err(AnchorError::NoSuchBlock(b(3)))
        );
        assert_eq!(
            quote(&text, &anchor(1, 4, 1, 4)),
            Err(AnchorError::BadOffset {
                block: b(1),
                offset: 4
            })
        );
        assert_eq!(
            quote(&text, &anchor(2, 1, 1, 2)),
            Err(AnchorError::Reversed)
        );
        assert_eq!(
            quote(&text, &anchor(1, 2, 1, 1)),
            Err(AnchorError::Reversed)
        );
        assert_eq!(quote(&text, &anchor(1, 1, 1, 1)), Err(AnchorError::Empty));
        assert_eq!(quote(&text, &anchor(1, 3, 2, 0)), Err(AnchorError::Empty));
        let root = Section {
            blocks: vec![Block::new(BlockKind::Cite(vec![Block::paragraph("x")]))],
            ..Section::default()
        };
        let text = BookText::new(&BookContent::new(
            vec![Body::main(root)],
            Default::default(),
        ));
        assert_eq!(
            quote(&text, &anchor(1, 0, 2, 1)),
            Err(AnchorError::NoSuchBlock(b(1)))
        );
        assert!(bookmark_quote(&text, b(1)).is_err());
        assert_eq!(bookmark_quote(&text, b(2)).unwrap().suffix, "x");
    }

    #[test]
    fn find_prefers_the_nearest_full_match() {
        // Identical paragraphs with more identical context than the fingerprint keeps.
        let para = format!("{} two {}", "x".repeat(40), "y".repeat(40));
        let old = book(&[&para, "z", &para]);
        let q = quote(&old, &anchor(3, 41, 3, 44)).unwrap();
        assert_eq!(q.text, "two");
        assert_eq!(find(&old, &q, b(3)), Some(anchor(3, 41, 3, 44)));
        assert_eq!(find(&old, &q, b(1)), Some(anchor(1, 41, 1, 44)));
        // Two paragraphs inserted before: found at its new place.
        let new = book(&["new", "new", &para, "z", &para]);
        assert_eq!(find(&new, &q, b(5)), Some(anchor(5, 41, 5, 44)));
        assert_eq!(find(&new, &q, b(2)), Some(anchor(3, 41, 3, 44)));
        // Gone.
        let gone = book(&["completely different"]);
        assert_eq!(find(&gone, &q, b(1)), None);
    }

    #[test]
    fn find_falls_back_to_a_long_bare_quote() {
        let old = book(&["before a fairly long quoted sentence after"]);
        let q = quote(&old, &anchor(1, 7, 1, 35)).unwrap();
        let edited = book(&["changed: a fairly long quoted sentence, changed"]);
        let found = find(&edited, &q, b(1)).unwrap();
        assert_eq!(quote(&edited, &found).unwrap().text, q.text);
        // A short quote needs its context.
        let q = quote(&old, &anchor(1, 7, 1, 8)).unwrap();
        assert_eq!(find(&edited, &q, b(1)), None);
    }

    #[test]
    fn bookmarks_are_found_by_block_start() {
        let old = book(&["Chapter", "It was a dark night."]);
        let q = bookmark_quote(&old, b(2)).unwrap();
        assert_eq!(q.suffix, "It was a dark night.");
        let new = book(&["Preface", "Chapter", "It was a dark night. Really."]);
        assert_eq!(find(&new, &q, b(2)), Some(Anchor::at_block(b(3))));
        assert_eq!(find(&book(&["x"]), &q, b(2)), None);
        assert_eq!(find(&new, &Quote::default(), b(1)), None);
    }

    #[test]
    fn long_blocks_keep_a_short_bookmark_suffix() {
        let long = "ж".repeat(200);
        let text = book(&[&long]);
        assert_eq!(char_len(&bookmark_quote(&text, b(1)).unwrap().suffix), 64);
    }
}
