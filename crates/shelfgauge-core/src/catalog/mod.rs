//! The catalog format: a library's Showcase (the books in the folders its admins
//! opened) as encrypted blobs on the hub, which every member's hub reader downloads,
//! keeps and searches offline.
//!
//! A catalog consists of blobs at these paths (relative to the library's catalog):
//!
//! - `index` ([`Index`]): the generation, the entry count and, per chunk, its number,
//!   the SHA-256 of its plaintext and the plaintext's length. Its plaintext header
//!   ([`CatalogHeader`], JSON) carries the catalog key sealed under the library key;
//! - `c/<n>` ([`Chunk`]): the [`Entry`]s of chunk `n`. A book's chunk is
//!   [`chunk_of`] its [`BookId`]: an HMAC under the catalog key, so a new book changes
//!   one chunk and the hub cannot tell which; the chunk count ([`chunk_count`]) is a
//!   power of two giving about [`CHUNK_TARGET`] entries per chunk;
//! - `t/<cover id>`: the thumbnail of a book's cover (WebP), at an opaque
//!   [`CoverId`] (an HMAC under the catalog key, so the hub cannot link a thumbnail to
//!   a book or across libraries).
//!
//! Every blob is sealed as a publication's ([`crate::publication`]): the plaintext
//! (postcard for the index and the chunks), raw deflate, [`pad`], AES-256-GCM under the
//! catalog key with the catalog's [`associated_data`], wrapped in the same
//! [`Envelope`] with its own [`CATALOG_FORMAT_VERSION`]. The associated data starts
//! with the domain `sg-catalog`, so a catalog blob can never pass for a publication's
//! blob, nor for another library's or another path's.
//!
//! The device checks every chunk against the index ([`Index::check_chunk`]), so the
//! hub cannot pair an old chunk with a new index, and keeps the highest
//! [`Index::generation`] it has seen, refusing a lower one (no rollback of the whole
//! catalog).
//!
//! The entry's fields take their names and meaning from OPDS 2.0 metadata (`title`,
//! `sortAs`, `author`, `belongsTo.series` with `position`, `subject`, `language`,
//! `published`, `description`), stored in this compact form. postcard is not
//! self-describing: any change of these types is a new [`CATALOG_FORMAT_VERSION`].
//!
//! [`pad`]: crate::publication::pad
//! [`Envelope`]: crate::publication::Envelope

mod search;

use std::fmt;
use std::str::FromStr;

use sha2::{Digest, Sha256};

use crate::password::hmac_sha256;
use crate::publication::{Envelope, FormatError, VersionRange};

pub use search::{Catalog, GenreCount, NameCount};

/// The catalog format version this crate writes.
pub const CATALOG_FORMAT_VERSION: u16 = 1;

/// The catalog format versions this crate reads.
pub const CATALOG_SUPPORTED_VERSIONS: VersionRange = VersionRange { min: 1, max: 1 };

/// The kind under which the catalog key is sealed in the index header (as a record of
/// that kind; the id is the library id in hex), so it never opens as a book key.
pub const CATALOG_KEY_KIND: &str = "catalog_key";

/// The number of entries a chunk is meant to hold.
pub const CHUNK_TARGET: u64 = 1000;

/// The most chunks of a catalog.
pub const MAX_CHUNKS: u32 = 1 << 14;

/// The longest description of an entry, in characters.
pub const DESCRIPTION_CHARS: usize = 300;

/// The largest plaintext of an index (the inflate limit before it is decoded).
pub const MAX_INDEX_LEN: u64 = 2 << 20;

/// The largest plaintext of a chunk.
pub const MAX_CHUNK_LEN: u64 = 16 << 20;

const DOMAIN: &[u8] = b"sg-catalog";

/// A book's id in the catalog: the first 16 bytes of SHA-256(`"sg-catalog book"` ‖ 0
/// ‖ content key). The content key is the one publications are matched by, so the id
/// stays when the book is rescanned or renamed. Shown as 32 lowercase hex digits;
/// `Debug` hides it (book ids are never logged).
///
/// # Examples
///
/// ```
/// use shelfgauge_core::catalog::BookId;
///
/// let id = BookId::from_content_key(&[1; 16]);
/// assert_eq!(id, BookId::from_content_key(&[1; 16]));
/// assert_ne!(id, BookId::from_content_key(&[2; 16]));
/// assert_eq!(id.to_string().parse::<BookId>(), Ok(id));
/// assert_eq!(format!("{id:?}"), "BookId(..)");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BookId([u8; 16]);

impl BookId {
    /// The id with these bytes.
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The id of the book with this content key.
    pub fn from_content_key(content_key: &[u8]) -> Self {
        let mut hash = Sha256::new();
        hash.update(b"sg-catalog book\0");
        hash.update(content_key);
        let digest: [u8; 32] = hash.finalize().into();
        Self(digest[..16].try_into().expect("16 of 32 bytes"))
    }

    /// The id's bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// A thumbnail's id: the first 16 bytes of HMAC-SHA256(catalog key, `"cover"` ‖ 0 ‖
/// book id). Shown as 32 lowercase hex digits (its blob path is `t/<hex>`); `Debug`
/// hides it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CoverId([u8; 16]);

impl CoverId {
    /// The id with these bytes.
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The id's bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

/// The error returned when parsing a string that is not a [`BookId`] or [`CoverId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid id (expected 32 lowercase hex digits)")]
pub struct ParseIdError;

macro_rules! hex_id {
    ($name:ident) => {
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(concat!(stringify!($name), "(..)"))
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }

        impl FromStr for $name {
            type Err = ParseIdError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                parse_hex16(s).map(Self).ok_or(ParseIdError)
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                self.0.serialize(s)
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                <[u8; 16]>::deserialize(d).map(Self)
            }
        }
    };
}

hex_id!(BookId);
hex_id!(CoverId);

fn parse_hex16(s: &str) -> Option<[u8; 16]> {
    let digits = s.as_bytes();
    if digits.len() != 32 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut bytes = [0u8; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = digit(digits[2 * i])? << 4 | digit(digits[2 * i + 1])?;
    }
    Some(bytes)
}

/// The chunk of book `id` among `chunks` (a power of two): the first 8 bytes of
/// HMAC-SHA256(catalog key, `"chunk"` ‖ 0 ‖ id), big-endian, modulo `chunks`.
///
/// # Panics
///
/// If `chunks` is not a power of two.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::catalog::{BookId, chunk_of};
///
/// let id = BookId::new([5; 16]);
/// assert_eq!(chunk_of(&[1; 32], &id, 1), 0);
/// assert!(chunk_of(&[1; 32], &id, 8) < 8);
/// ```
pub fn chunk_of(catalog_key: &[u8; 32], id: &BookId, chunks: u32) -> u32 {
    assert!(
        chunks.is_power_of_two(),
        "the chunk count is a power of two"
    );
    let mac = hmac_sha256(catalog_key, &[b"chunk\0", id.as_bytes()]);
    let n = u64::from_be_bytes(mac[..8].try_into().expect("8 of 32 bytes"));
    (n % u64::from(chunks)) as u32
}

/// The thumbnail id of book `id` (see [`CoverId`]).
pub fn cover_id(catalog_key: &[u8; 32], id: &BookId) -> CoverId {
    let mac = hmac_sha256(catalog_key, &[b"cover\0", id.as_bytes()]);
    CoverId(mac[..16].try_into().expect("16 of 32 bytes"))
}

/// The chunk count for `entries` entries, given the `previous` count (`None` for a new
/// catalog or after a key change). A new count is the smallest power of two with
/// `entries <= 1000 n` (at most [`MAX_CHUNKS`]), so a chunk then holds 500 to 1000
/// entries; it stays while `250 n < entries <= 2000 n` (for `n = 1`: `entries <=
/// 2000`), so entries are redistributed only when their number doubles or halves.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::catalog::chunk_count;
///
/// assert_eq!(chunk_count(0, None), 1);
/// assert_eq!(chunk_count(1000, None), 1);
/// assert_eq!(chunk_count(1001, None), 2);
/// // Kept until the count doubles ...
/// assert_eq!(chunk_count(1900, Some(1)), 1);
/// assert_eq!(chunk_count(2001, Some(1)), 4);
/// // ... or halves.
/// assert_eq!(chunk_count(501, Some(2)), 2);
/// assert_eq!(chunk_count(500, Some(2)), 1);
/// ```
pub fn chunk_count(entries: u64, previous: Option<u32>) -> u32 {
    if let Some(n) = previous.filter(|n| n.is_power_of_two() && *n <= MAX_CHUNKS) {
        let n64 = u64::from(n);
        let low = if n == 1 { 0 } else { 250 * n64 };
        if entries > low && entries <= 2 * CHUNK_TARGET * n64 {
            return n;
        }
        if n == MAX_CHUNKS && entries > low {
            return n;
        }
    }
    let mut n = 1u32;
    while u64::from(n) * CHUNK_TARGET < entries && n < MAX_CHUNKS {
        n *= 2;
    }
    n
}

/// The path of a blob within a library's catalog.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::catalog::{CatalogPath, CoverId};
///
/// assert_eq!(CatalogPath::Chunk(3).to_string(), "c/3");
/// assert_eq!("index".parse(), Ok(CatalogPath::Index));
/// let cover = CatalogPath::Thumbnail(CoverId::new([0xab; 16]));
/// assert_eq!(cover.to_string().parse(), Ok(cover));
/// assert!("c/03".parse::<CatalogPath>().is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CatalogPath {
    /// `index`.
    Index,
    /// `c/<n>`: chunk `n` (0-based).
    Chunk(u32),
    /// `t/<cover id>`: a thumbnail.
    Thumbnail(CoverId),
}

impl fmt::Display for CatalogPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Index => f.write_str("index"),
            Self::Chunk(n) => write!(f, "c/{n}"),
            Self::Thumbnail(id) => write!(f, "t/{id}"),
        }
    }
}

/// The error returned when parsing a string that is not a [`CatalogPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid catalog path (expected `index`, `c/<n>` or `t/<cover id>`)")]
pub struct ParseCatalogPathError;

impl FromStr for CatalogPath {
    type Err = ParseCatalogPathError;

    /// Parses the canonical form only (no leading zeros, lowercase hex).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "index" {
            return Ok(Self::Index);
        }
        let (kind, rest) = s.split_once('/').ok_or(ParseCatalogPathError)?;
        match kind {
            "c" => {
                let canonical = !rest.is_empty()
                    && rest.bytes().all(|b| b.is_ascii_digit())
                    && (rest == "0" || !rest.starts_with('0'));
                rest.parse()
                    .ok()
                    .filter(|_| canonical)
                    .map(Self::Chunk)
                    .ok_or(ParseCatalogPathError)
            }
            "t" => rest
                .parse()
                .map(Self::Thumbnail)
                .map_err(|_| ParseCatalogPathError),
            _ => Err(ParseCatalogPathError),
        }
    }
}

/// The AES-GCM associated data of a catalog blob: `"sg-catalog"` ‖ 0 ‖ version (u16
/// BE) ‖ library id (16 bytes) ‖ path (UTF-8) ‖ 0 ‖ header.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::catalog::{CatalogPath, associated_data};
///
/// let ad = associated_data(1, &[7; 16], CatalogPath::Chunk(2), b"");
/// assert_eq!(&ad[..11], b"sg-catalog\0");
/// assert_eq!(&ad[11..13], &[0, 1]);
/// assert_eq!(&ad[13..29], &[7; 16]);
/// assert_eq!(&ad[29..], b"c/2\0");
/// ```
pub fn associated_data(
    version: u16,
    library: &[u8; 16],
    path: CatalogPath,
    header: &[u8],
) -> Vec<u8> {
    let path = path.to_string();
    let mut ad = Vec::with_capacity(DOMAIN.len() + 1 + 2 + 16 + path.len() + 1 + header.len());
    ad.extend_from_slice(DOMAIN);
    ad.push(0);
    ad.extend_from_slice(&version.to_be_bytes());
    ad.extend_from_slice(library);
    ad.extend_from_slice(path.as_bytes());
    ad.push(0);
    ad.extend_from_slice(header);
    ad
}

/// Takes a catalog blob apart (see [`Envelope::parse_in`]); a version outside
/// [`CATALOG_SUPPORTED_VERSIONS`] is
/// [`FormatError::UnsupportedCatalogVersion`].
pub fn parse_envelope(blob: &[u8]) -> Result<Envelope<'_>, FormatError> {
    Envelope::parse_in(blob, CATALOG_SUPPORTED_VERSIONS).map_err(|e| match e {
        FormatError::UnsupportedVersion { found, supported } => {
            FormatError::UnsupportedCatalogVersion { found, supported }
        }
        e => e,
    })
}

/// The plaintext header of a catalog's index: whose catalog it is and its catalog key
/// sealed under that library's key (or, in a circle, the circle's key). The same
/// scope fields as a manifest's [`KeyHeader`]; untrusted until the index decrypts
/// (it is authenticated as associated data).
///
/// [`KeyHeader`]: crate::publication::KeyHeader
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogHeader {
    /// The public id of the library, hex.
    pub library: String,
    /// Whose key sealed `catalog_key`: [`LIBRARY_SCOPE`](crate::publication::LIBRARY_SCOPE).
    pub key_scope: String,
    /// The public id of the key's owner, hex.
    pub key_owner: String,
    /// The generation of the owner's key.
    pub key_id: u32,
    /// The catalog key sealed as a record of kind [`CATALOG_KEY_KIND`] (id: the
    /// library id in hex) under that key, base64url.
    pub catalog_key: String,
}

impl CatalogHeader {
    /// The header's bytes (compact JSON).
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a catalog header always serializes")
    }

    /// Decodes a header; any other JSON is malformed.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        serde_json::from_slice(bytes).map_err(|e| FormatError::Malformed(e.to_string()))
    }
}

/// A book's format in an [`Entry`]: 1 `fb2`, 2 `epub`, 3 `txt`, 4 `html`; 0 for any
/// other.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::catalog::{format_code, format_name};
///
/// assert_eq!(format_code("epub"), 2);
/// assert_eq!(format_name(2), Some("epub"));
/// assert_eq!(format_code("pdf"), 0);
/// assert_eq!(format_name(0), None);
/// ```
pub fn format_code(name: &str) -> u8 {
    FORMATS
        .iter()
        .position(|f| *f == name)
        .map_or(0, |i| i as u8 + 1)
}

/// The name of format `code` (see [`format_code`]), `None` for an unknown one.
pub fn format_name(code: u8) -> Option<&'static str> {
    FORMATS.get(usize::from(code).checked_sub(1)?).copied()
}

const FORMATS: [&str; 4] = ["fb2", "epub", "txt", "html"];

/// A series a book belongs to (OPDS `belongsTo.series`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SeriesRef {
    /// The series' name.
    pub name: String,
    /// The book's place in it, as written in the book (`"3"`, `"2.5"`).
    pub position: Option<String>,
}

/// A book of the catalog (field names after OPDS 2.0 metadata).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// The book's id.
    pub id: BookId,
    /// The title.
    pub title: String,
    /// The title for sorting (`sortAs`).
    pub sort_as: String,
    /// The authors' display names.
    pub author: Vec<String>,
    /// The series (`belongsTo.series`).
    pub series: Vec<SeriesRef>,
    /// Genre codes or subjects (`subject`); FB2 codes are named on the device with
    /// [`crate::format::genres`].
    pub subject: Vec<String>,
    /// The language tag.
    pub language: Option<String>,
    /// The year of publication (`published`).
    pub published: Option<i32>,
    /// The format (see [`format_code`]).
    pub format: u8,
    /// The size of the book's file in bytes.
    pub size: u64,
    /// The annotation, cut with [`cut_description`].
    pub description: Option<String>,
    /// When the book was added to the library (Unix seconds).
    pub added: u64,
    /// The publication on the hub, when the book is already published.
    pub publication: Option<[u8; 16]>,
    /// The thumbnail, when the book has a cover.
    pub cover: Option<CoverId>,
}

/// A chunk's plaintext: its entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Chunk {
    /// The entries, in any order.
    pub entries: Vec<Entry>,
}

impl Chunk {
    /// The postcard encoding.
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("serializing to a Vec cannot fail")
    }

    /// Decodes a chunk.
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        Ok(postcard::from_bytes(bytes)?)
    }
}

/// A chunk as the index lists it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChunkInfo {
    /// The chunk's number.
    pub number: u32,
    /// SHA-256 of the chunk's plaintext (its postcard encoding).
    pub sha256: [u8; 32],
    /// The plaintext's length in bytes.
    pub size: u32,
}

/// The index's plaintext.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Index {
    /// Grows with every upload; a device refuses an index older than one it has seen.
    pub generation: u64,
    /// The number of entries in all chunks.
    pub entries: u32,
    /// The chunks, numbered `0..n` in order; `n` is a power of two.
    pub chunks: Vec<ChunkInfo>,
}

impl Index {
    /// The index of the encoded `chunks` (plaintexts in chunk order).
    pub fn new(generation: u64, entries: u32, chunks: &[Vec<u8>]) -> Self {
        Self {
            generation,
            entries,
            chunks: chunks
                .iter()
                .enumerate()
                .map(|(n, plain)| ChunkInfo {
                    number: n as u32,
                    sha256: chunk_hash(plain),
                    size: u32::try_from(plain.len()).expect("a chunk is below 4 GiB"),
                })
                .collect(),
        }
    }

    /// The postcard encoding.
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_stdvec(self).expect("serializing to a Vec cannot fail")
    }

    /// Decodes an index; fails for malformed data, for a chunk count that is not a
    /// power of two up to [`MAX_CHUNKS`], for chunks out of order and for a chunk
    /// larger than [`MAX_CHUNK_LEN`].
    pub fn decode(bytes: &[u8]) -> Result<Self, FormatError> {
        let index: Self = postcard::from_bytes(bytes)?;
        let n = index.chunks.len();
        if !n.is_power_of_two() || n > MAX_CHUNKS as usize {
            return Err(FormatError::Malformed(format!("{n} chunks")));
        }
        if index
            .chunks
            .iter()
            .enumerate()
            .any(|(i, c)| c.number as usize != i || u64::from(c.size) > MAX_CHUNK_LEN)
        {
            return Err(FormatError::Malformed("the chunk list is broken".into()));
        }
        Ok(index)
    }

    /// Checks the plaintext of chunk `number` against the index.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge_core::catalog::{Chunk, Index};
    ///
    /// let plain = Chunk::default().encode();
    /// let index = Index::new(1, 0, &[plain.clone()]);
    /// assert!(index.check_chunk(0, &plain).is_ok());
    /// assert!(index.check_chunk(0, b"other").is_err());
    /// assert!(index.check_chunk(1, &plain).is_err());
    /// ```
    pub fn check_chunk(&self, number: u32, plain: &[u8]) -> Result<(), FormatError> {
        let info = self
            .chunks
            .get(number as usize)
            .ok_or_else(|| FormatError::Malformed(format!("no chunk {number}")))?;
        if info.size as usize != plain.len() || info.sha256 != chunk_hash(plain) {
            return Err(FormatError::Malformed(format!(
                "chunk {number} does not match the index"
            )));
        }
        Ok(())
    }
}

/// SHA-256 of a chunk's plaintext.
pub fn chunk_hash(plain: &[u8]) -> [u8; 32] {
    Sha256::digest(plain).into()
}

/// An annotation cut for the catalog: whitespace collapsed; over
/// [`DESCRIPTION_CHARS`] characters it is cut at the last space within the first 299
/// (or at 299 if that would leave fewer than 200) and `…` is appended. `None` for an
/// empty one.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::catalog::cut_description;
///
/// assert_eq!(cut_description("  A   short\nnote "), Some("A short note".into()));
/// assert_eq!(cut_description(" "), None);
/// let long = cut_description(&"word ".repeat(100)).unwrap();
/// assert!(long.ends_with("word…"));
/// assert!(long.chars().count() <= 300);
/// ```
pub fn cut_description(text: &str) -> Option<String> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= DESCRIPTION_CHARS {
        return Some(text);
    }
    let keep = DESCRIPTION_CHARS - 1;
    let end = text.char_indices().nth(keep).map_or(text.len(), |(i, _)| i);
    let head = &text[..end];
    let cut = match head.rfind(' ') {
        Some(space) if head[..space].chars().count() >= 200 => &head[..space],
        _ => head,
    };
    let cut = cut.trim_end_matches(|c: char| c.is_whitespace() || ",;:-–—".contains(c));
    Some(format!("{cut}…"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(n: u8) -> Entry {
        Entry {
            id: BookId::new([n; 16]),
            title: format!("Книга {n}"),
            sort_as: format!("книга {n}"),
            author: vec!["Лев Толстой".into()],
            series: vec![SeriesRef {
                name: "Серия".into(),
                position: Some(n.to_string()),
            }],
            subject: vec!["sf".into()],
            language: Some("ru".into()),
            published: Some(1869),
            format: format_code("fb2"),
            size: 12345,
            description: cut_description("Описание"),
            added: 1_700_000_000,
            publication: None,
            cover: Some(CoverId::new([n; 16])),
        }
    }

    #[test]
    fn chunks_and_indexes_round_trip() {
        let chunk = Chunk {
            entries: (0..3).map(entry).collect(),
        };
        let plain = chunk.encode();
        assert_eq!(Chunk::decode(&plain).unwrap(), chunk);
        let index = Index::new(7, 3, &[plain.clone(), Chunk::default().encode()]);
        let decoded = Index::decode(&index.encode()).unwrap();
        assert_eq!(decoded, index);
        assert!(decoded.check_chunk(0, &plain).is_ok());
        assert!(decoded.check_chunk(1, &plain).is_err());
        assert!(Chunk::decode(&plain[..plain.len() - 1]).is_err());
    }

    #[test]
    fn indexes_with_broken_chunk_lists_are_refused() {
        let plain = Chunk::default().encode();
        let three = Index::new(1, 0, &[plain.clone(), plain.clone(), plain.clone()]);
        assert!(Index::decode(&three.encode()).is_err());
        let none = Index::new(1, 0, &[]);
        assert!(Index::decode(&none.encode()).is_err());
        let mut swapped = Index::new(1, 0, &[plain.clone(), plain]);
        swapped.chunks.swap(0, 1);
        assert!(Index::decode(&swapped.encode()).is_err());
    }

    #[test]
    fn chunk_counts_do_not_flap_at_a_boundary() {
        // Growing one book at a time and then shrinking again changes the count only
        // when the number of entries doubles or halves.
        let mut n = chunk_count(0, None);
        let mut changes = Vec::new();
        for entries in (0..=9000).chain((0..9000).rev()) {
            let next = chunk_count(entries, Some(n));
            if next != n {
                changes.push((entries, next));
            }
            assert!(entries <= 2 * CHUNK_TARGET * u64::from(next));
            n = next;
        }
        assert_eq!(changes, [(2001, 4), (8001, 16), (4000, 4), (1000, 1)]);
        assert_eq!(chunk_count(100_000, None), 128);
        assert_eq!(chunk_count(u64::MAX, None), MAX_CHUNKS);
        assert_eq!(chunk_count(u64::MAX, Some(MAX_CHUNKS)), MAX_CHUNKS);
        assert_eq!(chunk_count(10, Some(3)), 1);
    }

    #[test]
    fn chunks_spread_books_evenly() {
        let key = [9u8; 32];
        let mut counts = [0u32; 8];
        for i in 0..8000u32 {
            let id = BookId::from_content_key(&i.to_le_bytes());
            counts[chunk_of(&key, &id, 8) as usize] += 1;
        }
        assert!(
            counts.iter().all(|&c| (850..1150).contains(&c)),
            "{counts:?}"
        );
        // Another key, another spread.
        let id = BookId::from_content_key(b"x");
        assert!((0..32).any(|k| chunk_of(&[k; 32], &id, 8) != chunk_of(&key, &id, 8)));
    }

    #[test]
    fn cover_ids_hide_the_book_id() {
        let id = BookId::new([1; 16]);
        let cover = cover_id(&[2; 32], &id);
        assert_ne!(cover.as_bytes(), id.as_bytes());
        assert_ne!(cover, cover_id(&[3; 32], &id));
        assert_eq!(format!("{cover:?}"), "CoverId(..)");
    }

    #[test]
    fn descriptions_are_cut_at_a_word() {
        let cyrillic = "слово ".repeat(80);
        let cut = cut_description(&cyrillic).unwrap();
        assert!(cut.chars().count() <= DESCRIPTION_CHARS);
        assert!(cut.ends_with("слово…"), "{cut}");
        // No spaces at all: cut at 299 characters.
        let solid = "я".repeat(400);
        let cut = cut_description(&solid).unwrap();
        assert_eq!(cut.chars().count(), DESCRIPTION_CHARS);
        assert!(cut.ends_with("я…"));
        // A last space too early is not used.
        let early = format!("{} {}", "a".repeat(10), "b".repeat(400));
        assert_eq!(
            cut_description(&early).unwrap().chars().count(),
            DESCRIPTION_CHARS
        );
        // Exactly 300 characters stay whole.
        let exact = "ю".repeat(300);
        assert_eq!(cut_description(&exact).unwrap(), exact);
        // A cut does not end in a comma before the ellipsis.
        let comma = format!("{}, {}", "a ".repeat(130).trim_end(), "b".repeat(200));
        assert!(!cut_description(&comma).unwrap().contains(",…"));
    }

    #[test]
    fn catalog_blobs_differ_from_publication_blobs() {
        use crate::publication::{BlobPath, PublicationId};
        let id = [7u8; 16];
        let catalog = associated_data(1, &id, CatalogPath::Index, b"");
        let publication = crate::publication::associated_data(
            2,
            &PublicationId::new(id),
            BlobPath::Manifest,
            b"",
        );
        assert_ne!(
            &catalog[..publication.len().min(catalog.len())],
            &publication[..]
        );
        assert_ne!(
            associated_data(1, &id, CatalogPath::Chunk(1), b""),
            associated_data(1, &id, CatalogPath::Chunk(2), b"")
        );
    }

    #[test]
    fn envelopes_of_other_versions_are_refused_with_the_catalog_message() {
        let blob = [&[0u8, 2, 0, 0][..], &[0; 12], &[0; 16]].concat();
        let error = parse_envelope(&blob).unwrap_err();
        assert!(matches!(
            error,
            FormatError::UnsupportedCatalogVersion { found: 2, .. }
        ));
        assert!(error.to_string().starts_with("catalog format version 2"));
        let blob = [&[0u8, 1, 0, 0][..], &[0; 12], &[0; 16]].concat();
        assert_eq!(parse_envelope(&blob).unwrap().version, 1);
        // And a catalog blob is not a publication blob.
        assert!(Envelope::parse(&blob).is_err());
    }

    #[test]
    fn paths_parse_only_their_canonical_form() {
        for bad in [
            "", "c", "c/", "c/01", "c/-1", "t/", "t/AB", "x/1", "index/", "/index",
        ] {
            assert!(bad.parse::<CatalogPath>().is_err(), "{bad:?}");
        }
        assert!(
            format!("t/{}", "AB".repeat(16))
                .parse::<CatalogPath>()
                .is_err()
        );
        assert_eq!("c/0".parse(), Ok(CatalogPath::Chunk(0)));
    }

    #[test]
    fn headers_round_trip() {
        let header = CatalogHeader {
            library: "ab".repeat(16),
            key_scope: crate::publication::LIBRARY_SCOPE.into(),
            key_owner: "ab".repeat(16),
            key_id: 2,
            catalog_key: "c2VhbGVk".into(),
        };
        assert_eq!(CatalogHeader::decode(&header.encode()).unwrap(), header);
        assert!(CatalogHeader::decode(b"{\"library\":\"x\"}").is_err());
    }
}
