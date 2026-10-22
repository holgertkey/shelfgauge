//! Building publications: a book as the encrypted blobs of the publication format
//! (`shelfgauge_core::publication`, re-exported here as [`format`](mod@format)).
//!
//! [`build`] turns a book's content into a [`Publication`] (manifest, part blobs,
//! images; DRM-protected books are refused); [`Publication::seal`] compresses, pads and
//! encrypts every blob under a fresh [`BookKey`]; [`open`] reverses that for one blob.
//! Nothing here touches the network: uploading is the hub client's job.
//!
//! Book keys and publication ids are secrets: their `Debug` output hides the value and
//! they are never logged (`docs/ARCHITECTURE.md`, "Logging").

mod seal;

pub use seal::{OpenError, compress, encrypt_padded, inflate, open, seal};
pub use shelfgauge_core::publication as format;

use crate::annotations::text::BookText;
use crate::cache::covers;
use crate::config::ReaderConfig;
use crate::format::model::{BookContent, Image};
use crate::render::html::toc_entries;
use crate::render::outline::{chapter_ticks, html_lang, part_progress, title_before};
use crate::render::split::split;
use crate::storage::books::BookDetails;
use format::{BlobPath, FORMAT_VERSION, ImageInfo, Manifest, PartBlob, PartInfo, PublicationId};

/// A book key: 32 random bytes encrypting one publication's blobs (AES-256-GCM).
#[derive(Clone, PartialEq, Eq)]
pub struct BookKey([u8; 32]);

impl BookKey {
    /// A fresh random key.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key)?;
        Ok(Self(key))
    }

    /// The key with these bytes.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The key's bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for BookKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BookKey(..)")
    }
}

/// A fresh random publication id.
pub fn generate_publication_id() -> Result<PublicationId, getrandom::Error> {
    let mut id = [0u8; 16];
    getrandom::fill(&mut id)?;
    Ok(PublicationId::new(id))
}

/// What the library knows about a book, as the manifest shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookInfo {
    /// The title as the library shows it.
    pub title: String,
    /// The authors' display names.
    pub authors: Vec<String>,
    /// The series, as `Name #n` or `Name`.
    pub series: Vec<String>,
    /// The language code from the book.
    pub language: Option<String>,
    /// The DRM scheme, if the book is protected.
    pub drm: Option<String>,
    /// The content key ([`content_key`](crate::render::load::content_key)).
    pub content_key: Vec<u8>,
}

impl BookInfo {
    /// The info of a library book; `None` without a content key (a book never
    /// scanned completely).
    pub fn of(book: &BookDetails) -> Option<Self> {
        Some(Self {
            title: book.title.clone(),
            authors: book
                .authors
                .iter()
                .map(|a| a.display_name.clone())
                .collect(),
            series: book
                .series
                .iter()
                .map(|s| match &s.number {
                    Some(n) => format!("{} #{n}", s.name),
                    None => s.name.clone(),
                })
                .collect(),
            language: book.language.clone(),
            drm: book.drm.clone(),
            content_key: crate::render::load::content_key(book)?,
        })
    }
}

/// Why a book cannot be published.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PublishError {
    /// The book is protected by DRM; such books are never published.
    #[error("the book is protected by DRM ({0}) and cannot be published")]
    Protected(String),
}

/// A built publication, not yet encrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Publication {
    /// The manifest.
    pub manifest: Manifest,
    /// The part blobs, one per [`Manifest::parts`] entry.
    pub parts: Vec<PartBlob>,
    /// The image bytes, one per [`Manifest::images`] entry.
    pub images: Vec<Vec<u8>>,
}

/// Builds the publication of a book from its content, split by `reader`, with its
/// cover (if any).
///
/// Only raster images of the types the instance serves are included (an SVG could
/// carry scripts); a reference to another image renders as an image that does not
/// load, as on the instance. A cover that is one of the content's images is stored
/// once.
pub fn build(
    info: &BookInfo,
    content: &BookContent,
    cover: Option<&Image>,
    reader: &ReaderConfig,
) -> Result<Publication, PublishError> {
    if let Some(drm) = &info.drm {
        return Err(PublishError::Protected(drm.clone()));
    }
    let parts = split(content, reader);
    let text = BookText::new(content);

    let mut images = Vec::new();
    let mut infos = Vec::new();
    for (id, image) in content.images() {
        if let Some(mime) = covers::known_mime(&image.mime) {
            infos.push(ImageInfo {
                id: id.clone(),
                mime: mime.to_owned(),
            });
            images.push(image.data.clone());
        }
    }
    let cover = cover.and_then(|cover| {
        let mime = covers::known_mime(&cover.mime)?;
        let same = infos
            .iter()
            .zip(&images)
            .position(|(info, data)| info.mime == mime && *data == cover.data);
        Some(same.unwrap_or_else(|| {
            infos.push(ImageInfo {
                id: String::new(),
                mime: mime.to_owned(),
            });
            images.push(cover.data.clone());
            infos.len() - 1
        }) as u32)
    });

    let manifest = Manifest {
        format_version: FORMAT_VERSION,
        title: info.title.clone(),
        authors: info.authors.clone(),
        series: info.series.clone(),
        language: html_lang(info.language.as_deref()),
        content_key: info.content_key.clone(),
        block_count: content.block_count() as u32,
        parts: parts
            .iter()
            .enumerate()
            .map(|(index, part)| PartInfo {
                start: part.start,
                end: part.end,
                title: part.title.clone(),
                head: title_before(content, part.start),
                progress: part_progress(&text, &parts, index),
            })
            .collect(),
        toc: toc_entries(content),
        chapters: chapter_ticks(content, &text),
        images: infos,
        cover,
    };
    Ok(Publication {
        manifest,
        parts: PartBlob::extract_all(content, &parts),
        images,
    })
}

impl Publication {
    /// The plaintext of every blob, by path: the encoded manifest and parts, the image
    /// bytes.
    pub fn blobs(&self) -> Vec<(BlobPath, Vec<u8>)> {
        let mut blobs = vec![(BlobPath::Manifest, self.manifest.encode())];
        blobs.extend(
            self.parts
                .iter()
                .enumerate()
                .map(|(n, part)| (BlobPath::Part(n as u32), part.encode())),
        );
        blobs.extend(
            self.images
                .iter()
                .enumerate()
                .map(|(n, data)| (BlobPath::Image(n as u32), data.clone())),
        );
        blobs
    }

    /// Every blob compressed, padded and encrypted for publication `id` under `key`,
    /// each with a fresh random nonce.
    pub fn seal(
        &self,
        id: &PublicationId,
        key: &BookKey,
    ) -> Result<Vec<(BlobPath, Vec<u8>)>, getrandom::Error> {
        self.blobs()
            .into_iter()
            .map(|(path, plain)| Ok((path, seal(id, key, path, &plain)?)))
            .collect()
    }
}
