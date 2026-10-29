//! The hub database, `<data>/hub.sqlite`: libraries with their accounts, devices,
//! pairings and member invites, hub invite codes, publications and their blobs, sync
//! records.
//!
//! Used through one connection behind a mutex (like the user data of an instance).
//! Functions take a [`Connection`] and the current time (Unix seconds), so tests set
//! the clock. Secrets (library and device tokens, pairing and invite secrets, hub
//! invite codes) and login keys are stored as their SHA-256 only. Every lookup by token
//! is bound to the token's library or account: an id of another library behaves as
//! "not found".

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::config::{LibrariesConfig, PasswordsConfig, QuotasConfig, Registration};
use super::{AccountId, HubError, Secret};
use crate::publication::format::{BlobPath, PublicationId};
use crate::storage::migrations;

/// Schema migrations of the hub database; index `i` upgrades version `i` to `i + 1`.
/// Append only once a hub has been released.
pub(crate) const MIGRATIONS: &[&str] = &[V1, V2, V3, V4];

/// The schema version of 0.20, the last with per-profile accounts.
const ACCOUNTS_SCHEMA: u32 = 3;

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

/// Version 2: an account's own quotas (`shelfgauge hub quota`); NULL = the hub's value
/// from `[quotas]`, 0 = no limit.
const V2: &str = r#"
ALTER TABLE accounts ADD COLUMN max_bytes INTEGER;
ALTER TABLE accounts ADD COLUMN max_publications INTEGER;
ALTER TABLE accounts ADD COLUMN max_devices INTEGER;
"#;

/// Version 3: the bytes of all blobs and records in one row, `hub_bytes`, kept by
/// triggers (foreign key cascades fire them too), so `max_total_bytes` is checked
/// without summing every row (440 ms for 2 million records, `.debug/perf-hub.md`).
const V3: &str = r#"
CREATE TABLE hub_bytes (
    id    INTEGER PRIMARY KEY CHECK (id = 1),
    bytes INTEGER NOT NULL
);

INSERT INTO hub_bytes (id, bytes) VALUES (1,
    coalesce((SELECT sum(size) FROM blobs), 0)
  + coalesce((SELECT sum(length(data)) FROM records), 0));

CREATE TRIGGER blobs_added AFTER INSERT ON blobs BEGIN
    UPDATE hub_bytes SET bytes = bytes + NEW.size WHERE id = 1;
END;

CREATE TRIGGER blobs_resized AFTER UPDATE OF size ON blobs BEGIN
    UPDATE hub_bytes SET bytes = bytes - OLD.size + NEW.size WHERE id = 1;
END;

CREATE TRIGGER blobs_removed AFTER DELETE ON blobs BEGIN
    UPDATE hub_bytes SET bytes = bytes - OLD.size WHERE id = 1;
END;

CREATE TRIGGER records_added AFTER INSERT ON records BEGIN
    UPDATE hub_bytes SET bytes = bytes + coalesce(length(NEW.data), 0) WHERE id = 1;
END;

CREATE TRIGGER records_changed AFTER UPDATE OF data ON records BEGIN
    UPDATE hub_bytes
    SET bytes = bytes - coalesce(length(OLD.data), 0) + coalesce(length(NEW.data), 0)
    WHERE id = 1;
END;

CREATE TRIGGER records_removed AFTER DELETE ON records BEGIN
    UPDATE hub_bytes SET bytes = bytes - coalesce(length(OLD.data), 0) WHERE id = 1;
END;
"#;

/// Version 4 (0.21): libraries with accounts `name@handle` replace the per-profile
/// accounts, which are not converted (a database that holds any is moved only by
/// `shelfgauge hub migrate`, [`old_accounts`]).
///
/// - A library has a token (`token_hash`) of its own; its `owner` account (one per
///   library) is the library's, `reader` accounts are its members. `last_seen_at` of a
///   library is its token's, of an account its devices'.
/// - An account's password is the SHA-256 of the login key derived in the browser, the
///   salt and Argon2id parameters and the key bundle sealed under the wrapping key; all
///   NULL for an owner without a password. `password_changed_*` tell the account's
///   devices of the last change (`via`: `device`, `reset` or `library`).
/// - `member_invites` with an `account_id` reset that account's password, without one
///   create an account; `payload` is sealed by the library under the invite's one-time
///   key. `public_id` is kept by the account it created (`invite_id`).
/// - `hub_invites` are the operator's codes for creating a library (`invites` before).
/// - `hub_secret.key` makes the stable fake salts of unknown addresses.
/// - Records, blobs and the byte counter keep the rules of versions 1 and 3;
///   `takedowns` stays.
const V4: &str = r#"
DROP TABLE records;
DROP TABLE blobs;
DROP TABLE publications;
DROP TABLE pairings;
DROP TABLE devices;
DROP TABLE invites;
DROP TABLE accounts;
UPDATE hub_bytes SET bytes = 0 WHERE id = 1;

CREATE TABLE hub_secret (
    id  INTEGER PRIMARY KEY CHECK (id = 1),
    key BLOB    NOT NULL
);

INSERT INTO hub_secret (id, key) VALUES (1, randomblob(32));

CREATE TABLE hub_invites (
    code_hash  BLOB    PRIMARY KEY,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);

CREATE TABLE libraries (
    id               INTEGER PRIMARY KEY,
    public_id        BLOB    NOT NULL UNIQUE,
    handle           TEXT    NOT NULL UNIQUE,
    token_hash       BLOB    NOT NULL UNIQUE,
    created_at       INTEGER NOT NULL,
    last_seen_at     INTEGER NOT NULL,
    max_bytes        INTEGER,
    max_publications INTEGER
);

CREATE TABLE accounts (
    id                      INTEGER PRIMARY KEY,
    library_id              INTEGER NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    public_id               BLOB    NOT NULL UNIQUE,
    name                    TEXT    NOT NULL,
    role                    TEXT    NOT NULL CHECK (role IN ('owner', 'reader')),
    login_hash              BLOB,
    salt                    BLOB,
    kdf_memory              INTEGER,
    kdf_iterations          INTEGER,
    bundle                  BLOB,
    invite_id               BLOB,
    password_changed_at     INTEGER,
    password_changed_via    TEXT,
    password_changed_device INTEGER,
    seq                     INTEGER NOT NULL DEFAULT 0,
    created_at              INTEGER NOT NULL,
    last_seen_at            INTEGER NOT NULL,
    UNIQUE (library_id, name)
);

CREATE UNIQUE INDEX accounts_owner ON accounts (library_id) WHERE role = 'owner';

CREATE TABLE devices (
    id           INTEGER PRIMARY KEY,
    account_id   INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    token_hash   BLOB    NOT NULL UNIQUE,
    name         BLOB,
    created_at   INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL
);

CREATE INDEX devices_account ON devices (account_id);

CREATE TABLE pairings (
    secret_hash BLOB    PRIMARY KEY,
    account_id  INTEGER NOT NULL REFERENCES accounts (id) ON DELETE CASCADE,
    expires_at  INTEGER NOT NULL
);

CREATE TABLE member_invites (
    id          INTEGER PRIMARY KEY,
    public_id   BLOB    NOT NULL UNIQUE,
    library_id  INTEGER NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    secret_hash BLOB    NOT NULL UNIQUE,
    account_id  INTEGER REFERENCES accounts (id) ON DELETE CASCADE,
    payload     BLOB    NOT NULL,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL
);

CREATE INDEX member_invites_library ON member_invites (library_id);

CREATE TABLE publications (
    id             INTEGER PRIMARY KEY,
    public_id      BLOB    NOT NULL UNIQUE,
    library_id     INTEGER NOT NULL REFERENCES libraries (id) ON DELETE CASCADE,
    created_at     INTEGER NOT NULL,
    last_opened_at INTEGER NOT NULL
);

CREATE INDEX publications_library ON publications (library_id);

CREATE TABLE blobs (
    publication_id INTEGER NOT NULL REFERENCES publications (id) ON DELETE CASCADE,
    path           TEXT    NOT NULL,
    size           INTEGER NOT NULL,
    PRIMARY KEY (publication_id, path)
) WITHOUT ROWID;

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

CREATE TRIGGER blobs_added AFTER INSERT ON blobs BEGIN
    UPDATE hub_bytes SET bytes = bytes + NEW.size WHERE id = 1;
END;

CREATE TRIGGER blobs_resized AFTER UPDATE OF size ON blobs BEGIN
    UPDATE hub_bytes SET bytes = bytes - OLD.size + NEW.size WHERE id = 1;
END;

CREATE TRIGGER blobs_removed AFTER DELETE ON blobs BEGIN
    UPDATE hub_bytes SET bytes = bytes - OLD.size WHERE id = 1;
END;

CREATE TRIGGER records_added AFTER INSERT ON records BEGIN
    UPDATE hub_bytes SET bytes = bytes + coalesce(length(NEW.data), 0) WHERE id = 1;
END;

CREATE TRIGGER records_changed AFTER UPDATE OF data ON records BEGIN
    UPDATE hub_bytes
    SET bytes = bytes - coalesce(length(OLD.data), 0) + coalesce(length(NEW.data), 0)
    WHERE id = 1;
END;

CREATE TRIGGER records_removed AFTER DELETE ON records BEGIN
    UPDATE hub_bytes SET bytes = bytes - coalesce(length(OLD.data), 0) WHERE id = 1;
END;
"#;

/// `last_seen_at` and `last_opened_at` are written at most this often (seconds).
const SEEN_INTERVAL: i64 = 3600;

/// How long a pairing secret is valid (seconds).
pub const PAIRING_TTL: i64 = 600;

/// The most accounts of a library, the owner's included (the profile limit of a
/// library); pending invites of new people count too.
pub const MAX_LIBRARY_ACCOUNTS: u64 = 16;

/// The length of a password salt in bytes.
pub const SALT_LEN: usize = shelfgauge_core::password::SALT_LEN;

/// Seconds per day.
const DAY: i64 = 86_400;

/// Opens (creating and migrating if needed) the hub database at `path`; refuses a
/// database of 0.18–0.20 that still holds accounts ([`HubError::OldAccounts`]).
pub(crate) fn open(path: &std::path::Path) -> Result<Connection, HubError> {
    let mut conn = crate::storage::open_wal(path)?;
    if let Some((accounts, publications)) = old_accounts(&conn)? {
        return Err(HubError::OldAccounts {
            accounts,
            publications,
        });
    }
    migrations::apply(&mut conn, MIGRATIONS)?;
    Ok(conn)
}

/// Opens the database at `path` and moves it to the current schema even if it holds
/// accounts of 0.18–0.20, which are deleted with their devices, publications and
/// records (`takedowns` stay); returns how many accounts and publications it held.
/// The caller removes the blob files.
pub(crate) fn migrate_old(path: &std::path::Path) -> Result<(u64, u64), HubError> {
    let mut conn = crate::storage::open_wal(path)?;
    let old = old_accounts(&conn)?.unwrap_or_default();
    migrations::apply(&mut conn, MIGRATIONS)?;
    Ok(old)
}

/// The numbers of accounts and publications of a database still on the schema of
/// 0.18–0.20, `None` if it is on another schema or holds no account (an empty one
/// migrates without asking).
fn old_accounts(conn: &Connection) -> Result<Option<(u64, u64)>, HubError> {
    let version: u32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    if version == 0 || version > ACCOUNTS_SCHEMA {
        return Ok(None);
    }
    let (accounts, publications): (i64, i64) = conn.query_row(
        "SELECT (SELECT count(*) FROM accounts), (SELECT count(*) FROM publications)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((accounts > 0).then_some((accounts as u64, publications as u64)))
}

/// Writes a database on the schema of 0.20 with one account, one publication with a
/// blob row and one takedown at `path` (for tests).
#[cfg(test)]
pub(crate) fn write_old_database(path: &std::path::Path) {
    let mut conn = crate::storage::open_wal(path).unwrap();
    migrations::apply(&mut conn, &MIGRATIONS[..3]).unwrap();
    conn.execute_batch(
        "INSERT INTO accounts (public_id, recovery_hash, created_at)
           VALUES (x'00112233445566778899aabbccddeeff', x'00', 0);
         INSERT INTO publications (public_id, account_id, created_at, last_opened_at)
           VALUES (x'ffeeddccbbaa99887766554433221100', 1, 0, 0);
         INSERT INTO blobs (publication_id, path, size) VALUES (1, 'manifest', 9);
         INSERT INTO takedowns (public_id, taken_at)
           VALUES (x'0123456789abcdef0123456789abcdef', 0);",
    )
    .unwrap();
}

/// A migrated in-memory database (for tests).
#[cfg(test)]
pub(crate) fn open_in_memory() -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::storage::configure(&conn).unwrap();
    migrations::apply(&mut conn, MIGRATIONS).unwrap();
    conn
}

/// The role of an account in its library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The library's own account (the owner profile's).
    Owner,
    /// A member invited by an admin.
    Reader,
}

impl Role {
    /// The role as stored and sent: `owner` or `reader`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Reader => "reader",
        }
    }

    fn parse(text: &str) -> Self {
        if text == "owner" {
            Self::Owner
        } else {
            Self::Reader
        }
    }
}

/// The device a token belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeviceAuth {
    /// The internal library id.
    pub library: i64,
    /// The internal account id.
    pub account: i64,
    /// The device id (unique on the hub).
    pub device: i64,
    /// The account's role.
    pub role: Role,
}

/// What a bearer token authenticates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// A device of an account.
    Device(DeviceAuth),
    /// A library's own token (the internal library id).
    Library(i64),
}

impl Auth {
    /// The internal id of the library.
    pub fn library(&self) -> i64 {
        match self {
            Self::Device(device) => device.library,
            Self::Library(library) => *library,
        }
    }
}

/// A device token just issued.
#[derive(Debug)]
pub struct NewDevice {
    /// The internal library id.
    pub library: i64,
    /// The internal account id.
    pub account: i64,
    /// The public account id.
    pub account_id: AccountId,
    /// The public library id.
    pub library_id: AccountId,
    /// The device id.
    pub device: i64,
    /// The token (shown once, stored hashed).
    pub token: Secret,
}

/// A library just created with its owner account.
#[derive(Debug)]
pub struct NewLibrary {
    /// The internal library id.
    pub library: i64,
    /// The public library id.
    pub library_id: AccountId,
    /// The library's token (shown once, stored hashed).
    pub token: Secret,
    /// The internal id of the owner account.
    pub owner: i64,
    /// The public id of the owner account.
    pub owner_id: AccountId,
}

/// A device as the device list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// The device id.
    pub id: i64,
    /// The name, encrypted by the client with the account key.
    pub name: Option<Vec<u8>>,
    /// When it was added.
    pub created_at: i64,
    /// When it last made a request (to the hour).
    pub last_seen_at: i64,
}

/// The Argon2id parameters of a password (one lane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Kdf {
    /// Memory in KiB.
    pub memory_kib: u32,
    /// Iterations.
    pub iterations: u32,
}

impl From<&PasswordsConfig> for Kdf {
    fn from(config: &PasswordsConfig) -> Self {
        Self {
            memory_kib: config.memory_kib,
            iterations: config.iterations,
        }
    }
}

/// A password as the hub keeps it: what the browser derived from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Password {
    /// The SHA-256 of the login key.
    pub login_hash: [u8; 32],
    /// The salt ([`SALT_LEN`] bytes).
    pub salt: Vec<u8>,
    /// The parameters it was derived with.
    pub kdf: Kdf,
    /// The account's key bundle sealed under the wrapping key.
    pub bundle: Vec<u8>,
}

/// What `prelogin` answers: the salt and parameters to derive the login key with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prelogin {
    /// The salt ([`SALT_LEN`] bytes; a stable fake one for an unknown address).
    pub salt: Vec<u8>,
    /// The parameters.
    pub kdf: Kdf,
}

/// The last password change of an account, told to its devices on sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordChange {
    /// When.
    pub at: i64,
    /// How: `device` (by a signed-in device), `reset` (a link from the library) or
    /// `library` (the owner's password set on the library's computer).
    pub via: String,
    /// The device that changed it (for `device`).
    pub device: Option<i64>,
}

/// What a library stores.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Bytes of its blobs and of its accounts' records.
    pub bytes: u64,
    /// Publications.
    pub publications: u64,
    /// Accounts, the owner's included.
    pub accounts: u64,
    /// Devices of its accounts.
    pub devices: u64,
    /// Records of its accounts, tombstones included.
    pub records: u64,
}

/// A publication of a library.
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
    /// The record's kind (`shelf_item`, `annotation`, `position`, `library_key`,
    /// `profile`).
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

/// A library's own quotas; `None` takes the hub's value, `Some(0)` is no limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LibraryQuotas {
    /// Stored bytes.
    pub max_bytes: Option<u64>,
    /// Publications.
    pub max_publications: Option<u64>,
}

impl LibraryQuotas {
    /// The quotas in effect: these values over the hub's `defaults`.
    pub fn over(&self, defaults: &QuotasConfig) -> QuotasConfig {
        QuotasConfig {
            max_bytes: self.max_bytes.unwrap_or(defaults.max_bytes),
            max_publications: self.max_publications.unwrap_or(defaults.max_publications),
            max_devices: defaults.max_devices,
        }
    }

    /// Whether no value is set (the library uses the hub's quotas).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A library as `shelfgauge hub libraries` lists it (no handle, names or secrets).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibrarySummary {
    /// The internal id.
    pub id: i64,
    /// When it was created.
    pub created_at: i64,
    /// When its token or a device of its accounts last made a request.
    pub last_seen_at: i64,
    /// What it stores.
    pub usage: Usage,
    /// Its own quotas.
    pub quotas: LibraryQuotas,
}

/// An account as its library lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    /// The public id.
    pub id: AccountId,
    /// The name part of the address.
    pub name: String,
    /// The role.
    pub role: Role,
    /// Whether it has a password.
    pub has_password: bool,
    /// Its devices.
    pub devices: u64,
    /// When one of its devices last made a request (to the hour).
    pub last_seen_at: Option<i64>,
    /// When it was created.
    pub created_at: i64,
    /// The member invite that created it.
    pub invite_id: Option<AccountId>,
}

/// What a member invite shows before it is spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitePreview {
    /// The library's handle.
    pub handle: String,
    /// The name of the account whose password it resets; `None` for a new account.
    pub name: Option<String>,
    /// The library's payload, sealed under the invite's one-time key.
    pub payload: Vec<u8>,
    /// When it expires.
    pub expires_at: i64,
}

/// What redeeming a member invite sends.
#[derive(Debug, Clone)]
pub struct Redeem<'a> {
    /// The chosen name (normalized) for a new account; ignored for a reset.
    pub name: Option<&'a str>,
    /// The new password.
    pub password: &'a Password,
    /// The encrypted device name.
    pub device_name: Option<&'a [u8]>,
    /// A device of the account to remove first (at the device limit).
    pub remove_device: Option<i64>,
}

/// Whether two hashes are equal, in constant time.
fn same_hash(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The SHA-256 of a login key.
pub fn login_hash(login_key: &[u8]) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(login_key).into()
}

/// Stores a new hub invite code valid for `days`.
pub fn create_hub_invite(conn: &Connection, days: u32, now: i64) -> Result<Secret, HubError> {
    let code = Secret::generate()?;
    conn.execute("DELETE FROM hub_invites WHERE expires_at <= ?1", [now])?;
    conn.execute(
        "INSERT INTO hub_invites (code_hash, created_at, expires_at) VALUES (?1, ?2, ?3)",
        params![code.hash(), now, now + i64::from(days) * DAY],
    )?;
    Ok(code)
}

/// Creates a library with the `handle` and its owner account `owner_name` (both
/// normalized): checks the registration rule (a hub invite code is used up) and
/// `max_libraries`.
pub fn create_library(
    conn: &mut Connection,
    config: &LibrariesConfig,
    invite: Option<&Secret>,
    handle: &str,
    owner_name: &str,
    now: i64,
) -> Result<NewLibrary, HubError> {
    let tx = conn.transaction()?;
    if config.registration == Registration::Invite {
        let used = match invite {
            Some(code) => tx.execute(
                "DELETE FROM hub_invites WHERE code_hash = ?1 AND expires_at > ?2",
                params![code.hash(), now],
            )?,
            None => 0,
        };
        if used == 0 {
            return Err(HubError::InviteRequired);
        }
    }
    if config.max_libraries > 0 {
        let count: i64 = tx.query_row("SELECT count(*) FROM libraries", [], |r| r.get(0))?;
        if count as u64 >= config.max_libraries {
            return Err(HubError::Quota("libraries"));
        }
    }
    if handle_taken(&tx, handle)? {
        return Err(HubError::HandleTaken);
    }
    let library_id = AccountId::generate()?;
    let token = Secret::generate()?;
    tx.execute(
        "INSERT INTO libraries (public_id, handle, token_hash, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?4)",
        params![library_id.as_bytes(), handle, token.hash(), now],
    )?;
    let library = tx.last_insert_rowid();
    let owner_id = AccountId::generate()?;
    tx.execute(
        "INSERT INTO accounts (library_id, public_id, name, role, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, 'owner', ?4, ?4)",
        params![library, owner_id.as_bytes(), owner_name, now],
    )?;
    let owner = tx.last_insert_rowid();
    tx.commit()?;
    Ok(NewLibrary {
        library,
        library_id,
        token,
        owner,
        owner_id,
    })
}

fn handle_taken(conn: &Connection, handle: &str) -> Result<bool, HubError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM libraries WHERE handle = ?1",
            [handle],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Changes the handle of `library` (normalized); the old one is free at once.
pub fn set_handle(conn: &Connection, library: i64, handle: &str) -> Result<(), HubError> {
    let current: String = conn.query_row(
        "SELECT handle FROM libraries WHERE id = ?1",
        [library],
        |r| r.get(0),
    )?;
    if current == handle {
        return Ok(());
    }
    if handle_taken(conn, handle)? {
        return Err(HubError::HandleTaken);
    }
    conn.execute(
        "UPDATE libraries SET handle = ?2 WHERE id = ?1",
        params![library, handle],
    )?;
    Ok(())
}

/// What a bearer token authenticates, if anything: a device (its account and library
/// seen) or a library (its token seen), at most hourly.
pub fn authenticate(conn: &Connection, token: &Secret, now: i64) -> Result<Option<Auth>, HubError> {
    let hash = token.hash();
    let device: Option<(i64, i64, i64, i64, String, i64)> = conn
        .query_row(
            "SELECT d.id, d.account_id, d.last_seen_at, a.library_id, a.role, a.last_seen_at
             FROM devices d JOIN accounts a ON a.id = d.account_id
             WHERE d.token_hash = ?1",
            [hash],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .optional()?;
    if let Some((device, account, seen, library, role, account_seen)) = device {
        if now - seen >= SEEN_INTERVAL {
            conn.execute(
                "UPDATE devices SET last_seen_at = ?2 WHERE id = ?1",
                params![device, now],
            )?;
        }
        if now - account_seen >= SEEN_INTERVAL {
            conn.execute(
                "UPDATE accounts SET last_seen_at = ?2 WHERE id = ?1",
                params![account, now],
            )?;
        }
        return Ok(Some(Auth::Device(DeviceAuth {
            library,
            account,
            device,
            role: Role::parse(&role),
        })));
    }
    let library: Option<(i64, i64)> = conn
        .query_row(
            "SELECT id, last_seen_at FROM libraries WHERE token_hash = ?1",
            [hash],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((library, seen)) = library else {
        return Ok(None);
    };
    if now - seen >= SEEN_INTERVAL {
        conn.execute(
            "UPDATE libraries SET last_seen_at = ?2 WHERE id = ?1",
            params![library, now],
        )?;
    }
    Ok(Some(Auth::Library(library)))
}

/// The public id and handle of `library`.
pub fn library_ids(conn: &Connection, library: i64) -> Result<(AccountId, String), HubError> {
    let (id, handle): ([u8; 16], String) = conn.query_row(
        "SELECT public_id, handle FROM libraries WHERE id = ?1",
        [library],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((AccountId::new(id), handle))
}

/// The public id, name and role of `account`.
pub fn account_ids(conn: &Connection, account: i64) -> Result<(AccountId, String, Role), HubError> {
    let (id, name, role): ([u8; 16], String, String) = conn.query_row(
        "SELECT public_id, name, role FROM accounts WHERE id = ?1",
        [account],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok((AccountId::new(id), name, Role::parse(&role)))
}

/// The internal id of the account `id` of `library`; `NotFound` for an account of
/// another library.
pub fn library_account(conn: &Connection, library: i64, id: &AccountId) -> Result<i64, HubError> {
    conn.query_row(
        "SELECT id FROM accounts WHERE public_id = ?1 AND library_id = ?2",
        params![id.as_bytes(), library],
        |r| r.get(0),
    )
    .optional()?
    .ok_or(HubError::NotFound)
}

/// The internal id of the owner account of `library`.
pub fn owner_account(conn: &Connection, library: i64) -> Result<i64, HubError> {
    Ok(conn.query_row(
        "SELECT id FROM accounts WHERE library_id = ?1 AND role = 'owner'",
        [library],
        |r| r.get(0),
    )?)
}

/// The accounts of `library`, the owner first, then by age.
pub fn members(conn: &Connection, library: i64) -> Result<Vec<MemberInfo>, HubError> {
    let mut stmt = conn.prepare(
        "SELECT a.public_id, a.name, a.role, a.login_hash IS NOT NULL,
                (SELECT count(*) FROM devices WHERE account_id = a.id),
                (SELECT max(last_seen_at) FROM devices WHERE account_id = a.id),
                a.created_at, a.invite_id
         FROM accounts a WHERE a.library_id = ?1
         ORDER BY a.role = 'reader', a.id",
    )?;
    let rows = stmt.query_map([library], |r| {
        Ok(MemberInfo {
            id: AccountId::new(r.get(0)?),
            name: r.get(1)?,
            role: Role::parse(&r.get::<_, String>(2)?),
            has_password: r.get(3)?,
            devices: r.get::<_, i64>(4)? as u64,
            last_seen_at: r.get(5)?,
            created_at: r.get(6)?,
            invite_id: r.get::<_, Option<[u8; 16]>>(7)?.map(AccountId::new),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Deletes the reader account `account` with its devices and records; the owner
/// account ends only with its library.
pub fn delete_account(conn: &Connection, account: i64) -> Result<(), HubError> {
    let (_, _, role) = account_ids(conn, account)?;
    if role == Role::Owner {
        return Err(HubError::Forbidden(
            "the owner account ends only with its library",
        ));
    }
    conn.execute("DELETE FROM accounts WHERE id = ?1", [account])?;
    Ok(())
}

/// Removes every device of `account`; returns how many.
pub fn remove_all_devices(conn: &Connection, account: i64) -> Result<usize, HubError> {
    Ok(conn.execute("DELETE FROM devices WHERE account_id = ?1", [account])?)
}

/// Stores a member invite of `library`: for a new account (`account` is `None`;
/// refused when the accounts and pending invites of new people reach
/// [`MAX_LIBRARY_ACCOUNTS`]) or a password reset of the reader account `account`.
/// Returns the secret, the invite's public id and its expiry.
pub fn create_member_invite(
    conn: &Connection,
    library: i64,
    account: Option<i64>,
    payload: &[u8],
    days: u32,
    now: i64,
) -> Result<(Secret, AccountId, i64), HubError> {
    conn.execute("DELETE FROM member_invites WHERE expires_at <= ?1", [now])?;
    match account {
        None => {
            let taken: i64 = conn.query_row(
                "SELECT (SELECT count(*) FROM accounts WHERE library_id = ?1)
                      + (SELECT count(*) FROM member_invites
                         WHERE library_id = ?1 AND account_id IS NULL)",
                [library],
                |r| r.get(0),
            )?;
            if taken as u64 >= MAX_LIBRARY_ACCOUNTS {
                return Err(HubError::Quota("accounts"));
            }
        }
        Some(account) => {
            if account_ids(conn, account)?.2 == Role::Owner {
                return Err(HubError::Forbidden(
                    "the owner's password is set by the library",
                ));
            }
        }
    }
    let secret = Secret::generate()?;
    let id = AccountId::generate()?;
    let expires_at = now + i64::from(days) * DAY;
    conn.execute(
        "INSERT INTO member_invites
           (public_id, library_id, secret_hash, account_id, payload, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id.as_bytes(),
            library,
            secret.hash(),
            account,
            payload,
            now,
            expires_at
        ],
    )?;
    Ok((secret, id, expires_at))
}

/// Deletes the pending member invite `id` of `library`; returns whether it existed.
pub fn delete_member_invite(
    conn: &Connection,
    library: i64,
    id: &AccountId,
) -> Result<bool, HubError> {
    Ok(conn.execute(
        "DELETE FROM member_invites WHERE public_id = ?1 AND library_id = ?2",
        params![id.as_bytes(), library],
    )? > 0)
}

/// What the member invite `secret` shows, without spending it.
pub fn preview_invite(
    conn: &Connection,
    secret: &Secret,
    now: i64,
) -> Result<InvitePreview, HubError> {
    conn.query_row(
        "SELECT l.handle, a.name, i.payload, i.expires_at
         FROM member_invites i JOIN libraries l ON l.id = i.library_id
         LEFT JOIN accounts a ON a.id = i.account_id
         WHERE i.secret_hash = ?1 AND i.expires_at > ?2",
        params![secret.hash(), now],
        |r| {
            Ok(InvitePreview {
                handle: r.get(0)?,
                name: r.get(1)?,
                payload: r.get(2)?,
                expires_at: r.get(3)?,
            })
        },
    )
    .optional()?
    .ok_or(HubError::InvalidSecret)
}

fn write_password(
    conn: &Connection,
    account: i64,
    password: &Password,
    via: Option<(&str, Option<i64>)>,
    now: i64,
) -> Result<(), HubError> {
    let (via, device) = via.unzip();
    let at = via.map(|_| now);
    conn.execute(
        "UPDATE accounts SET login_hash = ?2, salt = ?3, kdf_memory = ?4, kdf_iterations = ?5,
           bundle = ?6,
           password_changed_at = coalesce(?7, password_changed_at),
           password_changed_via = coalesce(?8, password_changed_via),
           password_changed_device = CASE WHEN ?7 IS NULL THEN password_changed_device
                                          ELSE ?9 END
         WHERE id = ?1",
        params![
            account,
            password.login_hash,
            password.salt,
            password.kdf.memory_kib,
            password.kdf.iterations,
            password.bundle,
            at,
            via,
            device.flatten()
        ],
    )?;
    Ok(())
}

/// Spends the member invite `secret`: creates the reader account `redeem.name` with
/// the password (a new person; `NameTaken` leaves the invite unspent) or sets the new
/// password of the account it resets; then adds the device (see [`add_device`]).
pub fn redeem_invite(
    conn: &mut Connection,
    secret: &Secret,
    redeem: &Redeem<'_>,
    max_devices: u64,
    now: i64,
) -> Result<NewDevice, HubError> {
    let tx = conn.transaction()?;
    let found: Option<(i64, [u8; 16], i64, Option<i64>)> = tx
        .query_row(
            "DELETE FROM member_invites WHERE secret_hash = ?1 AND expires_at > ?2
             RETURNING id, public_id, library_id, account_id",
            params![secret.hash(), now],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((_, invite_id, library, existing)) = found else {
        return Err(HubError::InvalidSecret);
    };
    let account = match existing {
        Some(account) => {
            write_password(&tx, account, redeem.password, Some(("reset", None)), now)?;
            account
        }
        None => {
            let name = redeem
                .name
                .ok_or(HubError::BadRequest("a name is required"))?;
            if name_taken(&tx, library, name)? {
                return Err(HubError::NameTaken);
            }
            let id = AccountId::generate()?;
            tx.execute(
                "INSERT INTO accounts
                   (library_id, public_id, name, role, invite_id, created_at, last_seen_at)
                 VALUES (?1, ?2, ?3, 'reader', ?4, ?5, ?5)",
                params![library, id.as_bytes(), name, invite_id, now],
            )?;
            let account = tx.last_insert_rowid();
            write_password(&tx, account, redeem.password, None, now)?;
            account
        }
    };
    let device = add_device(
        &tx,
        account,
        max_devices,
        redeem.device_name,
        redeem.remove_device,
        now,
    )?;
    tx.commit()?;
    Ok(device)
}

fn name_taken(conn: &Connection, library: i64, name: &str) -> Result<bool, HubError> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM accounts WHERE library_id = ?1 AND name = ?2",
            params![library, name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// The hub's secret for fake salts.
fn hub_key(conn: &Connection) -> Result<Vec<u8>, HubError> {
    Ok(conn.query_row("SELECT key FROM hub_secret WHERE id = 1", [], |r| r.get(0))?)
}

/// The salt and parameters of the account `name@handle` (normalized). An unknown
/// address and an account without a password get a fake salt, the same on every
/// call, with the current `defaults`.
pub fn prelogin(
    conn: &Connection,
    name: &str,
    handle: &str,
    defaults: &PasswordsConfig,
) -> Result<Prelogin, HubError> {
    let found: Option<(Vec<u8>, u32, u32)> = conn
        .query_row(
            "SELECT a.salt, a.kdf_memory, a.kdf_iterations
             FROM accounts a JOIN libraries l ON l.id = a.library_id
             WHERE l.handle = ?1 AND a.name = ?2 AND a.login_hash IS NOT NULL",
            params![handle, name],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    if let Some((salt, memory_kib, iterations)) = found {
        return Ok(Prelogin {
            salt,
            kdf: Kdf {
                memory_kib,
                iterations,
            },
        });
    }
    use hmac::{Hmac, Mac as _};
    let mut mac = <Hmac<sha2::Sha256> as hmac::KeyInit>::new_from_slice(&hub_key(conn)?)
        .map_err(|_| HubError::BadRequest("invalid hub secret"))?;
    mac.update(b"shelfgauge fake salt\0");
    mac.update(format!("{name}@{handle}").as_bytes());
    let salt = mac.finalize().into_bytes()[..SALT_LEN].to_vec();
    Ok(Prelogin {
        salt,
        kdf: defaults.into(),
    })
}

/// Signs in to `name@handle` (normalized) with the login key and adds the device (see
/// [`add_device`]). A wrong key, an unknown address and an account without a password
/// are all `InvalidLogin`.
#[allow(clippy::too_many_arguments)]
pub fn login(
    conn: &mut Connection,
    name: &str,
    handle: &str,
    login_key: &[u8],
    device_name: Option<&[u8]>,
    remove_device: Option<i64>,
    max_devices: u64,
    now: i64,
) -> Result<NewDevice, HubError> {
    let tx = conn.transaction()?;
    let found: Option<(i64, Vec<u8>)> = tx
        .query_row(
            "SELECT a.id, a.login_hash FROM accounts a JOIN libraries l ON l.id = a.library_id
             WHERE l.handle = ?1 AND a.name = ?2 AND a.login_hash IS NOT NULL",
            params![handle, name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let hash = login_hash(login_key);
    let account = match found {
        Some((account, stored)) if same_hash(&stored, &hash) => account,
        _ => return Err(HubError::InvalidLogin),
    };
    let device = add_device(&tx, account, max_devices, device_name, remove_device, now)?;
    tx.commit()?;
    Ok(device)
}

/// `Ok` if `login_key` is the current password's of `account`, else `InvalidLogin`
/// (also for an account without a password).
pub fn check_password(conn: &Connection, account: i64, login_key: &[u8]) -> Result<(), HubError> {
    let stored: Option<Vec<u8>> = conn.query_row(
        "SELECT login_hash FROM accounts WHERE id = ?1",
        [account],
        |r| r.get(0),
    )?;
    match stored {
        Some(stored) if same_hash(&stored, &login_hash(login_key)) => Ok(()),
        _ => Err(HubError::InvalidLogin),
    }
}

/// The salt, parameters and sealed bundle of `account` (`NotFound` without a
/// password).
pub fn bundle(conn: &Connection, account: i64) -> Result<(Prelogin, Vec<u8>), HubError> {
    let found: Option<(Vec<u8>, u32, u32, Vec<u8>)> = conn
        .query_row(
            "SELECT salt, kdf_memory, kdf_iterations, bundle FROM accounts
             WHERE id = ?1 AND login_hash IS NOT NULL",
            [account],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let (salt, memory_kib, iterations, bundle) = found.ok_or(HubError::NotFound)?;
    Ok((
        Prelogin {
            salt,
            kdf: Kdf {
                memory_kib,
                iterations,
            },
        },
        bundle,
    ))
}

/// Replaces the password of `account`; `via` and `device` are told to its devices
/// (see [`PasswordChange`]).
pub fn set_password(
    conn: &Connection,
    account: i64,
    password: &Password,
    via: &str,
    device: Option<i64>,
    now: i64,
) -> Result<(), HubError> {
    write_password(conn, account, password, Some((via, device)), now)
}

/// The last password change of `account`, if any.
pub fn password_change(
    conn: &Connection,
    account: i64,
) -> Result<Option<PasswordChange>, HubError> {
    let (at, via, device): (Option<i64>, Option<String>, Option<i64>) = conn.query_row(
        "SELECT password_changed_at, password_changed_via, password_changed_device
         FROM accounts WHERE id = ?1",
        [account],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(at
        .zip(via)
        .map(|(at, via)| PasswordChange { at, via, device }))
}

/// Changes the name of `account` (normalized) within its library; the old one is
/// free at once.
pub fn set_name(conn: &Connection, account: i64, name: &str) -> Result<(), HubError> {
    let (library, current): (i64, String) = conn.query_row(
        "SELECT library_id, name FROM accounts WHERE id = ?1",
        [account],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    if current == name {
        return Ok(());
    }
    if name_taken(conn, library, name)? {
        return Err(HubError::NameTaken);
    }
    conn.execute(
        "UPDATE accounts SET name = ?2 WHERE id = ?1",
        params![account, name],
    )?;
    Ok(())
}

fn insert_device(
    conn: &Connection,
    account: i64,
    name: Option<&[u8]>,
    now: i64,
) -> Result<NewDevice, HubError> {
    let token = Secret::generate()?;
    conn.execute(
        "INSERT INTO devices (account_id, token_hash, name, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?4)",
        params![account, token.hash(), name, now],
    )?;
    let device = conn.last_insert_rowid();
    conn.execute(
        "UPDATE accounts SET last_seen_at = ?2 WHERE id = ?1",
        params![account, now],
    )?;
    let (library, account_id, library_id): (i64, [u8; 16], [u8; 16]) = conn.query_row(
        "SELECT a.library_id, a.public_id, l.public_id
         FROM accounts a JOIN libraries l ON l.id = a.library_id WHERE a.id = ?1",
        [account],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    Ok(NewDevice {
        library,
        account,
        account_id: AccountId::new(account_id),
        library_id: AccountId::new(library_id),
        device,
        token,
    })
}

/// Adds a device to `account`, first removing its device `remove` if given (it must
/// exist). At `max_devices` (0 = no limit) the answer is `DeviceLimit` with the
/// account's devices, so the person can choose one to remove; the caller's
/// transaction is then rolled back.
pub fn add_device(
    conn: &Connection,
    account: i64,
    max_devices: u64,
    name: Option<&[u8]>,
    remove: Option<i64>,
    now: i64,
) -> Result<NewDevice, HubError> {
    if let Some(remove) = remove
        && !remove_device(conn, account, remove)?
    {
        return Err(HubError::NotFound);
    }
    if max_devices > 0 {
        let list = devices(conn, account)?;
        if list.len() as u64 >= max_devices {
            return Err(HubError::DeviceLimit(list));
        }
    }
    insert_device(conn, account, name, now)
}

/// Stores a new pairing secret of the owner account `account`, valid for
/// [`PAIRING_TTL`].
pub fn create_pairing(conn: &Connection, account: i64, now: i64) -> Result<Secret, HubError> {
    let secret = Secret::generate()?;
    conn.execute("DELETE FROM pairings WHERE expires_at <= ?1", [now])?;
    conn.execute(
        "INSERT INTO pairings (secret_hash, account_id, expires_at) VALUES (?1, ?2, ?3)",
        params![secret.hash(), account, now + PAIRING_TTL],
    )?;
    Ok(secret)
}

/// Uses up a pairing secret and issues a device token of its account; at
/// `max_devices` it is `Quota("devices")` (the device list is managed on the library's
/// computer).
pub fn pair(
    conn: &mut Connection,
    secret: &Secret,
    max_devices: u64,
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
    let device = match add_device(&tx, account, max_devices, name, None, now) {
        Err(HubError::DeviceLimit(_)) => return Err(HubError::Quota("devices")),
        other => other?,
    };
    tx.commit()?;
    Ok(device)
}

/// The devices of `account`, oldest first.
pub fn devices(conn: &Connection, account: i64) -> Result<Vec<DeviceInfo>, HubError> {
    let mut stmt = conn.prepare(
        "SELECT id, name, created_at, last_seen_at FROM devices
         WHERE account_id = ?1 ORDER BY id",
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

/// Sets the sealed name of the device `device` of `account`; false if there is no
/// such device.
pub fn set_device_name(
    conn: &Connection,
    account: i64,
    device: i64,
    name: Option<&[u8]>,
) -> Result<bool, HubError> {
    let changed = conn.execute(
        "UPDATE devices SET name = ?3 WHERE id = ?1 AND account_id = ?2",
        params![device, account, name],
    )?;
    Ok(changed > 0)
}

/// Removes the device `device` of `account` (its token stops working); false if there
/// is no such device.
pub fn remove_device(conn: &Connection, account: i64, device: i64) -> Result<bool, HubError> {
    let changed = conn.execute(
        "DELETE FROM devices WHERE id = ?1 AND account_id = ?2",
        params![device, account],
    )?;
    Ok(changed > 0)
}

/// What `library` stores.
pub fn usage(conn: &Connection, library: i64) -> Result<Usage, HubError> {
    Ok(conn.query_row(
        "SELECT
           coalesce((SELECT sum(b.size) FROM blobs b
                     JOIN publications p ON p.id = b.publication_id
                     WHERE p.library_id = ?1), 0)
         + coalesce((SELECT sum(length(r.data)) FROM records r
                     JOIN accounts a ON a.id = r.account_id
                     WHERE a.library_id = ?1), 0),
           (SELECT count(*) FROM publications WHERE library_id = ?1),
           (SELECT count(*) FROM accounts WHERE library_id = ?1),
           (SELECT count(*) FROM devices d JOIN accounts a ON a.id = d.account_id
            WHERE a.library_id = ?1),
           (SELECT count(*) FROM records r JOIN accounts a ON a.id = r.account_id
            WHERE a.library_id = ?1)",
        [library],
        |r| {
            Ok(Usage {
                bytes: r.get::<_, i64>(0)? as u64,
                publications: r.get::<_, i64>(1)? as u64,
                accounts: r.get::<_, i64>(2)? as u64,
                devices: r.get::<_, i64>(3)? as u64,
                records: r.get::<_, i64>(4)? as u64,
            })
        },
    )?)
}

/// The own quotas of `library` (none if there is no such library).
pub fn library_quotas(conn: &Connection, library: i64) -> Result<Option<LibraryQuotas>, HubError> {
    let quotas = conn
        .query_row(
            "SELECT max_bytes, max_publications FROM libraries WHERE id = ?1",
            [library],
            |r| {
                let value = |i: usize| r.get::<_, Option<i64>>(i).map(|v| v.map(|v| v as u64));
                Ok(LibraryQuotas {
                    max_bytes: value(0)?,
                    max_publications: value(1)?,
                })
            },
        )
        .optional()?;
    Ok(quotas)
}

/// Sets the own quotas of `library`; returns whether the library exists.
pub fn set_library_quotas(
    conn: &Connection,
    library: i64,
    quotas: &LibraryQuotas,
) -> Result<bool, HubError> {
    let value = |v: Option<u64>| v.map(|v| v as i64);
    let changed = conn.execute(
        "UPDATE libraries SET max_bytes = ?2, max_publications = ?3 WHERE id = ?1",
        params![
            library,
            value(quotas.max_bytes),
            value(quotas.max_publications)
        ],
    )?;
    Ok(changed > 0)
}

/// The quotas in effect for `library`: its own over the hub's `defaults`.
pub fn effective_quotas(
    conn: &Connection,
    library: i64,
    defaults: &QuotasConfig,
) -> Result<QuotasConfig, HubError> {
    Ok(library_quotas(conn, library)?
        .unwrap_or_default()
        .over(defaults))
}

/// The bytes the hub stores for all libraries: blobs and records (kept in
/// `hub_bytes` by triggers).
pub fn total_bytes(conn: &Connection) -> Result<u64, HubError> {
    let bytes: i64 =
        conn.query_row("SELECT bytes FROM hub_bytes WHERE id = 1", [], |r| r.get(0))?;
    Ok(bytes as u64)
}

/// `Err(Full)` when the hub stores more than `max_total_bytes` (0 = no limit).
fn check_total(conn: &Connection, max_total_bytes: u64) -> Result<(), HubError> {
    if max_total_bytes > 0 && total_bytes(conn)? > max_total_bytes {
        return Err(HubError::Full);
    }
    Ok(())
}

/// The publications of `library`, oldest first.
pub fn publications(conn: &Connection, library: i64) -> Result<Vec<PublicationInfo>, HubError> {
    let mut stmt = conn.prepare(
        "SELECT p.public_id, p.created_at, p.last_opened_at,
                coalesce(sum(b.size), 0), count(b.path)
         FROM publications p LEFT JOIN blobs b ON b.publication_id = p.id
         WHERE p.library_id = ?1 GROUP BY p.id ORDER BY p.id",
    )?;
    let rows = stmt.query_map([library], |r| {
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

/// The publication `id` of `library`: its row id, or `NotFound` (also for another
/// library's publication) or `Gone` (taken down).
///
/// Every check whether an account may read a publication goes through here: one of
/// its library's.
fn own_publication(conn: &Connection, library: i64, id: &PublicationId) -> Result<i64, HubError> {
    let found: Option<(i64, i64)> = conn
        .query_row(
            "SELECT id, library_id FROM publications WHERE public_id = ?1",
            [id.as_bytes()],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match found {
        Some((row, owner)) if owner == library => Ok(row),
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

/// Records the blob `path` of publication `id` with `size` bytes for `library`
/// before its file is written: creates the publication on its first blob (taken-down
/// ids are refused, another library's id is "not found"), checks the library's
/// publication and byte quotas (its own over the hub's `defaults`) and the hub's
/// `max_total_bytes`.
#[allow(clippy::too_many_arguments)]
pub fn reserve_blob(
    conn: &mut Connection,
    library: i64,
    id: &PublicationId,
    path: BlobPath,
    size: u64,
    defaults: &QuotasConfig,
    max_total_bytes: u64,
    now: i64,
) -> Result<Reservation, HubError> {
    let tx = conn.transaction()?;
    let quotas = effective_quotas(&tx, library, defaults)?;
    let (publication, created) = match own_publication(&tx, library, id) {
        Ok(row) => (row, false),
        Err(HubError::NotFound) => {
            let taken: Option<i64> = tx
                .query_row(
                    "SELECT library_id FROM publications WHERE public_id = ?1",
                    [id.as_bytes()],
                    |r| r.get(0),
                )
                .optional()?;
            if taken.is_some() {
                return Err(HubError::NotFound);
            }
            if quotas.max_publications > 0
                && usage(&tx, library)?.publications >= quotas.max_publications
            {
                return Err(HubError::Quota("publications"));
            }
            tx.execute(
                "INSERT INTO publications (public_id, library_id, created_at, last_opened_at)
                 VALUES (?1, ?2, ?3, ?3)",
                params![id.as_bytes(), library, now],
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
    if quotas.max_bytes > 0 && usage(&tx, library)?.bytes > quotas.max_bytes {
        return Err(HubError::Quota("bytes"));
    }
    if old_size.is_none_or(|old| size as i64 > old) {
        // A blob that does not grow is let through on a full hub.
        check_total(&tx, max_total_bytes)?;
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

/// Checks that `library` (its token or a device of one of its accounts) may read the
/// blob `path` of publication `id` and that it exists; a fetch of the manifest counts
/// as opening the publication.
pub fn open_blob(
    conn: &Connection,
    library: i64,
    id: &PublicationId,
    path: BlobPath,
    now: i64,
) -> Result<(), HubError> {
    let publication = own_publication(conn, library, id)?;
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

/// Deletes the publication `id` of `library` with its blob rows (the caller removes
/// the files).
pub fn delete_publication(
    conn: &Connection,
    library: i64,
    id: &PublicationId,
) -> Result<(), HubError> {
    let publication = own_publication(conn, library, id)?;
    conn.execute("DELETE FROM publications WHERE id = ?1", [publication])?;
    Ok(())
}

/// Takes the publication `id` down: deletes it (of whichever library) and refuses its
/// id from now on. Returns the internal id of its library, `None` if it did not exist
/// (the id is refused anyway).
pub fn takedown(conn: &Connection, id: &PublicationId, now: i64) -> Result<Option<i64>, HubError> {
    let library: Option<i64> = conn
        .query_row(
            "DELETE FROM publications WHERE public_id = ?1 RETURNING library_id",
            [id.as_bytes()],
            |r| r.get(0),
        )
        .optional()?;
    conn.execute(
        "INSERT OR IGNORE INTO takedowns (public_id, taken_at) VALUES (?1, ?2)",
        params![id.as_bytes(), now],
    )?;
    Ok(library)
}

/// Deletes the publications not opened for `days` days (none if `days` is 0), at most
/// `limit` of them, and returns their ids (the caller removes the files).
pub fn expire(
    conn: &Connection,
    days: u32,
    now: i64,
    limit: usize,
) -> Result<Vec<PublicationId>, HubError> {
    if days == 0 {
        return Ok(Vec::new());
    }
    let cutoff = now - i64::from(days) * DAY;
    let mut stmt = conn.prepare(
        "DELETE FROM publications
         WHERE id IN (SELECT id FROM publications WHERE last_opened_at <= ?1 LIMIT ?2)
         RETURNING public_id",
    )?;
    let ids = stmt.query_map(params![cutoff, limit as i64], |r| {
        Ok(PublicationId::new(r.get(0)?))
    })?;
    Ok(ids.collect::<Result<_, _>>()?)
}

/// What one batch of library and account expiry deleted.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expired {
    /// Internal ids of the libraries deleted with everything they store.
    pub libraries: Vec<i64>,
    /// Internal ids of the member accounts deleted.
    pub accounts: Vec<i64>,
    /// The ids of the deleted libraries' publications (the caller removes the files).
    pub publications: Vec<PublicationId>,
}

impl Expired {
    /// How many libraries and accounts were deleted.
    pub fn len(&self) -> usize {
        self.libraries.len() + self.accounts.len()
    }

    /// Whether nothing was deleted.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Deletes, at most `limit` together, the libraries neither whose token nor any of
/// whose accounts' devices made a request for `days` days, then the reader accounts
/// none of whose devices did (the library's token keeps no member account alive; the
/// owner account lives with its library); none if `days` is 0.
pub fn expire_libraries(
    conn: &mut Connection,
    days: u32,
    now: i64,
    limit: usize,
) -> Result<Expired, HubError> {
    if days == 0 {
        return Ok(Expired::default());
    }
    let cutoff = now - i64::from(days) * DAY;
    let tx = conn.transaction()?;
    let libraries: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT l.id FROM libraries l
             WHERE max(l.last_seen_at,
                       coalesce((SELECT max(last_seen_at) FROM accounts
                                 WHERE library_id = l.id), 0)) <= ?1
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![cutoff, limit as i64], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut expired = Expired::default();
    for library in libraries {
        expired
            .publications
            .extend(delete_library_rows(&tx, library)?.unwrap_or_default());
        expired.libraries.push(library);
    }
    let left = limit.saturating_sub(expired.libraries.len());
    expired.accounts = {
        let mut stmt = tx.prepare(
            "DELETE FROM accounts WHERE id IN (
               SELECT id FROM accounts WHERE role = 'reader' AND last_seen_at <= ?1 LIMIT ?2)
             RETURNING id",
        )?;
        let rows = stmt.query_map(params![cutoff, left as i64], |r| r.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    tx.commit()?;
    Ok(expired)
}

/// Deletes `library` with its accounts, devices, invites, publications and records;
/// returns the ids of its publications (the caller removes the files), or `None` if
/// there is no such library.
pub fn delete_library(
    conn: &mut Connection,
    library: i64,
) -> Result<Option<Vec<PublicationId>>, HubError> {
    let tx = conn.transaction()?;
    let ids = delete_library_rows(&tx, library)?;
    tx.commit()?;
    Ok(ids)
}

fn delete_library_rows(
    tx: &Transaction<'_>,
    library: i64,
) -> Result<Option<Vec<PublicationId>>, HubError> {
    let ids: Vec<PublicationId> = {
        let mut stmt = tx.prepare("SELECT public_id FROM publications WHERE library_id = ?1")?;
        let rows = stmt.query_map([library], |r| Ok(PublicationId::new(r.get(0)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    // The foreign keys cascade to accounts, devices, pairings, invites, publications,
    // blobs and records.
    let deleted = tx.execute("DELETE FROM libraries WHERE id = ?1", [library])?;
    Ok((deleted > 0).then_some(ids))
}

/// Applies `changes` of `account` (of `library`) in order, each with the next sequence
/// number; the whole batch is refused if it would exceed the library's byte quota
/// (its own over the hub's `defaults`) or, when it adds bytes, the hub's
/// `max_total_bytes`. Returns the sequence numbers.
#[allow(clippy::too_many_arguments)]
pub fn push_records(
    conn: &mut Connection,
    library: i64,
    account: i64,
    changes: &[RecordChange],
    defaults: &QuotasConfig,
    max_total_bytes: u64,
    now: i64,
) -> Result<Vec<i64>, HubError> {
    let tx = conn.transaction()?;
    let quotas = effective_quotas(&tx, library, defaults)?;
    let before = if max_total_bytes > 0 {
        total_bytes(&tx)?
    } else {
        0
    };
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
    if quotas.max_bytes > 0 && usage(&tx, library)?.bytes > quotas.max_bytes {
        return Err(HubError::Quota("bytes"));
    }
    if max_total_bytes > 0 {
        // Deletions and changes that do not grow are let through on a full hub.
        let after = total_bytes(&tx)?;
        if after > before && after > max_total_bytes {
            return Err(HubError::Full);
        }
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

/// The last sequence number given out for `account`.
pub fn current_seq(conn: &Connection, account: i64) -> Result<i64, HubError> {
    Ok(
        conn.query_row("SELECT seq FROM accounts WHERE id = ?1", [account], |r| {
            r.get(0)
        })?,
    )
}

/// Every library with its usage, oldest first.
pub fn libraries(conn: &Connection) -> Result<Vec<LibrarySummary>, HubError> {
    let mut stmt = conn.prepare(
        "SELECT l.id, l.created_at,
                max(l.last_seen_at, coalesce((SELECT max(last_seen_at) FROM accounts
                                              WHERE library_id = l.id), 0))
         FROM libraries l ORDER BY l.id",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let rows: Vec<(i64, i64, i64)> = rows.collect::<Result<_, _>>()?;
    rows.into_iter()
        .map(|(id, created_at, last_seen_at)| {
            Ok(LibrarySummary {
                id,
                created_at,
                last_seen_at,
                usage: usage(conn, id)?,
                quotas: library_quotas(conn, id)?.unwrap_or_default(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_libraries() -> LibrariesConfig {
        LibrariesConfig {
            registration: Registration::Open,
            ..LibrariesConfig::default()
        }
    }

    /// A library `handle` created at 1000.
    fn library(conn: &mut Connection, handle: &str) -> NewLibrary {
        create_library(conn, &open_libraries(), None, handle, "admin", 1000).unwrap()
    }

    fn password(byte: u8) -> Password {
        Password {
            login_hash: login_hash(&[byte; 32]),
            salt: vec![byte; SALT_LEN],
            kdf: Kdf::from(&PasswordsConfig::default()),
            bundle: vec![byte; 48],
        }
    }

    /// A reader account `name` of `library` with one device, made through an invite.
    fn member(conn: &mut Connection, library: i64, name: &str, now: i64) -> NewDevice {
        let (secret, _, _) = create_member_invite(conn, library, None, b"p", 7, now).unwrap();
        let password = password(1);
        let redeem = Redeem {
            name: Some(name),
            password: &password,
            device_name: None,
            remove_device: None,
        };
        redeem_invite(conn, &secret, &redeem, 5, now).unwrap()
    }

    #[test]
    fn hub_invites_are_single_use_and_expire() {
        let mut conn = open_in_memory();
        let config = LibrariesConfig::default();
        let create = |conn: &mut Connection, code: Option<&Secret>, handle: &str, now| {
            create_library(conn, &config, code, handle, "admin", now)
        };
        let err = create(&mut conn, None, "one", 0).unwrap_err();
        assert!(matches!(err, HubError::InviteRequired), "{err}");
        let code = create_hub_invite(&conn, 1, 0).unwrap();
        create(&mut conn, Some(&code), "one", 10).unwrap();
        let again = create(&mut conn, Some(&code), "two", 20);
        assert!(matches!(again, Err(HubError::InviteRequired)));
        let late = create_hub_invite(&conn, 1, 0).unwrap();
        let expired = create(&mut conn, Some(&late), "two", DAY);
        assert!(matches!(expired, Err(HubError::InviteRequired)));
    }

    #[test]
    fn max_libraries_and_handles_are_enforced() {
        let mut conn = open_in_memory();
        let config = LibrariesConfig {
            max_libraries: 2,
            ..open_libraries()
        };
        create_library(&mut conn, &config, None, "one", "admin", 0).unwrap();
        let err = create_library(&mut conn, &config, None, "one", "admin", 0).unwrap_err();
        assert!(matches!(err, HubError::HandleTaken), "{err}");
        let two = create_library(&mut conn, &config, None, "two", "admin", 0).unwrap();
        let err = create_library(&mut conn, &config, None, "three", "admin", 0).unwrap_err();
        assert!(matches!(err, HubError::Quota("libraries")), "{err}");
        // A changed handle frees the old one at once.
        assert!(matches!(
            set_handle(&conn, two.library, "one"),
            Err(HubError::HandleTaken)
        ));
        set_handle(&conn, two.library, "deux").unwrap();
        delete_library(&mut conn, two.library).unwrap();
        create_library(&mut conn, &config, None, "two", "admin", 0).unwrap();
    }

    #[test]
    fn tokens_are_stored_hashed_and_tell_library_from_device() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let olga = member(&mut conn, lib.library, "olga", 1000);
        let stored: Vec<u8> = conn
            .query_row("SELECT token_hash FROM libraries", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored, lib.token.hash());
        assert_eq!(
            authenticate(&conn, &lib.token, 1000).unwrap(),
            Some(Auth::Library(lib.library))
        );
        assert_eq!(
            authenticate(&conn, &olga.token, 1000).unwrap(),
            Some(Auth::Device(DeviceAuth {
                library: lib.library,
                account: olga.account,
                device: olga.device,
                role: Role::Reader,
            }))
        );
        let unknown = Secret::generate().unwrap();
        assert_eq!(authenticate(&conn, &unknown, 1000).unwrap(), None);
    }

    #[test]
    fn last_seen_is_written_at_most_hourly() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let olga = member(&mut conn, lib.library, "olga", 1000);
        let seen = |conn: &Connection| -> (i64, i64, i64) {
            conn.query_row(
                "SELECT (SELECT last_seen_at FROM devices),
                        (SELECT last_seen_at FROM accounts WHERE role = 'reader'),
                        (SELECT last_seen_at FROM libraries)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        authenticate(&conn, &olga.token, 1000 + SEEN_INTERVAL - 1).unwrap();
        authenticate(&conn, &lib.token, 1000 + SEEN_INTERVAL - 1).unwrap();
        assert_eq!(seen(&conn), (1000, 1000, 1000));
        let later = 1000 + SEEN_INTERVAL;
        authenticate(&conn, &olga.token, later).unwrap();
        assert_eq!(seen(&conn), (later, later, 1000));
        authenticate(&conn, &lib.token, later).unwrap();
        assert_eq!(seen(&conn), (later, later, later));
    }

    #[test]
    fn an_invite_creates_an_account_once_and_a_taken_name_spends_nothing() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let (secret, invite_id, expires) =
            create_member_invite(&conn, lib.library, None, b"sealed", 7, 0).unwrap();
        assert_eq!(expires, 7 * DAY);
        let preview = preview_invite(&conn, &secret, 0).unwrap();
        assert_eq!(preview.handle, "smiths");
        assert_eq!(preview.name, None);
        assert_eq!(preview.payload, b"sealed");
        let password = password(2);
        let mut redeem = Redeem {
            name: Some("admin"),
            password: &password,
            device_name: None,
            remove_device: None,
        };
        let err = redeem_invite(&mut conn, &secret, &redeem, 5, 0).unwrap_err();
        assert!(matches!(err, HubError::NameTaken), "{err}");
        redeem.name = Some("olga");
        let device = redeem_invite(&mut conn, &secret, &redeem, 5, 0).unwrap();
        assert_eq!(device.library, lib.library);
        let err = redeem_invite(&mut conn, &secret, &redeem, 5, 0).unwrap_err();
        assert!(matches!(err, HubError::InvalidSecret), "{err}");
        assert!(matches!(
            preview_invite(&conn, &secret, 0),
            Err(HubError::InvalidSecret)
        ));
        let list = members(&conn, lib.library).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(
            (list[0].role, list[0].name.as_str()),
            (Role::Owner, "admin")
        );
        assert!(!list[0].has_password);
        assert_eq!(list[1].name, "olga");
        assert_eq!(list[1].invite_id, Some(invite_id));
        assert_eq!(list[1].devices, 1);
        assert!(list[1].has_password);
        // The name signs in.
        let prelogin = prelogin(&conn, "olga", "smiths", &PasswordsConfig::default()).unwrap();
        assert_eq!(prelogin.salt, password.salt);
        login(&mut conn, "olga", "smiths", &[2; 32], None, None, 5, 0).unwrap();
    }

    #[test]
    fn invites_expire_and_count_against_the_library_cap() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let (late, _, _) = create_member_invite(&conn, lib.library, None, b"p", 1, 0).unwrap();
        assert!(matches!(
            preview_invite(&conn, &late, DAY),
            Err(HubError::InvalidSecret)
        ));
        // The owner and 15 pending invites fill the library; an expired one is gone.
        for _ in 0..MAX_LIBRARY_ACCOUNTS - 1 {
            create_member_invite(&conn, lib.library, None, b"p", 7, DAY).unwrap();
        }
        let err = create_member_invite(&conn, lib.library, None, b"p", 7, DAY).unwrap_err();
        assert!(matches!(err, HubError::Quota("accounts")), "{err}");
        // A pending invite withdrawn makes room again.
        let (_, id, _) = {
            delete_member_invite(&conn, lib.library, &AccountId::new([0; 16])).unwrap();
            let first: [u8; 16] = conn
                .query_row("SELECT public_id FROM member_invites LIMIT 1", [], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(delete_member_invite(&conn, lib.library, &AccountId::new(first)).unwrap());
            create_member_invite(&conn, lib.library, None, b"p", 7, DAY).unwrap()
        };
        let _ = id;
        // The owner's password is not reset by an invite.
        let err =
            create_member_invite(&conn, lib.library, Some(lib.owner), b"p", 7, DAY).unwrap_err();
        assert!(matches!(err, HubError::Forbidden(_)), "{err}");
    }

    #[test]
    fn a_reset_sets_a_new_password_and_tells_the_devices() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let olga = member(&mut conn, lib.library, "olga", 0);
        assert_eq!(password_change(&conn, olga.account).unwrap(), None);
        let (secret, _, _) =
            create_member_invite(&conn, lib.library, Some(olga.account), b"r", 7, 5).unwrap();
        let preview = preview_invite(&conn, &secret, 5).unwrap();
        assert_eq!(preview.name.as_deref(), Some("olga"));
        let new = password(3);
        let redeem = Redeem {
            name: None,
            password: &new,
            device_name: None,
            remove_device: None,
        };
        redeem_invite(&mut conn, &secret, &redeem, 5, 9).unwrap();
        assert!(matches!(
            login(&mut conn, "olga", "smiths", &[1; 32], None, None, 5, 9),
            Err(HubError::InvalidLogin)
        ));
        login(&mut conn, "olga", "smiths", &[3; 32], None, None, 5, 9).unwrap();
        assert_eq!(
            password_change(&conn, olga.account).unwrap(),
            Some(PasswordChange {
                at: 9,
                via: "reset".into(),
                device: None
            })
        );
        // The old device stays signed in.
        assert!(authenticate(&conn, &olga.token, 9).unwrap().is_some());
    }

    #[test]
    fn unknown_addresses_get_a_stable_fake_salt() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        member(&mut conn, lib.library, "olga", 0);
        let defaults = PasswordsConfig::default();
        let fake = prelogin(&conn, "nobody", "smiths", &defaults).unwrap();
        assert_eq!(fake.salt.len(), SALT_LEN);
        assert_eq!(fake.kdf, Kdf::from(&defaults));
        assert_eq!(
            prelogin(&conn, "nobody", "smiths", &defaults).unwrap(),
            fake
        );
        assert_ne!(prelogin(&conn, "nobody", "other", &defaults).unwrap(), fake);
        // The owner without a password looks unknown too.
        let owner = prelogin(&conn, "admin", "smiths", &defaults).unwrap();
        assert_ne!(owner.salt, vec![0; SALT_LEN]);
        assert!(matches!(
            login(&mut conn, "admin", "smiths", &[0; 32], None, None, 5, 0),
            Err(HubError::InvalidLogin)
        ));
    }

    #[test]
    fn the_sixth_device_removes_one_first() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let olga = member(&mut conn, lib.library, "olga", 0);
        for _ in 0..4 {
            login(&mut conn, "olga", "smiths", &[1; 32], None, None, 5, 0).unwrap();
        }
        let err = login(&mut conn, "olga", "smiths", &[1; 32], None, None, 5, 0).unwrap_err();
        let HubError::DeviceLimit(list) = err else {
            panic!("{err}")
        };
        assert_eq!(list.len(), 5);
        // Removing a device of another account is refused and removes nothing.
        let other = member(&mut conn, lib.library, "ivan", 0);
        let err = login(
            &mut conn,
            "olga",
            "smiths",
            &[1; 32],
            None,
            Some(other.device),
            5,
            0,
        )
        .unwrap_err();
        assert!(matches!(err, HubError::NotFound), "{err}");
        login(
            &mut conn,
            "olga",
            "smiths",
            &[1; 32],
            None,
            Some(olga.device),
            5,
            0,
        )
        .unwrap();
        assert_eq!(devices(&conn, olga.account).unwrap().len(), 5);
        assert_eq!(authenticate(&conn, &olga.token, 0).unwrap(), None);
    }

    #[test]
    fn pairing_adds_devices_to_the_owner_within_the_limit() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let secret = create_pairing(&conn, lib.owner, 0).unwrap();
        let phone = pair(&mut conn, &secret, 1, None, 0).unwrap();
        assert_eq!(phone.account, lib.owner);
        assert!(matches!(
            pair(&mut conn, &secret, 1, None, 0),
            Err(HubError::InvalidSecret)
        ));
        let second = create_pairing(&conn, lib.owner, 0).unwrap();
        let err = pair(&mut conn, &second, 1, None, 0).unwrap_err();
        assert!(matches!(err, HubError::Quota("devices")), "{err}");
        let late = create_pairing(&conn, lib.owner, 0).unwrap();
        assert!(matches!(
            pair(&mut conn, &late, 5, None, PAIRING_TTL),
            Err(HubError::InvalidSecret)
        ));
    }

    #[test]
    fn names_change_within_the_library_and_the_owner_cannot_be_deleted() {
        let mut conn = open_in_memory();
        let one = library(&mut conn, "smiths");
        let two = library(&mut conn, "joneses");
        let olga = member(&mut conn, one.library, "olga", 0);
        member(&mut conn, one.library, "ivan", 0);
        member(&mut conn, two.library, "olga", 0);
        assert!(matches!(
            set_name(&conn, olga.account, "ivan"),
            Err(HubError::NameTaken)
        ));
        set_name(&conn, olga.account, "olga.s").unwrap();
        set_name(&conn, olga.account, "olga.s").unwrap();
        login(&mut conn, "olga.s", "smiths", &[1; 32], None, None, 5, 0).unwrap();
        member(&mut conn, one.library, "olga", 0);
        assert!(matches!(
            delete_account(&conn, one.owner),
            Err(HubError::Forbidden(_))
        ));
        delete_account(&conn, olga.account).unwrap();
        assert_eq!(authenticate(&conn, &olga.token, 0).unwrap(), None);
    }

    #[test]
    fn a_failed_upload_releases_its_reservation() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let id = PublicationId::new([3; 16]);
        let quotas = QuotasConfig::default();
        let first = reserve_blob(
            &mut conn,
            lib.library,
            &id,
            BlobPath::Part(0),
            10,
            &quotas,
            0,
            0,
        )
        .unwrap();
        assert!(first.created);
        // A replacement that fails restores the old size; a new blob that fails is
        // dropped together with the publication it created.
        let second = reserve_blob(
            &mut conn,
            lib.library,
            &id,
            BlobPath::Part(0),
            99,
            &quotas,
            0,
            0,
        )
        .unwrap();
        release_blob(&conn, second, BlobPath::Part(0)).unwrap();
        assert_eq!(usage(&conn, lib.library).unwrap().bytes, 10);
        release_blob(&conn, first, BlobPath::Part(0)).unwrap();
        assert_eq!(usage(&conn, lib.library).unwrap().publications, 0);
    }

    fn change(id: &str, data: &[u8]) -> RecordChange {
        RecordChange {
            kind: "annotation".into(),
            id: id.into(),
            data: Some(data.to_vec()),
        }
    }

    #[test]
    fn deleted_records_stay_as_tombstones_with_a_new_seq() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let quotas = QuotasConfig::default();
        let tombstone = RecordChange {
            data: None,
            ..change("a1", b"")
        };
        let seqs = push_records(
            &mut conn,
            lib.library,
            lib.owner,
            &[change("a1", b"x"), tombstone],
            &quotas,
            0,
            5,
        )
        .unwrap();
        assert_eq!(seqs, [1, 2]);
        let (records, more) = pull_records(&conn, lib.owner, 0, 10).unwrap();
        assert!(!more);
        assert_eq!(records.len(), 1);
        assert_eq!((records[0].seq, records[0].data.as_ref()), (2, None));
    }

    #[test]
    fn a_library_quota_overrides_the_hub_value_for_all_its_accounts() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let olga = member(&mut conn, lib.library, "olga", 0);
        let defaults = QuotasConfig {
            max_bytes: 10,
            max_publications: 1,
            max_devices: 5,
        };
        let push = |conn: &mut Connection, account: i64, id: &str, len: usize| {
            push_records(
                conn,
                lib.library,
                account,
                &[change(id, &vec![0; len])],
                &defaults,
                0,
                0,
            )
        };
        push(&mut conn, lib.owner, "a", 6).unwrap();
        assert!(matches!(
            push(&mut conn, olga.account, "a", 6),
            Err(HubError::Quota("bytes"))
        ));
        let own = LibraryQuotas {
            max_bytes: Some(0),
            ..LibraryQuotas::default()
        };
        assert!(set_library_quotas(&conn, lib.library, &own).unwrap());
        assert!(!set_library_quotas(&conn, 999, &own).unwrap());
        assert_eq!(
            effective_quotas(&conn, lib.library, &defaults).unwrap(),
            QuotasConfig {
                max_bytes: 0,
                max_publications: 1,
                max_devices: 5,
            }
        );
        push(&mut conn, olga.account, "a", 6).unwrap();
        let summary = &libraries(&conn).unwrap()[0];
        assert_eq!(summary.quotas, own);
        assert_eq!(summary.usage.bytes, 12);
        assert_eq!(summary.usage.accounts, 2);
        assert_eq!(library_quotas(&conn, 999).unwrap(), None);
    }

    #[test]
    fn a_full_hub_refuses_growth_only() {
        let mut conn = open_in_memory();
        let one = library(&mut conn, "one");
        let two = library(&mut conn, "two");
        let quotas = QuotasConfig::default();
        let id = PublicationId::new([4; 16]);
        reserve_blob(
            &mut conn,
            one.library,
            &id,
            BlobPath::Part(0),
            60,
            &quotas,
            100,
            0,
        )
        .unwrap();
        let push = |conn: &mut Connection, changes: &[RecordChange]| {
            push_records(conn, two.library, two.owner, changes, &quotas, 100, 0)
        };
        push(&mut conn, &[change("a", &[0; 40])]).unwrap();
        assert_eq!(total_bytes(&conn).unwrap(), 100);
        let err = push(&mut conn, &[change("b", &[0; 1])]).unwrap_err();
        assert!(matches!(err, HubError::Full), "{err}");
        let err = reserve_blob(
            &mut conn,
            one.library,
            &id,
            BlobPath::Part(1),
            1,
            &quotas,
            100,
            0,
        )
        .unwrap_err();
        assert!(matches!(err, HubError::Full), "{err}");
        // Not growing: a replaced blob of the same size, a smaller record, a deletion.
        reserve_blob(
            &mut conn,
            one.library,
            &id,
            BlobPath::Part(0),
            60,
            &quotas,
            100,
            0,
        )
        .unwrap();
        push(&mut conn, &[change("a", &[0; 10])]).unwrap();
        let tombstone = RecordChange {
            data: None,
            ..change("a", &[])
        };
        push(&mut conn, &[tombstone]).unwrap();
        assert_eq!(total_bytes(&conn).unwrap(), 60);
    }

    #[test]
    fn idle_members_and_libraries_expire_by_their_own_requests() {
        let mut conn = open_in_memory();
        // `library` creates both at 1000.
        let idle = library(&mut conn, "idle");
        let active = library(&mut conn, "active");
        let olga = member(&mut conn, active.library, "olga", 1000);
        let ivan = member(&mut conn, active.library, "ivan", 1000);
        let id = PublicationId::new([5; 16]);
        let quotas = QuotasConfig::default();
        reserve_blob(
            &mut conn,
            idle.library,
            &id,
            BlobPath::Part(0),
            5,
            &quotas,
            0,
            1000,
        )
        .unwrap();
        let later = 1000 + 30 * DAY;
        // The library token keeps the library and its owner alive, not a member; a
        // member's own device keeps them.
        authenticate(&conn, &active.token, later).unwrap().unwrap();
        authenticate(&conn, &ivan.token, later).unwrap().unwrap();
        assert!(
            expire_libraries(&mut conn, 0, later + DAY, 10)
                .unwrap()
                .is_empty()
        );
        let expired = expire_libraries(&mut conn, 30, later, 10).unwrap();
        assert_eq!(expired.libraries, [idle.library]);
        assert_eq!(expired.accounts, [olga.account]);
        assert_eq!(expired.publications, [id]);
        assert_eq!(authenticate(&conn, &idle.token, later).unwrap(), None);
        assert_eq!(authenticate(&conn, &olga.token, later).unwrap(), None);
        assert_eq!(total_bytes(&conn).unwrap(), 0);
        let left: Vec<String> = members(&conn, active.library)
            .unwrap()
            .into_iter()
            .map(|m| m.name)
            .collect();
        assert_eq!(left, ["admin", "ivan"]);
        // A library whose only sign of life is a member's device stays.
        let much_later = later + 30 * DAY;
        authenticate(&conn, &ivan.token, much_later)
            .unwrap()
            .unwrap();
        let expired = expire_libraries(&mut conn, 30, much_later, 10).unwrap();
        assert!(expired.is_empty(), "{expired:?}");
    }

    #[test]
    fn deleting_a_library_returns_its_publications() {
        let mut conn = open_in_memory();
        let lib = library(&mut conn, "smiths");
        let olga = member(&mut conn, lib.library, "olga", 0);
        let id = PublicationId::new([6; 16]);
        reserve_blob(
            &mut conn,
            lib.library,
            &id,
            BlobPath::Part(0),
            5,
            &QuotasConfig::default(),
            0,
            0,
        )
        .unwrap();
        assert_eq!(
            delete_library(&mut conn, lib.library).unwrap(),
            Some(vec![id])
        );
        assert_eq!(delete_library(&mut conn, lib.library).unwrap(), None);
        assert!(libraries(&conn).unwrap().is_empty());
        assert_eq!(authenticate(&conn, &olga.token, 0).unwrap(), None);
        let accounts: i64 = conn
            .query_row("SELECT count(*) FROM accounts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(accounts, 0);
    }

    /// The bytes of all blobs and records, summed over every row.
    fn summed_bytes(conn: &Connection) -> u64 {
        conn.query_row(
            "SELECT coalesce((SELECT sum(size) FROM blobs), 0)
                  + coalesce((SELECT sum(length(data)) FROM records), 0)",
            [],
            |r| r.get::<_, i64>(0),
        )
        .unwrap() as u64
    }

    #[test]
    fn the_byte_counter_follows_every_change() {
        let mut conn = open_in_memory();
        let one = library(&mut conn, "one");
        let two = library(&mut conn, "two");
        let olga = member(&mut conn, two.library, "olga", 0);
        let quotas = QuotasConfig::default();
        let check = |conn: &Connection, expected: u64| {
            assert_eq!(total_bytes(conn).unwrap(), expected);
            assert_eq!(summed_bytes(conn), expected);
        };
        let reserve = |conn: &mut Connection, library: i64, id: &PublicationId, n: u32, size| {
            reserve_blob(conn, library, id, BlobPath::Part(n), size, &quotas, 0, 0).unwrap()
        };
        let push = |conn: &mut Connection, changes: &[RecordChange]| {
            push_records(conn, two.library, olga.account, changes, &quotas, 0, 0).unwrap();
        };
        let a = PublicationId::new([1; 16]);
        let b = PublicationId::new([2; 16]);
        check(&conn, 0);
        reserve(&mut conn, one.library, &a, 0, 10);
        reserve(&mut conn, one.library, &a, 1, 20);
        check(&conn, 30);
        reserve(&mut conn, one.library, &a, 0, 15);
        reserve(&mut conn, one.library, &a, 1, 5);
        check(&conn, 20);
        let new = reserve(&mut conn, one.library, &a, 2, 7);
        release_blob(&conn, new, BlobPath::Part(2)).unwrap();
        let replaced = reserve(&mut conn, one.library, &a, 0, 100);
        release_blob(&conn, replaced, BlobPath::Part(0)).unwrap();
        check(&conn, 20);
        push(&mut conn, &[change("x", &[0; 8]), change("y", &[0; 4])]);
        check(&conn, 32);
        push(&mut conn, &[change("x", &[0; 3])]);
        check(&conn, 27);
        let tombstone = RecordChange {
            data: None,
            ..change("y", &[])
        };
        push(&mut conn, &[tombstone]);
        check(&conn, 23);
        push(&mut conn, &[change("y", &[0; 6])]);
        check(&conn, 29);
        // Deletions through foreign key cascades: a publication, an account, a library.
        reserve(&mut conn, two.library, &b, 0, 50);
        check(&conn, 79);
        delete_publication(&conn, two.library, &b).unwrap();
        check(&conn, 29);
        delete_library(&mut conn, one.library).unwrap();
        check(&conn, 9);
        delete_account(&conn, olga.account).unwrap();
        check(&conn, 0);
    }

    #[test]
    fn a_database_with_old_accounts_is_moved_only_on_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.sqlite");
        write_old_database(&path);
        let err = open(&path).unwrap_err();
        assert!(
            matches!(
                err,
                HubError::OldAccounts {
                    accounts: 1,
                    publications: 1
                }
            ),
            "{err}"
        );
        assert_eq!(migrate_old(&path).unwrap(), (1, 1));
        let conn = open(&path).unwrap();
        assert!(libraries(&conn).unwrap().is_empty());
        assert_eq!(total_bytes(&conn).unwrap(), 0);
        assert!(
            is_taken_down(
                &conn,
                &PublicationId::new([
                    0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef, 0x01, 0x23, 0x45, 0x67, 0x89,
                    0xab, 0xcd, 0xef
                ])
            )
            .unwrap()
        );
    }

    #[test]
    fn an_empty_old_database_migrates_silently() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.sqlite");
        {
            let mut conn = crate::storage::open_wal(&path).unwrap();
            migrations::apply(&mut conn, &MIGRATIONS[..3]).unwrap();
        }
        let mut conn = open(&path).unwrap();
        library(&mut conn, "smiths");
    }

    #[test]
    fn expiry_deletes_at_most_its_limit_per_call() {
        let mut conn = open_in_memory();
        let ids: Vec<i64> = ["one", "two", "three"]
            .iter()
            .enumerate()
            .map(|(n, handle)| {
                let lib = library(&mut conn, handle);
                reserve_blob(
                    &mut conn,
                    lib.library,
                    &PublicationId::new([n as u8; 16]),
                    BlobPath::Part(0),
                    1,
                    &QuotasConfig::default(),
                    0,
                    1000,
                )
                .unwrap();
                lib.library
            })
            .collect();
        let later = 1000 + 30 * DAY;
        assert_eq!(expire(&conn, 30, later, 2).unwrap().len(), 2);
        assert_eq!(expire(&conn, 30, later, 2).unwrap().len(), 1);
        assert!(expire(&conn, 30, later, 2).unwrap().is_empty());
        let first = expire_libraries(&mut conn, 30, later, 2).unwrap();
        let second = expire_libraries(&mut conn, 30, later, 2).unwrap();
        assert_eq!([first.libraries, second.libraries].concat(), ids);
        assert!(libraries(&conn).unwrap().is_empty());
    }
}
