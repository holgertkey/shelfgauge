//! The manifest of a publication.

use serde::{Deserialize, Serialize};

use super::{FORMAT_VERSION, FormatError, SUPPORTED_VERSIONS};
use crate::format::model::BlockId;
use crate::render::html::TocEntry;
use crate::render::outline::{ChapterTick, PartProgress};
use crate::render::split::Part;

/// The manifest: everything about a publication except the text and the images.
///
/// It carries what the instance's reader page computes from the whole book, so a
/// device can show any part with only that part's blob.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The format version (the same as the envelope's).
    pub format_version: u16,
    /// The book title as the library shows it.
    pub title: String,
    /// The authors' display names, in order.
    pub authors: Vec<String>,
    /// The series, as the library shows them (e.g. `Foundation #2`).
    pub series: Vec<String>,
    /// The book's language as an HTML `lang` tag
    /// ([`html_lang`](crate::render::outline::html_lang)), if valid.
    pub language: Option<String>,
    /// The book's content key (matches annotations and positions between instances
    /// and devices).
    pub content_key: Vec<u8>,
    /// The number of blocks of the book.
    pub block_count: u32,
    /// The parts in order.
    pub parts: Vec<PartInfo>,
    /// The table of contents.
    pub toc: Vec<TocEntry>,
    /// The chapter ticks of the status line's track.
    pub chapters: Vec<ChapterTick>,
    /// The images: image `n` is the blob `i/<n>`.
    pub images: Vec<ImageInfo>,
    /// The cover, an index into [`images`](Self::images).
    pub cover: Option<u32>,
}

/// One part of the book, as [`Part`] plus the figures of its page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartInfo {
    /// The first block.
    pub start: BlockId,
    /// The last block (inclusive).
    pub end: BlockId,
    /// The part's title ([`Part::title`]).
    pub title: String,
    /// The running head: the section title in effect where the part starts
    /// ([`title_before`](crate::render::outline::title_before)).
    pub head: Option<String>,
    /// The reading progress over the part; `None` for a part in the notes.
    pub progress: Option<PartProgress>,
}

impl PartInfo {
    /// The part as the splitter made it.
    pub fn part(&self) -> Part {
        Part {
            start: self.start,
            end: self.end,
            title: self.title.clone(),
        }
    }
}

/// An image of the publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// The image's source id ([`ImageRef::id`](crate::format::model::ImageRef::id));
    /// empty for a cover that the text does not show.
    pub id: String,
    /// The MIME type, one of the raster types the instance serves.
    pub mime: String,
}

impl Manifest {
    /// The parts as the splitter made them (for [`PartBlob::render`](super::PartBlob::render)).
    pub fn split(&self) -> Vec<Part> {
        self.parts.iter().map(PartInfo::part).collect()
    }

    /// The index of the image with source id `id` (what a device's
    /// [`ReaderLinks::image`](crate::render::html::ReaderLinks::image) maps to `i/<n>`).
    pub fn image_index(&self, id: &str) -> Option<u32> {
        if id.is_empty() {
            return None;
        }
        let index = self.images.iter().position(|image| image.id == id)?;
        u32::try_from(index).ok()
    }

    /// The postcard encoding.
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("serializing to a Vec cannot fail")
    }

    /// Decodes a manifest; fails for malformed data and for a manifest of another
    /// format version.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let manifest: Self = postcard::from_bytes(bytes)?;
        if manifest.format_version != FORMAT_VERSION {
            return Err(FormatError::UnsupportedVersion {
                found: manifest.format_version,
                supported: SUPPORTED_VERSIONS,
            });
        }
        Ok(manifest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        let id = |n| BlockId::new(n).unwrap();
        Manifest {
            format_version: FORMAT_VERSION,
            title: "Book".into(),
            authors: vec!["A. Author".into()],
            series: vec![],
            language: Some("en".into()),
            content_key: vec![1; 16],
            block_count: 9,
            parts: vec![PartInfo {
                start: id(1),
                end: id(9),
                title: "Book".into(),
                head: None,
                progress: Some(PartProgress {
                    start: 0,
                    end: 10_000,
                }),
            }],
            toc: vec![],
            chapters: vec![],
            images: vec![ImageInfo {
                id: "pic".into(),
                mime: "image/png".into(),
            }],
            cover: Some(0),
        }
    }

    #[test]
    fn manifests_round_trip() {
        let manifest = manifest();
        assert_eq!(Manifest::decode(&manifest.encode()).unwrap(), manifest);
        assert_eq!(manifest.image_index("pic"), Some(0));
        assert_eq!(manifest.image_index(""), None);
        assert_eq!(manifest.split()[0].end, BlockId::new(9).unwrap());
    }

    #[test]
    fn garbage_and_truncated_manifests_are_errors() {
        let bytes = manifest().encode();
        for len in 0..bytes.len() {
            assert!(Manifest::decode(&bytes[..len]).is_err(), "{len}");
        }
        assert!(Manifest::decode(&[0xff; 64]).is_err());
        let mut other = manifest();
        other.format_version = 2;
        assert!(matches!(
            Manifest::decode(&other.encode()),
            Err(FormatError::UnsupportedVersion { found: 2, .. })
        ));
    }
}
