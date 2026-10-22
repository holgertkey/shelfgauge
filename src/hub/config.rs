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

[accounts]
# Who may create an account: "invite" (a one-time code from `shelfgauge hub invite`)
# or "open" (anyone who reaches the hub).
registration = "invite"
# The most accounts the hub keeps (0 = no limit).
max_accounts = 0
# How long an invite code is valid, in days.
invite_days = 7

[quotas]
# Per account; 0 = no limit.
# Stored bytes (publication blobs and sync records).
max_bytes = 0
# Publications.
max_publications = 0
# Devices and instances with a token.
max_devices = 0

[limits]
# The largest blob (one part or image of a publication) in bytes.
max_blob_bytes = 67108864
# A publication not opened for this many days is deleted (0 = never).
publication_expiry_days = 0
# API requests per minute and device token.
requests_per_minute = 600
# Requests per hour to the routes without a token (account, pair, recover), per
# client address; all clients behind a reverse proxy on this machine share one budget.
public_requests_per_hour = 60
"#;

/// The complete configuration of a hub.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HubConfig {
    /// The `[server]` section.
    pub server: HubServerConfig,
    /// The `[accounts]` section.
    pub accounts: AccountsConfig,
    /// The `[quotas]` section.
    pub quotas: QuotasConfig,
    /// The `[limits]` section.
    pub limits: LimitsConfig,
}

/// The `[server]` section: where the hub listens.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HubServerConfig {
    /// Address to listen on; any address is allowed (device tokens authenticate).
    pub bind: IpAddr,
    /// Fixed port; a busy port is a hard error.
    pub port: u16,
    /// Extra accepted `Host` header values (the hub's name behind a reverse proxy).
    pub allowed_hosts: Vec<String>,
    /// URL prefix for all routes; normalized to `/` or `/a/b` (no trailing slash).
    pub base_path: String,
}

impl Default for HubServerConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_HUB_PORT,
            allowed_hosts: Vec::new(),
            base_path: "/".to_owned(),
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

/// Who may create an account: `[accounts] registration`.
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

/// The `[accounts]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccountsConfig {
    /// Who may create an account.
    pub registration: Registration,
    /// The most accounts the hub keeps; 0 = no limit.
    pub max_accounts: u64,
    /// How long an invite code is valid, in days (at least 1).
    pub invite_days: u32,
}

impl Default for AccountsConfig {
    fn default() -> Self {
        Self {
            registration: Registration::Invite,
            max_accounts: 0,
            invite_days: 7,
        }
    }
}

/// The `[quotas]` section: per-account limits, 0 = no limit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuotasConfig {
    /// Stored bytes: publication blobs and sync records.
    pub max_bytes: u64,
    /// Publications.
    pub max_publications: u64,
    /// Devices and instances with a token (revoked ones do not count).
    pub max_devices: u64,
}

/// The `[limits]` section.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LimitsConfig {
    /// The largest blob in bytes (at least 1024).
    pub max_blob_bytes: u64,
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
    /// let text = "[accounts]\nregistration = \"open\"\n[server]\nprot = 1\n";
    /// let (config, unknown) = HubConfig::from_toml(text, Path::new("hub.toml")).unwrap();
    /// assert_eq!(config.accounts.registration, Registration::Open);
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
        self.server.base_path = normalize_base_path(&self.server.base_path)
            .map_err(|message| ConfigError::new("server.base_path", message))?;
        if self.accounts.invite_days == 0 {
            return Err(ConfigError::new(
                "accounts.invite_days",
                "must be at least 1",
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
        assert_eq!(key("[accounts]\ninvite_days = 0"), "accounts.invite_days");
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
            parse("[accounts]\nregistration = \"closed\"").unwrap_err(),
            Error::ConfigSyntax { .. }
        ));
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
