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

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// The default server port.
pub const DEFAULT_PORT: u16 = 8742;

/// The content of a newly created `config.toml`: every key with its default and a comment.
///
/// Parsing it yields [`Config::default`].
pub const DEFAULT_CONFIG_TOML: &str = r#"# shelfgauge library configuration.
#
# Created with default values on first run. Missing keys use their defaults.
# Command-line flags (--port, --bind) override the values in this file.

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

[scan]
# Gitignore-style patterns of paths to skip, in addition to .shelfgauge/ itself.
exclude = []

[library]
# Number of entries per sidebar/list page.
page_size = 100

[reader]
# How a book is split into parts for reading: "none", "section" or "size".
split = "section"
# For split = "section": split at sections of this nesting depth (1 = top level).
section_depth = 1
# For split = "size": approximate number of characters per part.
part_size = 50000
"#;

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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
            allowed_hosts: Vec::new(),
            base_path: "/".to_owned(),
        }
    }
}

/// The `[scan]` section: what the scanner indexes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    /// Gitignore-style patterns of paths to skip, in addition to `.shelfgauge/`.
    pub exclude: Vec<String>,
}

/// The `[library]` section: library-mode UI settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibraryConfig {
    /// Number of entries per sidebar/list page.
    pub page_size: u32,
}

impl Default for LibraryConfig {
    fn default() -> Self {
        Self { page_size: 100 }
    }
}

/// The `[reader]` section: how books are split into parts in reading mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReaderConfig {
    /// The split mode.
    pub split: SplitMode,
    /// For [`SplitMode::Section`]: split at sections of this nesting depth (1 = top level).
    pub section_depth: u32,
    /// For [`SplitMode::Size`]: approximate number of characters per part.
    pub part_size: u32,
}

impl Default for ReaderConfig {
    fn default() -> Self {
        Self {
            split: SplitMode::Section,
            section_depth: 1,
            part_size: 50_000,
        }
    }
}

/// How a book is split into parts for reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitMode {
    /// The whole book is one part.
    None,
    /// One part per section at [`ReaderConfig::section_depth`].
    Section,
    /// Parts of about [`ReaderConfig::part_size`] characters, cut at block boundaries.
    Size,
}

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
    fn new(key: &'static str, message: impl fmt::Display) -> Self {
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
        let syntax_error = |e: toml::de::Error| Error::ConfigSyntax {
            path: path.to_owned(),
            message: e.to_string().trim_end().to_owned(),
        };
        let deserializer = toml::de::Deserializer::parse(text).map_err(syntax_error)?;
        let mut unknown_keys = Vec::new();
        let mut config: Self = serde_ignored::deserialize(deserializer, |key| {
            unknown_keys.push(key.to_string());
        })
        .map_err(syntax_error)?;
        unknown_keys.sort();
        config.validate().map_err(|error| Error::InvalidConfig {
            path: path.to_owned(),
            error,
        })?;
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
        Ok(())
    }

    /// The effective configuration as TOML (without comments).
    pub fn to_toml(&self) -> String {
        toml::to_string(self).expect("config always serializes to TOML")
    }
}

/// Normalizes a URL prefix to `/` or `/a/b`: adds the leading slash, drops trailing and
/// duplicate slashes, and rejects `.`/`..` segments and characters that are not allowed
/// in a path prefix.
fn normalize_base_path(path: &str) -> Result<String, String> {
    if let Some(c) = path
        .chars()
        .find(|c| c.is_whitespace() || c.is_control() || matches!(c, '?' | '#' | '\\'))
    {
        return Err(format!("{path:?} contains the invalid character {c:?}"));
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
        config.scan.exclude = vec!["*.tmp".to_owned()];
        config.reader.split = SplitMode::Size;
        let (parsed, unknown) = parse(&config.to_toml()).unwrap();
        assert_eq!(parsed, config);
        assert!(unknown.is_empty());
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
        for input in ["/a/./b", "/..", "/a b", "/a?x", "/a#x", "\\a"] {
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
