//! Library configuration: `.shelfgauge/config.toml`.
//!
//! The config file is created with commented defaults ([`DEFAULT_CONFIG_TOML`]) on first
//! run. Missing keys and sections fall back to their defaults, unknown keys are reported
//! (so they can be logged as warnings) but do not fail loading, and invalid values are
//! rejected with an error naming the key. Command-line flags override file values via
//! [`Config::apply_overrides`].

use std::fmt;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

pub use shelfgauge_core::config::{ReaderConfig, SplitMode};

use crate::{Error, Result};

pub mod edit;

/// The default server port.
pub const DEFAULT_PORT: u16 = 8742;

/// The content of a newly created `config.toml`: every key with its default and a comment.
///
/// Parsing it yields [`Config::default`].
pub const DEFAULT_CONFIG_TOML: &str = r#"# shelfgauge library configuration.
#
# Created with default values on first run. Missing keys use their defaults.
# Command-line flags (--port, --bind) override the values in this file.
# All sections but [server] and [hub] can also be changed on the Settings page (the
# gear in the top bar); comments in this file are kept.

[server]
# Address to listen on.
bind = "127.0.0.1"
# Fixed port. If it is busy, shelfgauge exits with an error instead of picking
# another one: the port is part of the browser origin.
port = 8742
# Extra Host header values to accept, e.g. "books.example.org" behind a reverse proxy.
allowed_hosts = []
# URL prefix when served behind a reverse proxy on a subpath, e.g. "/books".
base_path = "/"
# Login for other devices: "none" (only this computer) or "password" (each person
# logs into their profile with a password from other devices; requests through an
# allowed_hosts name count as other devices). Needed for a bind address other than
# 127.0.0.1 or ::1. Set a password first: on My profile, or with
# shelfgauge profiles set-password <name>.
auth = "none"
# Whether admin profiles may use the settings page, rescans and profile management
# from other devices (with auth = "password").
remote_admin = false

[scan]
# Gitignore-style patterns of paths to skip, in addition to .shelfgauge/ itself.
exclude = []
# Book formats to index: "fb2" (also .fbz), "epub", "txt", "html" (.html, .htm,
# .xhtml; not listed by default, since most HTML files are not books). Text files and
# pages that are too short (or mostly links, or named like readme.txt) are skipped.
formats = ["fb2", "epub", "txt"]

[library]
# Number of entries per sidebar/list page.
page_size = 100
# Books shown under "Currently reading" on the library page; 0 = no list.
reading_list_size = 10

[reader]
# How a book is split into parts for reading: "none", "section" or "size".
split = "section"
# For split = "section": split at sections down to this nesting depth (1 = top
# level); very short untitled parts (a copyright notice, an epigraph) join the next.
section_depth = 1
# For split = "size": approximate number of characters per part.
part_size = 50000
# Longest part in characters, in every mode: a longer part (e.g. a book without
# chapters) is cut into equal pieces at block boundaries. 0 = no limit.
max_part_size = 300000
# Remember where you stopped reading each book ("Continue reading" on the book page).
remember_position = true

[annotations]
# Longest text that can be marked, in characters (about 3-4 book pages).
max_text_length = 10000
# Longest note, in characters.
max_note_length = 10000
# Days to keep the marks of a book that was removed from the library, in case it
# comes back (a moved folder, an unmounted drive); 0 = delete at once.
removed_book_retention_days = 30

[hub]
# The hub for reading on phones and other devices (shelfgauge hub), e.g.
# "https://hub.example.org"; empty = not used. HTTPS is required (http only for
# 127.0.0.1, ::1 or localhost). Each profile registers its own account on it under
# My profile → Phone and other devices; an account keeps the hub it was made on.
url = ""
# The name of this library on your other devices; empty = the library folder's name.
instance_name = ""
# Minutes between syncs with the hub (also after each change).
sync_interval_minutes = 5
"#;

/// The largest accepted value of `annotations.max_text_length` and
/// `annotations.max_note_length`.
pub const MAX_ANNOTATION_LENGTH: u32 = 1_000_000;

/// The complete configuration of a library.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// The `[server]` section.
    pub server: ServerConfig,
    /// The `[scan]` section.
    pub scan: ScanConfig,
    /// The `[library]` section.
    pub library: LibraryConfig,
    /// The `[reader]` section.
    pub reader: ReaderConfig,
    /// The `[annotations]` section.
    pub annotations: AnnotationsConfig,
    /// The `[hub]` section.
    pub hub: HubConfig,
}

/// The `[server]` section: where and how the web server listens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address to listen on.
    pub bind: IpAddr,
    /// Fixed port; a busy port is a hard error.
    pub port: u16,
    /// Extra accepted `Host` header values (e.g. a domain behind a reverse proxy).
    pub allowed_hosts: Vec<String>,
    /// URL prefix for all routes; normalized to `/` or `/a/b` (no trailing slash).
    pub base_path: String,
    /// How requests from other devices log in.
    pub auth: AuthMode,
    /// Whether admins may use the admin pages from other devices.
    pub remote_admin: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
            allowed_hosts: Vec::new(),
            base_path: "/".to_owned(),
            auth: AuthMode::None,
            remote_admin: false,
        }
    }
}

/// How requests from other devices (not this computer) log in: `[server] auth`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// No login: only requests from this computer are served.
    #[default]
    None,
    /// Each person logs into their profile with its password.
    Password,
}

impl AuthMode {
    /// The value as written in `config.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Password => "password",
        }
    }
}

/// The `[scan]` section: what the scanner indexes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    /// Gitignore-style patterns of paths to skip, in addition to `.shelfgauge/`.
    pub exclude: Vec<String>,
    /// The book formats to index ([`FORMATS`](crate::format::FORMATS)); normalized to
    /// lowercase without repeats by [`Config::validate`].
    pub formats: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            exclude: Vec::new(),
            formats: crate::format::DEFAULT_FORMATS
                .iter()
                .map(|&f| f.to_owned())
                .collect(),
        }
    }
}

impl ScanConfig {
    /// The registry of the enabled formats.
    ///
    /// # Panics
    ///
    /// If a format is unknown: call it on a [validated](Config::validate) config.
    pub fn registry(&self) -> crate::format::Registry {
        crate::format::Registry::with_formats(&self.formats)
            .expect("formats are validated with the config")
    }
}

/// The `[library]` section: library-mode UI settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryConfig {
    /// Number of entries per sidebar/list page.
    pub page_size: u32,
    /// Books shown under "Currently reading" on the library page; 0 hides the list.
    pub reading_list_size: u32,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self {
            page_size: 100,
            reading_list_size: 10,
        }
    }
}

/// The `[annotations]` section: limits of marks and notes, and cleanup.
///
/// Lengths are in Unicode code points. They apply when an annotation is created or its
/// note edited; lowering them never touches stored annotations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AnnotationsConfig {
    /// The longest text that can be marked.
    pub max_text_length: u32,
    /// The longest note.
    pub max_note_length: u32,
    /// Days to keep the annotations of a book removed from the library; 0 deletes them
    /// at the first cleanup.
    pub removed_book_retention_days: u32,
}

impl Default for AnnotationsConfig {
    fn default() -> Self {
        Self {
            max_text_length: 10_000,
            max_note_length: 10_000,
            removed_book_retention_days: 30,
        }
    }
}

/// The `[hub]` section: the hub that profiles of this library sync with.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
    /// The hub's URL for new accounts; empty = no hub. Validated by
    /// [`HubUrl::parse`](crate::hub_client::HubUrl::parse) and normalized to its form.
    pub url: String,
    /// The library's name on the other devices; empty = the library folder's name.
    pub instance_name: String,
    /// Minutes between syncs.
    pub sync_interval_minutes: u32,
}

impl Default for HubConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            instance_name: String::new(),
            sync_interval_minutes: 5,
        }
    }
}

/// The longest `hub.instance_name`, in characters.
pub const MAX_INSTANCE_NAME: usize = 64;

/// Values given on the command line that override the config file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigOverrides {
    /// `--port`: overrides `server.port`.
    pub port: Option<u16>,
    /// `--bind`: overrides `server.bind`.
    pub bind: Option<IpAddr>,
}

/// An invalid configuration value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{key}` {message}")]
pub struct ConfigError {
    /// The dotted key, e.g. `server.port`.
    pub key: &'static str,
    /// What is wrong with the value.
    pub message: String,
}

impl ConfigError {
    pub(crate) fn new(key: &'static str, message: impl fmt::Display) -> Self {
        Self {
            key,
            message: message.to_string(),
        }
    }
}

/// The result of [`Config::load_or_create`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedConfig {
    /// The validated configuration.
    pub config: Config,
    /// Dotted paths of keys in the file that shelfgauge does not know (e.g. `server.prot`).
    pub unknown_keys: Vec<String>,
    /// Whether the file did not exist and was created with defaults.
    pub created: bool,
}

/// The configuration of a running server, shared by the request handlers and the
/// background scan, and replaced as a whole when the settings page saves.
///
/// Readers take a snapshot with [`get`](Self::get) and use it for the whole request, so
/// a save never mixes old and new values within one request. Clones share the value.
///
/// # Examples
///
/// ```
/// use shelfgauge::config::{Config, LiveConfig};
///
/// let live = LiveConfig::new(Config::default());
/// let other = live.clone();
/// let mut changed = Config::default();
/// changed.library.page_size = 20;
/// live.set(changed);
/// assert_eq!(other.get().library.page_size, 20);
/// ```
#[derive(Debug, Clone)]
pub struct LiveConfig(Arc<RwLock<Arc<Config>>>);

impl LiveConfig {
    /// Shares `config`, which must be [validated](Config::validate).
    pub fn new(config: Config) -> Self {
        Self(Arc::new(RwLock::new(Arc::new(config))))
    }

    /// The current configuration.
    pub fn get(&self) -> Arc<Config> {
        Arc::clone(&self.0.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Replaces the configuration; `config` must be [validated](Config::validate).
    pub fn set(&self, config: Config) {
        *self.0.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(config);
    }
}

impl Config {
    /// Loads the config file at `path`, first creating it with [`DEFAULT_CONFIG_TOML`] if
    /// it does not exist.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::config::Config;
    ///
    /// let dir = tempfile::tempdir().unwrap();
    /// let path = dir.path().join("config.toml");
    /// let loaded = Config::load_or_create(&path).unwrap();
    /// assert!(loaded.created);
    /// assert_eq!(loaded.config, Config::default());
    /// assert!(!Config::load_or_create(&path).unwrap().created);
    /// ```
    pub fn load_or_create(path: &Path) -> Result<LoadedConfig> {
        let created = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(DEFAULT_CONFIG_TOML.as_bytes())
                    .map_err(|e| Error::io(path, e))?;
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => false,
            Err(e) => return Err(Error::io(path, e)),
        };
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let (config, unknown_keys) = Self::from_toml(&text, path)?;
        Ok(LoadedConfig {
            config,
            unknown_keys,
            created,
        })
    }

    /// Parses and validates config file content; `path` is only used in error messages.
    ///
    /// Returns the config and the sorted dotted paths of unknown keys.
    pub fn from_toml(text: &str, path: &Path) -> Result<(Self, Vec<String>)> {
        let (mut config, unknown_keys) =
            Self::parse_unvalidated(text).map_err(|message| Error::ConfigSyntax {
                path: path.to_owned(),
                message,
            })?;
        config.validate().map_err(|error| Error::InvalidConfig {
            path: path.to_owned(),
            error,
        })?;
        Ok((config, unknown_keys))
    }

    /// Parses config file content without [validating](Self::validate) it, so values
    /// out of range are kept as they are (the settings page shows them at their field).
    ///
    /// Returns the config and the sorted dotted paths of unknown keys, or the syntax or
    /// type error (with its location, without a file name).
    pub fn parse_unvalidated(text: &str) -> Result<(Self, Vec<String>), String> {
        let syntax_error = |e: toml::de::Error| e.to_string().trim_end().to_owned();
        let deserializer = toml::de::Deserializer::parse(text).map_err(syntax_error)?;
        let mut unknown_keys = Vec::new();
        let config: Self = serde_ignored::deserialize(deserializer, |key| {
            unknown_keys.push(key.to_string());
        })
        .map_err(syntax_error)?;
        unknown_keys.sort();
        Ok((config, unknown_keys))
    }

    /// Applies command-line overrides; CLI values take precedence over file values.
    ///
    /// # Examples
    ///
    /// ```
    /// use shelfgauge::config::{Config, ConfigOverrides};
    ///
    /// let mut config = Config::default();
    /// config
    ///     .apply_overrides(&ConfigOverrides { port: Some(9000), bind: None })
    ///     .unwrap();
    /// assert_eq!(config.server.port, 9000);
    /// ```
    pub fn apply_overrides(&mut self, overrides: &ConfigOverrides) -> Result<()> {
        if let Some(port) = overrides.port {
            self.server.port = port;
        }
        if let Some(bind) = overrides.bind {
            self.server.bind = bind;
        }
        self.validate().map_err(Error::InvalidOption)
    }

    /// Checks all values and normalizes `server.base_path` in place.
    pub fn validate(&mut self) -> Result<(), ConfigError> {
        if self.server.port == 0 {
            return Err(ConfigError::new(
                "server.port",
                "must be between 1 and 65535 (a fixed port is required)",
            ));
        }
        for host in &self.server.allowed_hosts {
            if host.is_empty() || host.contains(|c: char| c.is_whitespace() || c == '/') {
                return Err(ConfigError::new(
                    "server.allowed_hosts",
                    format_args!("contains an invalid host {host:?}"),
                ));
            }
        }
        self.server.base_path = normalize_base_path(&self.server.base_path)
            .map_err(|message| ConfigError::new("server.base_path", message))?;
        let mut formats: Vec<String> = Vec::new();
        for name in &self.scan.formats {
            let normalized = name.trim().to_ascii_lowercase();
            if !crate::format::FORMATS.contains(&normalized.as_str()) {
                return Err(ConfigError::new(
                    "scan.formats",
                    crate::format::UnknownFormat { name: name.clone() },
                ));
            }
            if !formats.contains(&normalized) {
                formats.push(normalized);
            }
        }
        if formats.is_empty() {
            return Err(ConfigError::new(
                "scan.formats",
                "must list at least one format",
            ));
        }
        self.scan.formats = formats;
        if self.library.page_size == 0 {
            return Err(ConfigError::new(
                "library.page_size",
                "must be greater than 0",
            ));
        }
        if self.reader.section_depth == 0 {
            return Err(ConfigError::new(
                "reader.section_depth",
                "must be greater than 0",
            ));
        }
        if self.reader.part_size == 0 {
            return Err(ConfigError::new(
                "reader.part_size",
                "must be greater than 0",
            ));
        }
        for (key, value) in [
            (
                "annotations.max_text_length",
                self.annotations.max_text_length,
            ),
            (
                "annotations.max_note_length",
                self.annotations.max_note_length,
            ),
        ] {
            if !(1..=MAX_ANNOTATION_LENGTH).contains(&value) {
                return Err(ConfigError::new(
                    key,
                    format_args!("must be between 1 and {MAX_ANNOTATION_LENGTH}"),
                ));
            }
        }
        if !self.hub.url.trim().is_empty() {
            self.hub.url = crate::hub_client::HubUrl::parse(&self.hub.url)
                .map_err(|e| ConfigError::new("hub.url", e))?
                .to_string();
        } else {
            self.hub.url = String::new();
        }
        self.hub.instance_name = self.hub.instance_name.trim().to_owned();
        if self.hub.instance_name.chars().count() > MAX_INSTANCE_NAME
            || self.hub.instance_name.chars().any(char::is_control)
        {
            return Err(ConfigError::new(
                "hub.instance_name",
                format_args!(
                    "must be at most {MAX_INSTANCE_NAME} characters, without control characters"
                ),
            ));
        }
        if !(1..=1440).contains(&self.hub.sync_interval_minutes) {
            return Err(ConfigError::new(
                "hub.sync_interval_minutes",
                "must be between 1 and 1440",
            ));
        }
        Ok(())
    }

    /// The effective configuration as TOML (without comments).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("config always serializes to TOML")
    }
}

/// Normalizes a URL prefix to `/` or `/a/b`: adds the leading slash, drops trailing and
/// duplicate slashes, and rejects `.`/`..` segments and characters other than ASCII
/// letters, digits, `-`, `.`, `_` and `~`.
pub(crate) fn normalize_base_path(path: &str) -> Result<String, String> {
    // Only unreserved URL characters: the prefix is used verbatim in generated URLs and
    // in router paths, where `{`, `}` and `*` would be parameters.
    if let Some(c) = path
        .chars()
        .find(|&c| !(c.is_ascii_alphanumeric() || matches!(c, '/' | '-' | '.' | '_' | '~')))
    {
        return Err(format!(
            "{path:?} contains the invalid character {c:?} (allowed: letters, digits, \
             `-`, `.`, `_`, `~` and `/`)"
        ));
    }
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.iter().any(|s| *s == "." || *s == "..") {
        return Err(format!("{path:?} must not contain `.` or `..` segments"));
    }
    Ok(format!("/{}", segments.join("/")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<(Config, Vec<String>)> {
        Config::from_toml(text, Path::new("config.toml"))
    }

    fn invalid_key(text: &str) -> &'static str {
        match parse(text).unwrap_err() {
            Error::InvalidConfig { error, .. } => error.key,
            other => panic!("expected InvalidConfig, got {other:?}"),
        }
    }

    #[test]
    fn default_template_parses_to_default_config() {
        let (config, unknown) = parse(DEFAULT_CONFIG_TOML).unwrap();
        assert_eq!(config, Config::default());
        assert!(unknown.is_empty(), "{unknown:?}");
    }

    #[test]
    fn effective_config_round_trips() {
        let mut config = Config::default();
        config.server.port = 9000;
        config.server.allowed_hosts = vec!["books.example.org".to_owned()];
        config.server.base_path = "/books".to_owned();
        config.server.auth = AuthMode::Password;
        config.server.remote_admin = true;
        config.scan.exclude = vec!["*.tmp".to_owned()];
        config.reader.split = SplitMode::Size;
        let (parsed, unknown) = parse(&config.to_toml()).unwrap();
        assert_eq!(parsed, config);
        assert!(unknown.is_empty());
    }

    #[test]
    fn auth_settings() {
        let (config, unknown) =
            parse("[server]\nauth = \"password\"\nremote_admin = true\n").unwrap();
        assert_eq!(config.server.auth, AuthMode::Password);
        assert!(config.server.remote_admin);
        assert!(unknown.is_empty());
        let defaults = Config::default().server;
        assert_eq!(defaults.auth, AuthMode::None);
        assert!(!defaults.remote_admin);
        let (_, unknown) = parse("[server]\nauht = \"password\"\n").unwrap();
        assert_eq!(unknown, ["server.auht"]);
        let err = parse("[server]\nauth = \"Password\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("none") && err.contains("password"), "{err}");
    }

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(parse("").unwrap().0, Config::default());
    }

    #[test]
    fn partial_config_fills_defaults() {
        let (config, _) = parse("[server]\nport = 9000\n\n[reader]\nsplit = \"none\"\n").unwrap();
        let mut expected = Config::default();
        expected.server.port = 9000;
        expected.reader.split = SplitMode::None;
        assert_eq!(config, expected);
    }

    #[test]
    fn unknown_keys_are_reported_not_rejected() {
        let (config, unknown) =
            parse("[server]\nprot = 1\n\n[extra]\nkey = true\n\n[reader]\nsplit = \"size\"\n")
                .unwrap();
        assert_eq!(config.reader.split, SplitMode::Size);
        assert_eq!(unknown, ["extra", "server.prot"]);
    }

    #[test]
    fn syntax_errors_name_the_file_and_location() {
        let err = parse("[server]\nport = \n").unwrap_err();
        assert!(matches!(err, Error::ConfigSyntax { .. }), "{err:?}");
        let message = err.to_string();
        assert!(
            message.starts_with("invalid config file config.toml:"),
            "{message}"
        );
        assert!(message.contains("line 2"), "{message}");
    }

    #[test]
    fn wrong_types_and_values_are_readable_errors() {
        for (text, expected) in [
            ("[server]\nport = 99999\n", "port"),
            ("[server]\nport = \"x\"\n", "port"),
            ("[server]\nbind = \"localhost\"\n", "bind"),
            ("[reader]\nsplit = \"pages\"\n", "pages"),
            ("[server]\nauth = \"basic\"\n", "basic"),
            ("[server]\nremote_admin = \"yes\"\n", "remote_admin"),
            ("[library]\npage_size = -1\n", "page_size"),
        ] {
            let err = parse(text).unwrap_err();
            assert!(matches!(err, Error::ConfigSyntax { .. }), "{text}: {err:?}");
            assert!(err.to_string().contains(expected), "{text}: {err}");
        }
        let err = parse("[reader]\nsplit = \"pages\"\n")
            .unwrap_err()
            .to_string();
        assert!(err.contains("none") && err.contains("section"), "{err}");
    }

    #[test]
    fn out_of_range_values_name_the_key() {
        assert_eq!(invalid_key("[server]\nport = 0\n"), "server.port");
        assert_eq!(
            invalid_key("[library]\npage_size = 0\n"),
            "library.page_size"
        );
        assert_eq!(
            invalid_key("[reader]\nsection_depth = 0\n"),
            "reader.section_depth"
        );
        assert_eq!(invalid_key("[reader]\npart_size = 0\n"), "reader.part_size");
        assert_eq!(
            invalid_key("[annotations]\nmax_text_length = 0\n"),
            "annotations.max_text_length"
        );
        assert_eq!(
            invalid_key("[annotations]\nmax_note_length = 1000001\n"),
            "annotations.max_note_length"
        );
        assert_eq!(
            invalid_key("[server]\nallowed_hosts = [\"\"]\n"),
            "server.allowed_hosts"
        );
        assert_eq!(
            invalid_key("[server]\nbase_path = \"/a/../b\"\n"),
            "server.base_path"
        );
        let err = parse("[server]\nport = 0\n").unwrap_err().to_string();
        assert_eq!(
            err,
            "invalid config file config.toml: `server.port` must be between 1 and 65535 \
             (a fixed port is required)"
        );
    }

    #[test]
    fn annotation_limits_accept_the_whole_range() {
        let (config, _) = parse(
            "[annotations]\nmax_text_length = 1\nmax_note_length = 1000000\n\
             removed_book_retention_days = 0\n\n[reader]\nremember_position = false\n",
        )
        .unwrap();
        assert_eq!(config.annotations.max_text_length, 1);
        assert_eq!(config.annotations.max_note_length, 1_000_000);
        assert_eq!(config.annotations.removed_book_retention_days, 0);
        assert!(!config.reader.remember_position);
        let err = parse("[annotations]\nremoved_book_retention_days = -1\n").unwrap_err();
        assert!(matches!(err, Error::ConfigSyntax { .. }), "{err:?}");
    }

    #[test]
    fn base_path_is_normalized() {
        for (input, expected) in [
            ("/", "/"),
            ("", "/"),
            ("books", "/books"),
            ("/books/", "/books"),
            ("//library//books//", "/library/books"),
        ] {
            assert_eq!(normalize_base_path(input).unwrap(), expected, "{input:?}");
        }
        let (config, _) = parse("[server]\nbase_path = \"books/\"\n").unwrap();
        assert_eq!(config.server.base_path, "/books");
    }

    #[test]
    fn base_path_rejects_invalid_input() {
        for input in [
            "/a/./b",
            "/..",
            "/a b",
            "/a?x",
            "/a#x",
            "\\a",
            "/{id}",
            "/a/*x",
            "/a:b",
            "/%20",
            "/книги",
            "/a&b",
        ] {
            assert!(normalize_base_path(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn cli_overrides_take_precedence() {
        let (mut config, _) = parse("[server]\nport = 9000\nbind = \"127.0.0.1\"\n").unwrap();
        config
            .apply_overrides(&ConfigOverrides {
                port: Some(9100),
                bind: Some("::1".parse().unwrap()),
            })
            .unwrap();
        assert_eq!(config.server.port, 9100);
        assert_eq!(config.server.bind, "::1".parse::<IpAddr>().unwrap());
    }

    #[test]
    fn missing_cli_overrides_keep_file_values() {
        let (mut config, _) = parse("[server]\nport = 9000\n").unwrap();
        let before = config.clone();
        config.apply_overrides(&ConfigOverrides::default()).unwrap();
        assert_eq!(config, before);
    }

    #[test]
    fn scan_formats_are_validated_and_normalized() {
        let (config, _) = parse("[scan]\nformats = [\"TXT\", \" fb2\", \"txt\"]").unwrap();
        assert_eq!(config.scan.formats, ["txt", "fb2"]);
        assert_eq!(config.scan.registry().formats(), ["txt", "fb2"]);
        assert_eq!(Config::default().scan.formats, ["fb2", "epub", "txt"]);
        assert_eq!(
            invalid_key("[scan]\nformats = [\"fb2\", \"mobi\"]"),
            "scan.formats"
        );
        let (config, _) = parse("[scan]\nformats = [\"fb2\", \"HTML\"]").unwrap();
        assert_eq!(config.scan.formats, ["fb2", "html"], "HTML is opt-in");
        assert_eq!(invalid_key("[scan]\nformats = []"), "scan.formats");
        match parse("[scan]\nformats = [\"mobi\"]").unwrap_err() {
            Error::InvalidConfig { error, .. } => assert_eq!(
                error.message,
                r#"unknown format "mobi" (known formats: fb2, epub, txt, html)"#
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn invalid_cli_override_is_an_option_error() {
        let mut config = Config::default();
        let err = config
            .apply_overrides(&ConfigOverrides {
                port: Some(0),
                bind: None,
            })
            .unwrap_err();
        assert!(
            matches!(&err, Error::InvalidOption(e) if e.key == "server.port"),
            "{err:?}"
        );
    }

    #[test]
    fn load_or_create_writes_default_file_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let loaded = Config::load_or_create(&path).unwrap();
        assert!(loaded.created);
        assert_eq!(loaded.config, Config::default());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), DEFAULT_CONFIG_TOML);

        std::fs::write(&path, "[server]\nport = 9000\n").unwrap();
        let loaded = Config::load_or_create(&path).unwrap();
        assert!(!loaded.created);
        assert_eq!(loaded.config.server.port, 9000);
    }

    #[test]
    fn load_or_create_reports_invalid_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[server]\nport = 0\n").unwrap();
        let err = Config::load_or_create(&path).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig { .. }), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[server]\nport = 0\n",
            "an invalid file must not be overwritten"
        );
    }
}
