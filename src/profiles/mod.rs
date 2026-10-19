//! Profiles: the people who share one instance and one library.
//!
//! A profile has its own annotations, reading positions, "Currently reading", theme
//! and reader settings; the index, scans, caches and `config.toml` belong to the
//! instance. Profiles live in the user-data database (`userdata.sqlite`, schema v3).
//! The *owner* profile (id 1) is created by the migration, is always an admin and
//! cannot be deleted. While it is the only profile, there is no profile UI at all.
//!
//! A profile is not authentication: its optional PIN only keeps family members apart
//! on a shared computer, and it never makes a non-loopback bind allowed.
//!
//! Integer ids are what the log shows (`profile = 2`); names, PINs and session tokens
//! are never logged.
//!
//! - [`store`]: profiles in the database;
//! - [`session`]: the session tokens behind the `shelfgauge-profile` cookie;
//! - [`pin`]: PIN hashing and the lockout after wrong tries.

pub mod pin;
pub mod session;
pub mod store;

use std::collections::BTreeMap;
use std::fmt;

use rusqlite::ToSql;
use rusqlite::types::{FromSql, FromSqlResult, ToSqlOutput, ValueRef};

/// The id of a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProfileId(pub i64);

impl ProfileId {
    /// The owner profile, created by the migration: always an admin, never deleted.
    pub const OWNER: Self = Self(1);

    /// Whether this is the owner profile.
    pub fn is_owner(self) -> bool {
        self == Self::OWNER
    }
}

impl fmt::Display for ProfileId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ToSql for ProfileId {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        self.0.to_sql()
    }
}

impl FromSql for ProfileId {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        i64::column_result(value).map(Self)
    }
}

/// What a profile may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Settings, Rescan, maintenance and managing profiles.
    Admin,
    /// Everything else: reading, annotations, the own profile.
    Reader,
}

impl Role {
    /// The stored name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Reader => "reader",
        }
    }

    /// The role for a stored name.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "admin" => Some(Self::Admin),
            "reader" => Some(Self::Reader),
            _ => None,
        }
    }
}

/// The number of profile colors (palette indexes `0..COLORS`).
pub const COLORS: u8 = 8;

/// The names of the profile colors, for screen readers and the CLI.
pub const COLOR_NAMES: [&str; COLORS as usize] = [
    "Default", "Green", "Purple", "Orange", "Red", "Teal", "Pink", "Brown",
];

/// The most profiles an instance may have.
pub const MAX_PROFILES: usize = 16;

/// The longest profile name in characters.
pub const MAX_NAME_CHARS: usize = 32;

/// A stored profile.
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    /// The id.
    pub id: ProfileId,
    /// The name (1–32 characters, unique ignoring case).
    pub name: String,
    /// The palette index of its color (`0..COLORS`).
    pub color: u8,
    /// What it may do.
    pub role: Role,
    /// Whether it has a PIN.
    pub has_pin: bool,
    /// Whether the other profiles see what it is reading.
    pub share_activity: bool,
    /// The theme and reader settings; `None` until they were first saved.
    pub settings: Option<ProfileSettings>,
    /// When it was created (Unix seconds).
    pub created_at: i64,
    /// When one of its sessions was last used (Unix seconds), if ever.
    pub last_seen_at: Option<i64>,
}

// Names are never logged: `Debug` shows the id and role only.
impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("id", &self.id)
            .field("role", &self.role)
            .finish_non_exhaustive()
    }
}

impl Profile {
    /// Whether it may use the admin pages.
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }

    /// Up to two initials of the name, for the avatar.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::profiles::initials;
    ///
    /// assert_eq!(initials("anna maria"), "AM");
    /// assert_eq!(initials("Пётр"), "П");
    /// assert_eq!(initials("  "), "?");
    /// ```
    pub fn initials(&self) -> String {
        initials(&self.name)
    }
}

/// Up to two initials of `name` (first letters of its first two words, uppercased).
pub fn initials(name: &str) -> String {
    let letters: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().find(|c| c.is_alphanumeric()))
        .take(2)
        .flat_map(char::to_uppercase)
        .collect();
    if letters.is_empty() {
        "?".to_owned()
    } else {
        letters
    }
}

/// The theme and reader settings: name → allowed values, the first being the default.
///
/// The reader settings mirror `OPTIONS` in `assets/theme.js`, and the themes the theme
/// menu; `tests/fixtures/profiles/settings.json` keeps the three lists equal.
pub const SETTINGS: &[(&str, &[&str])] = &[
    ("theme", &["", "light", "dark", "sepia", "navy", "black"]),
    ("font", &["sans", "serif"]),
    ("size", &["3", "0", "1", "2", "4", "5", "6", "7"]),
    ("leading", &["normal", "tight", "loose"]),
    ("width", &["medium", "narrow", "wide", "full"]),
    ("align", &["left", "justify"]),
    ("status", &["on", "off"]),
];

/// A profile's theme and reader settings: only known names with allowed values that
/// are not the default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileSettings(BTreeMap<&'static str, &'static str>);

impl ProfileSettings {
    /// The settings in a JSON object, keeping only known names with allowed values
    /// (anything else, such as old or broken data, is dropped).
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::profiles::ProfileSettings;
    ///
    /// let value = serde_json::json!({"theme": "dark", "size": "5", "font": "comic", "x": 1});
    /// let settings = ProfileSettings::from_json(&value);
    /// assert_eq!(settings.get("theme"), Some("dark"));
    /// assert_eq!(settings.get("font"), None);
    /// assert_eq!(settings.to_json(), r#"{"size":"5","theme":"dark"}"#);
    /// ```
    pub fn from_json(value: &serde_json::Value) -> Self {
        let mut settings = BTreeMap::new();
        if let Some(object) = value.as_object() {
            for (name, allowed) in SETTINGS {
                let Some(value) = object.get(*name).and_then(|v| v.as_str()) else {
                    continue;
                };
                if let Some(v) = allowed.iter().skip(1).find(|a| **a == value) {
                    settings.insert(*name, *v);
                }
            }
        }
        Self(settings)
    }

    /// The stored settings; unparsable text counts as no settings.
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text)
            .map(|value| Self::from_json(&value))
            .unwrap_or_default()
    }

    /// The JSON text to store.
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.0).expect("a map of strings serializes")
    }

    /// The value of setting `name`, if it is not the default.
    pub fn get(&self, name: &str) -> Option<&'static str> {
        self.0.get(name).copied()
    }

    /// The settings that are not the default, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &'static str)> + '_ {
        self.0.iter().map(|(k, v)| (*k, *v))
    }

    /// Whether every setting is the default.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_of_names() {
        assert_eq!(initials("Anna"), "A");
        assert_eq!(initials("anna  maria lee"), "AM");
        assert_eq!(initials("ёлка"), "Ё");
        assert_eq!(initials("(Dad)"), "D");
        assert_eq!(initials(""), "?");
    }

    #[test]
    fn settings_whitelist() {
        let s = ProfileSettings::parse(r#"{"theme":"navy","size":"3","align":"justify"}"#);
        // The default size is dropped; a default stays implicit.
        assert_eq!(s.to_json(), r#"{"align":"justify","theme":"navy"}"#);
        assert!(ProfileSettings::parse("not json").is_empty());
        assert!(ProfileSettings::parse(r#"["theme"]"#).is_empty());
        assert!(ProfileSettings::parse(r#"{"theme":5}"#).is_empty());
        assert_eq!(ProfileSettings::parse(&s.to_json()), s);
    }

    /// The whitelist equals the shared fixture, and the fixture equals `theme.js`'s
    /// `OPTIONS` and the theme menu of `base.html`.
    #[test]
    fn settings_match_the_fixture_and_the_scripts() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/profiles/settings.json"))
                .unwrap();
        let fixture: Vec<(String, Vec<String>)> = fixture
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                let values = v.as_array().unwrap();
                (
                    k.clone(),
                    values
                        .iter()
                        .map(|v| v.as_str().unwrap().to_owned())
                        .collect(),
                )
            })
            .collect();
        let mut ours: Vec<(String, Vec<String>)> = SETTINGS
            .iter()
            .map(|(k, v)| ((*k).to_owned(), v.iter().map(|s| (*s).to_owned()).collect()))
            .collect();
        ours.sort();
        let mut sorted = fixture.clone();
        sorted.sort();
        assert_eq!(ours, sorted);

        // theme.js: the `OPTIONS` object, one `name: ["a", "b"],` per line.
        let js = include_str!("../../assets/theme.js");
        let block = js
            .split("const OPTIONS = {")
            .nth(1)
            .and_then(|rest| rest.split("};").next())
            .expect("OPTIONS in theme.js");
        let mut from_js: Vec<(String, Vec<String>)> = block
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with("//"))
            .map(|line| {
                let (name, values) = line.split_once(':').unwrap();
                let values = values
                    .trim()
                    .trim_end_matches(',')
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|v| v.trim().trim_matches('"').to_owned())
                    .collect();
                (name.trim().to_owned(), values)
            })
            .collect();
        let themes = fixture.iter().find(|(k, _)| k == "theme").unwrap().clone();
        from_js.push(themes.clone());
        from_js.sort();
        assert_eq!(from_js, sorted, "theme.js OPTIONS");

        // The theme menu offers the same themes.
        let base = include_str!("../../templates/base.html");
        let menu = base
            .split("<select class=\"theme-select\"")
            .nth(1)
            .and_then(|rest| rest.split("</select>").next())
            .unwrap();
        let values: Vec<String> = menu
            .split("<option value=\"")
            .skip(1)
            .map(|o| o.split('"').next().unwrap().to_owned())
            .collect();
        assert_eq!(values, themes.1, "theme menu");
    }

    #[test]
    fn roles_round_trip() {
        for role in [Role::Admin, Role::Reader] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("root"), None);
    }

    #[test]
    fn debug_hides_the_name() {
        let profile = Profile {
            id: ProfileId(3),
            name: "Secret Name".into(),
            color: 0,
            role: Role::Reader,
            has_pin: false,
            share_activity: false,
            settings: None,
            created_at: 0,
            last_seen_at: None,
        };
        assert!(!format!("{profile:?}").contains("Secret"));
    }
}
