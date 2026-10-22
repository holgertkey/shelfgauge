//! The hub database, `<data>/hub.sqlite`: accounts, devices, pairings, invites,
//! publications and their blobs, sync records.
//!
//! Used through one connection behind a mutex (like the user data of an instance).
//! Functions take a [`Connection`] and the current time (Unix seconds), so tests set
//! the clock. Secrets (device tokens, pairing secrets, invite codes) are stored as
//! their SHA-256 only; the recovery secret's SHA-256 is computed by the instance, the
//! hub never sees the secret before a recovery. Every lookup by token is bound to the
//! token's account: an id of another account behaves as "not found".

use rusqlite::{Connection, OptionalExtension, params};

use super::config::{AccountsConfig, QuotasConfig, Registration};
use super::{AccountId, HubError, Secret};
use crate::publication::format::{BlobPath, PublicationId};
use crate::storage::{StorageError, migrations};

/// Schema migrations of the hub database; index `i` upgrades version `i` to `i + 1`.
/// Append only once a hub has been released.
pub(crate) const MIGRATIONS: &[&str] = &[V1];

/// Version 1.
///
/// - `accounts.public_id` is the id in the API and the recovery code (16 random
///   bytes); `id` is the internal id used in logs. `seq` is the last sync sequence
///   number given out for the account.
/// - Hashes are SHA-256 of the secret's text; devices are revoked by setting
///   `revoked_at` (the token stops working, the row stays out of the device list).
/// - `records.data` NULL is a tombstone: a deleted record keeps its row and gets a new
///   `seq`, so other clients learn about the deletion.
/// - `takedowns` remembers taken-down publication ids, which cannot be uploaded again.
const V1: &str = r#"
CREATE TABLE accounts (
    id            INTEGER PRIMARY KEY,
    public_id     BLOB    NOT NULL UNIQUE,
    recovery_hash BLOB    NOT NULL,
    seq           INTEGER NOT NULL DEFAULT 0,
    created_at    INTEGER NOT NULL
);

CREATE TABLE devices (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    token_hash   BLOB    NOT NULL UNIQUE,
    name         BLOB,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    revoked_at   INTEGER
);

CREATE INDEX devices_account ON devices (account_id);

CREATE TABLE pairings (
    secret_hash BLOB    PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    expires_at  INTEGER NOT NULL
);

CREATE TABLE invites (
    code_hash  BLOB    PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE TABLE publications (
    id             INTEGER PRIMARY KEY,
    public_id      BLOB    NOT NULL UNIQUE,
    account_id     INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    created_at     INTEGER NOT NULL,
    last_opened_at INTEGER NOT NULL
);

CREATE INDEX publications_account ON publications (account_id);

CREATE TABLE blobs (
    publication_id INTEGER NOT NULL REFERENCES publications (id) ON DELETE CASCADE,
    path           TEXT    NOT NULL,
    size           INTEGER NOT NULL,
    PRIMARY KEY (publication_id, path)
) WITHOUT ROWID;

CREATE TABLE takedowns (
    public_id BLOB    PRIMARY KEY,
    taken_at  INTEGER NOT NULL
);

CREATE TABLE records (
    account_id INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    kind       TEXT    NOT NULL,
    record_id  TEXT    NOT NULL,
    seq        INTEGER NOT NULL,
    data       BLOB,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (account_id, kind, record_id)
) WITHOUT ROWID;

CREATE UNIQUE INDEX records_seq ON records (account_id, seq);
"#;

/// `last_seen_at` and `last_opened_at` are written at most this often (seconds).
const SEEN_INTERVAL: i64 = 3600;

/// How long a pairing secret is valid (seconds).
pub const PAIRING_TTL: i64 = 600;

/// Seconds per day.
const DAY: i64 = 86_400;

/// Opens (creating and migrating if needed) the hub database at `path`.
pub(crate) fn open(path: &std::path::Path) -> Result<Connection, StorageError> {
    let mut conn = crate::storage::open_wal(path)?;
    migrations::apply(&mut conn, MIGRATIONS)?;
    Ok(conn)
}

/// A migrated in-memory database (for tests).
#[cfg(test)]
pub(crate) fn open_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::storage::configure(&conn).unwrap();
    migrations::apply(&mut conn, MIGRATIONS).unwrap();
    conn
}

/// The device a token belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceAuth {
    /// The internal account id.
    pub account: i64,
    /// The device id (unique on the hub).
    pub device: i64,
}

/// A device token just issued.
#[derive(Debug)]
pub struct NewDevice {
    /// The internal account id.
    pub account: i64,
    /// The public account id.
    pub account_id: AccountId,
    /// The device id.
    pub device: i64,
    /// The token (shown once, stored hashed).
    pub token: Secret,
}

/// A device as the device list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The device id.
    pub id: i64,
    /// The name, encrypted by the client with the account key.
    pub name: Option<Vec<u8>>,
    /// When it was paired.
    pub created_at: i64,
    /// When it last made a request (to the hour).
    pub last_seen_at: i64,
}

/// What an account stores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Bytes of blobs and records.
    pub bytes: u64,
    /// Publications.
    pub publications: u64,
    /// Devices that are not revoked.
    pub devices: u64,
    /// Records, tombstones included.
    pub records: u64,
}

/// A publication of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicationInfo {
    /// The id.
    pub id: PublicationId,
    /// When its first blob was uploaded.
    pub created_at: i64,
    /// When its manifest was last fetched (to the hour), else the creation time.
    pub last_opened_at: i64,
    /// The bytes of its blobs.
    pub bytes: u64,
    /// The number of its blobs.
    pub blobs: u64,
}

/// A record as stored and sent to clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// `shelf_item`, `annotation` or `position`.
    pub kind: String,
    /// The client's id of the record.
    pub id: String,
    /// The account's sequence number of its last change.
    pub seq: i64,
    /// The ciphertext; `None` for a deleted record.
    pub data: Option<Vec<u8>>,
    /// When the hub received the change.
    pub updated_at: i64,
}

/// One change pushed by a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordChange {
    /// The record's kind.
    pub kind: String,
    /// The record's id.
    pub id: String,
    /// The new ciphertext; `None` deletes the record.
    pub data: Option<Vec<u8>>,
}

/// An account as `shelfgauge hub accounts` lists it (no secrets, no public id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// The internal id.
    pub id: i64,
    /// When it was created.
    pub created_at: i64,
    /// When one of its devices last made a request.
    pub last_seen_at: Option<i64>,
    /// What it stores.
    pub usage: Usage,
}

/// Stores a new invite code valid for `days`.
pub fn create_invite(conn: &Connection, days: u32, now: i64) -> Result<Secret, HubError> {
    let code = Secret::generate()?;
    conn.execute("DELETE FROM invites WHERE expires_at <= ?1", [now])?;
    conn.execute(
        "INSERT INTO invites (code_hash, created_at, expires_at) VALUES (?1, ?2, ?3)",
        params![code.hash(), now, now + i64::from(days) * DAY],
    )?;
    Ok(code)
}

/// Creates an account with its first device: checks the registration rule (an invite
/// code is used up) and `max_accounts`.
pub fn create_account(
    conn: &mut Connection,
    accounts: &AccountsConfig,
    invite: Option<&Secret>,
    recovery_hash: &[u8; 32],
    device_name: Option<&[u8]>,
    now: i64,
) -> Result<NewDevice, HubError> {
    let tx = conn.transaction()?;
    if accounts.registration == Registration::Invite {
        let used = match invite {
            Some(code) => tx.execute(
                "DELETE FROM invites WHERE code_hash = ?1 AND expires_at > ?2",
                params![code.hash(), now],
            )?,
            None => 0,
        };
        if used == 0 {
            return Err(HubError::InviteRequired);
        }
    }
    if accounts.max_accounts > 0 {
        let count: i64 = tx.query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))?;
        if count as u64 >= accounts.max_accounts {
            return Err(HubError::Quota("accounts"));
        }
    }
    let account_id = AccountId::generate()?;
    tx.execute(
        "INSERT INTO accounts (public_id, recovery_hash, created_at) VALUES (?1, ?2, ?3)",
        params![account_id.as_bytes(), recovery_hash, now],
    )?;
    let account = tx.last_insert_rowid();
    let device = insert_device(&tx, account, account_id, device_name, now)?;
    tx.commit()?;
    Ok(device)
}

fn insert_device(
    conn: &Connection,
    account: i64,
    account_id: AccountId,
    name: Option<&[u8]>,
    now: i64,
) -> Result<NewDevice, HubError> {
    let token = Secret::generate()?;
    conn.execute(
        "INSERT INTO devices (account_id, token_hash, name, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?4)",
        params![account, token.hash(), name, now],
    )?;
    Ok(NewDevice {
        account,
        account_id,
        device: conn.last_insert_rowid(),
        token,
    })
}

/// Adds a device to `account` unless the device quota is used up.
fn add_device(
    conn: &Connection,
    account: i64,
    quotas: &QuotasConfig,
    name: Option<&[u8]>,
    now: i64,
) -> Result<NewDevice, HubError> {
    if quotas.max_devices > 0 && usage(conn, account)?.devices >= quotas.max_devices {
        return Err(HubError::Quota("devices"));
    }
    insert_device(conn, account, account_id(conn, account)?, name, now)
}

/// Stores a new pairing secret of `account`, valid for [`PAIRING_TTL`].
pub fn create_pairing(conn: &Connection, account: i64, now: i64) -> Result<Secret, HubError> {
    let secret = Secret::generate()?;
    conn.execute("DELETE FROM pairings WHERE expires_at <= ?1", [now])?;
    conn.execute(
        "INSERT INTO pairings (secret_hash, account_id, expires_at) VALUES (?1, ?2, ?3)",
        params![secret.hash(), account, now + PAIRING_TTL],
    )?;
    Ok(secret)
}

/// Uses up a pairing secret and issues a device token of its account.
pub fn pair(
    conn: &mut Connection,
    secret: &Secret,
    quotas: &QuotasConfig,
    name: Option<&[u8]>,
    now: i64,
) -> Result<NewDevice, HubError> {
    let tx = conn.transaction()?;
    let account: Option<(i64, i64)> = tx
        .query_row(
            "DELETE FROM pairings WHERE secret_hash = ?1 RETURNING account_id, expires_at",
            [secret.hash()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let account = match account {
        Some((account, expires_at)) if expires_at > now => account,
        Some(_) => {
            // The expired secret is gone either way.
            tx.commit()?;
            return Err(HubError::InvalidSecret);
        }
        None => return Err(HubError::InvalidSecret),
    };
    let device = add_device(&tx, account, quotas, name, now)?;
    tx.commit()?;
    Ok(device)
}

/// Issues a device token of the account `account_id` for its recovery secret.
pub fn recover(
    conn: &Connection,
    account_id: AccountId,
    secret: &Secret,
    quotas: &QuotasConfig,
    name: Option<&[u8]>,
    now: i64,
) -> Result<NewDevice, HubError> {
    let account: Option<i64> = conn
        .query_row(
            "SELECT id FROM accounts WHERE public_id = ?1 AND recovery_hash = ?2",
            params![account_id.as_bytes(), secret.hash()],
            |r| r.get(0),
        )
        .optional()?;
    let account = account.ok_or(HubError::InvalidSecret)?;
    add_device(conn, account, quotas, name, now)
}

/// The device of a token, if it is valid; notes the device as seen (at most hourly).
pub fn device_by_token(
    conn: &Connection,
    token: &Secret,
    now: i64,
) -> Result<Option<DeviceAuth>, HubError> {
    let found: Option<(i64, i64, i64)> = conn
        .query_row(
            "SELECT id, account_id, last_seen_at FROM devices
             WHERE token_hash = ?1 AND revoked_at IS NULL",
            [token.hash()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((device, account, last_seen)) = found else {
        return Ok(None);
    };
    if now - last_seen >= SEEN_INTERVAL {
        conn.execute(
            "UPDATE devices SET last_seen_at = ?2 WHERE id = ?1",
            params![device, now],
        )?;
    }
    Ok(Some(DeviceAuth { account, device }))
}

/// The devices of `account` that are not revoked, oldest first.
pub fn devices(conn: &Connection, account: i64) -> Result<Vec<DeviceInfo>, HubError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, created_at, last_seen_at FROM devices
         WHERE account_id = ?1 AND revoked_at IS NULL ORDER BY id",
    )?;
    let rows = stmt.query_map([account], |r| {
        Ok(DeviceInfo {
            id: r.get(0)?,
            name: r.get(1)?,
            created_at: r.get(2)?,
            last_seen_at: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Revokes the device `device` of `account`; false if there is no such device.
pub fn revoke_device(
    conn: &Connection,
    account: i64,
    device: i64,
    now: i64,
) -> Result<bool, HubError> {
    let changed = conn.execute(
        "UPDATE devices SET revoked_at = ?3
         WHERE id = ?1 AND account_id = ?2 AND revoked_at IS NULL",
        params![device, account, now],
    )?;
    Ok(changed > 0)
}

/// What `account` stores.
pub fn usage(conn: &Connection, account: i64) -> Result<Usage, HubError> {
    Ok(conn.query_row(
        "SELECT
           coalesce((SELECT sum(b.size) FROM blobs b
                     JOIN publications p ON p.id = b.publication_id
                     WHERE p.account_id = ?1), 0)
         + coalesce((SELECT sum(length(data)) FROM records WHERE account_id = ?1), 0),
           (SELECT count(*) FROM publications WHERE account_id = ?1),
           (SELECT count(*) FROM devices WHERE account_id = ?1 AND revoked_at IS NULL),
           (SELECT count(*) FROM records WHERE account_id = ?1)",
        [account],
        |r| {
            Ok(Usage {
                bytes: r.get::<_, i64>(0)? as u64,
                publications: r.get::<_, i64>(1)? as u64,
                devices: r.get::<_, i64>(2)? as u64,
                records: r.get::<_, i64>(3)? as u64,
            })
        },
    )?)
}

/// The publications of `account`, oldest first.
pub fn publications(conn: &Connection, account: i64) -> Result<Vec<PublicationInfo>, HubError> {
    let mut stmt = conn.prepare(
        "SELECT p.public_id, p.created_at, p.last_opened_at,
                coalesce(sum(b.size), 0), count(b.path)
         FROM publications p LEFT JOIN blobs b ON b.publication_id = p.id
         WHERE p.account_id = ?1 GROUP BY p.id ORDER BY p.id",
    )?;
    let rows = stmt.query_map([account], |r| {
        Ok(PublicationInfo {
            id: PublicationId::new(r.get(0)?),
            created_at: r.get(1)?,
            last_opened_at: r.get(2)?,
            bytes: r.get::<_, i64>(3)? as u64,
            blobs: r.get::<_, i64>(4)? as u64,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The publication `id` of `account`: its row id, or `NotFound` (also for another
/// account's publication) or `Gone` (taken down).
fn own_publication(conn: &Connection, account: i64, id: &PublicationId) -> Result<i64, HubError> {
    let found: Option<(i64, i64)> = conn
        .query_row(
            "SELECT id, account_id FROM publications WHERE public_id = ?1",
            [id.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match found {
        Some((row, owner)) if owner == account => Ok(row),
        Some(_) => Err(HubError::NotFound),
        None if is_taken_down(conn, id)? => Err(HubError::Gone),
        None => Err(HubError::NotFound),
    }
}

fn is_taken_down(conn: &Connection, id: &PublicationId) -> Result<bool, HubError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM takedowns WHERE public_id = ?1",
            [id.as_bytes()],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// What [`reserve_blob`] replaced, for [`release_blob`] when writing the file fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reservation {
    publication: i64,
    old_size: Option<i64>,
    created: bool,
}

/// Records the blob `path` of publication `id` with `size` bytes for `account`
/// before its file is written: creates the publication on its first blob (taken-down
/// ids are refused, another account's id is "not found"), checks the publication and
/// byte quotas.
pub fn reserve_blob(
    conn: &mut Connection,
    account: i64,
    id: &PublicationId,
    path: BlobPath,
    size: u64,
    quotas: &QuotasConfig,
    now: i64,
) -> Result<Reservation, HubError> {
    let tx = conn.transaction()?;
    let (publication, created) = match own_publication(&tx, account, id) {
        Ok(row) => (row, false),
        Err(HubError::NotFound) => {
            let taken: Option<i64> = tx
                .query_row(
                    "SELECT account_id FROM publications WHERE public_id = ?1",
                    [id.as_bytes()],
                    |r| r.get(0),
                )
                .optional()?;
            if taken.is_some() {
                return Err(HubError::NotFound);
            }
            if quotas.max_publications > 0
                && usage(&tx, account)?.publications >= quotas.max_publications
            {
                return Err(HubError::Quota("publications"));
            }
            tx.execute(
                "INSERT INTO publications (public_id, account_id, created_at, last_opened_at)
                 VALUES (?1, ?2, ?3, ?3)",
                params![id.as_bytes(), account, now],
            )?;
            (tx.last_insert_rowid(), true)
        }
        Err(e) => return Err(e),
    };
    let path = path.to_string();
    let old_size: Option<i64> = tx
        .query_row(
            "SELECT size FROM blobs WHERE publication_id = ?1 AND path = ?2",
            params![publication, path],
            |r| r.get(0),
        )
        .optional()?;
    tx.execute(
        "INSERT INTO blobs (publication_id, path, size) VALUES (?1, ?2, ?3)
         ON CONFLICT (publication_id, path) DO UPDATE SET size = excluded.size",
        params![publication, path, size as i64],
    )?;
    if quotas.max_bytes > 0 && usage(&tx, account)?.bytes > quotas.max_bytes {
        return Err(HubError::Quota("bytes"));
    }
    tx.commit()?;
    Ok(Reservation {
        publication,
        old_size,
        created,
    })
}

impl Reservation {
    /// Whether the blob existed before (it is replaced).
    pub fn replaced(&self) -> bool {
        self.old_size.is_some()
    }
}

/// Undoes a [`reserve_blob`] whose file could not be written.
pub fn release_blob(
    conn: &Connection,
    reservation: Reservation,
    path: BlobPath,
) -> Result<(), HubError> {
    let path = path.to_string();
    match reservation.old_size {
        Some(size) => conn.execute(
            "UPDATE blobs SET size = ?3 WHERE publication_id = ?1 AND path = ?2",
            params![reservation.publication, path, size],
        )?,
        None => conn.execute(
            "DELETE FROM blobs WHERE publication_id = ?1 AND path = ?2",
            params![reservation.publication, path],
        )?,
    };
    if reservation.created {
        conn.execute(
            "DELETE FROM publications WHERE id = ?1
             AND NOT EXISTS (SELECT 1 FROM blobs WHERE publication_id = ?1)",
            [reservation.publication],
        )?;
    }
    Ok(())
}

/// Checks that `account` may read the blob `path` of publication `id` and that it
/// exists; a fetch of the manifest counts as opening the publication.
pub fn open_blob(
    conn: &Connection,
    account: i64,
    id: &PublicationId,
    path: BlobPath,
    now: i64,
) -> Result<(), HubError> {
    let publication = own_publication(conn, account, id)?;
    let exists = conn
        .query_row(
            "SELECT 1 FROM blobs WHERE publication_id = ?1 AND path = ?2",
            params![publication, path.to_string()],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        return Err(HubError::NotFound);
    }
    if path == BlobPath::Manifest {
        conn.execute(
            "UPDATE publications SET last_opened_at = ?2
             WHERE id = ?1 AND last_opened_at <= ?2 - ?3",
            params![publication, now, SEEN_INTERVAL],
        )?;
    }
    Ok(())
}

/// Deletes the publication `id` of `account` with its blob rows (the caller removes
/// the files).
pub fn delete_publication(
    conn: &Connection,
    account: i64,
    id: &PublicationId,
) -> Result<(), HubError> {
    let publication = own_publication(conn, account, id)?;
    conn.execute("DELETE FROM publications WHERE id = ?1", [publication])?;
    Ok(())
}

/// Takes the publication `id` down: deletes it (of whichever account) and refuses its
/// id from now on. Returns the internal id of its account, `None` if it did not exist
/// (the id is refused anyway).
pub fn takedown(conn: &Connection, id: &PublicationId, now: i64) -> Result<Option<i64>, HubError> {
    let account: Option<i64> = conn
        .query_row(
            "DELETE FROM publications WHERE public_id = ?1 RETURNING account_id",
            [id.as_bytes()],
            |r| r.get(0),
        )
        .optional()?;
    conn.execute(
        "INSERT OR IGNORE INTO takedowns (public_id, taken_at) VALUES (?1, ?2)",
        params![id.as_bytes(), now],
    )?;
    Ok(account)
}

/// Deletes the publications not opened for `days` days (none if `days` is 0) and
/// returns their ids (the caller removes the files).
pub fn expire(conn: &Connection, days: u32, now: i64) -> Result<Vec<PublicationId>, HubError> {
    if days == 0 {
        return Ok(Vec::new());
    }
    let cutoff = now - i64::from(days) * DAY;
    let mut stmt =
        conn.prepare("DELETE FROM publications WHERE last_opened_at <= ?1 RETURNING public_id")?;
    let ids = stmt.query_map([cutoff], |r| Ok(PublicationId::new(r.get(0)?)))?;
    Ok(ids.collect::<Result<_, _>>()?)
}

/// Applies `changes` of `account` in order, each with the next sequence number; with
/// a byte quota, the whole batch is refused if it would exceed it. Returns the
/// sequence numbers.
pub fn push_records(
    conn: &mut Connection,
    account: i64,
    changes: &[RecordChange],
    quotas: &QuotasConfig,
    now: i64,
) -> Result<Vec<i64>, HubError> {
    let tx = conn.transaction()?;
    let mut seqs = Vec::with_capacity(changes.len());
    for change in changes {
        let seq: i64 = tx.query_row(
            "UPDATE accounts SET seq = seq + 1 WHERE id = ?1 RETURNING seq",
            [account],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO records (account_id, kind, record_id, seq, data, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (account_id, kind, record_id) DO UPDATE SET
               seq = excluded.seq, data = excluded.data, updated_at = excluded.updated_at",
            params![account, change.kind, change.id, seq, change.data, now],
        )?;
        seqs.push(seq);
    }
    if quotas.max_bytes > 0 && usage(&tx, account)?.bytes > quotas.max_bytes {
        return Err(HubError::Quota("bytes"));
    }
    tx.commit()?;
    Ok(seqs)
}

/// The records of `account` changed after `since`, in sequence order, at most
/// `limit`; and whether more follow.
pub fn pull_records(
    conn: &Connection,
    account: i64,
    since: i64,
    limit: u32,
) -> Result<(Vec<Record>, bool), HubError> {
    let mut stmt = conn.prepare(
        "SELECT kind, record_id, seq, data, updated_at FROM records
         WHERE account_id = ?1 AND seq > ?2 ORDER BY seq LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![account, since, i64::from(limit) + 1], |r| {
        Ok(Record {
            kind: r.get(0)?,
            id: r.get(1)?,
            seq: r.get(2)?,
            data: r.get(3)?,
            updated_at: r.get(4)?,
        })
    })?;
    let mut records: Vec<Record> = rows.collect::<Result<_, _>>()?;
    let more = records.len() > limit as usize;
    records.truncate(limit as usize);
    Ok((records, more))
}

/// The public id of `account`.
pub fn account_id(conn: &Connection, account: i64) -> Result<AccountId, HubError> {
    let public: [u8; 16] = conn.query_row(
        "SELECT public_id FROM accounts WHERE id = ?1",
        [account],
        |r| r.get(0),
    )?;
    Ok(AccountId::new(public))
}

/// The last sequence number given out for `account`.
pub fn current_seq(conn: &Connection, account: i64) -> Result<i64, HubError> {
    Ok(
        conn.query_row("SELECT seq FROM accounts WHERE id = ?1", [account], |r| {
            r.get(0)
        })?,
    )
}

/// Every account with its usage, oldest first.
pub fn accounts(conn: &Connection) -> Result<Vec<AccountSummary>, HubError> {
    let mut stmt = conn.prepare(
        "SELECT a.id, a.created_at,
                (SELECT max(last_seen_at) FROM devices WHERE account_id = a.id)
         FROM accounts a ORDER BY a.id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let rows: Vec<(i64, i64, Option<i64>)> = rows.collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(id, created_at, last_seen_at)| {
            Ok(AccountSummary {
                id,
                created_at,
                last_seen_at,
                usage: usage(conn, id)?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_accounts() -> AccountsConfig {
        AccountsConfig {
            registration: Registration::Open,
            ..AccountsConfig::default()
        }
    }

    fn account(conn: &mut Connection) -> NewDevice {
        create_account(conn, &open_accounts(), None, &[7; 32], None, 1000).unwrap()
    }

    #[test]
    fn invites_are_single_use_and_expire() {
        let mut conn = open_in_memory();
        let accounts = AccountsConfig::default();
        let err = create_account(&mut conn, &accounts, None, &[1; 32], None, 0).unwrap_err();
        assert!(matches!(err, HubError::InviteRequired), "{err}");
        let code = create_invite(&conn, 1, 0).unwrap();
        create_account(&mut conn, &accounts, Some(&code), &[1; 32], None, 10).unwrap();
        let again = create_account(&mut conn, &accounts, Some(&code), &[1; 32], None, 20);
        assert!(matches!(again, Err(HubError::InviteRequired)));
        let late = create_invite(&conn, 1, 0).unwrap();
        let expired = create_account(&mut conn, &accounts, Some(&late), &[1; 32], None, DAY);
        assert!(matches!(expired, Err(HubError::InviteRequired)));
    }

    #[test]
    fn max_accounts_is_enforced() {
        let mut conn = open_in_memory();
        let accounts = AccountsConfig {
            max_accounts: 1,
            ..open_accounts()
        };
        create_account(&mut conn, &accounts, None, &[1; 32], None, 0).unwrap();
        let err = create_account(&mut conn, &accounts, None, &[2; 32], None, 0).unwrap_err();
        assert!(matches!(err, HubError::Quota("accounts")), "{err}");
    }

    #[test]
    fn tokens_are_stored_hashed() {
        let mut conn = open_in_memory();
        let device = account(&mut conn);
        let stored: Vec<u8> = conn
            .query_row("SELECT token_hash FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_ne!(stored, device.token.as_str().as_bytes());
        assert_eq!(stored, device.token.hash());
    }

    #[test]
    fn last_seen_is_written_at_most_hourly() {
        let mut conn = open_in_memory();
        let device = account(&mut conn);
        let seen = |conn: &Connection| -> i64 {
            conn.query_row("SELECT last_seen_at FROM devices", [], |r| r.get(0))
                .unwrap()
        };
        device_by_token(&conn, &device.token, 1000 + SEEN_INTERVAL - 1).unwrap();
        assert_eq!(seen(&conn), 1000);
        device_by_token(&conn, &device.token, 1000 + SEEN_INTERVAL).unwrap();
        assert_eq!(seen(&conn), 1000 + SEEN_INTERVAL);
    }

    #[test]
    fn a_failed_upload_releases_its_reservation() {
        let mut conn = open_in_memory();
        let device = account(&mut conn);
        let id = PublicationId::new([3; 16]);
        let quotas = QuotasConfig::default();
        let first = reserve_blob(
            &mut conn,
            device.account,
            &id,
            BlobPath::Part(0),
            10,
            &quotas,
            0,
        )
        .unwrap();
        assert!(first.created);
        // A replacement that fails restores the old size; a new blob that fails is
        // dropped together with the publication it created.
        let second = reserve_blob(
            &mut conn,
            device.account,
            &id,
            BlobPath::Part(0),
            99,
            &quotas,
            0,
        )
        .unwrap();
        release_blob(&conn, second, BlobPath::Part(0)).unwrap();
        assert_eq!(usage(&conn, device.account).unwrap().bytes, 10);
        release_blob(&conn, first, BlobPath::Part(0)).unwrap();
        assert_eq!(usage(&conn, device.account).unwrap().publications, 0);
    }

    #[test]
    fn deleted_records_stay_as_tombstones_with_a_new_seq() {
        let mut conn = open_in_memory();
        let device = account(&mut conn);
        let quotas = QuotasConfig::default();
        let change = |data: Option<&[u8]>| RecordChange {
            kind: "annotation".into(),
            id: "a1".into(),
            data: data.map(<[u8]>::to_vec),
        };
        let seqs = push_records(
            &mut conn,
            device.account,
            &[change(Some(b"x")), change(None)],
            &quotas,
            5,
        )
        .unwrap();
        assert_eq!(seqs, [1, 2]);
        let (records, more) = pull_records(&conn, device.account, 0, 10).unwrap();
        assert!(!more);
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].seq, records[0].data.as_ref()), (2, None));
    }
}
