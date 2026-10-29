//! Merging pulled records into `userdata.sqlite` (concept §11, rules final since
//! 2026-10-19): per record the newest `updated_at` wins, deletions by the person are
//! tombstones (`deleted_at`) and win like any change, positions are last write wins.
//! On equal times the record whose plaintext sorts higher (bytewise) wins, so every
//! client of the account ends with the same state.
//!
//! A record that wins is written and its outbox entry dropped (the merge is no local
//! change to push back). Records of books this library does not have are stored like
//! any others (with no book id), so they attach when the book appears (§10).
//!
//! A deleted (`null`) annotation or position on the hub means the book left the
//! shelf: local marks stay untouched. A deleted shelf item takes the book off this
//! library's shelf.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use super::InRecord;
use super::keys::{self, AccountKey};
use super::records;
use super::store;
use crate::profiles::ProfileId;
use crate::publication::BookKey;
use crate::publication::format::PublicationId;
use crate::storage::StorageError;

/// What applying a record did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied {
    /// Nothing: the local state is newer, the same, or the record is not understood.
    Kept,
    /// The member's display name or colour (a `profile` record) renamed the profile.
    Profile,
    /// The shelf changed.
    Shelf,
    /// An annotation of the book with this content key changed.
    Annotation(Vec<u8>),
    /// The position in the book with this content key changed.
    Position(Vec<u8>),
}

#[derive(Deserialize)]
struct ShelfItemIn {
    publication: Option<String>,
    book_key: Option<String>,
    content_key: String,
    kind: String,
    instance: String,
    title: String,
    authors: String,
    updated_at: i64,
}

#[derive(Deserialize)]
struct AnnotationIn {
    id: String,
    content_key: String,
    start_block: i64,
    start_offset: i64,
    end_block: i64,
    end_offset: i64,
    text: String,
    prefix: String,
    suffix: String,
    section_title: Option<String>,
    note: Option<String>,
    book_title: String,
    book_authors: String,
    created_at: i64,
    updated_at: i64,
    deleted_at: Option<i64>,
}

#[derive(Deserialize)]
struct PositionIn {
    content_key: String,
    block: i64,
    offset: i64,
    suffix: String,
    progress: i64,
    book_title: String,
    book_authors: String,
    updated_at: i64,
}

fn b64(text: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(text).ok()
}

/// Whether the remote state `(time, plaintext)` wins over the local one.
fn newer(remote: (i64, &[u8]), local: Option<(i64, &[u8])>) -> bool {
    match local {
        None => true,
        Some(local) => remote > local,
    }
}

fn drop_outbox(
    conn: &Connection,
    profile: ProfileId,
    kind: &str,
    key: &dyn rusqlite::ToSql,
) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM hub_outbox WHERE profile_id = ?1 AND kind = ?2 AND key = ?3",
        params![profile.0, kind, key],
    )?;
    Ok(())
}

/// Applies one pulled record of `profile`'s account; `book_id` finds the library's
/// book with a content key, `instance` is this library's name on the shelf.
pub fn apply(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    record: &InRecord,
    book_id: &dyn Fn(&[u8]) -> Option<i64>,
    instance: &str,
) -> Result<Applied, StorageError> {
    let Some(data) = &record.data else {
        return match record.kind.as_str() {
            "shelf_item" => shelf_deleted(conn, profile, key, record),
            _ => Ok(Applied::Kept),
        };
    };
    let Ok(plain) = keys::open_record(key, &record.kind, &record.id, data) else {
        return Ok(Applied::Kept);
    };
    match record.kind.as_str() {
        "shelf_item" => match serde_json::from_slice(&plain) {
            Ok(item) => shelf_item(conn, profile, key, record, &plain, item, instance),
            Err(_) => Ok(Applied::Kept),
        },
        "annotation" => match serde_json::from_slice(&plain) {
            Ok(a) => annotation(conn, profile, record, &plain, a, book_id),
            Err(_) => Ok(Applied::Kept),
        },
        "position" => match serde_json::from_slice(&plain) {
            Ok(p) => position(conn, profile, key, record, &plain, p, book_id),
            Err(_) => Ok(Applied::Kept),
        },
        // Written by a member's device; `library_key` records are the library's own.
        keys::PROFILE_KIND if record.id == keys::PROFILE_ID => {
            match serde_json::from_slice(&plain) {
                Ok(p) => profile_record(conn, profile, p),
                Err(_) => Ok(Applied::Kept),
            }
        }
        _ => Ok(Applied::Kept),
    }
}

#[derive(Deserialize)]
struct ProfileIn {
    display_name: String,
    color: u8,
}

/// A member's display name and colour rename their profile (a name taken by another
/// profile or not allowed is left as it is; so is an unknown colour).
fn profile_record(
    conn: &Connection,
    profile: ProfileId,
    record: ProfileIn,
) -> Result<Applied, StorageError> {
    let renamed = crate::profiles::store::rename(conn, profile, &record.display_name).is_ok();
    let coloured = crate::profiles::store::set_color(conn, profile, record.color).is_ok();
    Ok(if renamed || coloured {
        Applied::Profile
    } else {
        Applied::Kept
    })
}

fn shelf_item(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    record: &InRecord,
    plain: &[u8],
    item: ShelfItemIn,
    instance: &str,
) -> Result<Applied, StorageError> {
    let Some(content) = b64(&item.content_key) else {
        return Ok(Applied::Kept);
    };
    if keys::shelf_id(key, &content) != record.id {
        return Ok(Applied::Kept);
    }
    let publication: Option<PublicationId> = match &item.publication {
        Some(p) => match p.parse() {
            Ok(p) => Some(p),
            Err(_) => return Ok(Applied::Kept),
        },
        None => None,
    };
    let book_key = match item.book_key.as_deref().map(b64) {
        Some(Some(k)) => match <[u8; 32]>::try_from(k) {
            Ok(k) => Some(BookKey::new(k)),
            Err(_) => return Ok(Applied::Kept),
        },
        Some(None) => return Ok(Applied::Kept),
        None => None,
    };
    let local = store::shelf_entry(conn, profile, &content)?;
    let local_plain = records::shelf_item(conn, profile, &content)?;
    let local_state = local
        .as_ref()
        .map(|l| (l.updated_at, local_plain.as_deref().unwrap_or(&[] as &[u8])));
    if !newer((item.updated_at, plain), local_state) {
        return Ok(Applied::Kept);
    }
    // A book file this library exported stays its own when a device puts the book
    // back on the shelf by opening the file (its shelf item names this library).
    let own = local
        .as_ref()
        .is_some_and(|l| l.own && l.publication.is_some() && l.publication == publication)
        || (item.kind == "file" && item.instance == instance);
    let kind = if item.kind == "file" {
        "file"
    } else {
        "upload"
    };
    conn.execute(
        "INSERT OR REPLACE INTO hub_shelf
             (profile_id, book_key, publication_id, cipher_key, own, kind, state, instance,
              title, authors, published_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'published', ?7, ?8, ?9, ?10, ?11)",
        params![
            profile.0,
            content,
            publication.map(|p| p.as_bytes().to_vec()),
            book_key.as_ref().map(|k| k.as_bytes().to_vec()),
            own,
            kind,
            item.instance,
            item.title,
            item.authors,
            local.as_ref().and_then(|l| l.published_at),
            item.updated_at,
        ],
    )?;
    drop_outbox(conn, profile, "shelf_item", &content)?;
    Ok(Applied::Shelf)
}

/// A shelf item deleted on the hub: the book left the shelf on another client, unless
/// this library changed its entry later.
fn shelf_deleted(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    record: &InRecord,
) -> Result<Applied, StorageError> {
    let found = store::shelf(conn, profile)?
        .into_iter()
        .find(|e| keys::shelf_id(key, &e.content_key) == record.id);
    match found {
        Some(entry) if entry.updated_at <= record.updated_at => {
            store::delete_entry(conn, profile, &entry.content_key)?;
            drop_outbox(conn, profile, "shelf_item", &entry.content_key)?;
            Ok(Applied::Shelf)
        }
        _ => Ok(Applied::Kept),
    }
}

fn annotation(
    conn: &Connection,
    profile: ProfileId,
    record: &InRecord,
    plain: &[u8],
    a: AnnotationIn,
    book_id: &dyn Fn(&[u8]) -> Option<i64>,
) -> Result<Applied, StorageError> {
    let Some(content) = b64(&a.content_key) else {
        return Ok(Applied::Kept);
    };
    if a.id != record.id || a.start_block < 0 || a.end_block < a.start_block {
        return Ok(Applied::Kept);
    }
    // `(profile, updated_at, book id, anchor lost, removed with its book)`
    let local: Option<(i64, i64, i64, bool, bool)> = conn
        .query_row(
            "SELECT profile_id, updated_at, book_id, anchor_lost,
                    deleted_reason IS 'book_removed'
               FROM annotations WHERE id = ?1",
            [&a.id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    if let Some((owner, ..)) = local
        && owner != profile.0
    {
        // An id of another profile's annotation: never touched.
        return Ok(Applied::Kept);
    }
    let local_plain = records::annotation(conn, profile, &a.id)?.map(|r| r.plain);
    let local_state =
        local.map(|(_, updated, ..)| (updated, local_plain.as_deref().unwrap_or(&[] as &[u8])));
    if !newer((a.updated_at, plain), local_state) {
        return Ok(Applied::Kept);
    }
    let (deleted_at, reason) = match (a.deleted_at, local) {
        (Some(at), _) => (Some(at), Some("user")),
        // A book removed from this library stays removed until it comes back.
        (None, Some((.., true))) => (Some(a.updated_at), Some("book_removed")),
        (None, _) => (None, None),
    };
    let book = local
        .map(|(_, _, id, ..)| id)
        .or_else(|| book_id(&content))
        .unwrap_or(0);
    let anchor_lost = local.is_some_and(|(_, _, _, lost, _)| lost);
    conn.execute(
        "INSERT INTO annotations
             (id, profile_id, book_key, book_id, start_block, start_offset, end_block,
              end_offset, text, prefix, suffix, section_title, book_title, book_authors,
              anchor_lost, note, created_at, updated_at, deleted_at, deleted_reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                 ?17, ?18, ?19, ?20)
         ON CONFLICT (id) DO UPDATE SET
             book_key = excluded.book_key, start_block = excluded.start_block,
             start_offset = excluded.start_offset, end_block = excluded.end_block,
             end_offset = excluded.end_offset, text = excluded.text,
             prefix = excluded.prefix, suffix = excluded.suffix,
             section_title = excluded.section_title, book_title = excluded.book_title,
             book_authors = excluded.book_authors, note = excluded.note,
             created_at = excluded.created_at, updated_at = excluded.updated_at,
             deleted_at = excluded.deleted_at, deleted_reason = excluded.deleted_reason",
        params![
            a.id,
            profile.0,
            content,
            book,
            a.start_block,
            a.start_offset,
            a.end_block,
            a.end_offset,
            a.text,
            a.prefix,
            a.suffix,
            a.section_title,
            a.book_title,
            a.book_authors,
            anchor_lost,
            a.note,
            a.created_at,
            a.updated_at,
            deleted_at,
            reason,
        ],
    )?;
    drop_outbox(conn, profile, "annotation", &a.id)?;
    Ok(Applied::Annotation(content))
}

fn position(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    record: &InRecord,
    plain: &[u8],
    p: PositionIn,
    book_id: &dyn Fn(&[u8]) -> Option<i64>,
) -> Result<Applied, StorageError> {
    let Some(content) = b64(&p.content_key) else {
        return Ok(Applied::Kept);
    };
    if keys::position_id(key, &content) != record.id || p.block < 0 {
        return Ok(Applied::Kept);
    }
    let local: Option<(i64, i64)> = conn
        .query_row(
            "SELECT updated_at, book_id FROM reading_positions
              WHERE profile_id = ?1 AND book_key = ?2",
            params![profile.0, content],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let local_plain = records::position(conn, profile, &content)?;
    let local_state =
        local.map(|(updated, _)| (updated, local_plain.as_deref().unwrap_or(&[] as &[u8])));
    if !newer((p.updated_at, plain), local_state) {
        return Ok(Applied::Kept);
    }
    let book = local
        .map(|(_, id)| id)
        .or_else(|| book_id(&content))
        .unwrap_or(0);
    // A newer position brings the book back to "Currently reading"; a book removed
    // from this library keeps its removal mark.
    conn.execute(
        "INSERT INTO reading_positions
             (profile_id, book_key, book_id, block, offset, suffix, progress, book_title,
              book_authors, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (profile_id, book_key) DO UPDATE SET
             block = excluded.block, offset = excluded.offset, suffix = excluded.suffix,
             progress = excluded.progress, book_title = excluded.book_title,
             book_authors = excluded.book_authors, updated_at = excluded.updated_at,
             dismissed_at = NULL",
        params![
            profile.0,
            content,
            book,
            p.block,
            p.offset,
            p.suffix,
            p.progress.clamp(0, 1000),
            p.book_title,
            p.book_authors,
            p.updated_at,
        ],
    )?;
    drop_outbox(conn, profile, "position", &content)?;
    Ok(Applied::Position(content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::store::UserData;
    use crate::hub::{AccountId, Secret};
    use crate::hub_client::HubUrl;
    use crate::hub_client::keys::seal_record;

    fn setup() -> (UserData, AccountKey) {
        let userdata = UserData::open_in_memory().unwrap();
        let key = AccountKey::new([9; 32]);
        {
            let conn = userdata.lock();
            let url = HubUrl::parse("https://hub.example.org").unwrap();
            store::save_library(
                &conn,
                &url,
                AccountId::new([2; 16]),
                "home",
                &Secret::from_text("t"),
                &AccountKey::new([3; 32]),
                ProfileId::OWNER,
                AccountId::new([1; 16]),
                &key,
                "admin",
                0,
            )
            .unwrap();
            conn.execute(
                "INSERT INTO profiles (id, name, role, created_at) VALUES (2, 'Anna', 'reader', 0)",
                [],
            )
            .unwrap();
        }
        (userdata, key)
    }

    fn annotation_json(id: &str, note: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id": id, "content_key": "AAEC", "start_block": 1, "start_offset": 0,
            "end_block": 1, "end_offset": 0, "text": "", "prefix": "", "suffix": "s",
            "section_title": null, "note": note, "book_title": "T", "book_authors": "A",
            "created_at": 1, "updated_at": 5, "deleted_at": null,
        }))
        .unwrap()
    }

    fn record(kind: &str, id: &str, data: Option<Vec<u8>>) -> InRecord {
        InRecord {
            kind: kind.into(),
            id: id.into(),
            data,
            updated_at: 5,
        }
    }

    #[test]
    fn records_that_do_not_belong_are_ignored() {
        let (userdata, key) = setup();
        let conn = userdata.lock();
        let none = |_: &[u8]| None;
        let apply = |r: &InRecord| apply(&conn, ProfileId::OWNER, &key, r, &none, "Home").unwrap();

        // Sealed with another key, or for another record id.
        let plain = annotation_json("a1", "n");
        let other = AccountKey::new([8; 32]);
        let foreign = record(
            "annotation",
            "a1",
            Some(seal_record(&other, "annotation", "a1", &plain).unwrap()),
        );
        assert_eq!(apply(&foreign), Applied::Kept);
        let moved = record(
            "annotation",
            "a2",
            Some(seal_record(&key, "annotation", "a1", &plain).unwrap()),
        );
        assert_eq!(apply(&moved), Applied::Kept);
        // The plaintext's id must be the record's.
        let renamed = record(
            "annotation",
            "a2",
            Some(seal_record(&key, "annotation", "a2", &plain).unwrap()),
        );
        assert_eq!(apply(&renamed), Applied::Kept);
        // Not JSON of the kind.
        let junk = record(
            "position",
            "p",
            Some(seal_record(&key, "position", "p", b"[]").unwrap()),
        );
        assert_eq!(apply(&junk), Applied::Kept);
        // A position under the wrong id.
        let position = serde_json::to_vec(&serde_json::json!({
            "content_key": "AAEC", "block": 1, "offset": 0, "suffix": "", "progress": 5,
            "book_title": "T", "book_authors": "A", "updated_at": 5,
        }))
        .unwrap();
        let wrong = record(
            "position",
            "p",
            Some(seal_record(&key, "position", "p", &position).unwrap()),
        );
        assert_eq!(apply(&wrong), Applied::Kept);
        // Deleted annotations and positions on the hub never touch local data.
        assert_eq!(apply(&record("annotation", "a1", None)), Applied::Kept);

        let count: i64 = conn
            .query_row("SELECT (SELECT count(*) FROM annotations) + (SELECT count(*) FROM reading_positions)", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // A valid record is stored, once.
        let good = record(
            "annotation",
            "a1",
            Some(seal_record(&key, "annotation", "a1", &plain).unwrap()),
        );
        assert_eq!(apply(&good), Applied::Annotation(vec![0, 1, 2]));
        assert_eq!(
            apply(&good),
            Applied::Kept,
            "the same state again changes nothing"
        );
    }

    #[test]
    fn another_profiles_annotation_is_never_overwritten() {
        let (userdata, key) = setup();
        let conn = userdata.lock();
        conn.execute(
            "INSERT INTO annotations (id, profile_id, book_key, book_id, start_block,
                 start_offset, end_block, end_offset, text, prefix, suffix, book_title,
                 book_authors, created_at, updated_at)
             VALUES ('a1', 2, X'00', 0, 1, 0, 1, 0, '', '', '', 'T', 'A', 1, 1)",
            [],
        )
        .unwrap();
        let plain = annotation_json("a1", "taken");
        let r = record(
            "annotation",
            "a1",
            Some(seal_record(&key, "annotation", "a1", &plain).unwrap()),
        );
        assert_eq!(
            apply(&conn, ProfileId::OWNER, &key, &r, &|_| None, "Home").unwrap(),
            Applied::Kept
        );
        let owner: i64 = conn
            .query_row(
                "SELECT profile_id FROM annotations WHERE id = 'a1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(owner, 2);
    }
}
