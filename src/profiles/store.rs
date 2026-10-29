//! Profiles in the user-data database (table `profiles`, schema v4).
//!
//! Functions take a [`Connection`] to `userdata.sqlite` (see
//! [`UserData`](crate::annotations::store::UserData)). Names are trimmed, 1–32
//! characters without control characters, and unique ignoring case (Unicode
//! lowercasing here; the column's `COLLATE NOCASE` is a backstop for ASCII). The owner
//! profile cannot be deleted or lose the admin role, and there are at most
//! [`MAX_PROFILES`].

use rusqlite::{Connection, OptionalExtension, Row, params};

use super::{COLORS, MAX_NAME_CHARS, MAX_PROFILES, Profile, ProfileId, ProfileSettings, Role};
use crate::storage::StorageError;

/// Why a profile change was refused.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// The name is empty, too long or has control characters.
    #[error("{0}")]
    InvalidName(&'static str),
    /// Another profile has this name (ignoring case).
    #[error("another profile already has this name")]
    DuplicateName,
    /// There are already [`MAX_PROFILES`] profiles.
    #[error("there can be at most {MAX_PROFILES} profiles")]
    TooMany,
    /// The change would break a rule of the owner profile.
    #[error("the owner profile {0}")]
    Owner(&'static str),
    /// The color is not a palette index.
    #[error("unknown color")]
    InvalidColor,
    /// No such profile.
    #[error("no such profile")]
    NotFound,
    /// The database failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl From<rusqlite::Error> for ProfileError {
    fn from(e: rusqlite::Error) -> Self {
        match e {
            rusqlite::Error::SqliteFailure(f, _)
                if f.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                Self::DuplicateName
            }
            e => Self::Storage(e.into()),
        }
    }
}

/// A valid profile name: `name` trimmed.
///
/// # Examples
///
/// ```
/// use shelfgauge::profiles::store::valid_name;
///
/// assert_eq!(valid_name("  Anna ").unwrap(), "Anna");
/// assert!(valid_name(" ").is_err());
/// assert!(valid_name("a\u{7}b").is_err());
/// ```
pub fn valid_name(name: &str) -> Result<String, ProfileError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ProfileError::InvalidName("the name is empty"));
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(ProfileError::InvalidName(
            "the name is longer than 32 characters",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(ProfileError::InvalidName("the name has control characters"));
    }
    Ok(name.to_owned())
}

/// Two names are the same profile name if they are equal after Unicode lowercasing.
fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

const COLUMNS: &str = "p.id, p.name, p.color, p.role, p.pin_hash IS NOT NULL, p.share_activity, \
    p.settings, p.created_at, \
    (SELECT max(last_seen_at) FROM profile_sessions s WHERE s.profile_id = p.id), \
    p.password_hash IS NOT NULL, p.hub_only";

fn profile_row(row: &Row<'_>) -> rusqlite::Result<Profile> {
    let role: String = row.get(3)?;
    let settings: Option<String> = row.get(6)?;
    Ok(Profile {
        id: row.get(0)?,
        name: row.get(1)?,
        color: row.get::<_, u8>(2)?.min(COLORS - 1),
        role: Role::parse(&role).unwrap_or(Role::Reader),
        has_pin: row.get(4)?,
        share_activity: row.get(5)?,
        settings: settings.as_deref().map(ProfileSettings::parse),
        created_at: row.get(7)?,
        last_seen_at: row.get(8)?,
        has_password: row.get(9)?,
        hub_only: row.get(10)?,
    })
}

/// All profiles, the owner first, then in the order they were created.
pub fn list(conn: &Connection) -> Result<Vec<Profile>, StorageError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM profiles p ORDER BY p.id != 1, p.sort, p.id"
    ))?;
    let rows = stmt.query_map([], profile_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The profiles a person picks on the computer or logs in to: all but those made by a
/// hub invite ([`Profile::hub_only`]).
pub fn list_local(conn: &Connection) -> Result<Vec<Profile>, StorageError> {
    let mut profiles = list(conn)?;
    profiles.retain(|p| !p.hub_only);
    Ok(profiles)
}

/// The number of profiles a person picks on the computer ([`list_local`]): with one,
/// nobody is asked who is reading.
pub fn count_local(conn: &Connection) -> Result<usize, StorageError> {
    Ok(conn.query_row(
        "SELECT count(*) FROM profiles WHERE hub_only = 0",
        [],
        |row| row.get::<_, i64>(0),
    )? as usize)
}

/// Marks profile `id` as made by a hub invite (or not).
pub fn set_hub_only(conn: &Connection, id: ProfileId, hub_only: bool) -> Result<(), StorageError> {
    conn.execute(
        "UPDATE profiles SET hub_only = ?2 WHERE id = ?1",
        rusqlite::params![id, hub_only],
    )?;
    Ok(())
}

/// The number of profiles.
pub fn count(conn: &Connection) -> Result<usize, StorageError> {
    Ok(conn.query_row("SELECT count(*) FROM profiles", [], |row| {
        row.get::<_, i64>(0)
    })? as usize)
}

/// The profile with `id`.
pub fn get(conn: &Connection, id: ProfileId) -> Result<Option<Profile>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM profiles p WHERE p.id = ?1"),
            [id],
            profile_row,
        )
        .optional()?)
}

/// The profile named `name` (ignoring case and surrounding whitespace).
pub fn by_name(conn: &Connection, name: &str) -> Result<Option<Profile>, StorageError> {
    let name = name.trim();
    Ok(list(conn)?.into_iter().find(|p| same_name(&p.name, name)))
}

/// Refuses `name` if a profile other than `except` has it.
fn check_unique(
    conn: &Connection,
    name: &str,
    except: Option<ProfileId>,
) -> Result<(), ProfileError> {
    let taken = list(conn)?
        .iter()
        .any(|p| Some(p.id) != except && same_name(&p.name, name));
    if taken {
        Err(ProfileError::DuplicateName)
    } else {
        Ok(())
    }
}

fn check_color(color: u8) -> Result<(), ProfileError> {
    if color < COLORS {
        Ok(())
    } else {
        Err(ProfileError::InvalidColor)
    }
}

/// Creates a profile without a PIN and returns it.
///
/// # Examples
///
/// ```
/// use shelfgauge::annotations::store::UserData;
/// use shelfgauge::profiles::{Role, store};
///
/// let data = UserData::open_in_memory().unwrap();
/// let conn = data.lock();
/// let anna = store::create(&conn, "Anna", 2, Role::Reader, 0).unwrap();
/// assert_eq!(store::count(&conn).unwrap(), 2);
/// assert!(store::create(&conn, "ANNA", 0, Role::Reader, 0).is_err());
/// assert_eq!(store::by_name(&conn, "anna").unwrap().unwrap().id, anna.id);
/// ```
pub fn create(
    conn: &Connection,
    name: &str,
    color: u8,
    role: Role,
    now: i64,
) -> Result<Profile, ProfileError> {
    let name = valid_name(name)?;
    check_color(color)?;
    if count(conn)? >= MAX_PROFILES {
        return Err(ProfileError::TooMany);
    }
    check_unique(conn, &name, None)?;
    conn.execute(
        "INSERT INTO profiles (name, color, role, sort, created_at)
         VALUES (?1, ?2, ?3, (SELECT coalesce(max(sort), 0) + 1 FROM profiles), ?4)",
        params![name, color, role.as_str(), now],
    )?;
    let id = ProfileId(conn.last_insert_rowid());
    get(conn, id)?.ok_or(ProfileError::NotFound)
}

fn changed(rows: usize) -> Result<(), ProfileError> {
    if rows == 0 {
        Err(ProfileError::NotFound)
    } else {
        Ok(())
    }
}

/// Renames a profile; returns the trimmed new name.
pub fn rename(conn: &Connection, id: ProfileId, name: &str) -> Result<String, ProfileError> {
    let name = valid_name(name)?;
    check_unique(conn, &name, Some(id))?;
    changed(conn.execute(
        "UPDATE profiles SET name = ?2 WHERE id = ?1",
        params![id, name],
    )?)?;
    Ok(name)
}

/// Sets a profile's color.
pub fn set_color(conn: &Connection, id: ProfileId, color: u8) -> Result<(), ProfileError> {
    check_color(color)?;
    changed(conn.execute(
        "UPDATE profiles SET color = ?2 WHERE id = ?1",
        params![id, color],
    )?)
}

/// Sets a profile's role; the owner stays an admin.
pub fn set_role(conn: &Connection, id: ProfileId, role: Role) -> Result<(), ProfileError> {
    if id.is_owner() && role != Role::Admin {
        return Err(ProfileError::Owner("is always an admin"));
    }
    changed(conn.execute(
        "UPDATE profiles SET role = ?2 WHERE id = ?1",
        params![id, role.as_str()],
    )?)
}

/// The PHC string of a profile's PIN, if it has one.
pub fn pin_hash(conn: &Connection, id: ProfileId) -> Result<Option<String>, StorageError> {
    Ok(conn
        .query_row("SELECT pin_hash FROM profiles WHERE id = ?1", [id], |row| {
            row.get(0)
        })
        .optional()?
        .flatten())
}

/// Sets (a PHC string from [`pin::hash`](super::pin::hash)) or removes (`None`) a
/// profile's PIN. The caller ends the profile's other sessions (see
/// [`session`](super::session)).
pub fn set_pin_hash(
    conn: &Connection,
    id: ProfileId,
    hash: Option<&str>,
) -> Result<(), ProfileError> {
    changed(conn.execute(
        "UPDATE profiles SET pin_hash = ?2 WHERE id = ?1",
        params![id, hash],
    )?)
}

/// The PHC string of a profile's password, if it has one.
pub fn password_hash(conn: &Connection, id: ProfileId) -> Result<Option<String>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT password_hash FROM profiles WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .flatten())
}

/// Sets (a PHC string from [`password::hash`](super::password::hash)) or removes
/// (`None`) a profile's password at `now`. The caller ends the profile's other password
/// sessions (see [`session::end_password_sessions`](super::session::end_password_sessions)).
pub fn set_password_hash(
    conn: &Connection,
    id: ProfileId,
    hash: Option<&str>,
    now: i64,
) -> Result<(), ProfileError> {
    changed(conn.execute(
        "UPDATE profiles SET password_hash = ?2, password_changed_at = ?3 WHERE id = ?1",
        params![id, hash, now],
    )?)
}

/// Whether an admin profile has a password (so `[server] auth = "password"` leaves
/// someone able to manage the instance from another device).
pub fn admin_has_password(conn: &Connection) -> Result<bool, StorageError> {
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM profiles
                        WHERE role = 'admin' AND password_hash IS NOT NULL)",
        [],
        |row| row.get(0),
    )?)
}

/// Sets whether the other profiles see what this one is reading.
pub fn set_sharing(conn: &Connection, id: ProfileId, share: bool) -> Result<(), ProfileError> {
    changed(conn.execute(
        "UPDATE profiles SET share_activity = ?2 WHERE id = ?1",
        params![id, share],
    )?)
}

/// Stores a profile's theme and reader settings.
pub fn save_settings(
    conn: &Connection,
    id: ProfileId,
    settings: &ProfileSettings,
) -> Result<(), ProfileError> {
    changed(conn.execute(
        "UPDATE profiles SET settings = ?2 WHERE id = ?1",
        params![id, settings.to_json()],
    )?)
}

/// Deletes a profile with its annotations, reading positions and sessions (the owner
/// cannot be deleted). Its export folder is kept.
pub fn delete(conn: &Connection, id: ProfileId) -> Result<(), ProfileError> {
    if id.is_owner() {
        return Err(ProfileError::Owner("cannot be deleted"));
    }
    changed(conn.execute("DELETE FROM profiles WHERE id = ?1", [id])?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::annotations::store::UserData;

    #[test]
    fn the_migration_creates_the_owner() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let all = list(&conn).unwrap();
        assert_eq!(all.len(), 1);
        let owner = &all[0];
        assert_eq!(owner.id, ProfileId::OWNER);
        assert_eq!(owner.name, "Me");
        assert_eq!((owner.role, owner.color), (Role::Admin, 0));
        assert!(!owner.has_pin && !owner.has_password && !owner.share_activity);
        assert_eq!(owner.settings, None);
    }

    #[test]
    fn names_are_unique_ignoring_case_also_beyond_ascii() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        create(&conn, "Мама", 1, Role::Reader, 0).unwrap();
        for name in ["мама", " МАМА ", "me", "ME"] {
            assert!(
                matches!(
                    create(&conn, name, 1, Role::Reader, 0),
                    Err(ProfileError::DuplicateName)
                ),
                "{name}"
            );
        }
        let papa = create(&conn, "Папа", 1, Role::Reader, 0).unwrap();
        assert!(matches!(
            rename(&conn, papa.id, "мАма"),
            Err(ProfileError::DuplicateName)
        ));
        // A profile may change the case of its own name.
        assert_eq!(rename(&conn, papa.id, " ПАПА ").unwrap(), "ПАПА");
        assert!(matches!(
            create(&conn, &"x".repeat(33), 0, Role::Reader, 0),
            Err(ProfileError::InvalidName(_))
        ));
        create(&conn, &"я".repeat(32), 0, Role::Reader, 0).unwrap();
    }

    #[test]
    fn limits_and_owner_rules() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        for i in 1..MAX_PROFILES {
            create(&conn, &format!("P{i}"), 0, Role::Reader, 0).unwrap();
        }
        assert!(matches!(
            create(&conn, "One more", 0, Role::Reader, 0),
            Err(ProfileError::TooMany)
        ));
        assert!(matches!(
            delete(&conn, ProfileId::OWNER),
            Err(ProfileError::Owner(_))
        ));
        assert!(matches!(
            set_role(&conn, ProfileId::OWNER, Role::Reader),
            Err(ProfileError::Owner(_))
        ));
        assert!(matches!(
            set_color(&conn, ProfileId::OWNER, 8),
            Err(ProfileError::InvalidColor)
        ));
        let p = by_name(&conn, "p3").unwrap().unwrap();
        set_role(&conn, p.id, Role::Admin).unwrap();
        set_color(&conn, p.id, 7).unwrap();
        set_sharing(&conn, p.id, true).unwrap();
        set_pin_hash(&conn, p.id, Some("$argon2id$x")).unwrap();
        let p = get(&conn, p.id).unwrap().unwrap();
        assert!(p.is_admin() && p.share_activity && p.has_pin);
        assert_eq!(p.color, 7);
        assert_eq!(
            pin_hash(&conn, p.id).unwrap().as_deref(),
            Some("$argon2id$x")
        );
        delete(&conn, p.id).unwrap();
        assert!(get(&conn, p.id).unwrap().is_none());
        assert!(matches!(delete(&conn, p.id), Err(ProfileError::NotFound)));
        create(&conn, "One more", 0, Role::Reader, 0).unwrap();
    }

    #[test]
    fn passwords_are_stored_apart_from_pins() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let anna = create(&conn, "Anna", 1, Role::Reader, 0).unwrap();
        assert!(!admin_has_password(&conn).unwrap());
        set_password_hash(&conn, anna.id, Some("$argon2id$a"), 5).unwrap();
        assert!(!admin_has_password(&conn).unwrap(), "a reader");
        let anna = get(&conn, anna.id).unwrap().unwrap();
        assert!(anna.has_password && !anna.has_pin);
        assert_eq!(
            password_hash(&conn, anna.id).unwrap().as_deref(),
            Some("$argon2id$a")
        );
        assert_eq!(pin_hash(&conn, anna.id).unwrap(), None);
        set_password_hash(&conn, ProfileId::OWNER, Some("$argon2id$o"), 6).unwrap();
        assert!(admin_has_password(&conn).unwrap());
        let changed_at: Option<i64> = conn
            .query_row(
                "SELECT password_changed_at FROM profiles WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(changed_at, Some(6));
        set_password_hash(&conn, ProfileId::OWNER, None, 7).unwrap();
        assert!(!admin_has_password(&conn).unwrap());
        assert!(matches!(
            set_password_hash(&conn, ProfileId(99), None, 7),
            Err(ProfileError::NotFound)
        ));
    }

    #[test]
    fn settings_are_stored_cleaned() {
        let data = UserData::open_in_memory().unwrap();
        let conn = data.lock();
        let settings = ProfileSettings::parse(r#"{"theme":"sepia","width":"wide"}"#);
        save_settings(&conn, ProfileId::OWNER, &settings).unwrap();
        let owner = get(&conn, ProfileId::OWNER).unwrap().unwrap();
        assert_eq!(owner.settings, Some(settings));
        // Broken stored text counts as empty settings, not as "never saved".
        conn.execute("UPDATE profiles SET settings = 'x' WHERE id = 1", [])
            .unwrap();
        let owner = get(&conn, ProfileId::OWNER).unwrap().unwrap();
        assert_eq!(owner.settings, Some(ProfileSettings::default()));
    }
}
