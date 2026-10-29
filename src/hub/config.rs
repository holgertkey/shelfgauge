//! The hub's configuration file, `<data>/hub.toml`.
//!
//! Loaded like a library's `config.toml`: created with [`DEFAULT_HUB_TOML`] when the
//! hub is created, unknown keys are reported (not fatal), values are validated. A hub
//! has no settings page: the file is edited and the hub restarted.

use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::{ConfigError, ServerConfig, normalize_base_path};
use crate::error::{Error, Result};

/// The hub's default port (an instance listens on 8742).
pub const DEFAULT_HUB_PORT: u16 = 8743;

/// The default [`LimitsConfig::max_blob_bytes`]: 64 MiB.
pub const DEFAULT_MAX_BLOB_BYTES: u64 = 64 * 1024 * 1024;

/// The content of a newly created `hub.toml`; parses to [`HubConfig::default`].
pub const DEFAULT_HUB_TOML: &str = r#"# shelfgauge hub configuration. Restart the hub after editing.

[server]
# Address to listen on. The hub has no TLS of its own: put it behind a reverse proxy
# with HTTPS (see README, "Running a hub") and keep this on 127.0.0.1 unless the proxy
# runs on another machine.
bind = "127.0.0.1"
# Fixed port; a busy port is an error.
port = 8743
# The names the hub is reached under through the proxy (e.g. "hub.example.org").
allowed_hosts = []
# URL prefix of all routes, when the proxy serves the hub under a path.
base_path = "/"
# Addresses or CIDR ranges of reverse proxies whose X-Forwarded-For names the client
# (e.g. ["127.0.0.1", "::1"] for a proxy on this machine). Empty: all clients behind a
# proxy on this machine share one budget of `public_requests_per_hour`.
trusted_proxies = []

[libraries]
# Who may create a library on this hub: "invite" (a one-time code from
# `shelfgauge hub invite`) or "open" (anyone who reaches the hub). Members of a library
# join by their library admin's invites either way.
registration = "invite"
# The most libraries the hub keeps (0 = no limit).
max_libraries = 0
# How long a hub invite code is valid, in days.
invite_days = 7
# With open registration: libraries created per client address (an IPv6 /64) and day
# (0 = no limit). Behind a reverse proxy, set `server.trusted_proxies`, or all clients
# share this budget.
new_libraries_per_address_per_day = 3
# A member account none of whose devices made a request for this many days is deleted;
# a library is deleted with everything it stores when neither it nor any device of its
# accounts made a request for this long (0 = never).
expiry_days = 0
# The longest a library admin's invite of a member is valid, in days.
member_invite_max_days = 7

[operator]
# Shown by the hub's /terms page, GET /api/info and the reader's This device page.
# Who runs this hub (e.g. "Example public hub").
name = ""
# Where to report abuse or ask for a takedown: an e-mail address or an https:// URL.
contact = ""
# Terms of use as plain text, a file in the data directory (served at /terms; a
# missing file means no terms). Blank lines separate paragraphs.
terms_file = "terms.txt"

[quotas]
# Per library; 0 = no limit.
# Stored bytes (publication blobs and the sync records of all its accounts).
max_bytes = 0
# Publications.
max_publications = 0
# Per account: signed-in browsers (the library itself is not counted; 0 = no limit).
max_devices = 5

[passwords]
# Argon2id parameters for new and changed hub passwords, derived in the browser. Never
# below the built-in minimum (19456 KiB, 2 iterations), which the reader app enforces.
memory_kib = 19456
iterations = 2

[limits]
# The largest blob (one part or image of a publication) in bytes.
max_blob_bytes = 67108864
# The most bytes the hub stores for all libraries together (0 = no limit); beyond it,
# new books and marks are refused until space is freed.
max_total_bytes = 0
# A publication not opened for this many days is deleted (0 = never).
publication_expiry_days = 0
# API requests per minute and device token.
requests_per_minute = 600
# Requests per hour to the routes without a token (sign-in, invites, pairing, new
# libraries), per client address (an IPv6 /64); see `server.trusted_proxies`.
public_requests_per_hour = 60
"#;

/// The complete configuration of a hub.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
    /// The `[server]` section.
    pub server: HubServerConfig,
    /// The `[libraries]` section.
    pub libraries: LibrariesConfig,
    /// The `[operator]` section.
    pub operator: OperatorConfig,
    /// The `[quotas]` section.
    pub quotas: QuotasConfig,
    /// The `[limits]` section.
    pub limits: LimitsConfig,
    /// The `[passwords]` section.
    pub passwords: PasswordsConfig,
}

/// The `[server]` section: where the hub listens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HubServerConfig {
    /// Address to listen on; any address is allowed (tokens authenticate).
    pub bind: IpAddr,
    /// Fixed port; a busy port is a hard error.
    pub port: u16,
    /// Extra accepted `Host` header values (the hub's name behind a reverse proxy).
    pub allowed_hosts: Vec<String>,
    /// URL prefix for all routes; normalized to `/` or `/a/b` (no trailing slash).
    pub base_path: String,
    /// Addresses or CIDR ranges of reverse proxies whose `X-Forwarded-For` names the
    /// client of a request (for the per-address limits).
    pub trusted_proxies: Vec<String>,
}

impl Default for HubServerConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_HUB_PORT,
            allowed_hosts: Vec::new(),
            base_path: "/".to_owned(),
            trusted_proxies: Vec::new(),
        }
    }
}

impl HubServerConfig {
    /// The same values as a library's [`ServerConfig`], for the shared `Host` allowlist
    /// and the listener.
    pub fn as_server_config(&self) -> ServerConfig {
        ServerConfig {
            bind: self.bind,
            port: self.port,
            allowed_hosts: self.allowed_hosts.clone(),
            base_path: self.base_path.clone(),
            ..ServerConfig::default()
        }
    }
}

/// Who may create a library: `[libraries] registration`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Registration {
    /// Only with a one-time invite code from `shelfgauge hub invite`.
    #[default]
    Invite,
    /// Anyone who reaches the hub.
    Open,
}

impl Registration {
    /// The value as written in `hub.toml`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invite => "invite",
            Self::Open => "open",
        }
    }
}

/// The `[libraries]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LibrariesConfig {
    /// Who may create a library.
    pub registration: Registration,
    /// The most libraries the hub keeps; 0 = no limit.
    pub max_libraries: u64,
    /// How long a hub invite code is valid, in days (at least 1).
    pub invite_days: u32,
    /// With open registration: libraries created per client address and day; 0 = no
    /// limit.
    pub new_libraries_per_address_per_day: u32,
    /// Days without a request after which a member account (by its own devices) or a
    /// library (by its token or any device of its accounts) is deleted; 0 = never.
    pub expiry_days: u32,
    /// The longest a member invite is valid, in days (at least 1).
    pub member_invite_max_days: u32,
}

impl Default for LibrariesConfig {
    fn default() -> Self {
        Self {
            registration: Registration::Invite,
            max_libraries: 0,
            invite_days: 7,
            new_libraries_per_address_per_day: 3,
            expiry_days: 0,
            member_invite_max_days: 7,
        }
    }
}

/// The `[operator]` section: who runs the hub, on what terms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OperatorConfig {
    /// Who runs the hub; empty = not said.
    pub name: String,
    /// An e-mail address or an `https://` URL for abuse reports; empty = none.
    pub contact: String,
    /// The terms of use: a plain text file in the data directory; empty = none.
    pub terms_file: String,
}

impl Default for OperatorConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            contact: String::new(),
            terms_file: "terms.txt".to_owned(),
        }
    }
}

/// The longest `[operator] name` and `contact`, in characters.
const MAX_OPERATOR_TEXT: usize = 200;

/// The `[quotas]` section, 0 = no limit: bytes and publications per library, devices
/// per account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotasConfig {
    /// Stored bytes of a library: its publication blobs and its accounts' records.
    pub max_bytes: u64,
    /// Publications of a library.
    pub max_publications: u64,
    /// Devices of an account (the library's token is not one).
    pub max_devices: u64,
}

/// The default [`QuotasConfig::max_devices`].
pub const DEFAULT_MAX_DEVICES: u64 = 5;

impl Default for QuotasConfig {
    fn default() -> Self {
        Self {
            max_bytes: 0,
            max_publications: 0,
            max_devices: DEFAULT_MAX_DEVICES,
        }
    }
}

/// The smallest Argon2id memory cost of a hub password, in KiB (OWASP's minimum; the
/// reader app refuses weaker parameters).
pub const MIN_PASSWORD_MEMORY_KIB: u32 = shelfgauge_core::password::MIN_MEMORY_KIB;

/// The smallest Argon2id iteration count of a hub password.
pub const MIN_PASSWORD_ITERATIONS: u32 = shelfgauge_core::password::MIN_ITERATIONS;

/// The `[passwords]` section: the Argon2id parameters given out for new and changed
/// hub passwords (the lanes are always 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PasswordsConfig {
    /// Memory in KiB (at least [`MIN_PASSWORD_MEMORY_KIB`]).
    pub memory_kib: u32,
    /// Iterations (at least [`MIN_PASSWORD_ITERATIONS`]).
    pub iterations: u32,
}

impl Default for PasswordsConfig {
    fn default() -> Self {
        Self {
            memory_kib: MIN_PASSWORD_MEMORY_KIB,
            iterations: MIN_PASSWORD_ITERATIONS,
        }
    }
}

/// The `[limits]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// The largest blob in bytes (at least 1024).
    pub max_blob_bytes: u64,
    /// The most bytes stored for all libraries together; 0 = no limit.
    pub max_total_bytes: u64,
    /// Days without opens after which a publication is deleted; 0 = never.
    pub publication_expiry_days: u32,
    /// API requests per minute and device token (at least 1).
    pub requests_per_minute: u32,
    /// Requests per hour to the routes without a token, per client address (at least 1).
    pub public_requests_per_hour: u32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_blob_bytes: DEFAULT_MAX_BLOB_BYTES,
            max_total_bytes: 0,
            publication_expiry_days: 0,
            requests_per_minute: 600,
            public_requests_per_hour: 60,
        }
    }
}

/// The result of [`HubConfig::load`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedHubConfig {
    /// The validated configuration.
    pub config: HubConfig,
    /// Dotted paths of keys in the file that the hub does not know.
    pub unknown_keys: Vec<String>,
}

impl HubConfig {
    /// Writes [`DEFAULT_HUB_TOML`] to `path` unless the file exists; returns whether it
    /// was created.
    pub fn create_default(path: &Path) -> Result<bool> {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
        {
            Ok(mut file) => {
                file.write_all(DEFAULT_HUB_TOML.as_bytes())
                    .map_err(|e| Error::io(path, e))?;
                Ok(true)
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    /// Loads and validates the config file at `path`.
    pub fn load(path: &Path) -> Result<LoadedHubConfig> {
        let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
        let (config, unknown_keys) = Self::from_toml(&text, path)?;
        Ok(LoadedHubConfig {
            config,
            unknown_keys,
        })
    }

    /// Parses and validates config file content; `path` is only used in error messages.
    ///
    /// Returns the config and the sorted dotted paths of unknown keys.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::path::Path;
    /// use shelfgauge::hub::config::{HubConfig, Registration};
    ///
    /// let text = "[libraries]\nregistration = \"open\"\n[server]\nprot = 1\n";
    /// let (config, unknown) = HubConfig::from_toml(text, Path::new("hub.toml")).unwrap();
    /// assert_eq!(config.libraries.registration, Registration::Open);
    /// assert_eq!(unknown, ["server.prot"]);
    /// ```
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
        if unknown_keys
            .iter()
            .any(|key| key == "accounts" || key.starts_with("accounts."))
        {
            // Ignoring it would silently drop a public hub's limits.
            return Err(Error::InvalidConfig {
                path: path.to_owned(),
                error: ConfigError::new(
                    "accounts",
                    "was replaced in shelfgauge 0.21 by [libraries] (registration, \
                     max_libraries, invite_days, new_libraries_per_address_per_day, \
                     expiry_days, member_invite_max_days); rename the section and its keys",
                ),
            });
        }
        config.validate().map_err(|error| Error::InvalidConfig {
            path: path.to_owned(),
            error,
        })?;
        Ok((config, unknown_keys))
    }

    /// Checks all values and normalizes `server.base_path` in place.
    pub fn validate(&mut self) -> Result<(), ConfigError> {
        if self.server.port == 0 {
            return Err(ConfigError::new(
                "server.port",
                "must be between 1 and 65535",
            ));
        }
        for host in &self.server.allowed_hosts {
            if host.is_empty() || host.contains(['/', '@', ' ', '#', '?']) {
                return Err(ConfigError::new(
                    "server.allowed_hosts",
                    format!("{host:?} is not a host name (`name` or `name:port`)"),
                ));
            }
        }
        for proxy in &self.server.trusted_proxies {
            if super::limits::IpRange::parse(proxy).is_none() {
                return Err(ConfigError::new(
                    "server.trusted_proxies",
                    format!("{proxy:?} is not an address or a CIDR range (`10.0.0.0/8`)"),
                ));
            }
        }
        self.server.base_path = normalize_base_path(&self.server.base_path)
            .map_err(|message| ConfigError::new("server.base_path", message))?;
        for (key, text) in [
            ("operator.name", &self.operator.name),
            ("operator.contact", &self.operator.contact),
        ] {
            if text.chars().count() > MAX_OPERATOR_TEXT || text.chars().any(char::is_control) {
                return Err(ConfigError::new(
                    key,
                    format!("must be one line of at most {MAX_OPERATOR_TEXT} characters"),
                ));
            }
        }
        let terms = &self.operator.terms_file;
        if terms.contains(['/', '\\']) || terms == "." || terms == ".." {
            return Err(ConfigError::new(
                "operator.terms_file",
                "must be a file name in the data directory",
            ));
        }
        if self.libraries.invite_days == 0 {
            return Err(ConfigError::new(
                "libraries.invite_days",
                "must be at least 1",
            ));
        }
        if self.libraries.member_invite_max_days == 0 {
            return Err(ConfigError::new(
                "libraries.member_invite_max_days",
                "must be at least 1",
            ));
        }
        if self.passwords.memory_kib < MIN_PASSWORD_MEMORY_KIB {
            return Err(ConfigError::new(
                "passwords.memory_kib",
                format!("must be at least {MIN_PASSWORD_MEMORY_KIB}"),
            ));
        }
        if self.passwords.iterations < MIN_PASSWORD_ITERATIONS {
            return Err(ConfigError::new(
                "passwords.iterations",
                format!("must be at least {MIN_PASSWORD_ITERATIONS}"),
            ));
        }
        if self.limits.max_blob_bytes < 1024 {
            return Err(ConfigError::new(
                "limits.max_blob_bytes",
                "must be at least 1024",
            ));
        }
        if self.limits.requests_per_minute == 0 {
            return Err(ConfigError::new(
                "limits.requests_per_minute",
                "must be at least 1",
            ));
        }
        if self.limits.public_requests_per_hour == 0 {
            return Err(ConfigError::new(
                "limits.public_requests_per_hour",
                "must be at least 1",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<(HubConfig, Vec<String>)> {
        HubConfig::from_toml(text, Path::new("hub.toml"))
    }

    #[test]
    fn default_template_parses_to_the_default_config() {
        let (config, unknown) = parse(DEFAULT_HUB_TOML).unwrap();
        assert_eq!(config, HubConfig::default());
        assert!(unknown.is_empty(), "{unknown:?}");
        assert_eq!(parse("").unwrap().0, HubConfig::default());
    }

    #[test]
    fn invalid_values_name_their_key() {
        let key = |text: &str| match parse(text).unwrap_err() {
            Error::InvalidConfig { error, .. } => error.key,
            other => panic!("expected InvalidConfig, got {other:?}"),
        };
        assert_eq!(key("[server]\nport = 0"), "server.port");
        assert_eq!(key("[server]\nbase_path = \"/a/../b\""), "server.base_path");
        assert_eq!(
            key("[server]\nallowed_hosts = [\"a/b\"]"),
            "server.allowed_hosts"
        );
        assert_eq!(
            key("[server]\ntrusted_proxies = [\"localhost\"]"),
            "server.trusted_proxies"
        );
        assert_eq!(key("[libraries]\ninvite_days = 0"), "libraries.invite_days");
        assert_eq!(
            key("[libraries]\nmember_invite_max_days = 0"),
            "libraries.member_invite_max_days"
        );
        assert_eq!(
            key("[passwords]\nmemory_kib = 19455"),
            "passwords.memory_kib"
        );
        assert_eq!(key("[passwords]\niterations = 1"), "passwords.iterations");
        assert_eq!(
            key("[operator]\nterms_file = \"../etc/passwd\""),
            "operator.terms_file"
        );
        assert_eq!(key("[operator]\nname = \"a\\nb\""), "operator.name");
        assert_eq!(
            key(&format!("[operator]\ncontact = \"{}\"", "x".repeat(201))),
            "operator.contact"
        );
        assert_eq!(
            key("[limits]\nmax_blob_bytes = 10"),
            "limits.max_blob_bytes"
        );
        assert_eq!(
            key("[limits]\nrequests_per_minute = 0"),
            "limits.requests_per_minute"
        );
        assert_eq!(
            key("[limits]\npublic_requests_per_hour = 0"),
            "limits.public_requests_per_hour"
        );
        assert!(matches!(
            parse("[libraries]\nregistration = \"closed\"").unwrap_err(),
            Error::ConfigSyntax { .. }
        ));
    }

    #[test]
    fn the_old_accounts_section_is_refused() {
        for text in [
            "[accounts]\nregistration = \"open\"",
            "[accounts]\n",
            "[accounts]\nmax_accounts = 3",
        ] {
            match parse(text).unwrap_err() {
                Error::InvalidConfig { error, .. } => {
                    assert_eq!(error.key, "accounts");
                    assert!(error.message.contains("[libraries]"), "{}", error.message);
                }
                other => panic!("expected InvalidConfig, got {other:?}"),
            }
        }
        let (config, _) = parse("[passwords]\nmemory_kib = 65536\niterations = 3").unwrap();
        assert_eq!(config.passwords.memory_kib, 65536);
    }

    #[test]
    fn base_path_is_normalized() {
        let (config, _) = parse("[server]\nbase_path = \"hub/\"").unwrap();
        assert_eq!(config.server.base_path, "/hub");
        assert_eq!(config.server.as_server_config().base_path, "/hub");
    }

    #[test]
    fn the_file_is_created_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hub.toml");
        assert!(HubConfig::create_default(&path).unwrap());
        assert!(!HubConfig::create_default(&path).unwrap());
        assert_eq!(HubConfig::load(&path).unwrap().config, HubConfig::default());
    }
}
