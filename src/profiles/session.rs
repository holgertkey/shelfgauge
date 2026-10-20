//! Profile sessions: the token in the `shelfgauge-profile` cookie.
//!
//! A token is 32 random bytes from the operating system (`getrandom`), sent as
//! unpadded base64url; the database keeps only its SHA-256 (`profile_sessions`), so a
//! copy of `userdata.sqlite` holds no usable token. Tokens are never logged: [`Token`]
//! has no `Display` and a redacted `Debug`.
//!
//! A session has a [`Level`]: a *local* one comes from the picker or a PIN, a
//! *password* one from a password login. Only password sessions serve requests from
//! other devices. A session also remembers whether a remote request created it.

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

/// How a session was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Chosen in the picker, with or without a PIN: this computer only.
    Local,
    /// Logged in with the profile's password: also valid from other devices.
    Password,
}

impl Level {
    fn to_sql(self) -> i64 {
        match self {
            Self::Local => 0,
            Self::Password => 1,
        }
    }

    fn from_sql(value: i64) -> Self {
        if value == 1 {
            Self::Password
        } else {
            Self::Local
        }
    }
}

/// A session found by [`lookup`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Session {
    /// Its profile.
    pub profile: ProfileId,
    /// How it was started.
    pub level: Level,
    /// Whether a request from another device started it.
    pub remote: bool,
}

/// A new random token.
fn new_token() -> Result<Token, StorageError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| StorageError::Random(e.to_string()))?;
    Ok(Token(URL_SAFE_NO_PAD.encode(bytes)))
}

/// Starts a session of `profile` at `level`, `remote` if a request from another
/// device starts it; returns its token.
///
/// # Examples
///
/// ```
/// use shelfgauge::annotations::store::UserData;
/// use shelfgauge::profiles::{ProfileId, session};
/// use shelfgauge::profiles::session::Level;
///
/// let data = UserData::open_in_memory().unwrap();
/// let conn = data.lock();
/// let token = session::create(&conn, ProfileId::OWNER, Level::Local, false, 100).unwrap();
/// let found = session::lookup(&conn, &token, 200).unwrap().unwrap();
/// assert_eq!((found.profile, found.level), (ProfileId::OWNER, Level::Local));
/// session::end(&conn, &token).unwrap();
/// assert_eq!(session::lookup(&conn, &token, 300).unwrap(), None);
/// ```
pub fn create(
    conn: &Connection,
    profile: ProfileId,
    level: Level,
    remote: bool,
    now: i64,
) -> Result<Token, StorageError> {
    let token = new_token()?;
    conn.execute(
        "INSERT INTO profile_sessions (token_hash, profile_id, created_at, last_seen_at,
             level, remote)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5)",
        params![token.hash(), profile, now, level.to_sql(), remote],
    )?;
    Ok(token)
}

/// A session, updating its `last_seen_at` at most once an hour.
pub fn lookup(conn: &Connection, token: &Token, now: i64) -> Result<Option<Session>, StorageError> {
    let hash = token.hash();
    let found: Option<(Session, i64)> = conn
        .query_row(
            "SELECT profile_id, last_seen_at, level, remote FROM profile_sessions
             WHERE token_hash = ?1",
            [&hash],
            |row| {
                Ok((
                    Session {
                        profile: row.get(0)?,
                        level: Level::from_sql(row.get(2)?),
                        remote: row.get(3)?,
                    },
                    row.get(1)?,
                ))
            },
        )
        .optional()?;
    let Some((session, seen)) = found else {
        return Ok(None);
    };
    if now - seen >= SEEN_INTERVAL {
        conn.execute(
            "UPDATE profile_sessions SET last_seen_at = ?2 WHERE token_hash = ?1",
            params![hash, now],
        )?;
    }
    Ok(Some(session))
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

/// Ends every password session of `profile` except `keep`; returns how many ended
/// (after a password change or removal: the local sessions stay).
pub fn end_password_sessions(
    conn: &Connection,
    profile: ProfileId,
    keep: Option<&Token>,
) -> Result<usize, StorageError> {
    let keep = keep.map(Token::hash).unwrap_or_default();
    Ok(conn.execute(
        "DELETE FROM profile_sessions
         WHERE profile_id = ?1 AND token_hash != ?2 AND level = 1",
        params![profile, keep],
    )?)
}

/// Replaces a session's token with a fresh one of the same level and origin (after a
/// PIN or password change); returns the new token, or `None` if there was no such
/// session.
pub fn renew(conn: &Connection, token: &Token, now: i64) -> Result<Option<Token>, StorageError> {
    let Some(session) = lookup(conn, token, now)? else {
        return Ok(None);
    };
    end(conn, token)?;
    create(conn, session.profile, session.level, session.remote, now).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::store::UserData;
    use crate::profiles::{Role, store};

    fn local(conn: &Connection, profile: ProfileId) -> Token {
        create(conn, profile, Level::Local, false, 0).unwrap()
    }

    fn profile_of(conn: &Connection, token: &Token) -> Option<ProfileId> {
        lookup(conn, token, 1).unwrap().map(|s| s.profile)
    }

    #[test]
    fn tokens_are_random_and_stored_hashed() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let a = local(&conn, ProfileId::OWNER);
        let b = local(&conn, ProfileId::OWNER);
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
        let token = create(&conn, ProfileId::OWNER, Level::Local, false, 1000).unwrap();
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
        let mine = local(&conn, anna.id);
        let other = local(&conn, anna.id);
        let owner = local(&conn, ProfileId::OWNER);

        let fresh = renew(&conn, &mine, 1).unwrap().unwrap();
        assert_eq!(lookup(&conn, &mine, 1).unwrap(), None);
        assert_eq!(end_others(&conn, anna.id, Some(&fresh)).unwrap(), 1);
        assert_eq!(profile_of(&conn, &other), None);
        assert_eq!(profile_of(&conn, &fresh), Some(anna.id));
        assert!(renew(&conn, &other, 1).unwrap().is_none());

        store::delete(&conn, anna.id).unwrap();
        assert_eq!(profile_of(&conn, &fresh), None, "cascade");
        assert_eq!(profile_of(&conn, &owner), Some(ProfileId::OWNER));
    }

    #[test]
    fn levels_survive_renewal_and_password_sessions_end_alone() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let at_home = local(&conn, ProfileId::OWNER);
        let phone = create(&conn, ProfileId::OWNER, Level::Password, true, 0).unwrap();
        let laptop = create(&conn, ProfileId::OWNER, Level::Password, false, 0).unwrap();
        let found = lookup(&conn, &phone, 1).unwrap().unwrap();
        assert_eq!((found.level, found.remote), (Level::Password, true));

        let phone = renew(&conn, &phone, 1).unwrap().unwrap();
        let found = lookup(&conn, &phone, 1).unwrap().unwrap();
        assert_eq!((found.level, found.remote), (Level::Password, true));

        assert_eq!(
            end_password_sessions(&conn, ProfileId::OWNER, Some(&phone)).unwrap(),
            1
        );
        assert_eq!(profile_of(&conn, &laptop), None);
        assert_eq!(profile_of(&conn, &phone), Some(ProfileId::OWNER));
        assert_eq!(profile_of(&conn, &at_home), Some(ProfileId::OWNER));
        assert_eq!(
            end_password_sessions(&conn, ProfileId::OWNER, None).unwrap(),
            1
        );
        assert_eq!(profile_of(&conn, &phone), None);
        assert_eq!(
            lookup(&conn, &at_home, 1).unwrap().unwrap().level,
            Level::Local
        );
    }
}
