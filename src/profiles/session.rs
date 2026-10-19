//! Profile sessions: the token in the `shelfgauge-profile` cookie.
//!
//! A token is 32 random bytes from the operating system (`getrandom`), sent as
//! unpadded base64url; the database keeps only its SHA-256 (`profile_sessions`), so a
//! copy of `userdata.sqlite` holds no usable token. Tokens are never logged: [`Token`]
//! has no `Display` and a redacted `Debug`.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::ProfileId;
use crate::storage::StorageError;

/// `last_seen_at` is written at most this often (seconds).
const SEEN_INTERVAL: i64 = 3600;

/// A session token as sent in the cookie.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl Token {
    /// A token received from a cookie (not checked: [`lookup`] finds it or not).
    pub fn from_cookie(value: &str) -> Self {
        Self(value.to_owned())
    }

    /// The cookie value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn hash(&self) -> [u8; 32] {
        Sha256::digest(self.0.as_bytes()).into()
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// A new random token.
fn new_token() -> Result<Token, StorageError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| StorageError::Random(e.to_string()))?;
    Ok(Token(URL_SAFE_NO_PAD.encode(bytes)))
}

/// Starts a session of `profile`; returns its token.
///
/// # Examples
///
/// ```
/// use shelfgauge::annotations::store::UserData;
/// use shelfgauge::profiles::{ProfileId, session};
///
/// let data = UserData::open_in_memory().unwrap();
/// let conn = data.lock();
/// let token = session::create(&conn, ProfileId::OWNER, 100).unwrap();
/// assert_eq!(session::lookup(&conn, &token, 200).unwrap(), Some(ProfileId::OWNER));
/// session::end(&conn, &token).unwrap();
/// assert_eq!(session::lookup(&conn, &token, 300).unwrap(), None);
/// ```
pub fn create(conn: &Connection, profile: ProfileId, now: i64) -> Result<Token, StorageError> {
    let token = new_token()?;
    conn.execute(
        "INSERT INTO profile_sessions (token_hash, profile_id, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?3)",
        params![token.hash(), profile, now],
    )?;
    Ok(token)
}

/// The profile of a session, updating its `last_seen_at` at most once an hour.
pub fn lookup(
    conn: &Connection,
    token: &Token,
    now: i64,
) -> Result<Option<ProfileId>, StorageError> {
    let hash = token.hash();
    let found: Option<(ProfileId, i64)> = conn
        .query_row(
            "SELECT profile_id, last_seen_at FROM profile_sessions WHERE token_hash = ?1",
            [&hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((profile, seen)) = found else {
        return Ok(None);
    };
    if now - seen >= SEEN_INTERVAL {
        conn.execute(
            "UPDATE profile_sessions SET last_seen_at = ?2 WHERE token_hash = ?1",
            params![hash, now],
        )?;
    }
    Ok(Some(profile))
}

/// Ends a session.
pub fn end(conn: &Connection, token: &Token) -> Result<(), StorageError> {
    conn.execute(
        "DELETE FROM profile_sessions WHERE token_hash = ?1",
        [token.hash()],
    )?;
    Ok(())
}

/// Ends every session of `profile` except `keep`; returns how many ended.
pub fn end_others(
    conn: &Connection,
    profile: ProfileId,
    keep: Option<&Token>,
) -> Result<usize, StorageError> {
    let keep = keep.map(Token::hash).unwrap_or_default();
    Ok(conn.execute(
        "DELETE FROM profile_sessions WHERE profile_id = ?1 AND token_hash != ?2",
        params![profile, keep],
    )?)
}

/// Replaces a session's token with a fresh one (after a PIN change); returns the new
/// token, or `None` if there was no such session.
pub fn renew(conn: &Connection, token: &Token, now: i64) -> Result<Option<Token>, StorageError> {
    let Some(profile) = lookup(conn, token, now)? else {
        return Ok(None);
    };
    end(conn, token)?;
    create(conn, profile, now).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::store::UserData;
    use crate::profiles::{Role, store};

    #[test]
    fn tokens_are_random_and_stored_hashed() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let a = create(&conn, ProfileId::OWNER, 0).unwrap();
        let b = create(&conn, ProfileId::OWNER, 0).unwrap();
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 43, "32 bytes in unpadded base64url");
        assert_eq!(format!("{a:?}"), "Token(..)");
        let stored: Vec<u8> = conn
            .query_row("SELECT token_hash FROM profile_sessions LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(stored.len(), 32);
        assert!(stored != a.as_str().as_bytes() && stored != b.as_str().as_bytes());
        assert_eq!(
            lookup(&conn, &Token::from_cookie("forged"), 0).unwrap(),
            None
        );
    }

    #[test]
    fn last_seen_is_written_at_most_hourly() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let token = create(&conn, ProfileId::OWNER, 1000).unwrap();
        let seen = |conn: &Connection| -> i64 {
            conn.query_row("SELECT last_seen_at FROM profile_sessions", [], |r| {
                r.get(0)
            })
            .unwrap()
        };
        lookup(&conn, &token, 1000 + SEEN_INTERVAL - 1).unwrap();
        assert_eq!(seen(&conn), 1000);
        lookup(&conn, &token, 1000 + SEEN_INTERVAL).unwrap();
        assert_eq!(seen(&conn), 1000 + SEEN_INTERVAL);
    }

    #[test]
    fn renew_end_others_and_profile_deletion() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let anna = store::create(&conn, "Anna", 1, Role::Reader, 0).unwrap();
        let mine = create(&conn, anna.id, 0).unwrap();
        let other = create(&conn, anna.id, 0).unwrap();
        let owner = create(&conn, ProfileId::OWNER, 0).unwrap();

        let fresh = renew(&conn, &mine, 1).unwrap().unwrap();
        assert_eq!(lookup(&conn, &mine, 1).unwrap(), None);
        assert_eq!(end_others(&conn, anna.id, Some(&fresh)).unwrap(), 1);
        assert_eq!(lookup(&conn, &other, 1).unwrap(), None);
        assert_eq!(lookup(&conn, &fresh, 1).unwrap(), Some(anna.id));
        assert!(renew(&conn, &other, 1).unwrap().is_none());

        store::delete(&conn, anna.id).unwrap();
        assert_eq!(lookup(&conn, &fresh, 1).unwrap(), None, "cascade");
        assert_eq!(lookup(&conn, &owner, 1).unwrap(), Some(ProfileId::OWNER));
    }
}
