//! The publication format: a book as versioned, padded, encrypted blobs for the hub
//! and for `.sgbook` files.
//!
//! A publication is the book after splitting, as structured data, never HTML. It
//! consists of blobs at these paths (relative to the publication):
//!
//! - `manifest` ([`Manifest`]): metadata, the content key, parts, the table of
//!   contents, everything the reader page derives from the whole book, the image list;
//! - `p/<n>` ([`PartBlob`]): the blocks of part `n` (0-based) and the targets of its
//!   internal links;
//! - `i/<n>`: the bytes of image `n` of [`Manifest::images`].
//!
//! The manifest and the parts are postcard-encoded. Every blob is then sealed by the
//! instance (this crate does no compression or cryptography: the instance uses Rust
//! crates, the device the browser's `DecompressionStream` and WebCrypto):
//!
//! 1. compressed with raw deflate (RFC 1951, no zlib header);
//! 2. padded with [`pad`]: a 4-byte big-endian length, the compressed bytes, zeros up
//!    to the next [`bucket`] size (a geometric sequence with ratio 9/8), so blob sizes
//!    say little about the book;
//! 3. encrypted with AES-256-GCM under the publication's book key, a random 96-bit
//!    nonce and the [`associated_data`] of the blob (format version, publication id,
//!    blob path), so blobs cannot be swapped between positions or publications;
//! 4. wrapped as `[format version: u16 BE][nonce: 12 bytes][ciphertext and 16-byte tag]`
//!    ([`Envelope`]): the version is readable before decrypting, so a reader refuses
//!    an unknown one with a clear error, and it is authenticated through the
//!    associated data.
//!
//! Opening reverses this: [`Envelope::parse`], decrypt, [`unpad`], inflate, then
//! [`Manifest::decode`] or [`PartBlob::decode`]. The data is untrusted (any account
//! can upload anything): decoding returns errors, never panics, and rendering escapes
//! everything as the server does ([`PartBlob::render`]).
//!
//! postcard is not self-describing: any change to these types is a new
//! [`FORMAT_VERSION`]. So is any change to the model ([`PARSER_VERSION`]), whose types
//! the parts contain.
//!
//! [`PARSER_VERSION`]: crate::format::model::PARSER_VERSION

mod manifest;
mod part;

use std::fmt;
use std::str::FromStr;

pub use manifest::{ImageInfo, Manifest, PartInfo};
pub use part::{PartBlob, PartBody, assemble};

/// The publication format version this crate writes.
pub const FORMAT_VERSION: u16 = 1;

/// The format versions this crate reads.
pub const SUPPORTED_VERSIONS: VersionRange = VersionRange { min: 1, max: 1 };

/// An inclusive range of format versions (what a hub advertises in `/api/info`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct VersionRange {
    /// The oldest version.
    pub min: u16,
    /// The newest version.
    pub max: u16,
}

impl VersionRange {
    /// Whether `version` is in the range.
    pub fn contains(self, version: u16) -> bool {
        self.min <= version && version <= self.max
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.min == self.max {
            write!(f, "{}", self.min)
        } else {
            write!(f, "{}–{}", self.min, self.max)
        }
    }
}

/// Why a blob could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FormatError {
    /// The blob was written in a format version this reader does not support.
    #[error(
        "publication format version {found} is not supported (supported: {supported}); \
         a newer version of shelfgauge may be needed"
    )]
    UnsupportedVersion {
        /// The blob's version.
        found: u16,
        /// What this reader supports.
        supported: VersionRange,
    },
    /// The blob is shorter than its framing requires.
    #[error("the blob is truncated")]
    Truncated,
    /// The blob's content does not decode.
    #[error("the blob is malformed: {0}")]
    Malformed(String),
}

impl From<postcard::Error> for FormatError {
    fn from(error: postcard::Error) -> Self {
        Self::Malformed(error.to_string())
    }
}

/// A publication id: 128 random bits, shown as 32 lowercase hex digits.
///
/// Publication ids are secrets of the account (they are in the book links): `Debug`
/// does not show the value, and they are never logged.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::publication::PublicationId;
///
/// let id = PublicationId::new([0xab; 16]);
/// assert_eq!(id.to_string(), "ab".repeat(16));
/// assert_eq!(id.to_string().parse::<PublicationId>(), Ok(id));
/// assert_eq!(format!("{id:?}"), "PublicationId(..)");
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicationId([u8; 16]);

impl PublicationId {
    /// The id with these bytes.
    pub fn new(bytes: [u8; 16]) -> Self {
        Self(bytes)
    }

    /// The id's bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }
}

impl fmt::Debug for PublicationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PublicationId(..)")
    }
}

impl fmt::Display for PublicationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// The error returned when parsing a string that is not a [`PublicationId`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid publication id (expected 32 lowercase hex digits)")]
pub struct ParsePublicationIdError;

impl FromStr for PublicationId {
    type Err = ParsePublicationIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let digits = s.as_bytes();
        if digits.len() != 32 {
            return Err(ParsePublicationIdError);
        }
        let digit = |c: u8| match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            _ => Err(ParsePublicationIdError),
        };
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            *byte = digit(digits[2 * i])? << 4 | digit(digits[2 * i + 1])?;
        }
        Ok(Self(bytes))
    }
}

/// The path of a blob within its publication.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::publication::BlobPath;
///
/// assert_eq!(BlobPath::Part(3).to_string(), "p/3");
/// assert_eq!("i/0".parse(), Ok(BlobPath::Image(0)));
/// assert!("p/03".parse::<BlobPath>().is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlobPath {
    /// `manifest`.
    Manifest,
    /// `p/<n>`: part `n` (0-based).
    Part(u32),
    /// `i/<n>`: image `n` of [`Manifest::images`] (0-based).
    Image(u32),
}

impl fmt::Display for BlobPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest => f.write_str("manifest"),
            Self::Part(n) => write!(f, "p/{n}"),
            Self::Image(n) => write!(f, "i/{n}"),
        }
    }
}

/// The error returned when parsing a string that is not a [`BlobPath`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid blob path (expected `manifest`, `p/<n>` or `i/<n>`)")]
pub struct ParseBlobPathError;

impl FromStr for BlobPath {
    type Err = ParseBlobPathError;

    /// Parses the canonical form only (no leading zeros, no slash around it).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "manifest" {
            return Ok(Self::Manifest);
        }
        let (kind, n) = s.split_once('/').ok_or(ParseBlobPathError)?;
        let canonical = !n.is_empty()
            && n.bytes().all(|b| b.is_ascii_digit())
            && (n == "0" || !n.starts_with('0'));
        let n: u32 = n
            .parse()
            .ok()
            .filter(|_| canonical)
            .ok_or(ParseBlobPathError)?;
        match kind {
            "p" => Ok(Self::Part(n)),
            "i" => Ok(Self::Image(n)),
            _ => Err(ParseBlobPathError),
        }
    }
}

/// The AES-GCM associated data of a blob: `[version: u16 BE][publication id: 16
/// bytes][path: UTF-8, e.g. "p/3"]`. Only the last field has a variable length.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::publication::{BlobPath, PublicationId, associated_data};
///
/// let ad = associated_data(1, &PublicationId::new([7; 16]), BlobPath::Part(3));
/// assert_eq!(&ad[..2], &[0, 1]);
/// assert_eq!(&ad[2..18], &[7; 16]);
/// assert_eq!(&ad[18..], b"p/3");
/// ```
pub fn associated_data(version: u16, id: &PublicationId, path: BlobPath) -> Vec<u8> {
    let path = path.to_string();
    let mut ad = Vec::with_capacity(2 + 16 + path.len());
    ad.extend_from_slice(&version.to_be_bytes());
    ad.extend_from_slice(id.as_bytes());
    ad.extend_from_slice(path.as_bytes());
    ad
}

/// The length of an AES-GCM nonce.
pub const NONCE_LEN: usize = 12;

/// The length of an AES-GCM tag (at the end of the ciphertext).
pub const TAG_LEN: usize = 16;

/// The length of the envelope header: the version and the nonce.
pub const HEADER_LEN: usize = 2 + NONCE_LEN;

/// A sealed blob taken apart: `[version: u16 BE][nonce][ciphertext and tag]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Envelope<'a> {
    /// The format version the blob was written in.
    pub version: u16,
    /// The AES-GCM nonce.
    pub nonce: &'a [u8; NONCE_LEN],
    /// The ciphertext followed by the tag.
    pub ciphertext: &'a [u8],
}

impl<'a> Envelope<'a> {
    /// Takes `blob` apart; fails for an unsupported version before anything else is
    /// looked at, and for a blob too short to hold a tag.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge_core::publication::{Envelope, FormatError};
    ///
    /// let blob = [&[0u8, 9][..], &[0; 12], &[0; 16]].concat();
    /// assert!(matches!(
    ///     Envelope::parse(&blob),
    ///     Err(FormatError::UnsupportedVersion { found: 9, .. })
    /// ));
    /// ```
    pub fn parse(blob: &'a [u8]) -> Result<Self, FormatError> {
        let version = u16::from_be_bytes(
            blob.get(..2)
                .ok_or(FormatError::Truncated)?
                .try_into()
                .expect("two bytes"),
        );
        if !SUPPORTED_VERSIONS.contains(version) {
            return Err(FormatError::UnsupportedVersion {
                found: version,
                supported: SUPPORTED_VERSIONS,
            });
        }
        if blob.len() < HEADER_LEN + TAG_LEN {
            return Err(FormatError::Truncated);
        }
        let (header, ciphertext) = blob.split_at(HEADER_LEN);
        Ok(Self {
            version,
            nonce: header[2..].try_into().expect("nonce length"),
            ciphertext,
        })
    }

    /// The blob's bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut blob = Vec::with_capacity(HEADER_LEN + self.ciphertext.len());
        blob.extend_from_slice(&self.version.to_be_bytes());
        blob.extend_from_slice(self.nonce);
        blob.extend_from_slice(self.ciphertext);
        blob
    }
}

/// The smallest padded size.
pub const MIN_BUCKET: usize = 256;

/// The padded size for `len` bytes (the length field included): the smallest size of
/// the sequence `256, 288, 324, ...` (each the previous plus an eighth, rounded down)
/// that holds them. Above [`MIN_BUCKET`] the padding is less than an eighth of `len`.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::publication::bucket;
///
/// assert_eq!(bucket(0), 256);
/// assert_eq!(bucket(256), 256);
/// assert_eq!(bucket(257), 288);
/// assert_eq!(bucket(289), 324);
/// ```
pub fn bucket(len: usize) -> usize {
    let mut size = MIN_BUCKET;
    while size < len {
        size = size.saturating_add(size / 8);
    }
    size
}

/// The largest content [`pad`] accepts (the length field is 32 bits).
pub const MAX_PADDED_CONTENT: usize = u32::MAX as usize - 4;

/// Pads `data` to its [`bucket`]: `[len: u32 BE][data][zeros]`.
///
/// # Panics
///
/// If `data` is longer than [`MAX_PADDED_CONTENT`] bytes.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::publication::{pad, unpad};
///
/// let padded = pad(b"book");
/// assert_eq!(padded.len(), 256);
/// assert_eq!(unpad(&padded).unwrap(), b"book");
/// ```
pub fn pad(data: &[u8]) -> Vec<u8> {
    let len = u32::try_from(data.len())
        .ok()
        .filter(|&len| len as usize <= MAX_PADDED_CONTENT)
        .expect("blob content too large to pad");
    let mut out = Vec::with_capacity(bucket(4 + data.len()));
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(data);
    out.resize(bucket(4 + data.len()), 0);
    out
}

/// The content of a [`pad`]ded blob.
pub fn unpad(padded: &[u8]) -> Result<&[u8], FormatError> {
    let len = padded.get(..4).ok_or(FormatError::Truncated)?;
    let len = u32::from_be_bytes(len.try_into().expect("four bytes")) as usize;
    padded
        .get(4..)
        .and_then(|rest| rest.get(..len))
        .ok_or(FormatError::Truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buckets_grow_by_at_most_an_eighth() {
        let mut previous = MIN_BUCKET;
        let mut size = bucket(MIN_BUCKET + 1);
        while size < 1 << 30 {
            assert!(size > previous);
            assert!((size - previous) * 8 <= previous, "{previous} → {size}");
            previous = size;
            size = bucket(size + 1);
        }
    }

    #[test]
    fn padding_overhead_stays_below_an_eighth() {
        for len in (MIN_BUCKET..200_000).step_by(37).chain([1 << 20, 50 << 20]) {
            let padded = bucket(len);
            assert!(padded >= len);
            assert!((padded - len) * 8 < len, "{len} → {padded}");
        }
    }

    #[test]
    fn bucket_boundaries() {
        assert_eq!(bucket(1), MIN_BUCKET);
        assert_eq!(bucket(288), 288);
        assert_eq!(bucket(289), 324);
        assert_eq!(bucket(324), 324);
        assert_eq!(bucket(325), 364);
    }

    #[test]
    fn unpad_recovers_the_true_length() {
        for len in [0, 1, 251, 252, 253, 283, 284, 285, 10_000] {
            let data: Vec<u8> = (0..len).map(|i| i as u8).collect();
            let padded = pad(&data);
            assert_eq!(padded.len(), bucket(len + 4), "{len}");
            assert_eq!(unpad(&padded).unwrap(), data.as_slice());
        }
    }

    #[test]
    fn unpad_refuses_lengths_beyond_the_blob() {
        assert_eq!(unpad(&[0, 0]), Err(FormatError::Truncated));
        assert_eq!(unpad(&[0, 0, 0, 5, 1, 2]), Err(FormatError::Truncated));
        assert_eq!(unpad(&[0xff; 300]), Err(FormatError::Truncated));
    }

    #[test]
    fn envelopes_round_trip_and_refuse_short_or_unknown_blobs() {
        let nonce = [3u8; NONCE_LEN];
        let ciphertext = [9u8; 40];
        let blob = Envelope {
            version: FORMAT_VERSION,
            nonce: &nonce,
            ciphertext: &ciphertext,
        }
        .to_bytes();
        assert_eq!(&blob[..2], &FORMAT_VERSION.to_be_bytes());
        let envelope = Envelope::parse(&blob).unwrap();
        assert_eq!(envelope.nonce, &nonce);
        assert_eq!(envelope.ciphertext, &ciphertext);

        assert_eq!(Envelope::parse(&[]), Err(FormatError::Truncated));
        assert_eq!(
            Envelope::parse(&blob[..HEADER_LEN + TAG_LEN - 1]),
            Err(FormatError::Truncated)
        );
        let mut future = blob.clone();
        future[..2].copy_from_slice(&2u16.to_be_bytes());
        let error = Envelope::parse(&future).unwrap_err();
        assert_eq!(
            error,
            FormatError::UnsupportedVersion {
                found: 2,
                supported: SUPPORTED_VERSIONS
            }
        );
        assert!(
            error
                .to_string()
                .contains("version 2 is not supported (supported: 1)"),
            "{error}"
        );
        future[..2].copy_from_slice(&0u16.to_be_bytes());
        assert!(matches!(
            Envelope::parse(&future),
            Err(FormatError::UnsupportedVersion { .. })
        ));
    }

    #[test]
    fn blob_paths_and_ids_parse_only_their_canonical_form() {
        for bad in [
            "",
            "p",
            "p/",
            "p/-1",
            "p/+1",
            "p/01",
            "x/1",
            "/p/1",
            "p/1/",
            "manifest/",
        ] {
            assert!(bad.parse::<BlobPath>().is_err(), "{bad:?}");
        }
        assert_eq!("p/0".parse(), Ok(BlobPath::Part(0)));
        assert_eq!("manifest".parse(), Ok(BlobPath::Manifest));
        for bad in [
            "",
            "AB".repeat(16).as_str(),
            &"a".repeat(31),
            &"g".repeat(32),
        ] {
            assert!(bad.parse::<PublicationId>().is_err(), "{bad:?}");
        }
    }
}
