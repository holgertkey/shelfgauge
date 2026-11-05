//! Members' requests to their library (Phase 31): a book of the Showcase or
//! thumbnails, written by a member's device as records of their account, read by the
//! library at its sync. A book on the member's own shelf whose copy on the hub is gone
//! (expired, Phase 31.6) is asked for the same way, by the reading view, and served
//! even when it is not on the Showcase.
//!
//! - `book_request` (id: the catalog book id in hex), JSON `{book, reason, state,
//!   updated_at}`: `state` `pending` (the device) or `refused` (the library, with a
//!   `reason`: `closed` (not on the Showcase now, nor on the member's shelf), `drm`, `quota`, `too-large`,
//!   `missing` (no such book any more) or `failed`). A request served is deleted by the
//!   library: the book is on the person's shelf (a `shelf_item`).
//! - `cover_request` (any id the device chooses), JSON `{covers, updated_at}`: thumbnail
//!   ids in hex, at most [`MAX_COVERS`]; deleted by the library once it uploaded what it
//!   could.
//!
//! A request pulled is kept in `hub_requests` in the same transaction as the account's
//! sync cursor, so a request whose answer fails (the hub unreachable, a crash) is
//! answered at the next sync. A refusal's `updated_at` is later than the request's, so
//! it wins the devices' merge even when this computer's clock is behind.

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::InRecord;
use super::keys::{self, AccountKey};
use crate::profiles::ProfileId;
use crate::storage::StorageError;

/// The kind of a book request.
pub const BOOK_REQUEST_KIND: &str = "book_request";

/// The kind of a thumbnail request.
pub const COVER_REQUEST_KIND: &str = "cover_request";

/// The most thumbnails one request asks for (more are ignored).
pub const MAX_COVERS: usize = 200;

/// The width of a thumbnail in pixels (the height follows the cover, at most twice
/// the width).
pub const THUMBNAIL_WIDTH: u32 = 120;

/// The JPEG quality of thumbnails.
const THUMBNAIL_QUALITY: u8 = 75;

/// A book request's plaintext (keys in alphabetical order, as the devices write
/// records).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookRequest {
    /// The catalog book id, hex.
    pub book: String,
    /// Why it was refused.
    #[serde(default)]
    pub reason: Option<String>,
    /// `pending` or `refused`.
    pub state: String,
    /// When it was written (Unix seconds).
    pub updated_at: i64,
}

impl BookRequest {
    /// The refusal of this request for `reason`, later than the request.
    pub fn refused(&self, reason: &str, now: i64) -> Self {
        Self {
            book: self.book.clone(),
            reason: Some(reason.to_owned()),
            state: "refused".to_owned(),
            updated_at: now.max(self.updated_at + 1),
        }
    }

    /// The plaintext.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a request always serializes")
    }
}

/// A thumbnail request's plaintext.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct CoverRequest {
    /// Thumbnail ids, hex.
    pub covers: Vec<String>,
}

/// A request kept until it is answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The profile whose account it came from.
    pub profile: ProfileId,
    /// [`BOOK_REQUEST_KIND`] or [`COVER_REQUEST_KIND`].
    pub kind: String,
    /// The record id.
    pub id: String,
    /// The record's plaintext.
    pub plain: Vec<u8>,
}

/// Keeps a pulled record of a request kind (`conn` the transaction of the pull; other
/// kinds are ignored): a record that opens with the account key is stored, a deleted
/// one forgotten. A record the library itself wrote (a refusal) is stored as well and
/// skipped when serving.
pub fn keep_pulled(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    record: &InRecord,
    now: i64,
) -> Result<(), StorageError> {
    if record.kind != BOOK_REQUEST_KIND && record.kind != COVER_REQUEST_KIND {
        return Ok(());
    }
    let plain = record
        .data
        .as_deref()
        .and_then(|data| keys::open_record(key, &record.kind, &record.id, data).ok());
    match plain {
        Some(plain) => conn.execute(
            "INSERT INTO hub_requests (profile_id, kind, record_id, plain, pulled_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (profile_id, kind, record_id) DO UPDATE SET
               plain = excluded.plain, pulled_at = excluded.pulled_at",
            params![profile.0, record.kind, record.id, plain, now],
        )?,
        None => conn.execute(
            "DELETE FROM hub_requests WHERE profile_id = ?1 AND kind = ?2 AND record_id = ?3",
            params![profile.0, record.kind, record.id],
        )?,
    };
    Ok(())
}

/// The requests waiting for an answer, oldest first.
pub fn pending(conn: &Connection) -> Result<Vec<Pending>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT profile_id, kind, record_id, plain FROM hub_requests ORDER BY pulled_at, rowid",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(Pending {
            profile: ProfileId(row.get(0)?),
            kind: row.get(1)?,
            id: row.get(2)?,
            plain: row.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Forgets a request once it is answered.
pub fn done(conn: &Connection, request: &Pending) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM hub_requests WHERE profile_id = ?1 AND kind = ?2 AND record_id = ?3",
        params![request.profile.0, request.kind, request.id],
    )?;
    Ok(())
}

/// A thumbnail of the cover image `data`: at most [`THUMBNAIL_WIDTH`] wide and twice
/// that high, as a JPEG; `None` for anything that does not decode within small
/// limits (covers come from the books: untrusted).
pub fn thumbnail(data: &[u8]) -> Option<Vec<u8>> {
    use image::codecs::jpeg::JpegEncoder;
    use image::imageops::FilterType;
    let mut reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let image = reader.decode().ok()?;
    let small = image.resize(THUMBNAIL_WIDTH, THUMBNAIL_WIDTH * 2, FilterType::Triangle);
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, THUMBNAIL_QUALITY)
        .encode_image(&small.to_rgb8())
        .ok()?;
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refusal_is_later_than_its_request_and_keeps_the_key_order() {
        let request = BookRequest {
            book: "ab".repeat(16),
            reason: None,
            state: "pending".into(),
            updated_at: 2_000,
        };
        let refused = request.refused("closed", 1_000);
        assert_eq!(refused.updated_at, 2_001, "a clock behind the phone's");
        assert_eq!(request.refused("drm", 5_000).updated_at, 5_000);
        assert_eq!(
            String::from_utf8(refused.encode()).unwrap(),
            format!(
                r#"{{"book":"{}","reason":"closed","state":"refused","updated_at":2001}}"#,
                "ab".repeat(16)
            )
        );
    }

    #[test]
    fn thumbnails_are_small_jpegs_and_garbage_is_refused() {
        // A 300×450 PNG.
        let mut png = Vec::new();
        image::DynamicImage::new_rgb8(300, 450)
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let thumb = thumbnail(&png).unwrap();
        assert_eq!(&thumb[..2], &[0xff, 0xd8], "a JPEG");
        let decoded = image::load_from_memory(&thumb).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (120, 180));
        assert!(thumbnail(b"not an image").is_none());
        assert!(thumbnail(&png[..png.len() / 2]).is_none());
    }
}
