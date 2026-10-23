//! An opened publication: the manifest and the part blobs decoded so far, and once
//! all parts are in, the book's text (quotes, positions, lists, search).

use std::collections::BTreeMap;

use askama::Template;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use shelfgauge_core::annotations::anchor::{self, Anchor, Quote};
use shelfgauge_core::annotations::lists::{ListItem, Lists, label};
use shelfgauge_core::annotations::results::FindResults;
use shelfgauge_core::annotations::text::BookText;
use shelfgauge_core::format::model::BlockId;
use shelfgauge_core::publication::{FormatError, Manifest, PartBlob, assemble};
use shelfgauge_core::render::html::{ReaderLinks, render_toc_entries};
use shelfgauge_core::render::outline::{ChapterTick, PartProgress, SHARE_SCALE, html_lang};
use shelfgauge_core::render::split::{Part, part_of};

/// Characters percent-encoded in query values (as the instance's `Urls`).
const QUERY_VALUE: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~')
    .remove(b'/');

/// Links of a part rendered on the device: other parts are `?part=<n>` of the same
/// page (`/r/<id>`), images get no URL (the app fills in `blob:` URLs by image id).
struct Links;

impl ReaderLinks for Links {
    fn block(&self, part: usize, id: BlockId) -> String {
        format!("?part={}#{id}", part + 1)
    }

    fn image(&self, _image_id: &str) -> String {
        String::new()
    }
}

/// What the reader page needs from the manifest, as JSON.
#[derive(Serialize)]
struct Summary<'a> {
    format_version: u16,
    title: &'a str,
    authors: &'a [String],
    series: &'a [String],
    /// The `lang` attribute of the page, if the language is known and well-formed.
    lang: Option<String>,
    /// base64url without padding, as in the sync records.
    content_key: String,
    block_count: u32,
    parts: Vec<PartSummary<'a>>,
    chapters: &'a [ChapterTick],
    images: Vec<ImageSummary<'a>>,
    cover: Option<u32>,
}

#[derive(Serialize)]
struct PartSummary<'a> {
    start: BlockId,
    end: BlockId,
    title: &'a str,
    head: Option<&'a str>,
    progress: Option<PartProgress>,
}

#[derive(Serialize)]
struct ImageSummary<'a> {
    id: &'a str,
    mime: &'a str,
}

/// A publication opened from its decrypted, inflated blobs.
#[derive(Debug)]
pub struct Book {
    manifest: Manifest,
    parts: Vec<Part>,
    blobs: Vec<Option<PartBlob>>,
    /// The whole book's text, once every part is in ([`Book::load_text`]).
    text: Option<BookText>,
}

impl Book {
    /// Opens a publication from its manifest (the postcard bytes).
    pub fn open(manifest: &[u8]) -> Result<Self, FormatError> {
        let manifest = Manifest::decode(manifest)?;
        let parts = manifest.split();
        let blobs = vec![None; parts.len()];
        Ok(Self {
            manifest,
            parts,
            blobs,
            text: None,
        })
    }

    /// The manifest's figures for the page, as JSON.
    pub fn summary(&self) -> String {
        let m = &self.manifest;
        let summary = Summary {
            format_version: m.format_version,
            title: &m.title,
            authors: &m.authors,
            series: &m.series,
            lang: html_lang(m.language.as_deref()),
            content_key: URL_SAFE_NO_PAD.encode(&m.content_key),
            block_count: m.block_count,
            parts: m
                .parts
                .iter()
                .map(|p| PartSummary {
                    start: p.start,
                    end: p.end,
                    title: &p.title,
                    head: p.head.as_deref(),
                    progress: p.progress,
                })
                .collect(),
            chapters: &m.chapters,
            images: m
                .images
                .iter()
                .map(|i| ImageSummary {
                    id: &i.id,
                    mime: &i.mime,
                })
                .collect(),
            cover: m.cover,
        };
        serde_json::to_string(&summary).expect("the summary serializes")
    }

    /// The number of parts.
    pub fn part_count(&self) -> usize {
        self.parts.len()
    }

    /// Adds the blob of part `index` (0-based; the postcard bytes).
    pub fn add_part(&mut self, index: usize, bytes: &[u8]) -> Result<(), FormatError> {
        let slot = self
            .blobs
            .get_mut(index)
            .ok_or_else(|| FormatError::Malformed(format!("there is no part {index}")))?;
        *slot = Some(PartBlob::decode(bytes)?);
        Ok(())
    }

    /// The HTML of part `index`, which must have been added.
    pub fn render_part(&self, index: usize) -> Result<String, FormatError> {
        let blob = self
            .blobs
            .get(index)
            .and_then(Option::as_ref)
            .ok_or_else(|| FormatError::Malformed(format!("part {index} is not loaded")))?;
        Ok(blob.render(&self.parts, index, &Links))
    }

    /// The table of contents with part `current` marked (empty without contents).
    pub fn render_toc(&self, current: usize) -> String {
        render_toc_entries(&self.manifest.toc, &self.parts, current, &Links)
    }

    /// Whether every part has been added.
    pub fn complete(&self) -> bool {
        self.blobs.iter().all(Option::is_some)
    }

    /// Builds the book's text from all parts (needed for quotes, positions, lists and
    /// search); fails while a part is missing or when the parts do not fit together.
    pub fn load_text(&mut self) -> Result<(), FormatError> {
        if self.text.is_some() {
            return Ok(());
        }
        let blobs = self
            .blobs
            .iter()
            .map(|blob| {
                blob.clone()
                    .ok_or_else(|| FormatError::Malformed("a part is not loaded".into()))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let content = assemble(blobs, BTreeMap::new(), self.manifest.block_count)?;
        self.text = Some(BookText::new(&content));
        Ok(())
    }

    fn text(&self) -> Result<&BookText, BookError> {
        self.text
            .as_ref()
            .ok_or_else(|| BookError("the book's text is not loaded".into()))
    }

    /// The 0-based part of `block`.
    pub fn part_of(&self, block: BlockId) -> Option<usize> {
        part_of(&self.parts, block)
    }

    /// The quote of a selection, as for a new annotation: refused across parts, empty,
    /// reversed or outside the book.
    pub fn quote(&self, anchor: &Anchor) -> Result<Quoted, BookError> {
        let text = self.text()?;
        let quote = anchor::quote(text, anchor).map_err(|e| BookError(e.to_string()))?;
        if self.part_of(anchor.start_block) != self.part_of(anchor.end_block) {
            return Err(BookError(
                "a selection must lie within one part of the book".into(),
            ));
        }
        Ok(Quoted::new(text, anchor.start_block, quote))
    }

    /// The quote of a bookmark on `block`.
    pub fn bookmark(&self, block: BlockId) -> Result<Quoted, BookError> {
        let text = self.text()?;
        let quote = anchor::bookmark_quote(text, block).map_err(|e| BookError(e.to_string()))?;
        Ok(Quoted::new(text, block, quote))
    }

    /// A reading position at `block`: its fingerprint and progress (per mille);
    /// `None` in the notes (not a reading position).
    pub fn position(&self, block: BlockId) -> Result<Option<Position>, BookError> {
        let text = self.text()?;
        let quote = anchor::bookmark_quote(text, block).map_err(|e| BookError(e.to_string()))?;
        Ok(text.progress(block).map(|progress| Position {
            suffix: quote.suffix,
            progress,
        }))
    }

    /// The block to open a saved position at: `block` if it starts with `suffix`, else
    /// where the fingerprint is found (the book changed), else `block` if it exists.
    pub fn resume(&self, block: BlockId, suffix: &str) -> Option<BlockId> {
        let text = self.text.as_ref()?;
        let fits = text
            .text(block)
            .is_some_and(|t| text.is_leaf(block) && t.starts_with(suffix));
        if fits {
            return Some(block);
        }
        let quote = Quote {
            suffix: suffix.to_owned(),
            ..Quote::default()
        };
        anchor::find(text, &quote, block)
            .map(|a| a.start_block)
            .or_else(|| text.text(block).map(|_| block))
    }

    /// The sidebar's bookmark and quote lists (the instance's
    /// `partials/annotation_lists.html`) for `annotations`, the book's live annotation
    /// records. Each delete form posts to `?delete=<id>` (the app handles it).
    pub fn lists(&self, annotations: &[AnnotationRecord]) -> Result<String, BookError> {
        let text = self.text()?;
        let mut sorted: Vec<&AnnotationRecord> = annotations.iter().collect();
        sorted.sort_by(|a, b| {
            (
                a.start_block,
                a.start_offset,
                a.end_block,
                a.end_offset,
                a.created_at,
                &a.id,
            )
                .cmp(&(
                    b.start_block,
                    b.start_offset,
                    b.end_block,
                    b.end_offset,
                    b.created_at,
                    &b.id,
                ))
        });
        let items = sorted.into_iter().filter_map(|a| {
            let anchor = a.anchor()?;
            let quote = Quote {
                text: a.text.clone(),
                prefix: a.prefix.clone(),
                suffix: a.suffix.clone(),
            };
            // A place that no longer holds the quote (cannot happen for the same
            // content key unless the record is damaged): listed as not found.
            let lost = if quote.text.is_empty() {
                anchor::bookmark_quote(text, anchor.start_block).is_err()
            } else {
                anchor::quote(text, &anchor).map_or(true, |q| q.text != quote.text)
            };
            let part = self.part_of(anchor.start_block).unwrap_or(0);
            Some(ListItem {
                id: a.id.clone(),
                href: format!("?part={}&from=mark#{}", part + 1, anchor.start_block),
                label: label(a.note.as_deref(), &quote, a.section_title.as_deref()),
                note: a
                    .note
                    .as_deref()
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or("")
                    .to_owned(),
                bookmark: quote.text.is_empty(),
                text: quote.text,
                lost,
                start_block: anchor.start_block.to_string(),
                start_offset: anchor.start_offset,
                end_block: anchor.end_block.to_string(),
                end_offset: anchor.end_offset,
                prefix: quote.prefix,
                suffix: quote.suffix,
                note_action: String::new(),
                delete_action: format!("?delete={}", a.id),
                progress: (!lost)
                    .then(|| text.share_before(anchor.start_block, SHARE_SCALE))
                    .flatten(),
            })
        });
        ListsFragment {
            lists: Lists::new(items, String::new()),
        }
        .render()
        .map_err(|e| BookError(e.to_string()))
    }

    /// The Search tab's results for the raw `query` (the instance's
    /// `partials/find_results.html`); `current` is the result followed to the page.
    pub fn find(&self, query: &str, current: Option<usize>) -> Result<String, BookError> {
        let text = self.text()?;
        let results = FindResults::new(&self.parts, text, query, current, &|part, query, index| {
            let mut url = String::from("?");
            if part > 1 {
                url.push_str(&format!("part={part}&"));
            }
            url.push_str(&format!(
                "find={}&hit={index}",
                utf8_percent_encode(query, QUERY_VALUE)
            ));
            url
        });
        FindFragment { results }
            .render()
            .map_err(|e| BookError(e.to_string()))
    }
}

/// A failure the reader app shows or logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BookError(pub String);

impl std::fmt::Display for BookError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A quote with what a new annotation stores besides it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Quoted {
    /// The selected text (empty for a bookmark).
    pub text: String,
    /// The context before.
    pub prefix: String,
    /// The context after (a bookmark: the start of the block).
    pub suffix: String,
    /// The innermost section title at the start.
    pub section_title: Option<String>,
}

impl Quoted {
    fn new(text: &BookText, block: BlockId, quote: Quote) -> Self {
        Self {
            text: quote.text,
            prefix: quote.prefix,
            suffix: quote.suffix,
            section_title: text.section_title(block).map(str::to_owned),
        }
    }
}

/// A reading position's fingerprint and progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Position {
    /// The start of the block.
    pub suffix: String,
    /// Per mille of the main text.
    pub progress: u16,
}

/// The fields of an `annotation` record the lists need (blocks as ordinals).
#[derive(Debug, Clone, Deserialize)]
pub struct AnnotationRecord {
    /// The UUID.
    pub id: String,
    /// The start block's ordinal.
    pub start_block: u32,
    /// Code points into the start block.
    pub start_offset: u32,
    /// The end block's ordinal.
    pub end_block: u32,
    /// Code points into the end block.
    pub end_offset: u32,
    /// The quoted text; empty for a bookmark.
    pub text: String,
    /// The context before.
    pub prefix: String,
    /// The context after.
    pub suffix: String,
    /// The section title at the start.
    pub section_title: Option<String>,
    /// The note.
    pub note: Option<String>,
    /// When it was made (Unix seconds).
    pub created_at: i64,
}

impl AnnotationRecord {
    fn anchor(&self) -> Option<Anchor> {
        Some(Anchor {
            start_block: BlockId::new(self.start_block)?,
            start_offset: self.start_offset,
            end_block: BlockId::new(self.end_block)?,
            end_offset: self.end_offset,
        })
    }
}

/// The lists, rendered as on the instance.
#[derive(Template)]
#[template(path = "partials/annotation_lists.html")]
struct ListsFragment {
    lists: Lists,
}

/// The search results, rendered as on the instance.
#[derive(Template)]
#[template(path = "partials/find_results.html")]
struct FindFragment {
    results: FindResults,
}
