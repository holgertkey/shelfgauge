//! Pushing a profile's changes to the hub: the outbox (`hub_outbox`) as sealed
//! records.
//!
//! The user-data lock is never held during a hub call. A pushed entry leaves the
//! outbox only if its record still reads the same afterwards: a change made while the
//! push was on the way stays queued for the next one.

use rusqlite::{Connection, params};

use super::keys::{self, AccountKey};
use super::records;
use super::store::HubAccount;
use super::{ClientError, HubClient, OutRecord};
use crate::annotations::store::UserData;
use crate::profiles::ProfileId;
use crate::storage::StorageError;

/// The most records in one push (the hub's limit is 500).
const BATCH: usize = 200;

/// The largest sealed record pushed (the hub's limit is 1 MiB); larger ones (a note
/// near `annotations.max_note_length` = 1 000 000) stay local.
const MAX_RECORD: usize = 1024 * 1024;

/// A failed push.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// The hub call failed.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// The user-data database failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl From<rusqlite::Error> for SyncError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.into())
    }
}

/// The key of an outbox entry: an annotation id or a content key.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OutboxKey {
    Text(String),
    Blob(Vec<u8>),
}

impl rusqlite::ToSql for OutboxKey {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            Self::Text(t) => t.to_sql(),
            Self::Blob(b) => b.to_sql(),
        }
    }
}

/// An outbox entry with its record as it reads now.
#[derive(Debug, Clone)]
struct Pending {
    kind: &'static str,
    key: OutboxKey,
    /// The record id on the hub.
    id: String,
    /// The plaintext; `None` deletes the record on the hub.
    plain: Option<Vec<u8>>,
}

fn kind(name: &str) -> Option<&'static str> {
    keys::RECORD_KINDS.iter().copied().find(|k| *k == name)
}

/// The record of an outbox entry as it reads now.
fn current(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    kind: &'static str,
    entry: &OutboxKey,
) -> Result<Option<Pending>, StorageError> {
    let (id, plain) = match (kind, entry) {
        ("shelf_item", OutboxKey::Blob(content)) => (
            keys::shelf_id(key, content),
            records::shelf_item(conn, profile, content)?,
        ),
        // Off the shelf: a deletion. On it, a missing record (the book was removed
        // from the library, or the row purged) is no change of the person's: nothing
        // is pushed, so a rescan with a drive unmounted never deletes marks on the
        // devices.
        ("position", OutboxKey::Blob(content)) => {
            let plain = if records::on_shelf(conn, profile, content)? {
                match records::position(conn, profile, content)? {
                    Some(plain) => Some(plain),
                    None => return Ok(None),
                }
            } else {
                None
            };
            (keys::position_id(key, content), plain)
        }
        ("annotation", OutboxKey::Text(id)) => {
            let Some(content) = records::annotation_book(conn, profile, id)? else {
                return Ok(None);
            };
            let plain = if records::on_shelf(conn, profile, &content)? {
                match records::annotation(conn, profile, id)? {
                    Some(record) => Some(record.plain),
                    None => return Ok(None),
                }
            } else {
                None
            };
            (id.clone(), plain)
        }
        _ => return Ok(None),
    };
    Ok(Some(Pending {
        kind,
        key: entry.clone(),
        id,
        plain,
    }))
}

/// Up to `limit` outbox entries of `profile` with their records.
fn pending(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    limit: usize,
) -> Result<Vec<Pending>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT kind, key FROM hub_outbox WHERE profile_id = ?1 ORDER BY kind, key LIMIT ?2",
    )?;
    let entries: Vec<(String, rusqlite::types::Value)> = stmt
        .query_map(params![profile.0, limit as i64], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut out = Vec::new();
    for (name, value) in entries {
        let entry = match value {
            rusqlite::types::Value::Text(t) => OutboxKey::Text(t),
            rusqlite::types::Value::Blob(b) => OutboxKey::Blob(b),
            _ => continue,
        };
        match kind(&name) {
            Some(kind) => match current(conn, profile, key, kind, &entry)? {
                Some(p) => out.push(p),
                None => forget(conn, profile, kind, &entry)?,
            },
            None => continue,
        }
    }
    Ok(out)
}

fn forget(
    conn: &Connection,
    profile: ProfileId,
    kind: &str,
    entry: &OutboxKey,
) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM hub_outbox WHERE profile_id = ?1 AND kind = ?2 AND key = ?3",
        params![profile.0, kind, entry],
    )?;
    Ok(())
}

/// Pushes the outbox of `account`'s profile; answers the number of records pushed.
///
/// Runs blocking HTTP: call it on a blocking thread.
pub fn push(
    userdata: &UserData,
    account: &HubAccount,
    client: &HubClient,
) -> Result<usize, SyncError> {
    let profile = account.profile;
    let mut pushed = 0;
    // Each round empties at least one entry or stops: a record changed during every
    // push would otherwise keep the loop going.
    for _ in 0..1000 {
        let batch = pending(&userdata.lock(), profile, &account.key, BATCH)?;
        if batch.is_empty() {
            break;
        }
        let mut out = Vec::with_capacity(batch.len());
        let mut too_large = Vec::new();
        for p in &batch {
            let data = p
                .plain
                .as_deref()
                .map(|plain| keys::seal_record(&account.key, p.kind, &p.id, plain))
                .transpose()
                .map_err(ClientError::from)?;
            if data.as_ref().is_some_and(|d| d.len() > MAX_RECORD) {
                too_large.push(p);
                continue;
            }
            out.push(OutRecord {
                kind: p.kind,
                id: p.id.clone(),
                data,
            });
        }
        if !out.is_empty() {
            client.push(&account.token, &out)?;
        }
        let conn = userdata.lock();
        let mut done = 0;
        for p in &batch {
            let now = current(&conn, profile, &account.key, p.kind, &p.key)?;
            if now.is_none_or(|n| n.plain == p.plain) {
                forget(&conn, profile, p.kind, &p.key)?;
                done += 1;
            }
        }
        if !too_large.is_empty() {
            tracing::warn!(
                profile = profile.0,
                count = too_large.len(),
                "records too large for the hub stay on this library"
            );
        }
        pushed += out.len();
        if done == 0 {
            break;
        }
    }
    if pushed > 0 {
        tracing::debug!(profile = profile.0, count = pushed, "records pushed");
    }
    Ok(pushed)
}
