//! Changing `config.toml` from the settings page.
//!
//! The file is edited, never rewritten from [`Config`]: comments, layout, key order and
//! unknown keys are kept, and only the [editable keys](FIELDS) whose value differs from
//! what the file gives now are set (a key that is absent stays absent while it keeps
//! its default). The caller validates the new text with [`Config::from_toml`] before
//! [writing](write()) it, compares [hashes](file_hash) to detect edits made since the
//! form was rendered, and writes atomically.
//!
//! Errors never contain paths (they are shown in the browser); the details go to the
//! log.

use std::collections::HashMap;
use std::fmt;
use std::io::Write as _;
use std::path::Path;
use std::sync::OnceLock;

use toml_edit::{Array, DocumentMut, Item, Table, Value};

use super::{Config, DEFAULT_CONFIG_TOML, MAX_ANNOTATION_LENGTH, SplitMode};

/// What kind of value a key holds, and how the form shows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// A whole number between `min` and `max` (inclusive).
    Number {
        /// The smallest accepted value.
        min: u32,
        /// The largest accepted value.
        max: u32,
    },
    /// On or off (a checkbox).
    Bool,
    /// One of a few words (radio buttons).
    Choice(&'static [&'static str]),
    /// A set of book formats (checkboxes), at least one.
    Formats,
    /// Exclude patterns, one per line (a text area).
    Lines,
}

/// A value of an editable key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// A number.
    Number(u32),
    /// A switch.
    Bool(bool),
    /// A word of a [`FieldKind::Choice`].
    Choice(String),
    /// A list (formats or patterns).
    List(Vec<String>),
}

impl FieldValue {
    /// Whether two values of `kind` mean the same: formats are compared as sets of
    /// lowercase names, so the checkbox order never counts as a change.
    pub fn same(&self, other: &Self, kind: FieldKind) -> bool {
        match (self, other, kind) {
            (Self::List(a), Self::List(b), FieldKind::Formats) => {
                let set = |list: &[String]| {
                    let mut list: Vec<String> =
                        list.iter().map(|f| f.trim().to_ascii_lowercase()).collect();
                    list.sort();
                    list.dedup();
                    list
                };
                set(a) == set(b)
            }
            _ => self == other,
        }
    }

    fn to_toml(&self) -> Value {
        match self {
            Self::Number(n) => Value::from(i64::from(*n)),
            Self::Bool(b) => Value::from(*b),
            Self::Choice(s) => Value::from(s.as_str()),
            Self::List(list) => Value::Array(list.iter().map(String::as_str).collect::<Array>()),
        }
    }
}

/// A key of `config.toml` that the settings page edits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Field {
    /// The dotted key, e.g. `library.page_size`; also the form field name.
    pub key: &'static str,
    /// The label on the page.
    pub label: &'static str,
    /// The kind of value.
    pub kind: FieldKind,
}

const UNBOUNDED: u32 = u32::MAX;

/// The editable keys, in page order: `[library]`, `[reader]`, `[annotations]`, then
/// `[scan]`. `[server]` is never edited from the browser.
pub const FIELDS: &[Field] = &[
    Field {
        key: "library.page_size",
        label: "Entries per page",
        kind: FieldKind::Number {
            min: 1,
            max: UNBOUNDED,
        },
    },
    Field {
        key: "library.reading_list_size",
        label: "Books under “Currently reading”",
        kind: FieldKind::Number {
            min: 0,
            max: UNBOUNDED,
        },
    },
    Field {
        key: "reader.split",
        label: "Split books into parts",
        kind: FieldKind::Choice(&["none", "section", "size"]),
    },
    Field {
        key: "reader.section_depth",
        label: "Section depth",
        kind: FieldKind::Number {
            min: 1,
            max: UNBOUNDED,
        },
    },
    Field {
        key: "reader.part_size",
        label: "Part size (characters)",
        kind: FieldKind::Number {
            min: 1,
            max: UNBOUNDED,
        },
    },
    Field {
        key: "reader.max_part_size",
        label: "Longest part (characters)",
        kind: FieldKind::Number {
            min: 0,
            max: UNBOUNDED,
        },
    },
    Field {
        key: "reader.remember_position",
        label: "Remember the reading position",
        kind: FieldKind::Bool,
    },
    Field {
        key: "annotations.max_text_length",
        label: "Longest marked text (characters)",
        kind: FieldKind::Number {
            min: 1,
            max: MAX_ANNOTATION_LENGTH,
        },
    },
    Field {
        key: "annotations.max_note_length",
        label: "Longest note (characters)",
        kind: FieldKind::Number {
            min: 1,
            max: MAX_ANNOTATION_LENGTH,
        },
    },
    Field {
        key: "annotations.removed_book_retention_days",
        label: "Keep marks of removed books (days)",
        kind: FieldKind::Number {
            min: 0,
            max: UNBOUNDED,
        },
    },
    Field {
        key: "scan.formats",
        label: "Book formats",
        kind: FieldKind::Formats,
    },
    Field {
        key: "scan.exclude",
        label: "Exclude patterns",
        kind: FieldKind::Lines,
    },
];

/// The editable field with the dotted `key`.
pub fn field(key: &str) -> Option<&'static Field> {
    FIELDS.iter().find(|f| f.key == key)
}

impl Field {
    /// The section, e.g. `library`.
    pub fn section(&self) -> &'static str {
        self.key.split_once('.').map_or(self.key, |(s, _)| s)
    }

    /// The key within its section, e.g. `page_size`.
    pub fn name(&self) -> &'static str {
        self.key.split_once('.').map_or(self.key, |(_, k)| k)
    }

    /// The value of this key in `config`.
    pub fn get(&self, config: &Config) -> FieldValue {
        use FieldValue::{Bool, Choice, List, Number};
        match self.key {
            "library.page_size" => Number(config.library.page_size),
            "library.reading_list_size" => Number(config.library.reading_list_size),
            "reader.split" => Choice(split_name(config.reader.split).to_owned()),
            "reader.section_depth" => Number(config.reader.section_depth),
            "reader.part_size" => Number(config.reader.part_size),
            "reader.max_part_size" => Number(config.reader.max_part_size),
            "reader.remember_position" => Bool(config.reader.remember_position),
            "annotations.max_text_length" => Number(config.annotations.max_text_length),
            "annotations.max_note_length" => Number(config.annotations.max_note_length),
            "annotations.removed_book_retention_days" => {
                Number(config.annotations.removed_book_retention_days)
            }
            "scan.formats" => List(config.scan.formats.clone()),
            "scan.exclude" => List(config.scan.exclude.clone()),
            key => unreachable!("unknown field {key}"),
        }
    }

    /// Sets this key in `config` to `value`, which must be of the field's kind.
    pub fn set(&self, config: &mut Config, value: FieldValue) {
        use FieldValue::{Bool, Choice, List, Number};
        match (self.key, value) {
            ("library.page_size", Number(n)) => config.library.page_size = n,
            ("library.reading_list_size", Number(n)) => config.library.reading_list_size = n,
            ("reader.split", Choice(s)) => {
                config.reader.split = match s.as_str() {
                    "none" => SplitMode::None,
                    "size" => SplitMode::Size,
                    _ => SplitMode::Section,
                };
            }
            ("reader.section_depth", Number(n)) => config.reader.section_depth = n,
            ("reader.part_size", Number(n)) => config.reader.part_size = n,
            ("reader.max_part_size", Number(n)) => config.reader.max_part_size = n,
            ("reader.remember_position", Bool(b)) => config.reader.remember_position = b,
            ("annotations.max_text_length", Number(n)) => config.annotations.max_text_length = n,
            ("annotations.max_note_length", Number(n)) => config.annotations.max_note_length = n,
            ("annotations.removed_book_retention_days", Number(n)) => {
                config.annotations.removed_book_retention_days = n;
            }
            ("scan.formats", List(list)) => config.scan.formats = list,
            ("scan.exclude", List(list)) => config.scan.exclude = list,
            (key, value) => unreachable!("{value:?} does not fit field {key}"),
        }
    }

    /// The default value.
    pub fn default_value(&self) -> FieldValue {
        self.get(&Config::default())
    }

    /// The comment above the key in [`DEFAULT_CONFIG_TOML`], as one line.
    pub fn hint(&self) -> &'static str {
        hints().get(self.key).map_or("", String::as_str)
    }

    /// Parses the submitted form values of this field (all values of the field name, in
    /// order; none for an unchecked checkbox).
    ///
    /// The error says what is wrong with the value, without the key.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::config::edit::{FieldValue, field};
    ///
    /// let page_size = field("library.page_size").unwrap();
    /// assert_eq!(page_size.parse(&[" 20 "]), Ok(FieldValue::Number(20)));
    /// assert!(page_size.parse(&["0"]).is_err());
    /// assert!(page_size.parse(&["many"]).is_err());
    /// ```
    pub fn parse(&self, values: &[&str]) -> Result<FieldValue, String> {
        match self.kind {
            FieldKind::Number { min, max } => {
                let text = values.first().map_or("", |v| v.trim());
                if text.is_empty() {
                    return Err("enter a number".to_owned());
                }
                let out_of_range = || {
                    if max == UNBOUNDED {
                        format!("must be at least {min}")
                    } else {
                        format!("must be between {min} and {max}")
                    }
                };
                if !text.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(
                        if text.starts_with('-') && text[1..].bytes().all(|b| b.is_ascii_digit()) {
                            out_of_range()
                        } else {
                            "must be a whole number".to_owned()
                        },
                    );
                }
                match text.parse::<u32>() {
                    Ok(n) if (min..=max).contains(&n) => Ok(FieldValue::Number(n)),
                    _ => Err(out_of_range()),
                }
            }
            FieldKind::Bool => Ok(FieldValue::Bool(
                values.iter().any(|v| matches!(*v, "true" | "on" | "1")),
            )),
            FieldKind::Choice(choices) => {
                let text = values.first().map_or("", |v| v.trim());
                choices
                    .iter()
                    .find(|c| **c == text)
                    .map(|c| FieldValue::Choice((*c).to_owned()))
                    .ok_or_else(|| format!("must be one of: {}", choices.join(", ")))
            }
            FieldKind::Formats => {
                let mut formats: Vec<String> = Vec::new();
                for value in values {
                    let name = value.trim().to_ascii_lowercase();
                    if !crate::format::FORMATS.contains(&name.as_str()) {
                        return Err(format!("unknown format {value:?}"));
                    }
                    if !formats.contains(&name) {
                        formats.push(name);
                    }
                }
                if formats.is_empty() {
                    return Err("choose at least one format".to_owned());
                }
                Ok(FieldValue::List(formats))
            }
            FieldKind::Lines => {
                let mut lines = Vec::new();
                for (i, line) in values
                    .iter()
                    .flat_map(|v| v.split('\n'))
                    .map(|line| line.trim_end())
                    .enumerate()
                {
                    if line.trim().is_empty() {
                        continue;
                    }
                    if let Err(e) = check_exclude(line) {
                        return Err(format!("line {}: {e}", i + 1));
                    }
                    lines.push(line.to_owned());
                }
                Ok(FieldValue::List(lines))
            }
        }
    }
}

/// Checks one exclude pattern as the scanner compiles it.
fn check_exclude(pattern: &str) -> Result<(), String> {
    crate::scanner::excludes(Path::new("."), &[pattern.to_owned()])
        .map(|_| ())
        .map_err(|e| match e {
            crate::scanner::ScanError::InvalidExclude { pattern, message } => {
                format!("invalid pattern {pattern:?}: {message}")
            }
            other => other.to_string(),
        })
}

fn split_name(mode: SplitMode) -> &'static str {
    match mode {
        SplitMode::None => "none",
        SplitMode::Section => "section",
        SplitMode::Size => "size",
    }
}

/// The comments of [`DEFAULT_CONFIG_TOML`] by dotted key, each joined into one line.
fn hints() -> &'static HashMap<String, String> {
    static HINTS: OnceLock<HashMap<String, String>> = OnceLock::new();
    HINTS.get_or_init(|| {
        let doc: DocumentMut = DEFAULT_CONFIG_TOML
            .parse()
            .expect("the default config is valid TOML");
        let mut hints = HashMap::new();
        for (section, item) in doc.iter() {
            let Some(table) = item.as_table() else {
                continue;
            };
            for (key, _) in table.iter() {
                let prefix = table
                    .key(key)
                    .and_then(|k| k.leaf_decor().prefix())
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let text: Vec<&str> = prefix
                    .lines()
                    .map(|line| line.trim().trim_start_matches('#').trim())
                    .filter(|line| !line.is_empty())
                    .collect();
                hints.insert(format!("{section}.{key}"), text.join(" "));
            }
        }
        hints
    })
}

/// A failure to change `config.toml`; the message never contains a path.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// The file is not valid TOML (it cannot be edited until it is fixed by hand).
    #[error("config.toml is not valid TOML: {0}")]
    Syntax(String),

    /// A section the form edits is not a table (e.g. `library = 1`).
    #[error("`{0}` in config.toml is not a table")]
    NotATable(&'static str),

    /// Reading the file failed.
    #[error("cannot read the config file ({0})")]
    Read(std::io::ErrorKind),

    /// Writing the file failed.
    #[error("cannot write the config file ({0})")]
    Write(std::io::ErrorKind),
}

/// The text of `config.toml` with the keys of `new` that differ from `text` set.
///
/// Keys whose value `text` already gives (including defaults of absent keys) are left
/// alone, comments (also those after a value) and unknown keys are kept, and missing
/// sections are added. The result is not validated: parse it with
/// [`Config::from_toml`] before writing it.
///
/// # Examples
///
/// ```
/// use shelfgauge::config::Config;
/// use shelfgauge::config::edit::edit;
///
/// let text = "# my library\n[library]\npage_size = 50 # small screen\n";
/// let mut config = Config::default();
/// config.library.page_size = 20;
/// assert_eq!(
///     edit(text, &config).unwrap(),
///     "# my library\n[library]\npage_size = 20 # small screen\n"
/// );
/// ```
pub fn edit(text: &str, new: &Config) -> Result<String, EditError> {
    let (old, _) = Config::parse_unvalidated(text).map_err(EditError::Syntax)?;
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        EditError::Syntax(e.to_string().trim_end().to_owned())
    })?;
    for field in FIELDS {
        let value = field.get(new);
        if value.same(&field.get(&old), field.kind) {
            continue;
        }
        let section = doc
            .entry(field.section())
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_like_mut()
            .ok_or(EditError::NotATable(field.section()))?;
        let mut replacement = value.to_toml();
        match section.get_mut(field.name()) {
            Some(Item::Value(old)) => {
                // Keep the spacing and a comment after the value.
                *replacement.decor_mut() = old.decor().clone();
                *old = replacement;
            }
            _ => {
                section.insert(field.name(), Item::Value(replacement));
            }
        }
    }
    Ok(doc.to_string())
}

/// The keys of `[server]` and `[hub]` that the settings page edits on this computer
/// only; they apply after a restart.
pub const RESTART_KEYS: &[&str] = &[
    "server.bind",
    "server.port",
    "server.allowed_hosts",
    "server.auth",
    "server.remote_admin",
    "hub.url",
    "hub.instance_name",
    "hub.sync_interval_minutes",
];

/// The value of restart key `key` in `config`, as TOML.
fn restart_value(config: &Config, key: &str) -> Value {
    let server = &config.server;
    let hub = &config.hub;
    match key {
        "server.bind" => Value::from(server.bind.to_string()),
        "server.port" => Value::from(i64::from(server.port)),
        "server.allowed_hosts" => Value::Array(
            server
                .allowed_hosts
                .iter()
                .map(String::as_str)
                .collect::<Array>(),
        ),
        "server.auth" => Value::from(server.auth.as_str()),
        "server.remote_admin" => Value::from(server.remote_admin),
        "hub.url" => Value::from(hub.url.as_str()),
        "hub.instance_name" => Value::from(hub.instance_name.as_str()),
        "hub.sync_interval_minutes" => Value::from(i64::from(hub.sync_interval_minutes)),
        _ => unreachable!("not a restart key: {key}"),
    }
}

/// The text of `config.toml` with the [restart keys](RESTART_KEYS) of `new` that differ
/// from `text` set, as [`edit`] does for the other keys (comments, layout and unknown
/// keys are kept). Not validated: parse the result with [`Config::from_toml`].
///
/// # Examples
///
/// ```
/// use shelfgauge::config::Config;
/// use shelfgauge::config::edit::edit_restart_keys;
///
/// let text = "[server]\nport = 8742 # fixed\n";
/// let mut config = Config::default();
/// config.server.port = 9000;
/// config.hub.url = "https://hub.example.org".into();
/// assert_eq!(
///     edit_restart_keys(text, &config).unwrap(),
///     "[server]\nport = 9000 # fixed\n\n[hub]\nurl = \"https://hub.example.org\"\n"
/// );
/// ```
pub fn edit_restart_keys(text: &str, new: &Config) -> Result<String, EditError> {
    let (old, _) = Config::parse_unvalidated(text).map_err(EditError::Syntax)?;
    let mut doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        EditError::Syntax(e.to_string().trim_end().to_owned())
    })?;
    for key in RESTART_KEYS {
        let value = restart_value(new, key);
        if value.to_string().trim() == restart_value(&old, key).to_string().trim() {
            continue;
        }
        let (section_name, name) = key.split_once('.').expect("dotted keys");
        let section = doc
            .entry(section_name)
            .or_insert_with(|| Item::Table(Table::new()))
            .as_table_like_mut()
            .ok_or(EditError::NotATable(if section_name == "hub" {
                "hub"
            } else {
                "server"
            }))?;
        let mut replacement = value;
        match section.get_mut(name) {
            Some(Item::Value(old)) => {
                *replacement.decor_mut() = old.decor().clone();
                *old = replacement;
            }
            _ => {
                section.insert(name, Item::Value(replacement));
            }
        }
    }
    Ok(doc.to_string())
}

/// The hash of a config file's content, carried by the form to detect edits made
/// since it was rendered.
pub fn file_hash(bytes: &[u8]) -> String {
    format!("{:016x}", xxhash_rust::xxh3::xxh3_64(bytes))
}

/// Reads the config file at `path`.
pub fn read(path: &Path) -> Result<String, EditError> {
    std::fs::read_to_string(path).map_err(|e| {
        tracing::error!(path = ?path, error = %e, "cannot read the config file");
        EditError::Read(e.kind())
    })
}

/// Writes `text` to the config file at `path` atomically: a temporary file in the same
/// directory with the old file's permissions is renamed over it.
///
/// A read-only file is not replaced (a rename would succeed on Unix, where only the
/// directory's permissions count).
pub fn write(path: &Path, text: &str) -> Result<(), EditError> {
    let fail = |e: std::io::Error| {
        tracing::error!(path = ?path, error = %e, "cannot write the config file");
        EditError::Write(e.kind())
    };
    let permissions = match std::fs::metadata(path) {
        Ok(metadata) if metadata.permissions().readonly() => {
            return Err(fail(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "the file is read-only",
            )));
        }
        Ok(metadata) => Some(metadata.permissions()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(fail(e)),
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(dir).map_err(fail)?;
    tmp.write_all(text.as_bytes()).map_err(fail)?;
    tmp.as_file().sync_all().map_err(fail)?;
    if let Some(permissions) = permissions {
        tmp.as_file().set_permissions(permissions).map_err(fail)?;
    }
    tmp.persist(path).map_err(|e| fail(e.error))?;
    Ok(())
}

impl fmt::Display for FieldValue {
    /// The value as the form shows it (lists one item per line).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::Bool(b) => write!(f, "{}", if *b { "on" } else { "off" }),
            Self::Choice(s) => f.write_str(s),
            Self::List(list) => f.write_str(&list.join("\n")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::from_toml(text, Path::new("config.toml")).unwrap().0
    }

    #[test]
    fn every_field_round_trips_through_config() {
        let config = Config::default();
        for field in FIELDS {
            let mut copy = config.clone();
            field.set(&mut copy, field.get(&config));
            assert_eq!(copy, config, "{}", field.key);
            assert!(!field.hint().is_empty(), "{} has no hint", field.key);
        }
    }

    #[test]
    fn hints_come_from_the_default_comments() {
        assert_eq!(
            field("library.page_size").unwrap().hint(),
            "Number of entries per sidebar/list page."
        );
        assert!(
            field("scan.formats")
                .unwrap()
                .hint()
                .contains("most HTML files are not books")
        );
    }

    /// The form's bounds accept exactly what [`Config::validate`] accepts.
    #[test]
    fn bounds_match_validation() {
        for field in FIELDS {
            let FieldKind::Number { min, max } = field.kind else {
                continue;
            };
            let valid = |n: u32| {
                let mut config = Config::default();
                field.set(&mut config, FieldValue::Number(n));
                config.validate().is_ok()
            };
            assert!(valid(min), "{} min", field.key);
            assert!(valid(max), "{} max", field.key);
            if min > 0 {
                assert!(!valid(min - 1), "{} below min", field.key);
                assert!(field.parse(&[&(min - 1).to_string()]).is_err());
            }
            if max < u32::MAX {
                assert!(!valid(max + 1), "{} above max", field.key);
                assert!(field.parse(&[&(max + 1).to_string()]).is_err());
            }
        }
    }

    #[test]
    fn parse_numbers() {
        let f = field("annotations.max_note_length").unwrap();
        assert_eq!(f.parse(&["1000000"]), Ok(FieldValue::Number(1_000_000)));
        assert_eq!(
            f.parse(&["1000001"]).unwrap_err(),
            "must be between 1 and 1000000"
        );
        assert_eq!(
            f.parse(&["-5"]).unwrap_err(),
            "must be between 1 and 1000000"
        );
        assert_eq!(f.parse(&["1.5"]).unwrap_err(), "must be a whole number");
        assert_eq!(f.parse(&["abc"]).unwrap_err(), "must be a whole number");
        assert_eq!(f.parse(&[""]).unwrap_err(), "enter a number");
        assert_eq!(f.parse(&[]).unwrap_err(), "enter a number");
        let f = field("library.page_size").unwrap();
        assert_eq!(f.parse(&["99999999999"]).unwrap_err(), "must be at least 1");
    }

    #[test]
    fn parse_other_kinds() {
        let f = field("reader.remember_position").unwrap();
        assert_eq!(f.parse(&[]), Ok(FieldValue::Bool(false)));
        assert_eq!(f.parse(&["true"]), Ok(FieldValue::Bool(true)));
        let f = field("reader.split").unwrap();
        assert_eq!(
            f.parse(&["size"]),
            Ok(FieldValue::Choice("size".to_owned()))
        );
        assert!(f.parse(&["pages"]).is_err());
        let f = field("scan.formats").unwrap();
        assert_eq!(
            f.parse(&["fb2", "HTML", "fb2"]),
            Ok(FieldValue::List(vec!["fb2".to_owned(), "html".to_owned()]))
        );
        assert_eq!(f.parse(&[]).unwrap_err(), "choose at least one format");
        assert!(f.parse(&["mobi"]).is_err());
        let f = field("scan.exclude").unwrap();
        assert_eq!(
            f.parse(&["*.tmp\r\n\r\n  \r\nold/  \r\n"]),
            Ok(FieldValue::List(vec![
                "*.tmp".to_owned(),
                "old/".to_owned()
            ]))
        );
        let err = f.parse(&["ok/\n[z-a]"]).unwrap_err();
        assert!(
            err.starts_with("line 2: invalid pattern \"[z-a]\""),
            "{err}"
        );
    }

    #[test]
    fn edit_keeps_comments_order_and_unknown_keys() {
        let text = "# Top comment.\n\n[reader]\n# How to split.\nsplit = \"section\"   # trailing\n\
                    part_size = 50000\nmystery = 1\n\n[extra]\nkeep = \"me\"\n";
        let mut config = parse(text);
        config.reader.split = SplitMode::Size;
        let out = edit(text, &config).unwrap();
        assert_eq!(
            out,
            "# Top comment.\n\n[reader]\n# How to split.\nsplit = \"size\"   # trailing\n\
             part_size = 50000\nmystery = 1\n\n[extra]\nkeep = \"me\"\n"
        );
    }

    #[test]
    fn edit_leaves_untouched_and_default_keys_alone() {
        let text = "[library]\npage_size = 100\n";
        let config = parse(text);
        assert_eq!(edit(text, &config).unwrap(), text, "nothing changed");
        // Absent and set to its default: stays absent.
        let mut config = parse("");
        config.reader.part_size = 50_000;
        assert_eq!(edit("", &config).unwrap(), "");
        // Formats in another order are no change.
        let text = "[scan]\nformats = [\"txt\", \"fb2\"]\n";
        let mut config = parse(text);
        config.scan.formats = vec!["fb2".to_owned(), "txt".to_owned()];
        assert_eq!(edit(text, &config).unwrap(), text);
    }

    #[test]
    fn edit_adds_missing_sections_and_keys() {
        let text = "[server]\nport = 9000\n";
        let mut config = parse(text);
        config.annotations.max_note_length = 500;
        config.scan.exclude = vec!["*.tmp".to_owned(), "a \"b\"".to_owned()];
        config.library.page_size = 30;
        let out = edit(text, &config).unwrap();
        let (parsed, unknown) = Config::from_toml(&out, Path::new("c")).unwrap();
        assert!(unknown.is_empty());
        assert_eq!(parsed, config, "{out}");
        assert!(out.starts_with("[server]\nport = 9000\n"), "{out}");
    }

    #[test]
    fn edit_works_with_dotted_keys_and_the_default_file() {
        let text = "reader.split = \"none\"\n";
        let mut config = parse(text);
        config.reader.split = SplitMode::Section;
        config.reader.section_depth = 2;
        let out = edit(text, &config).unwrap();
        assert_eq!(parse(&out), config, "{out}");

        let mut config = parse(DEFAULT_CONFIG_TOML);
        config.library.page_size = 40;
        let out = edit(DEFAULT_CONFIG_TOML, &config).unwrap();
        assert_eq!(
            out,
            DEFAULT_CONFIG_TOML.replace("page_size = 100", "page_size = 40")
        );
    }

    #[test]
    fn edit_refuses_broken_files() {
        let config = Config::default();
        assert!(matches!(
            edit("[library\n", &config),
            Err(EditError::Syntax(_))
        ));
    }

    #[test]
    fn write_replaces_atomically_and_refuses_read_only_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write(&path, "a = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 1\n");
        write(&path, "a = 2\n").unwrap();
        assert_eq!(read(&path).unwrap(), "a = 2\n");
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "no temp files left"
        );

        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&path, permissions).unwrap();
        let err = write(&path, "a = 3\n").unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot write the config file (permission denied)"
        );
        assert_eq!(read(&path).unwrap(), "a = 2\n");

        let err = read(&dir.path().join("missing.toml")).unwrap_err();
        assert_eq!(
            err.to_string(),
            "cannot read the config file (entity not found)"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write(&path, "x = 1\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }

    #[test]
    fn hashes_differ_for_different_content() {
        assert_eq!(file_hash(b"a"), file_hash(b"a"));
        assert_ne!(file_hash(b"a"), file_hash(b"b"));
        assert_eq!(file_hash(b"").len(), 16);
    }
}
