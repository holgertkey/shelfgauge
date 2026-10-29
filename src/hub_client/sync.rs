//! Pushing a profile's changes to the hub: the outbox (`hub_outbox`) as sealed
//! records.
//!
//! The user-data lock is never held during a hub call. A pushed entry leaves the
//! outbox only if its record still reads the same afterwards: a change made while the
//! push was on the way stays queued for the next one.

use rusqlite::{Connection, params};

use super::keys::{self, AccountKey};
use super::records;
use super::store::{
    self, HubAccount, HubLibrary, LegacyAccount, ShelfEntry, ShelfKind, ShelfState,
};
use super::{ClientError, HubClient, OutRecord};
use crate::annotations::store::UserData;
use crate::hub::AccountId;
use crate::profiles::ProfileId;
use crate::storage::StorageError;

/// The most records in one push (the hub's limit is 500).
const BATCH: usize = 200;

/// The largest sealed record pushed (the hub's limit is 1 MiB); larger ones (a note
/// near `annotations.max_note_length` = 1 000 000) stay local.
pub(crate) const MAX_RECORD: usize = 1024 * 1024;

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
    ["shelf_item", "annotation", "position"]
        .into_iter()
        .find(|k| *k == name)
}

/// Which account a sync call is for: the library's token naming the account.
#[derive(Debug, Clone, Copy)]
pub struct Target<'a> {
    /// The library's token.
    pub token: &'a crate::hub::Secret,
    /// The account's public id.
    pub id: &'a AccountId,
}

impl<'a> Target<'a> {
    /// The target of `account` of `library`; `None` while the account has no id on
    /// the hub (an invite not yet redeemed) or is gone.
    pub fn of(library: &'a HubLibrary, account: &'a HubAccount) -> Option<Self> {
        if account.gone_at.is_some() {
            return None;
        }
        Some(Self {
            token: &library.token,
            id: account.account.as_ref()?,
        })
    }
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
    target: Target<'_>,
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
            client.push(target.token, Some(target.id), &out)?;
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

/// Deletes the publication of a book being removed from the shelf, pushes the
/// deletions and forgets the entry; `false` when the hub could not be reached (the
/// entry stays `removing` for the next sync). A book on the shelf as files only has
/// no copy on the hub. Runs blocking HTTP.
pub fn take_down(
    userdata: &UserData,
    target: Target<'_>,
    account: &HubAccount,
    client: &HubClient,
    entry: &ShelfEntry,
) -> bool {
    if let Some(id) = entry
        .publication
        .as_ref()
        .filter(|_| entry.kind == ShelfKind::Upload && entry.own)
        && let Err(e) = client.delete_publication(target.token, id)
    {
        tracing::info!(error = %e, "the hub did not delete a publication");
        return false;
    }
    if let Err(e) = push(userdata, target, account, client) {
        tracing::info!(error = %e, "push after removing failed");
        return false;
    }
    let conn = userdata.lock();
    // Unless it was published again meanwhile.
    match store::shelf_entry(&conn, account.profile, &entry.content_key) {
        Ok(Some(now)) if now.state == ShelfState::Removing => {
            if let Err(e) = store::delete_entry(&conn, account.profile, &entry.content_key) {
                tracing::error!(error = %e, "cannot write the hub shelf");
            }
        }
        Ok(_) => {}
        Err(e) => tracing::error!(error = %e, "cannot read the hub shelf"),
    }
    true
}

/// Marks this library's publications that the hub no longer has as expired (books on
/// the shelf as files never were there and do not expire). Runs blocking HTTP.
pub fn check_expired(
    userdata: &UserData,
    target: Target<'_>,
    account: &HubAccount,
    client: &HubClient,
) -> Result<usize, SyncError> {
    let on_hub = client.publications(target.token)?;
    let conn = userdata.lock();
    let mut expired = 0;
    for entry in store::shelf(&conn, account.profile)? {
        if entry.own
            && entry.kind == ShelfKind::Upload
            && entry.state == ShelfState::Published
            && entry.publication.is_some_and(|id| !on_hub.contains(&id))
        {
            store::set_state(
                &conn,
                account.profile,
                &entry.content_key,
                ShelfState::Expired,
                None,
            )?;
            expired += 1;
        }
    }
    if expired > 0 {
        tracing::info!(
            profile = account.profile.0,
            count = expired,
            "publications expired on the hub"
        );
    }
    Ok(expired)
}

/// What [`sync_once`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Records pushed.
    pub pushed: usize,
    /// Records pulled.
    pub pulled: usize,
    /// Records that changed this library.
    pub merged: usize,
    /// Content keys of the books whose annotations changed (for the Markdown export).
    pub annotated: std::collections::BTreeSet<Vec<u8>>,
}

/// Pulls everything after `account`'s last `seq` and merges it; `book_id` finds the
/// library's book with a content key, `instance` is this library's name. Runs
/// blocking HTTP.
pub fn pull(
    userdata: &UserData,
    target: Target<'_>,
    account: &HubAccount,
    client: &HubClient,
    book_id: &dyn Fn(&[u8]) -> Option<i64>,
    instance: &str,
    report: &mut Report,
) -> Result<(), SyncError> {
    let profile = account.profile;
    let mut since = store::last_seq(&userdata.lock(), profile)?;
    loop {
        let page = client.pull(target.token, Some(target.id), since)?;
        let conn = userdata.lock();
        let tx = conn.unchecked_transaction()?;
        for record in &page.records {
            report.pulled += 1;
            match super::merge::apply(&tx, profile, &account.key, record, book_id, instance)? {
                super::merge::Applied::Kept => {}
                super::merge::Applied::Annotation(content) => {
                    report.merged += 1;
                    report.annotated.insert(content);
                }
                _ => report.merged += 1,
            }
        }
        store::set_last_seq(&tx, profile, page.seq)?;
        tx.commit()?;
        if !page.more || page.seq <= since {
            break;
        }
        since = page.seq;
    }
    Ok(())
}

/// Hands the library key generations the account has not got yet to it as
/// `library_key` records (the library is the only writer of these), newest last;
/// answers how many. Every key this library hands to its members goes through here.
/// Runs blocking HTTP.
pub fn send_library_keys(
    userdata: &UserData,
    target: Target<'_>,
    account: &HubAccount,
    client: &HubClient,
) -> Result<usize, SyncError> {
    let generations = store::library_keys(&userdata.lock())?;
    let missing: Vec<_> = generations
        .iter()
        .filter(|(key_id, _)| *key_id > account.keys_sent)
        .collect();
    let Some(newest) = missing.last().map(|(key_id, _)| *key_id) else {
        return Ok(0);
    };
    let now = crate::annotations::now();
    let out = missing
        .iter()
        .map(|(key_id, key)| {
            let id = keys::library_key_id(*key_id);
            let plain = keys::library_key_plain(*key_id, key, now);
            Ok(OutRecord {
                kind: keys::LIBRARY_KEY_KIND,
                data: Some(keys::seal_record(
                    &account.key,
                    keys::LIBRARY_KEY_KIND,
                    &id,
                    &plain,
                )?),
                id,
            })
        })
        .collect::<Result<Vec<_>, getrandom::Error>>()
        .map_err(ClientError::from)?;
    client.push(target.token, Some(target.id), &out)?;
    store::set_keys_sent(&userdata.lock(), account.profile, newest)?;
    Ok(out.len())
}

/// One full sync of `account`: hand it newer library keys, push the outbox, finish
/// removals, pull and merge, notice expired publications; records the outcome on the
/// account. `instance` is this library's name on the shelf. Runs blocking HTTP.
pub fn sync_once(
    userdata: &UserData,
    target: Target<'_>,
    account: &HubAccount,
    client: &HubClient,
    book_id: &dyn Fn(&[u8]) -> Option<i64>,
    instance: &str,
    now: i64,
) -> Result<Report, SyncError> {
    let mut report = Report::default();
    let result = (|| {
        report.pushed += send_library_keys(userdata, target, account, client)?;
        report.pushed += push(userdata, target, account, client)?;
        let removing: Vec<ShelfEntry> = store::shelf(&userdata.lock(), account.profile)?
            .into_iter()
            .filter(|e| e.state == ShelfState::Removing)
            .collect();
        for entry in &removing {
            take_down(userdata, target, account, client, entry);
        }
        pull(
            userdata,
            target,
            account,
            client,
            book_id,
            instance,
            &mut report,
        )?;
        check_expired(userdata, target, account, client)?;
        Ok(())
    })();
    let outcome = match &result {
        Ok(()) => Ok(now),
        Err(SyncError::Client(e)) => Err(e.user_text()),
        Err(e) => Err(e.to_string()),
    };
    store::record_sync(&userdata.lock(), account.profile, outcome)?;
    result.map(|()| report)
}

/// The last pull of an account of 0.18–0.20 (decision 26 of Phase 30): pulls its
/// annotations and positions once more with the old device token and merges them
/// (shelf items are skipped: their publications do not open any more), then forgets
/// the account. Kept for the next sync when the hub cannot be reached; forgotten
/// without a pull when the hub no longer knows the token. Answers whether the account
/// is done. Runs blocking HTTP.
pub fn pull_legacy(
    userdata: &UserData,
    legacy: &LegacyAccount,
    book_id: &dyn Fn(&[u8]) -> Option<i64>,
    instance: &str,
    report: &mut Report,
) -> Result<bool, SyncError> {
    let client = HubClient::new(legacy.hub_url.clone());
    let mut since = legacy.last_seq;
    loop {
        let page = match client.pull(&legacy.token, None, since) {
            Ok(page) => page,
            Err(e) if e.is_unauthorized() || e.code() == Some("not_found") => {
                store::forget_legacy(&userdata.lock(), legacy.profile)?;
                return Ok(true);
            }
            Err(e) => return Err(e.into()),
        };
        let conn = userdata.lock();
        let tx = conn.unchecked_transaction()?;
        for record in page.records.iter().filter(|r| r.kind != "shelf_item") {
            report.pulled += 1;
            match super::merge::apply(&tx, legacy.profile, &legacy.key, record, book_id, instance)?
            {
                super::merge::Applied::Kept => {}
                super::merge::Applied::Annotation(content) => {
                    report.merged += 1;
                    report.annotated.insert(content);
                }
                _ => report.merged += 1,
            }
        }
        store::set_legacy_seq(&tx, legacy.profile, page.seq)?;
        tx.commit()?;
        if !page.more || page.seq <= since {
            break;
        }
        since = page.seq;
    }
    store::forget_legacy(&userdata.lock(), legacy.profile)?;
    tracing::info!(
        profile = legacy.profile.0,
        "last pull of an old hub account done"
    );
    Ok(true)
}
