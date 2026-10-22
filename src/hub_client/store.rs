//! The hub tables of `userdata.sqlite` (schema version 5, see
//! [`crate::annotations::store`]): a profile's hub account, its shelf and the outbox.
//!
//! Functions take a [`Connection`] of the user-data database and never open a
//! transaction of their own unless they say so.

use std::fmt;

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::HubUrl;
use super::keys::AccountKey;
use crate::hub::{AccountId, Secret};
use crate::profiles::ProfileId;
use crate::storage::StorageError;

/// A profile's hub account as this instance holds it. `Debug` hides the secrets.
#[derive(Clone)]
pub struct HubAccount {
    /// The profile.
    pub profile: ProfileId,
    /// The hub it was made on (or joined).
    pub hub_url: HubUrl,
    /// The public account id.
    pub account: AccountId,
    /// The account key.
    pub key: AccountKey,
    /// The account key's id (1 until key rotation exists).
    pub key_id: i64,
    /// This instance's device id on the hub.
    pub device_id: i64,
    /// This instance's device token.
    pub token: Secret,
    /// Whether the recovery code still waits for "I saved it".
    pub recovery_pending: bool,
    /// When this instance registered or joined (Unix seconds).
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
            .field("hub_url", &self.hub_url)
            .field("device_id", &self.device_id)
            .finish_non_exhaustive()
    }
}

/// What [`save_account`] stores.
#[derive(Debug, Clone)]
pub struct NewHubAccount<'a> {
    /// The hub.
    pub hub_url: &'a HubUrl,
    /// The public account id.
    pub account: AccountId,
    /// The account key.
    pub key: &'a AccountKey,
    /// This instance's device id.
    pub device_id: i64,
    /// This instance's device token.
    pub token: &'a Secret,
    /// The recovery code text to show until it is confirmed (`None` when joining).
    pub recovery_code: Option<&'a str>,
}

/// A stored value is not what this build writes.
fn corrupt(what: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(0, what.to_owned(), rusqlite::types::Type::Blob)
}

fn account_row(row: &Row<'_>) -> rusqlite::Result<HubAccount> {
    let url: String = row.get("hub_url")?;
    let id: Vec<u8> = row.get("account_id")?;
    let key: Vec<u8> = row.get("account_key")?;
    let token: String = row.get("device_token")?;
    Ok(HubAccount {
        profile: ProfileId(row.get("profile_id")?),
        hub_url: HubUrl::parse(&url).map_err(|_| corrupt("hub_url"))?,
        account: AccountId::new(id.try_into().map_err(|_| corrupt("account_id"))?),
        key: AccountKey::new(key.try_into().map_err(|_| corrupt("account_key"))?),
        key_id: row.get("key_id")?,
        device_id: row.get("device_id")?,
        token: Secret::from_text(&token),
        recovery_pending: row.get::<_, Option<String>>("recovery_code")?.is_some(),
        created_at: row.get("created_at")?,
        last_seq: row.get("last_seq")?,
        synced_at: row.get("synced_at")?,
        sync_error: row.get("sync_error")?,
    })
}

/// The hub account of `profile`, if it has one.
pub fn account(conn: &Connection, profile: ProfileId) -> Result<Option<HubAccount>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT * FROM hub_accounts WHERE profile_id = ?1",
            [profile.0],
            account_row,
        )
        .optional()?)
}

/// The profiles with a hub account.
pub fn accounts(conn: &Connection) -> Result<Vec<HubAccount>, StorageError> {
    let mut stmt = conn.prepare("SELECT * FROM hub_accounts ORDER BY profile_id")?;
    let rows = stmt.query_map([], account_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Stores the hub account of `profile`; `false` when it already has one (nothing
/// changed).
pub fn save_account(
    conn: &Connection,
    profile: ProfileId,
    new: &NewHubAccount<'_>,
    now: i64,
) -> Result<bool, StorageError> {
    let inserted = conn.execute(
        "INSERT OR IGNORE INTO hub_accounts
             (profile_id, hub_url, account_id, account_key, device_id, device_token,
              recovery_code, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            profile.0,
            new.hub_url.to_string(),
            new.account.as_bytes().as_slice(),
            new.key.as_bytes().as_slice(),
            new.device_id,
            new.token.as_str(),
            new.recovery_code,
            now,
        ],
    )?;
    Ok(inserted == 1)
}

/// The recovery code of `profile` while it waits for "I saved it".
pub fn pending_recovery_code(
    conn: &Connection,
    profile: ProfileId,
) -> Result<Option<String>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT recovery_code FROM hub_accounts WHERE profile_id = ?1",
            [profile.0],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

/// Forgets the recovery code of `profile` ("I saved it").
pub fn confirm_recovery_code(conn: &Connection, profile: ProfileId) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE hub_accounts SET recovery_code = NULL WHERE profile_id = ?1",
        [profile.0],
    )?;
    Ok(())
}

/// Forgets the hub account of `profile` on this instance, with its shelf and outbox
/// (local annotations and positions stay); `false` when there was none.
pub fn forget_account(conn: &Connection, profile: ProfileId) -> Result<bool, StorageError> {
    Ok(conn.execute(
        "DELETE FROM hub_accounts WHERE profile_id = ?1",
        [profile.0],
    )? == 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::store::UserData;

    fn new_account<'a>(
        url: &'a HubUrl,
        key: &'a AccountKey,
        token: &'a Secret,
    ) -> NewHubAccount<'a> {
        NewHubAccount {
            hub_url: url,
            account: AccountId::new([9; 16]),
            key,
            device_id: 3,
            token,
            recovery_code: Some("SG1-CODE"),
        }
    }

    #[test]
    fn accounts_are_saved_once_per_profile_and_forgotten() {
        let userdata = UserData::open_in_memory().unwrap();
        let conn = userdata.lock();
        let url = HubUrl::parse("https://hub.example.org").unwrap();
        let key = AccountKey::new([4; 32]);
        let token = Secret::from_text("token");
        let owner = ProfileId::OWNER;
        assert!(account(&conn, owner).unwrap().is_none());
        assert!(save_account(&conn, owner, &new_account(&url, &key, &token), 100).unwrap());
        assert!(!save_account(&conn, owner, &new_account(&url, &key, &token), 200).unwrap());

        let saved = account(&conn, owner).unwrap().unwrap();
        assert_eq!(saved.hub_url, url);
        assert_eq!(saved.account, AccountId::new([9; 16]));
        assert_eq!(saved.key, key);
        assert_eq!(saved.token, token);
        assert_eq!(
            (saved.device_id, saved.created_at, saved.last_seq),
            (3, 100, 0)
        );
        assert!(saved.recovery_pending);
        assert!(!format!("{saved:?}").contains("token"));

        assert_eq!(
            pending_recovery_code(&conn, owner).unwrap().as_deref(),
            Some("SG1-CODE")
        );
        confirm_recovery_code(&conn, owner).unwrap();
        assert_eq!(pending_recovery_code(&conn, owner).unwrap(), None);
        assert!(!account(&conn, owner).unwrap().unwrap().recovery_pending);
        assert_eq!(accounts(&conn).unwrap().len(), 1);

        assert!(forget_account(&conn, owner).unwrap());
        assert!(!forget_account(&conn, owner).unwrap());
        assert!(account(&conn, owner).unwrap().is_none());
    }

    #[test]
    fn the_outbox_gets_changes_of_shelf_books_only() {
        let userdata = UserData::open_in_memory().unwrap();
        let conn = userdata.lock();
        let url = HubUrl::parse("https://hub.example.org").unwrap();
        let key = AccountKey::new([4; 32]);
        let token = Secret::from_text("token");
        save_account(&conn, ProfileId::OWNER, &new_account(&url, &key, &token), 1).unwrap();
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
