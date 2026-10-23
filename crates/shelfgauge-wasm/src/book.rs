//! An opened publication: the manifest and the part blobs decoded so far.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Serialize;
use shelfgauge_core::format::model::BlockId;
use shelfgauge_core::publication::{FormatError, Manifest, PartBlob};
use shelfgauge_core::render::html::{ReaderLinks, render_toc_entries};
use shelfgauge_core::render::outline::{ChapterTick, PartProgress, html_lang};
use shelfgauge_core::render::split::Part;

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
}
