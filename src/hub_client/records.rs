//! The plaintext of sync records: JSON objects built from the rows of
//! `userdata.sqlite` (keys sorted, so the same row always gives the same bytes).
//!
//! - `shelf_item`: `publication` (hex id or `null`), `book_key` (the publication's
//!   key, base64url), `content_key` (base64url), `title`, `authors`, `instance`,
//!   `kind` (`upload` | `file`), `format_version`, `updated_at`.
//! - `annotation`: `id`, `content_key`, `start_block`, `start_offset`, `end_block`,
//!   `end_offset` (blocks as ordinals, offsets in code points of rendered text),
//!   `text`, `prefix`, `suffix`, `section_title`, `note`, `book_title`,
//!   `book_authors`, `created_at`, `updated_at`, `deleted_at` (a deletion by the
//!   person; `null` otherwise). Local state (the book id hint, `anchor_lost`) stays
//!   local.
//! - `position`: `content_key`, `block`, `offset`, `suffix`, `progress` (per mille),
//!   `book_title`, `book_authors`, `updated_at`.
//!
//! Times are Unix seconds. The merge rules (concept §11) use `updated_at`.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::profiles::ProfileId;
use crate::publication::format::FORMAT_VERSION;
use crate::storage::StorageError;

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn bytes(value: &serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(value).expect("JSON values always serialize")
}

/// Whether the book with `content_key` is on `profile`'s shelf (not being removed).
pub fn on_shelf(
    conn: &Connection,
    profile: ProfileId,
    content_key: &[u8],
) -> Result<bool, StorageError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM hub_shelf
              WHERE profile_id = ?1 AND book_key = ?2 AND state <> 'removing'",
            params![profile.0, content_key],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The `shelf_item` of a book; `None` when it is not on the shelf (a deletion).
pub fn shelf_item(
    conn: &Connection,
    profile: ProfileId,
    content_key: &[u8],
) -> Result<Option<Vec<u8>>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT publication_id, cipher_key, kind, instance, title, authors, updated_at
               FROM hub_shelf
              WHERE profile_id = ?1 AND book_key = ?2 AND state <> 'removing'",
            params![profile.0, content_key],
            |row| {
                let publication: Option<Vec<u8>> = row.get(0)?;
                let key: Option<Vec<u8>> = row.get(1)?;
                Ok(json!({
                    "publication": publication.map(|p| p.iter().map(|b| format!("{b:02x}")).collect::<String>()),
                    "book_key": key.as_deref().map(b64),
                    "content_key": b64(content_key),
                    "kind": row.get::<_, String>(2)?,
                    "instance": row.get::<_, String>(3)?,
                    "title": row.get::<_, String>(4)?,
                    "authors": row.get::<_, String>(5)?,
                    "updated_at": row.get::<_, i64>(6)?,
                    "format_version": FORMAT_VERSION,
                }))
            },
        )
        .optional()?
        .map(|v| bytes(&v)))
}

/// The content key of an annotation's book, whatever its state; `None` when the row
/// is gone.
pub fn annotation_book(
    conn: &Connection,
    profile: ProfileId,
    id: &str,
) -> Result<Option<Vec<u8>>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT book_key FROM annotations WHERE profile_id = ?1 AND id = ?2",
            params![profile.0, id],
            |row| row.get(0),
        )
        .optional()?)
}

/// An annotation record: its book's content key and the plaintext.
pub struct AnnotationRecord {
    /// The book's content key.
    pub content_key: Vec<u8>,
    /// The JSON plaintext.
    pub plain: Vec<u8>,
}

/// An annotation's record; `None` when it is gone or its book was removed from the
/// library (not a deletion by the person).
pub fn annotation(
    conn: &Connection,
    profile: ProfileId,
    id: &str,
) -> Result<Option<AnnotationRecord>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT book_key, start_block, start_offset, end_block, end_offset, text, prefix,
                    suffix, section_title, note, book_title, book_authors, created_at,
                    updated_at, deleted_at
               FROM annotations
              WHERE profile_id = ?1 AND id = ?2 AND deleted_reason IS NOT 'book_removed'",
            params![profile.0, id],
            |row| {
                let key: Vec<u8> = row.get(0)?;
                let value = json!({
                    "id": id,
                    "content_key": b64(&key),
                    "start_block": row.get::<_, i64>(1)?,
                    "start_offset": row.get::<_, i64>(2)?,
                    "end_block": row.get::<_, i64>(3)?,
                    "end_offset": row.get::<_, i64>(4)?,
                    "text": row.get::<_, String>(5)?,
                    "prefix": row.get::<_, String>(6)?,
                    "suffix": row.get::<_, String>(7)?,
                    "section_title": row.get::<_, Option<String>>(8)?,
                    "note": row.get::<_, Option<String>>(9)?,
                    "book_title": row.get::<_, String>(10)?,
                    "book_authors": row.get::<_, String>(11)?,
                    "created_at": row.get::<_, i64>(12)?,
                    "updated_at": row.get::<_, i64>(13)?,
                    "deleted_at": row.get::<_, Option<i64>>(14)?,
                });
                Ok(AnnotationRecord {
                    plain: bytes(&value),
                    content_key: key,
                })
            },
        )
        .optional()?)
}

/// The `position` of a book; `None` when there is none (or its book was removed).
pub fn position(
    conn: &Connection,
    profile: ProfileId,
    content_key: &[u8],
) -> Result<Option<Vec<u8>>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT block, offset, suffix, progress, book_title, book_authors, updated_at
               FROM reading_positions
              WHERE profile_id = ?1 AND book_key = ?2 AND deleted_at IS NULL",
            params![profile.0, content_key],
            |row| {
                Ok(json!({
                    "content_key": b64(content_key),
                    "block": row.get::<_, i64>(0)?,
                    "offset": row.get::<_, i64>(1)?,
                    "suffix": row.get::<_, String>(2)?,
                    "progress": row.get::<_, i64>(3)?,
                    "book_title": row.get::<_, String>(4)?,
                    "book_authors": row.get::<_, String>(5)?,
                    "updated_at": row.get::<_, i64>(6)?,
                }))
            },
        )
        .optional()?
        .map(|v| bytes(&v)))
}
