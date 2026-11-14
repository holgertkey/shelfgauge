//! Profiles: the people who share one instance and one library.
//!
//! A profile has its own annotations, reading positions, "Currently reading", theme
//! and reader settings; the index, scans, caches and `config.toml` belong to the
//! instance. Profiles live in the user-data database (`userdata.sqlite`, schema v4).
//! The *owner* profile (id 1) is created by the migration, is always an admin and
//! cannot be deleted. While it is the only profile, there is no profile UI at all.
//!
//! A PIN is not authentication: it only keeps family members apart on a shared
//! computer, is asked only on this computer and never makes a non-loopback bind
//! allowed. The optional *password* is the login from other devices (`[server] auth =
//! "password"`); the two are independent.
//!
//! Integer ids are what the log shows (`profile = 2`); names, PINs, passwords and
//! session tokens are never logged.
//!
//! - [`store`]: profiles in the database;
//! - [`session`]: the session tokens behind the `shelfgauge-profile` cookie;
//! - [`pin`]: PIN hashing and the lockout after wrong tries;
//! - [`password`]: password hashing and the per-address budget of logins.

pub mod password;
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
    /// Whether it has a password (it can be used from other devices).
    pub has_password: bool,
    /// Whether the other profiles see what it is reading.
    pub share_activity: bool,
    /// The theme and reader settings; `None` until they were first saved.
    pub settings: Option<ProfileSettings>,
    /// When it was created (Unix seconds).
    pub created_at: i64,
    /// When one of its sessions was last used (Unix seconds), if ever.
    pub last_seen_at: Option<i64>,
    /// Made by a hub invite: the person reads only through the hub, so the profile is
    /// not in the picker or on `/login` (the admin still manages it).
    pub hub_only: bool,
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
    (
        "theme",
        &[
            "", "light", "sepia", "mist", "dusk", "dark", "navy", "forest", "wine", "custom",
        ],
    ),
    (
        "font",
        &[
            "sans",
            "serif",
            "literata",
            "pt-serif",
            "merriweather",
            "lora",
            "inter",
            "jetbrains-mono",
        ],
    ),
    ("size", &["3", "0", "1", "2", "4", "5", "6", "7"]),
    ("leading", &["4", "0", "1", "2", "3", "5", "6", "7"]),
    ("width", &["3", "0", "1", "2", "4", "5", "6", "7"]),
    ("align", &["left", "justify"]),
    ("status", &["on", "off"]),
    ("awake", &["10", "off", "5", "30"]),
];

/// The values of earlier versions that were replaced: name, old value, the value now.
/// Stored settings may still hold them; they are read as the value now (the steps of
/// line spacing and width, and the themes that were removed as the closest one left).
pub const RENAMED_VALUES: &[(&str, &str, &str)] = &[
    ("theme", "stone", "mist"),
    ("theme", "black", "dark"),
    ("leading", "tight", "2"),
    ("leading", "normal", "4"),
    ("leading", "loose", "6"),
    ("width", "narrow", "1"),
    ("width", "medium", "3"),
    ("width", "wide", "5"),
    ("width", "full", "7"),
];

/// The settings that hold a color (`#rrggbb`) instead of one of a list of values: the
/// Custom theme's background and accent (`assets/palette.js` derives the rest). Kept
/// while another theme is chosen, so going back to Custom brings them back. The same
/// list is `COLORS` in `assets/theme.js` and `colors` in
/// `tests/fixtures/profiles/settings.json`.
pub const COLOR_SETTINGS: &[&str] = &["cbg", "caccent"];

/// The settings the reading-settings reset keeps: the theme and its colors.
const KEPT_ON_RESET: &[&str] = &["theme", "cbg", "caccent"];

/// `value` as a color setting: `#rrggbb` (any case), lowercased; anything else is
/// `None`.
///
/// # Examples
///
/// ```
/// use shelfgauge::profiles::parse_color;
///
/// assert_eq!(parse_color("#1A2b3C").as_deref(), Some("#1a2b3c"));
/// assert_eq!(parse_color("#abc"), None);
/// assert_eq!(parse_color("red"), None);
/// ```
pub fn parse_color(value: &str) -> Option<String> {
    let digits = value.strip_prefix('#')?;
    (digits.len() == 6 && digits.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| value.to_ascii_lowercase())
}

/// Whether `#rrggbb` is a dark color: white text has more contrast on it than black
/// (WCAG 2), as `isDark` in `assets/palette.js`. The Custom theme's light or dark
/// scheme without JavaScript.
///
/// # Examples
///
/// ```
/// use shelfgauge::profiles::is_dark_color;
///
/// assert!(is_dark_color("#161f30"));
/// assert!(!is_dark_color("#f6efe2"));
/// ```
pub fn is_dark_color(hex: &str) -> bool {
    let Some(color) = parse_color(hex) else {
        return false;
    };
    let channel = |i: usize| {
        let c = f64::from(u8::from_str_radix(&color[i..i + 2], 16).unwrap_or(0)) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let lum = 0.2126 * channel(1) + 0.7152 * channel(3) + 0.0722 * channel(5);
    // White: (1.05) / (lum + 0.05); black: (lum + 0.05) / 0.05.
    1.05 / (lum + 0.05) >= (lum + 0.05) / 0.05
}

/// A profile's theme and reader settings: only known names with allowed values that
/// are not the default, and the colors of [`COLOR_SETTINGS`] that are valid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProfileSettings {
    values: BTreeMap<&'static str, &'static str>,
    colors: BTreeMap<&'static str, String>,
}

impl ProfileSettings {
    /// The settings in a JSON object, keeping only known names with allowed values
    /// and valid colors (anything else, such as broken data, is dropped; a value of an
    /// earlier version is read as its step, see [`RENAMED_VALUES`]).
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
    ///
    /// let value = serde_json::json!({"theme": "custom", "cbg": "#161F30", "caccent": "blue"});
    /// let settings = ProfileSettings::from_json(&value);
    /// assert_eq!(settings.color("cbg"), Some("#161f30"));
    /// assert_eq!(settings.color("caccent"), None);
    /// ```
    pub fn from_json(value: &serde_json::Value) -> Self {
        let mut settings = BTreeMap::new();
        let mut colors = BTreeMap::new();
        if let Some(object) = value.as_object() {
            for (name, allowed) in SETTINGS {
                let Some(value) = object.get(*name).and_then(|v| v.as_str()) else {
                    continue;
                };
                let value = RENAMED_VALUES
                    .iter()
                    .find(|(n, old, _)| n == name && *old == value)
                    .map_or(value, |(_, _, step)| step);
                if let Some(v) = allowed.iter().skip(1).find(|a| **a == value) {
                    settings.insert(*name, *v);
                }
            }
            for name in COLOR_SETTINGS {
                if let Some(color) = object
                    .get(*name)
                    .and_then(|v| v.as_str())
                    .and_then(parse_color)
                {
                    colors.insert(*name, color);
                }
            }
        }
        Self {
            values: settings,
            colors,
        }
    }

    /// The stored settings; unparsable text counts as no settings.
    pub fn parse(text: &str) -> Self {
        serde_json::from_str(text)
            .map(|value| Self::from_json(&value))
            .unwrap_or_default()
    }

    /// The JSON text to store.
    pub fn to_json(&self) -> String {
        let all: BTreeMap<&str, &str> = self.iter().collect();
        serde_json::to_string(&all).expect("a map of strings serializes")
    }

    /// The value of setting `name` (one with a list of values), if it is not the
    /// default.
    pub fn get(&self, name: &str) -> Option<&'static str> {
        self.values.get(name).copied()
    }

    /// The color of setting `name` (one of [`COLOR_SETTINGS`]), if set.
    pub fn color(&self, name: &str) -> Option<&str> {
        self.colors.get(name).map(String::as_str)
    }

    /// The settings that are not the default, the colors among them, in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&'static str, &str)> + '_ {
        let mut all: Vec<(&'static str, &str)> = self
            .values
            .iter()
            .map(|(k, v)| (*k, *v))
            .chain(self.colors.iter().map(|(k, v)| (*k, v.as_str())))
            .collect();
        all.sort_unstable();
        all.into_iter()
    }

    /// Whether every setting is the default.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty() && self.colors.is_empty()
    }

    /// The settings the reading-settings reset keeps: the theme and the Custom theme's
    /// colors.
    pub fn kept_on_reset(&self) -> Self {
        Self {
            values: self
                .values
                .iter()
                .filter(|(k, _)| KEPT_ON_RESET.contains(k))
                .map(|(k, v)| (*k, *v))
                .collect(),
            colors: self
                .colors
                .iter()
                .filter(|(k, _)| KEPT_ON_RESET.contains(k))
                .map(|(k, v)| (*k, v.clone()))
                .collect(),
        }
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

    #[test]
    fn keep_screen_on_minutes() {
        // 10 minutes is the default; only the panel's values are kept.
        assert!(ProfileSettings::parse(r#"{"awake":"10"}"#).is_empty());
        assert!(ProfileSettings::parse(r#"{"awake":"15"}"#).is_empty());
        for value in ["off", "5", "30"] {
            let s = ProfileSettings::parse(&format!(r#"{{"awake":"{value}"}}"#));
            assert_eq!(s.get("awake"), Some(value));
        }
    }

    /// The Custom theme's colors: `#rrggbb` only, lowercased; kept with any theme; the
    /// reading-settings reset keeps them with the theme.
    #[test]
    fn custom_theme_colors() {
        let s = ProfileSettings::parse(
            r##"{"theme":"custom","cbg":"#ABCDEF","caccent":"#12ab34","size":"5"}"##,
        );
        assert_eq!(s.get("theme"), Some("custom"));
        assert_eq!(s.color("cbg"), Some("#abcdef"));
        assert_eq!(s.color("caccent"), Some("#12ab34"));
        assert_eq!(
            s.to_json(),
            r##"{"caccent":"#12ab34","cbg":"#abcdef","size":"5","theme":"custom"}"##
        );
        assert_eq!(ProfileSettings::parse(&s.to_json()), s);
        for bad in [
            "#abc", "red", "#12345g", "#1234567", "123456", "#", "", " #123456",
        ] {
            let s = ProfileSettings::from_json(&serde_json::json!({ "cbg": bad }));
            assert!(s.is_empty(), "{bad:?}");
        }
        assert!(ProfileSettings::from_json(&serde_json::json!({ "cbg": 5 })).is_empty());
        // Kept while another theme is chosen.
        let s = ProfileSettings::parse(r##"{"theme":"navy","cbg":"#101010"}"##);
        assert_eq!(s.color("cbg"), Some("#101010"));
        // The reset keeps the theme and the colors only.
        let s = ProfileSettings::parse(
            r##"{"theme":"custom","cbg":"#101010","caccent":"#ff8800","font":"serif","size":"5"}"##,
        );
        assert_eq!(
            s.kept_on_reset().to_json(),
            r##"{"caccent":"#ff8800","cbg":"#101010","theme":"custom"}"##
        );
        assert!(
            ProfileSettings::parse(r#"{"font":"serif"}"#)
                .kept_on_reset()
                .is_empty()
        );
    }

    /// Light or dark as palette.js decides it: the vectors' backgrounds and schemes.
    #[test]
    fn dark_colors_as_palette_js() {
        let vectors: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/palette/vectors.json"))
                .unwrap();
        for vector in vectors.as_array().unwrap() {
            let bg = vector["vars"]["--bg"].as_str().unwrap();
            assert_eq!(
                is_dark_color(bg),
                vector["scheme"] == "dark",
                "{bg} {}",
                vector["scheme"]
            );
        }
        assert!(is_dark_color("#000000"));
        assert!(!is_dark_color("#ffffff"));
        assert!(!is_dark_color("not a color"));
    }

    /// A theme that was removed is read as the closest one left, here and in theme.js.
    #[test]
    fn removed_themes_become_the_closest_one_left() {
        let s = ProfileSettings::parse(r#"{"theme":"stone","size":"5"}"#);
        assert_eq!(s.to_json(), r#"{"size":"5","theme":"mist"}"#);
        let s = ProfileSettings::parse(r#"{"theme":"black"}"#);
        assert_eq!(s.get("theme"), Some("dark"));
        let js = include_str!("../../assets/theme.js");
        assert!(js.contains("theme: { stone: \"mist\", black: \"dark\" },"));
        assert!(js.contains("adoptedTheme(localStorage.getItem(THEME_KEY))"));
        assert!(js.contains("adoptedTheme(saved && saved.theme)"));
    }

    /// Settings saved by earlier versions keep their line spacing and width: each old
    /// value is read as the step with the same CSS value.
    #[test]
    fn old_line_spacing_and_width_values_become_their_steps() {
        let s = ProfileSettings::parse(r#"{"leading":"tight","width":"narrow"}"#);
        assert_eq!(s.to_json(), r#"{"leading":"2","width":"1"}"#);
        let s = ProfileSettings::parse(r#"{"leading":"loose","width":"wide"}"#);
        assert_eq!(s.to_json(), r#"{"leading":"6","width":"5"}"#);
        let s = ProfileSettings::parse(r#"{"width":"full"}"#);
        assert_eq!(s.get("width"), Some("7"));
        // The old defaults are the new defaults.
        let s = ProfileSettings::parse(r#"{"leading":"normal","width":"medium"}"#);
        assert!(s.is_empty());
        // Each renamed value maps to an allowed step of its setting.
        for (name, _, step) in RENAMED_VALUES {
            let (_, allowed) = SETTINGS.iter().find(|(n, _)| n == name).unwrap();
            assert!(allowed.contains(step), "{name} {step}");
        }
        // theme.js reads them the same way.
        let js = include_str!("../../assets/theme.js");
        for (name, old, step) in RENAMED_VALUES {
            let line = js
                .lines()
                .find(|l| l.trim_start().starts_with(&format!("{name}: {{ ")))
                .unwrap_or_else(|| panic!("{name} in RENAMED of theme.js"));
            assert!(line.contains(&format!("{old}: \"{step}\"")), "{line}");
        }
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
        // The color settings are a list of names, not of values.
        let (colors, fixture): (Vec<_>, Vec<_>) =
            fixture.into_iter().partition(|(k, _)| k == "colors");
        assert_eq!(colors[0].1, COLOR_SETTINGS, "colors of the fixture");
        let js_colors = format!(
            "const COLORS = [{}];",
            COLOR_SETTINGS
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert!(
            include_str!("../../assets/theme.js").contains(&js_colors),
            "{js_colors} in theme.js"
        );
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
            has_password: false,
            share_activity: false,
            settings: None,
            created_at: 0,
            last_seen_at: None,
            hub_only: false,
        };
        assert!(!format!("{profile:?}").contains("Secret"));
    }
}
