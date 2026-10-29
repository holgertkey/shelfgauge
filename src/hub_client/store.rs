//! The hub tables of `userdata.sqlite` (schema version 6, see
//! [`crate::annotations::store`]): the library on its hub with its key generations,
//! each profile's account in it, the removal queue, every profile's shelf and outbox,
//! and the accounts of 0.18–0.20 until their last pull.
//!
//! Functions take a [`Connection`] of the user-data database and never open a
//! transaction of their own unless they say so.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::HubUrl;
use super::keys::AccountKey;
use crate::hub::{AccountId, Secret};
use crate::profiles::ProfileId;
use crate::publication::BookKey;
use crate::publication::format::PublicationId;
use crate::storage::StorageError;

/// The library on its hub as this instance holds it. `Debug` hides the token.
#[derive(Clone)]
pub struct HubLibrary {
    /// The hub.
    pub hub_url: HubUrl,
    /// The library's public id (in every manifest header it publishes).
    pub library_id: AccountId,
    /// The handle: the `library` part of every member's address.
    pub handle: String,
    /// The library's token.
    pub token: Secret,
    /// When it was created on the hub (Unix seconds).
    pub created_at: i64,
}

impl fmt::Debug for HubLibrary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubLibrary")
            .field("hub_url", &self.hub_url)
            .finish_non_exhaustive()
    }
}

/// A stored value is not what this build writes.
fn corrupt(what: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(0, what.to_owned(), rusqlite::types::Type::Blob)
}

fn public_id(bytes: Vec<u8>, what: &str) -> rusqlite::Result<AccountId> {
    Ok(AccountId::new(bytes.try_into().map_err(|_| corrupt(what))?))
}

fn key(bytes: Vec<u8>, what: &str) -> rusqlite::Result<AccountKey> {
    Ok(AccountKey::new(
        bytes.try_into().map_err(|_| corrupt(what))?,
    ))
}

/// The library on its hub, if it was created there.
pub fn library(conn: &Connection) -> Result<Option<HubLibrary>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT hub_url, library_id, handle, token, created_at FROM hub_library",
            [],
            |row| {
                let url: String = row.get(0)?;
                Ok(HubLibrary {
                    hub_url: HubUrl::parse(&url).map_err(|_| corrupt("hub_url"))?,
                    library_id: public_id(row.get(1)?, "library_id")?,
                    handle: row.get(2)?,
                    token: Secret::from_text(&row.get::<_, String>(3)?),
                    created_at: row.get(4)?,
                })
            },
        )
        .optional()?)
}

/// Stores the library created on `hub_url` with its first key generation and the
/// owner profile's account, in one transaction.
#[allow(clippy::too_many_arguments)]
pub fn save_library(
    conn: &Connection,
    hub_url: &HubUrl,
    library_id: AccountId,
    handle: &str,
    token: &Secret,
    library_key: &AccountKey,
    owner: ProfileId,
    owner_account: AccountId,
    owner_key: &AccountKey,
    owner_name: &str,
    now: i64,
) -> Result<(), StorageError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO hub_library (id, hub_url, library_id, handle, token, created_at)
         VALUES (1, ?1, ?2, ?3, ?4, ?5)",
        params![
            hub_url.to_string(),
            library_id.as_bytes().as_slice(),
            handle,
            token.as_str(),
            now
        ],
    )?;
    tx.execute(
        "INSERT INTO hub_library_keys (key_id, key, created_at) VALUES (1, ?1, ?2)",
        params![library_key.as_bytes().as_slice(), now],
    )?;
    tx.execute(
        "INSERT INTO hub_accounts (profile_id, role, account_id, account_key, name, keys_sent,
             created_at)
         VALUES (?1, 'owner', ?2, ?3, ?4, 1, ?5)",
        params![
            owner.0,
            owner_account.as_bytes().as_slice(),
            owner_key.as_bytes().as_slice(),
            owner_name,
            now
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Stores a changed handle.
pub fn set_handle(conn: &Connection, handle: &str) -> Result<(), StorageError> {
    conn.execute("UPDATE hub_library SET handle = ?1", [handle])?;
    Ok(())
}

/// Forgets the library on its hub with its keys, every account, shelf and outbox and
/// the removal queue (local annotations and positions stay).
pub fn forget_library(conn: &Connection) -> Result<(), StorageError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(
        "DELETE FROM hub_accounts; DELETE FROM hub_removals; DELETE FROM hub_library_keys;
         DELETE FROM hub_library;",
    )?;
    tx.commit()?;
    Ok(())
}

/// Every generation of the library key, oldest first.
pub fn library_keys(conn: &Connection) -> Result<Vec<(u32, AccountKey)>, StorageError> {
    let mut stmt = conn.prepare("SELECT key_id, key FROM hub_library_keys ORDER BY key_id")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, key(row.get(1)?, "key")?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The current (newest) generation of the library key.
pub fn current_library_key(conn: &Connection) -> Result<Option<(u32, AccountKey)>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT key_id, key FROM hub_library_keys ORDER BY key_id DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, key(row.get(1)?, "key")?)),
        )
        .optional()?)
}

/// Stores `key` as the next generation of the library key; returns its id.
pub fn add_library_key(conn: &Connection, key: &AccountKey, now: i64) -> Result<u32, StorageError> {
    Ok(conn.query_row(
        "INSERT INTO hub_library_keys (key_id, key, created_at)
         VALUES ((SELECT coalesce(max(key_id), 0) + 1 FROM hub_library_keys), ?1, ?2)
         RETURNING key_id",
        params![key.as_bytes().as_slice(), now],
        |row| row.get(0),
    )?)
}

/// The role of an account in the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The owner profile's account: the library's own.
    Owner,
    /// Any other profile's (another admin's too).
    Reader,
}

impl Role {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Owner => "owner",
            Self::Reader => "reader",
        }
    }
}

/// What a pending invite is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteKind {
    /// A new account for the profile.
    New,
    /// A new password for the profile's existing account.
    Reset,
}

impl InviteKind {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::Reset => "reset",
        }
    }
}

/// An invite the hub holds for a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInvite {
    /// The invite's public id (kept by the account it creates).
    pub id: AccountId,
    /// What it is for.
    pub kind: InviteKind,
    /// When it expires (Unix seconds).
    pub expires_at: i64,
}

/// A profile's account in the library as this instance holds it. `Debug` hides the
/// key.
#[derive(Clone)]
pub struct HubAccount {
    /// The profile.
    pub profile: ProfileId,
    /// Its role.
    pub role: Role,
    /// The account's public id; `None` until a new person's invite is redeemed.
    pub account: Option<AccountId>,
    /// The account key.
    pub key: AccountKey,
    /// The name part of the address, once known.
    pub name: Option<String>,
    /// The pending invite, if any.
    pub invite: Option<PendingInvite>,
    /// The newest library key generation pushed into the account.
    pub keys_sent: u32,
    /// Its devices, as the hub's account list said last.
    pub devices: u32,
    /// When one of its devices last called the hub, as the list said last.
    pub last_seen_at: Option<i64>,
    /// When the account was found gone from the hub's list (left or expired).
    pub gone_at: Option<i64>,
    /// When this row was made (Unix seconds).
    pub created_at: i64,
    /// The last record `seq` pulled.
    pub last_seq: i64,
    /// The last successful sync (Unix seconds).
    pub synced_at: Option<i64>,
    /// The error of the last failed sync, for the profile page.
    pub sync_error: Option<String>,
}

impl fmt::Debug for HubAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HubAccount")
            .field("profile", &self.profile)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl HubAccount {
    /// Whether the account exists on the hub as far as this instance knows: it has
    /// an id and was not found gone.
    pub fn on_hub(&self) -> bool {
        self.account.is_some() && self.gone_at.is_none()
    }
}

fn account_row(row: &Row<'_>) -> rusqlite::Result<HubAccount> {
    let invite = match (
        row.get::<_, Option<Vec<u8>>>("invite_id")?,
        row.get::<_, Option<String>>("invite_kind")?,
        row.get::<_, Option<i64>>("invite_expires_at")?,
    ) {
        (Some(id), Some(kind), Some(expires_at)) => Some(PendingInvite {
            id: public_id(id, "invite_id")?,
            kind: if kind == "reset" {
                InviteKind::Reset
            } else {
                InviteKind::New
            },
            expires_at,
        }),
        _ => None,
    };
    Ok(HubAccount {
        profile: ProfileId(row.get("profile_id")?),
        role: if row.get::<_, String>("role")? == "owner" {
            Role::Owner
        } else {
            Role::Reader
        },
        account: row
            .get::<_, Option<Vec<u8>>>("account_id")?
            .map(|id| public_id(id, "account_id"))
            .transpose()?,
        key: key(row.get("account_key")?, "account_key")?,
        name: row.get("name")?,
        invite,
        keys_sent: row.get("keys_sent")?,
        devices: row.get("devices")?,
        last_seen_at: row.get("last_seen_at")?,
        gone_at: row.get("gone_at")?,
        created_at: row.get("created_at")?,
        last_seq: row.get("last_seq")?,
        synced_at: row.get("synced_at")?,
        sync_error: row.get("sync_error")?,
    })
}

/// The account of `profile` in the library, if it has one (or a pending invite).
pub fn account(conn: &Connection, profile: ProfileId) -> Result<Option<HubAccount>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT * FROM hub_accounts WHERE profile_id = ?1",
            [profile.0],
            account_row,
        )
        .optional()?)
}

/// Every profile's account (with pending ones), the owner first.
pub fn accounts(conn: &Connection) -> Result<Vec<HubAccount>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT * FROM hub_accounts ORDER BY role = 'reader', profile_id")?;
    let rows = stmt.query_map([], account_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Stores a pending invite of `profile`: for a new account (a fresh account key
/// replaces any old row of a gone account, with its shelf and outbox) or a reset of
/// its existing one (the key stays).
pub fn save_invite(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    invite: &PendingInvite,
    now: i64,
) -> Result<(), StorageError> {
    match invite.kind {
        InviteKind::New => {
            conn.execute(
                "DELETE FROM hub_accounts WHERE profile_id = ?1",
                [profile.0],
            )?;
            conn.execute(
                "INSERT INTO hub_accounts (profile_id, role, account_key, invite_id, invite_kind,
                     invite_expires_at, created_at)
                 VALUES (?1, 'reader', ?2, ?3, 'new', ?4, ?5)",
                params![
                    profile.0,
                    key.as_bytes().as_slice(),
                    invite.id.as_bytes().as_slice(),
                    invite.expires_at,
                    now
                ],
            )?;
        }
        InviteKind::Reset => {
            conn.execute(
                "UPDATE hub_accounts SET invite_id = ?2, invite_kind = 'reset',
                     invite_expires_at = ?3
                 WHERE profile_id = ?1",
                params![
                    profile.0,
                    invite.id.as_bytes().as_slice(),
                    invite.expires_at
                ],
            )?;
        }
    }
    Ok(())
}

/// Drops the pending invite of `profile` (spent, withdrawn or expired).
pub fn clear_invite(conn: &Connection, profile: ProfileId) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE hub_accounts SET invite_id = NULL, invite_kind = NULL, invite_expires_at = NULL
         WHERE profile_id = ?1",
        [profile.0],
    )?;
    Ok(())
}

/// What the hub's account list says of one account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedAccount {
    /// The public id.
    pub id: AccountId,
    /// The name part of the address.
    pub name: String,
    /// Its devices.
    pub devices: u32,
    /// When one of them last called the hub.
    pub last_seen_at: Option<i64>,
    /// The member invite that created it.
    pub invite: Option<AccountId>,
}

/// Brings the accounts up to date with the hub's account list: an account whose
/// invite was redeemed gets its id and name; listed ones get their device count,
/// last call and name; an account with an id missing from the list is marked gone
/// at `now` (left or expired; its row stays for **Invite again**). Returns the
/// profiles whose accounts appeared or went.
pub fn apply_account_list(
    conn: &mut Connection,
    listed: &[ListedAccount],
    now: i64,
) -> Result<Vec<ProfileId>, StorageError> {
    let tx = conn.transaction()?;
    let mut changed = Vec::new();
    for account in listed {
        let joined = match account.invite {
            Some(invite) => tx
                .query_row(
                    "UPDATE hub_accounts SET account_id = ?2, invite_id = NULL,
                         invite_kind = NULL, invite_expires_at = NULL
                     WHERE invite_id = ?1 AND invite_kind = 'new' AND account_id IS NULL
                     RETURNING profile_id",
                    params![
                        invite.as_bytes().as_slice(),
                        account.id.as_bytes().as_slice()
                    ],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?,
            None => None,
        };
        if let Some(profile) = joined {
            changed.push(ProfileId(profile));
        }
        tx.execute(
            "UPDATE hub_accounts SET name = ?2, devices = ?3, last_seen_at = ?4, gone_at = NULL
             WHERE account_id = ?1",
            params![
                account.id.as_bytes().as_slice(),
                account.name,
                account.devices,
                account.last_seen_at
            ],
        )?;
    }
    let known: Vec<(i64, Vec<u8>)> = {
        let mut stmt = tx.prepare(
            "SELECT profile_id, account_id FROM hub_accounts
             WHERE account_id IS NOT NULL AND gone_at IS NULL",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<Result<_, _>>()?
    };
    for (profile, id) in known {
        if !listed.iter().any(|a| a.id.as_bytes().as_slice() == id) {
            tx.execute(
                "UPDATE hub_accounts SET gone_at = ?2, devices = 0 WHERE profile_id = ?1",
                params![profile, now],
            )?;
            changed.push(ProfileId(profile));
        }
    }
    tx.commit()?;
    Ok(changed)
}

/// Stores the newest library key generation pushed into the account of `profile`.
pub fn set_keys_sent(
    conn: &Connection,
    profile: ProfileId,
    key_id: u32,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE hub_accounts SET keys_sent = max(keys_sent, ?2) WHERE profile_id = ?1",
        params![profile.0, key_id],
    )?;
    Ok(())
}

/// Stores the owner account's name (the name part of its address).
pub fn set_name(conn: &Connection, profile: ProfileId, name: &str) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE hub_accounts SET name = ?2 WHERE profile_id = ?1",
        params![profile.0, name],
    )?;
    Ok(())
}

/// Forgets the account of `profile` on this instance, with its shelf and outbox
/// (local annotations and positions stay); `false` when there was none.
pub fn forget_account(conn: &Connection, profile: ProfileId) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "DELETE FROM hub_accounts WHERE profile_id = ?1",
        [profile.0],
    )? == 1)
}

/// What [`queue_removal`] deletes on the hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removal {
    /// A member's account.
    Account(AccountId),
    /// A pending invite.
    Invite(AccountId),
}

/// Queues the deletion of an account or invite on the hub (the profile is gone or
/// going; the hub may be unreachable now).
pub fn queue_removal(conn: &Connection, removal: Removal) -> Result<(), StorageError> {
    let (id, kind) = match removal {
        Removal::Account(id) => (id, "account"),
        Removal::Invite(id) => (id, "invite"),
    };
    conn.execute(
        "INSERT OR IGNORE INTO hub_removals (public_id, kind) VALUES (?1, ?2)",
        params![id.as_bytes().as_slice(), kind],
    )?;
    Ok(())
}

/// The queued removals.
pub fn removals(conn: &Connection) -> Result<Vec<Removal>, StorageError> {
    let mut stmt = conn.prepare("SELECT public_id, kind FROM hub_removals")?;
    let rows = stmt.query_map([], |row| {
        let id = public_id(row.get(0)?, "public_id")?;
        Ok(if row.get::<_, String>(1)? == "invite" {
            Removal::Invite(id)
        } else {
            Removal::Account(id)
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Drops a removal that the hub carried out.
pub fn removal_done(conn: &Connection, removal: Removal) -> Result<(), StorageError> {
    let (Removal::Account(id) | Removal::Invite(id)) = removal;
    conn.execute(
        "DELETE FROM hub_removals WHERE public_id = ?1",
        [id.as_bytes().as_slice()],
    )?;
    Ok(())
}

/// An account of shelfgauge 0.18–0.20 kept for a last pull (decision 26 of Phase 30).
/// `Debug` hides the secrets.
#[derive(Clone)]
pub struct LegacyAccount {
    /// The profile.
    pub profile: ProfileId,
    /// Its hub.
    pub hub_url: HubUrl,
    /// The account key.
    pub key: AccountKey,
    /// This instance's device token.
    pub token: Secret,
    /// The last record `seq` pulled.
    pub last_seq: i64,
}

impl fmt::Debug for LegacyAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LegacyAccount")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

/// The accounts of 0.18–0.20 still waiting for their last pull.
pub fn legacy_accounts(conn: &Connection) -> Result<Vec<LegacyAccount>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT profile_id, hub_url, account_key, device_token, last_seq
         FROM hub_legacy_accounts ORDER BY profile_id",
    )?;
    let rows = stmt.query_map([], |row| {
        let url: String = row.get(1)?;
        Ok(LegacyAccount {
            profile: ProfileId(row.get(0)?),
            hub_url: HubUrl::parse(&url).map_err(|_| corrupt("hub_url"))?,
            key: key(row.get(2)?, "account_key")?,
            token: Secret::from_text(&row.get::<_, String>(3)?),
            last_seq: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Stores the pull cursor of an old account.
pub fn set_legacy_seq(conn: &Connection, profile: ProfileId, seq: i64) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE hub_legacy_accounts SET last_seq = max(last_seq, ?2) WHERE profile_id = ?1",
        params![profile.0, seq],
    )?;
    Ok(())
}

/// Forgets an old account (pulled, or the hub no longer has it).
pub fn forget_legacy(conn: &Connection, profile: ProfileId) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM hub_legacy_accounts WHERE profile_id = ?1",
        [profile.0],
    )?;
    Ok(())
}

/// Where a book on the shelf stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShelfState {
    /// Its blobs are being uploaded (or an upload was interrupted).
    Uploading,
    /// On the hub.
    Published,
    /// Deleted by the hub after `publication_expiry_days` without opening.
    Expired,
    /// Taken off the shelf; the hub's copy and records are still to be deleted.
    Removing,
}

impl ShelfState {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Uploading => "uploading",
            Self::Published => "published",
            Self::Expired => "expired",
            Self::Removing => "removing",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "uploading" => Self::Uploading,
            "published" => Self::Published,
            "expired" => Self::Expired,
            "removing" => Self::Removing,
            _ => return None,
        })
    }
}

/// How a shelf book reaches the devices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShelfKind {
    /// Its publication is on the hub.
    Upload,
    /// As book files (`.sgbook`) only: the hub never had it.
    File,
}

impl ShelfKind {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::File => "file",
        }
    }
}

/// A book on a profile's shelf. `Debug` hides the publication id and book key.
#[derive(Clone)]
pub struct ShelfEntry {
    /// The book's content key.
    pub content_key: Vec<u8>,
    /// The publication, once one is made.
    pub publication: Option<PublicationId>,
    /// The publication's book key.
    pub book_key: Option<BookKey>,
    /// Whether this instance published it.
    pub own: bool,
    /// On the hub or as files.
    pub kind: ShelfKind,
    /// Where it stands.
    pub state: ShelfState,
    /// The name of the library that published it.
    pub instance: String,
    /// The title.
    pub title: String,
    /// The authors joined with `, `.
    pub authors: String,
    /// When it was published (Unix seconds).
    pub published_at: Option<i64>,
    /// The last change of the shelf item (Unix seconds).
    pub updated_at: i64,
}

impl fmt::Debug for ShelfEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ShelfEntry")
            .field("own", &self.own)
            .field("kind", &self.kind)
            .field("state", &self.state)
            .field("updated_at", &self.updated_at)
            .finish_non_exhaustive()
    }
}

fn shelf_row(row: &Row<'_>) -> rusqlite::Result<ShelfEntry> {
    let publication: Option<Vec<u8>> = row.get("publication_id")?;
    let key: Option<Vec<u8>> = row.get("cipher_key")?;
    let state: String = row.get("state")?;
    let kind: String = row.get("kind")?;
    Ok(ShelfEntry {
        content_key: row.get("book_key")?,
        publication: publication
            .map(|p| p.try_into().map(PublicationId::new))
            .transpose()
            .map_err(|_| corrupt("publication_id"))?,
        book_key: key
            .map(|k| k.try_into().map(BookKey::new))
            .transpose()
            .map_err(|_| corrupt("cipher_key"))?,
        own: row.get("own")?,
        kind: if kind == "file" {
            ShelfKind::File
        } else {
            ShelfKind::Upload
        },
        state: ShelfState::parse(&state).ok_or_else(|| corrupt("state"))?,
        instance: row.get("instance")?,
        title: row.get("title")?,
        authors: row.get("authors")?,
        published_at: row.get("published_at")?,
        updated_at: row.get("updated_at")?,
    })
}

/// The shelf entry of the book with `content_key`, if it is on `profile`'s shelf.
pub fn shelf_entry(
    conn: &Connection,
    profile: ProfileId,
    content_key: &[u8],
) -> Result<Option<ShelfEntry>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT * FROM hub_shelf WHERE profile_id = ?1 AND book_key = ?2",
            params![profile.0, content_key],
            shelf_row,
        )
        .optional()?)
}

/// The books on `profile`'s shelf, oldest change first.
pub fn shelf(conn: &Connection, profile: ProfileId) -> Result<Vec<ShelfEntry>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT * FROM hub_shelf WHERE profile_id = ?1 ORDER BY updated_at, rowid")?;
    let rows = stmt.query_map([profile.0], shelf_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Puts a book this instance is publishing on the shelf (state `uploading`), replacing
/// an earlier entry of the same book. Call [`enqueue_book`] once it is published.
pub fn begin_publish(
    conn: &Connection,
    profile: ProfileId,
    entry: &ShelfEntry,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT OR REPLACE INTO hub_shelf
             (profile_id, book_key, publication_id, cipher_key, own, kind, state, instance,
              title, authors, published_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 1, 'upload', 'uploading', ?5, ?6, ?7, ?8, ?9)",
        params![
            profile.0,
            entry.content_key,
            entry.publication.map(|p| p.as_bytes().to_vec()),
            entry.book_key.as_ref().map(|k| k.as_bytes().to_vec()),
            entry.instance,
            entry.title,
            entry.authors,
            entry.published_at,
            entry.updated_at,
        ],
    )?;
    Ok(())
}

/// Puts a book this instance exported as a book file on the shelf (kind `file`, state
/// `published`: there is nothing to upload), replacing an earlier entry of the same
/// book. Call [`enqueue_book`] after it.
pub fn put_file(
    conn: &Connection,
    profile: ProfileId,
    entry: &ShelfEntry,
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT OR REPLACE INTO hub_shelf
             (profile_id, book_key, publication_id, cipher_key, own, kind, state, instance,
              title, authors, published_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, 1, 'file', 'published', ?5, ?6, ?7, ?8, ?9)",
        params![
            profile.0,
            entry.content_key,
            entry.publication.map(|p| p.as_bytes().to_vec()),
            entry.book_key.as_ref().map(|k| k.as_bytes().to_vec()),
            entry.instance,
            entry.title,
            entry.authors,
            entry.published_at,
            entry.updated_at,
        ],
    )?;
    Ok(())
}

/// Sets the state of a shelf entry (and its `updated_at` when `now` is given).
pub fn set_state(
    conn: &Connection,
    profile: ProfileId,
    content_key: &[u8],
    state: ShelfState,
    now: Option<i64>,
) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE hub_shelf SET state = ?3, updated_at = coalesce(?4, updated_at)
          WHERE profile_id = ?1 AND book_key = ?2",
        params![profile.0, content_key, state.as_str(), now],
    )?;
    Ok(())
}

/// Deletes a shelf entry (its records are pushed as deletions from the outbox).
pub fn delete_entry(
    conn: &Connection,
    profile: ProfileId,
    content_key: &[u8],
) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM hub_shelf WHERE profile_id = ?1 AND book_key = ?2",
        params![profile.0, content_key],
    )?;
    Ok(())
}

/// Queues the shelf item of a book with all its annotations and its reading position:
/// after publishing (rows of a book removed from the library are then skipped by the
/// push) or taking it off the shelf (all of them become deletions).
pub fn enqueue_book(
    conn: &Connection,
    profile: ProfileId,
    content_key: &[u8],
) -> Result<(), StorageError> {
    conn.execute(
        "INSERT OR IGNORE INTO hub_outbox (profile_id, kind, key) VALUES (?1, 'shelf_item', ?2)",
        params![profile.0, content_key],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO hub_outbox (profile_id, kind, key)
         SELECT profile_id, 'annotation', id FROM annotations
          WHERE profile_id = ?1 AND book_key = ?2",
        params![profile.0, content_key],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO hub_outbox (profile_id, kind, key)
         SELECT profile_id, 'position', book_key FROM reading_positions
          WHERE profile_id = ?1 AND book_key = ?2",
        params![profile.0, content_key],
    )?;
    Ok(())
}

/// The number of changes waiting to be pushed for `profile`.
pub fn outbox_len(conn: &Connection, profile: ProfileId) -> Result<i64, StorageError> {
    Ok(conn.query_row(
        "SELECT count(*) FROM hub_outbox WHERE profile_id = ?1",
        [profile.0],
        |row| row.get(0),
    )?)
}

/// The last record `seq` pulled for `profile`.
pub fn last_seq(conn: &Connection, profile: ProfileId) -> Result<i64, StorageError> {
    Ok(conn
        .query_row(
            "SELECT last_seq FROM hub_accounts WHERE profile_id = ?1",
            [profile.0],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Stores the pull cursor of `profile`.
pub fn set_last_seq(conn: &Connection, profile: ProfileId, seq: i64) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE hub_accounts SET last_seq = max(last_seq, ?2) WHERE profile_id = ?1",
        params![profile.0, seq],
    )?;
    Ok(())
}

/// Records the outcome of a sync: `Ok(time)` of a success, or the error's text.
pub fn record_sync(
    conn: &Connection,
    profile: ProfileId,
    outcome: Result<i64, String>,
) -> Result<(), StorageError> {
    match outcome {
        Ok(at) => conn.execute(
            "UPDATE hub_accounts SET synced_at = ?2, sync_error = NULL WHERE profile_id = ?1",
            params![profile.0, at],
        )?,
        Err(error) => conn.execute(
            "UPDATE hub_accounts SET sync_error = ?2 WHERE profile_id = ?1",
            params![profile.0, error],
        )?,
    };
    Ok(())
}

/// The content keys on any profile's hub shelf (not being removed): their marks are
/// kept even while this library lacks the book.
pub fn shelved_keys(conn: &Connection) -> Result<std::collections::HashSet<Vec<u8>>, StorageError> {
    let mut stmt =
        conn.prepare("SELECT DISTINCT book_key FROM hub_shelf WHERE state <> 'removing'")?;
    let rows = stmt.query_map([], |row| row.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::store::UserData;

    fn library(conn: &Connection) -> AccountKey {
        let url = HubUrl::parse("https://hub.example.org").unwrap();
        let key = AccountKey::new([4; 32]);
        save_library(
            conn,
            &url,
            AccountId::new([7; 16]),
            "home",
            &Secret::from_text("token"),
            &AccountKey::new([5; 32]),
            ProfileId::OWNER,
            AccountId::new([9; 16]),
            &key,
            "admin",
            100,
        )
        .unwrap();
        key
    }

    fn reader(conn: &Connection, id: i64) -> ProfileId {
        conn.execute(
            "INSERT INTO profiles (id, name, role, created_at) VALUES (?1, ?2, 'reader', 0)",
            params![id, format!("P{id}")],
        )
        .unwrap();
        ProfileId(id)
    }

    #[test]
    fn the_library_keeps_its_owner_account_and_key_generations() {
        let userdata = UserData::open_in_memory().unwrap();
        let conn = userdata.lock();
        library(&conn);
        let saved = super::library(&conn).unwrap().unwrap();
        assert_eq!(saved.handle, "home");
        assert_eq!(saved.library_id, AccountId::new([7; 16]));
        assert!(!format!("{saved:?}").contains("token"));
        let owner = account(&conn, ProfileId::OWNER).unwrap().unwrap();
        assert_eq!(owner.role, Role::Owner);
        assert_eq!(owner.account, Some(AccountId::new([9; 16])));
        assert_eq!(owner.name.as_deref(), Some("admin"));
        assert_eq!(owner.keys_sent, 1);
        assert!(owner.on_hub());
        assert_eq!(current_library_key(&conn).unwrap().unwrap().0, 1);
        assert_eq!(
            add_library_key(&conn, &AccountKey::new([6; 32]), 200).unwrap(),
            2
        );
        let keys = library_keys(&conn).unwrap();
        assert_eq!(keys.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(
            current_library_key(&conn).unwrap().unwrap().1,
            AccountKey::new([6; 32])
        );
        forget_library(&conn).unwrap();
        assert!(super::library(&conn).unwrap().is_none());
        assert!(accounts(&conn).unwrap().is_empty());
        assert!(library_keys(&conn).unwrap().is_empty());
    }

    #[test]
    fn invited_accounts_join_from_the_list_and_go_when_missing_from_it() {
        let userdata = UserData::open_in_memory().unwrap();
        let mut conn = userdata.lock();
        library(&conn);
        let olga = reader(&conn, 2);
        let ivan = reader(&conn, 3);
        let invite = |id: u8, kind| PendingInvite {
            id: AccountId::new([id; 16]),
            kind,
            expires_at: 900,
        };
        save_invite(
            &conn,
            olga,
            &AccountKey::new([1; 32]),
            &invite(0x11, InviteKind::New),
            1,
        )
        .unwrap();
        save_invite(
            &conn,
            ivan,
            &AccountKey::new([2; 32]),
            &invite(0x22, InviteKind::New),
            1,
        )
        .unwrap();
        let pending = account(&conn, olga).unwrap().unwrap();
        assert_eq!(pending.account, None);
        assert!(!pending.on_hub());
        assert_eq!(pending.invite.as_ref().unwrap().kind, InviteKind::New);
        let listed = |id: u8, invite: Option<u8>, name: &str| ListedAccount {
            id: AccountId::new([id; 16]),
            name: name.to_owned(),
            devices: 2,
            last_seen_at: Some(50),
            invite: invite.map(|i| AccountId::new([i; 16])),
        };
        // Olga redeemed her invite; Ivan did not yet.
        let changed = apply_account_list(
            &mut conn,
            &[listed(9, None, "admin"), listed(0xa1, Some(0x11), "olga")],
            60,
        )
        .unwrap();
        assert_eq!(changed, [olga]);
        let joined = account(&conn, olga).unwrap().unwrap();
        assert_eq!(joined.account, Some(AccountId::new([0xa1; 16])));
        assert_eq!(joined.name.as_deref(), Some("olga"));
        assert_eq!((joined.devices, joined.last_seen_at), (2, Some(50)));
        assert!(joined.invite.is_none());
        assert!(account(&conn, ivan).unwrap().unwrap().invite.is_some());
        // A reset keeps the key; a new invite after the account went gives a new key.
        save_invite(
            &conn,
            olga,
            &AccountKey::new([8; 32]),
            &invite(0x33, InviteKind::Reset),
            70,
        )
        .unwrap();
        assert_eq!(
            account(&conn, olga).unwrap().unwrap().key,
            AccountKey::new([1; 32])
        );
        clear_invite(&conn, olga).unwrap();
        // Olga left: missing from the list.
        let changed = apply_account_list(&mut conn, &[listed(9, None, "admin")], 80).unwrap();
        assert_eq!(changed, [olga]);
        let gone = account(&conn, olga).unwrap().unwrap();
        assert_eq!(gone.gone_at, Some(80));
        assert!(!gone.on_hub());
        save_invite(
            &conn,
            olga,
            &AccountKey::new([8; 32]),
            &invite(0x44, InviteKind::New),
            90,
        )
        .unwrap();
        let again = account(&conn, olga).unwrap().unwrap();
        assert_eq!((again.account, again.gone_at), (None, None));
        assert_eq!(again.key, AccountKey::new([8; 32]));
    }

    #[test]
    fn removals_wait_in_a_queue() {
        let userdata = UserData::open_in_memory().unwrap();
        let conn = userdata.lock();
        let account = Removal::Account(AccountId::new([1; 16]));
        let invite = Removal::Invite(AccountId::new([2; 16]));
        queue_removal(&conn, account).unwrap();
        queue_removal(&conn, invite).unwrap();
        queue_removal(&conn, account).unwrap();
        assert_eq!(removals(&conn).unwrap().len(), 2);
        removal_done(&conn, account).unwrap();
        assert_eq!(removals(&conn).unwrap(), [invite]);
    }

    #[test]
    fn the_outbox_gets_changes_of_shelf_books_only() {
        let userdata = UserData::open_in_memory().unwrap();
        let conn = userdata.lock();
        library(&conn);
        conn.execute(
            "INSERT INTO hub_shelf (profile_id, book_key, state, instance, title, authors, updated_at)
             VALUES (1, X'aa', 'published', 'Home', 'T', 'A', 1),
                    (1, X'cc', 'removing', 'Home', 'T', 'A', 1)",
            [],
        )
        .unwrap();
        let annotate = |id: &str, book: &str, reason: Option<&str>| {
            conn.execute(
                &format!(
                    "INSERT INTO annotations (id, profile_id, book_key, book_id, start_block,
                         start_offset, end_block, end_offset, text, prefix, suffix,
                         book_title, book_authors, created_at, updated_at, deleted_at,
                         deleted_reason)
                     VALUES (?1, 1, X'{book}', 1, 0, 0, 0, 1, 't', '', '', 'T', 'A', 1, 1,
                         ?2, ?3)"
                ),
                params![id, reason.map(|_| 1), reason],
            )
            .unwrap();
        };
        annotate("on-shelf", "aa", None);
        annotate("not-on-shelf", "bb", None);
        annotate("being-removed", "cc", None);
        annotate("book-removed", "aa", Some("book_removed"));
        annotate("deleted-by-user", "aa", Some("user"));
        conn.execute(
            "INSERT INTO reading_positions (profile_id, book_key, book_id, block, suffix,
                 progress, book_title, book_authors, updated_at)
             VALUES (1, X'aa', 1, 3, '', 10, 'T', 'A', 1), (1, X'bb', 1, 3, '', 10, 'T', 'A', 1)",
            [],
        )
        .unwrap();
        // An update of a shelf book's position is queued once.
        conn.execute("UPDATE reading_positions SET block = 4", [])
            .unwrap();

        let mut stmt = conn
            .prepare("SELECT kind, quote(key) FROM hub_outbox ORDER BY kind, key")
            .unwrap();
        let queued: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            queued,
            [
                ("annotation".to_owned(), "'deleted-by-user'".to_owned()),
                ("annotation".to_owned(), "'on-shelf'".to_owned()),
                ("position".to_owned(), "X'AA'".to_owned()),
            ]
        );

        // Leaving the account clears its shelf and outbox.
        forget_account(&conn, ProfileId::OWNER).unwrap();
        let left: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM hub_outbox) + (SELECT count(*) FROM hub_shelf)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }
}
