//! The instance's side of the hub (`docs/ARCHITECTURE.md`, "V2: hub"): a profile's
//! hub account, its devices, publications and sync records.
//!
//! [`HubUrl`] checks a hub's address (HTTPS, plain HTTP only on loopback);
//! [`HubClient`] calls its API with blocking HTTP (`ureq` with rustls and the
//! platform's trust store; call it on a blocking thread); [`keys`] holds the account
//! key and the record crypto; [`store`] the hub tables of `userdata.sqlite`.
//!
//! Never logged: device tokens, pairing secrets, recovery codes, account keys, book
//! keys, public account ids and publication ids (also not inside URLs: client errors
//! never carry the request URL).

pub mod book_file;
pub mod keys;
pub mod merge;
pub mod records;
pub mod store;
pub mod sync;

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::hub::{AccountId, Secret};
use crate::publication::format::{BlobPath, PublicationId, VersionRange};
use keys::AccountKey;

/// A hub's base URL: `https://host[:port][/path]`, or `http://` for a loopback host;
/// lowercase scheme and host, no trailing slash, no query, fragment or user info.
///
/// # Examples
///
/// ```
/// use shelfgauge::hub_client::HubUrl;
///
/// let url = HubUrl::parse(" HTTPS://Hub.Example.org/sg/ ").unwrap();
/// assert_eq!(url.to_string(), "https://hub.example.org/sg");
/// assert_eq!(url.base_path(), "/sg");
/// assert_eq!(HubUrl::parse("https://hub.example.org").unwrap().base_path(), "");
/// assert!(HubUrl::parse("http://127.0.0.1:8743").is_ok());
/// assert!(HubUrl::parse("http://hub.example.org").is_err());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HubUrl(String);

/// Why a hub URL was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct HubUrlError(&'static str);

impl HubUrl {
    /// Checks and normalizes `text`.
    pub fn parse(text: &str) -> Result<Self, HubUrlError> {
        let text = text.trim();
        let (scheme, rest) = text
            .split_once("://")
            .ok_or(HubUrlError("must start with https://"))?;
        let scheme = scheme.to_ascii_lowercase();
        if scheme != "https" && scheme != "http" {
            return Err(HubUrlError("must start with https://"));
        }
        if rest.contains(|c: char| {
            c.is_whitespace() || c.is_control() || matches!(c, '?' | '#' | '@' | '\\')
        }) {
            return Err(HubUrlError(
                "must be a plain address without spaces, ?, #, @ or \\",
            ));
        }
        let (authority, path) = match rest.find('/') {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        let authority = authority.to_ascii_lowercase();
        let (host, port) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, after) = inner
                .split_once(']')
                .ok_or(HubUrlError("has an invalid host"))?;
            let port = match after {
                "" => None,
                p => Some(
                    p.strip_prefix(':')
                        .ok_or(HubUrlError("has an invalid host"))?,
                ),
            };
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(HubUrlError("has an invalid host"));
            }
            (host, port)
        } else {
            match authority.split_once(':') {
                Some((host, port)) => (host, Some(port)),
                None => (authority.as_str(), None),
            }
        };
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'))
        {
            return Err(HubUrlError("has an invalid host"));
        }
        if let Some(port) = port
            && !port.parse::<u16>().is_ok_and(|p| p > 0)
        {
            return Err(HubUrlError("has an invalid port"));
        }
        if scheme == "http" && !is_loopback(host) {
            return Err(HubUrlError(
                "must use https:// (http:// only for 127.0.0.1, ::1 or localhost)",
            ));
        }
        let path = crate::config::normalize_base_path(path)
            .map_err(|_| HubUrlError("has an invalid path"))?;
        let path = if path == "/" { "" } else { path.as_str() };
        Ok(Self(format!("{scheme}://{authority}{path}")))
    }

    /// The path part of the URL: `""` or `/a/b` (the hub's `base_path`).
    pub fn base_path(&self) -> &str {
        let after_scheme = self.0.find("://").map_or(0, |i| i + 3);
        self.0[after_scheme..]
            .find('/')
            .map_or("", |i| &self.0[after_scheme + i..])
    }

    /// The URL of `path` below the hub's base (`/sw.js`).
    pub fn page(&self, path: &str) -> String {
        format!("{}{path}", self.0)
    }

    /// The URL of the API route `route` (without `/api/`).
    pub fn api(&self, route: &str) -> String {
        format!("{}/api/{route}", self.0)
    }

    /// The link of a publication: it opens only on the account's paired devices.
    pub fn book_link(&self, id: &PublicationId) -> String {
        format!("{}/r/{id}", self.0)
    }

    /// The pairing link of a device: the secret and the account key travel in the
    /// fragment, which browsers never send to a server.
    pub fn pair_link(&self, secret: &Secret, key: &AccountKey) -> String {
        format!("{}/pair#{}.{}", self.0, secret.as_str(), key.to_text())
    }
}

impl fmt::Display for HubUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `host` (lowercase, IPv6 without brackets) is this computer.
fn is_loopback(host: &str) -> bool {
    host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// A failed hub call. The messages never contain the request URL or a secret.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClientError {
    /// The hub could not be reached (DNS, connection, TLS, timeout).
    #[error("the hub cannot be reached: {0}")]
    Unreachable(String),
    /// The hub answered with an error.
    #[error("the hub refused the request: {message}")]
    Hub {
        /// The HTTP status.
        status: u16,
        /// The hub's error code (`invite_required`, `quota_exceeded`, …).
        code: String,
        /// The hub's message.
        message: String,
    },
    /// The answer is not what the API promises (not a shelfgauge hub?).
    #[error("the hub's answer is not understood (is this a shelfgauge hub?)")]
    BadAnswer,
    /// No random numbers.
    #[error("no random numbers available: {0}")]
    Random(String),
}

impl ClientError {
    /// The hub's error code, if the hub answered with one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Hub { code, .. } => Some(code),
            _ => None,
        }
    }

    /// Whether the device token is no longer valid (revoked on the hub).
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Hub { status: 401, .. })
    }

    /// A short text for the person, without technical detail where there is a plainer
    /// one.
    pub fn user_text(&self) -> String {
        match self.code() {
            Some("invite_required") => {
                "The hub needs a valid invite code (its operator creates one with \
                 shelfgauge hub invite)."
                    .to_owned()
            }
            Some("invalid_secret") => "The code is not valid (wrong, used or expired).".to_owned(),
            Some("quota_exceeded") => format!("The hub refused: {}.", self.hub_message()),
            Some("rate_limited") => "The hub is busy; try again later.".to_owned(),
            Some("hub_full") => {
                "The hub is full and takes no new books or marks for now; try again later."
                    .to_owned()
            }
            Some("account_limit") => {
                "This hub creates only a few accounts a day from one address; try again \
                 tomorrow."
                    .to_owned()
            }
            Some("unauthorized") => {
                "This library is no longer paired with the hub account (it was removed on \
                 another device)."
                    .to_owned()
            }
            _ => self.to_string(),
        }
    }

    fn hub_message(&self) -> &str {
        match self {
            Self::Hub { message, .. } => message,
            _ => "",
        }
    }
}

impl From<getrandom::Error> for ClientError {
    fn from(e: getrandom::Error) -> Self {
        Self::Random(e.to_string())
    }
}

/// Maps a transport error to [`ClientError::Unreachable`] without the URL.
fn transport(e: ureq::Error) -> ClientError {
    let text = match e {
        ureq::Error::Timeout(_) => "no answer in time".to_owned(),
        ureq::Error::HostNotFound => "host name not found".to_owned(),
        ureq::Error::ConnectionFailed => "connection failed".to_owned(),
        // rustls reports certificate errors as `InvalidData` I/O errors.
        ureq::Error::Io(e) if e.kind() == std::io::ErrorKind::InvalidData => format!("TLS: {e}"),
        ureq::Error::Io(e) => e.kind().to_string(),
        ureq::Error::Tls(text) => format!("TLS: {text}"),
        ureq::Error::Rustls(e) => format!("TLS: {e}"),
        ureq::Error::TooManyRedirects | ureq::Error::RedirectFailed => {
            "the hub redirects (check the URL)".to_owned()
        }
        ureq::Error::BodyExceedsLimit(_) => return ClientError::BadAnswer,
        _ => "request failed".to_owned(),
    };
    ClientError::Unreachable(text)
}

/// What `GET /api/info` says.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HubInfo {
    /// Always `shelfgauge-hub`.
    pub service: String,
    /// The hub's program version.
    pub version: String,
    /// The publication format versions the hub accepts.
    pub formats: VersionRange,
    /// `invite` or `open`.
    pub registration: String,
    /// Size limits.
    pub limits: HubLimits,
}

/// The limits in [`HubInfo`].
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HubLimits {
    /// The largest blob.
    pub max_blob_bytes: u64,
    /// The largest record ciphertext.
    pub max_record_bytes: u64,
    /// The most records in one push.
    pub max_sync_records: u64,
    /// Days without opens before a publication is deleted; 0 = never.
    pub publication_expiry_days: u32,
}

/// A device token issued by the hub (registration, recovery).
#[derive(Debug, Clone)]
pub struct Issued {
    /// The account's public id.
    pub account: AccountId,
    /// The device's id on the hub.
    pub device: i64,
    /// The device token.
    pub token: Secret,
}

#[derive(Deserialize)]
struct IssuedBody {
    account: String,
    device: i64,
    token: String,
}

/// A pairing secret for a new device.
#[derive(Debug, Clone)]
pub struct Pairing {
    /// The one-time secret.
    pub secret: Secret,
    /// When it expires (Unix seconds).
    pub expires_at: i64,
}

/// A device of the account as the hub lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The device's id on the hub.
    pub id: i64,
    /// The sealed name, if the device gave one.
    pub name: Option<Vec<u8>>,
    /// When it was paired (Unix seconds).
    pub created_at: i64,
    /// When it last called the hub (Unix seconds).
    pub last_seen_at: i64,
    /// Whether it is the calling device.
    pub current: bool,
}

#[derive(Deserialize)]
struct DeviceBody {
    id: i64,
    name: Option<String>,
    created_at: i64,
    last_seen_at: i64,
    current: bool,
}

#[derive(Deserialize)]
struct ErrorBody {
    error: String,
    message: String,
}

/// The time limit of one ordinary call.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The time limit of uploading one blob.
const BLOB_TIMEOUT: Duration = Duration::from_secs(600);

/// The time limit of connecting.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest JSON answer read.
const MAX_ANSWER: u64 = 32 * 1024 * 1024;

/// A blocking client of one hub; the device token is passed per call.
#[derive(Debug, Clone)]
pub struct HubClient {
    url: HubUrl,
    agent: ureq::Agent,
}

impl HubClient {
    /// A client of the hub at `url`.
    pub fn new(url: HubUrl) -> Self {
        let tls = ureq::tls::TlsConfig::builder()
            .root_certs(ureq::tls::RootCerts::PlatformVerifier)
            .build();
        let agent = ureq::Agent::config_builder()
            .tls_config(tls)
            // A redirect to another host would carry the bearer token.
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_global(Some(CALL_TIMEOUT))
            .user_agent(format!("shelfgauge/{}", crate::VERSION))
            .build()
            .new_agent();
        Self { url, agent }
    }

    /// The hub's URL.
    pub fn url(&self) -> &HubUrl {
        &self.url
    }

    /// `GET /api/info`.
    pub fn info(&self) -> Result<HubInfo, ClientError> {
        let response = self
            .agent
            .get(self.url.api("info"))
            .call()
            .map_err(transport)?;
        let info: HubInfo = answer(response)?;
        if info.service != "shelfgauge-hub" {
            return Err(ClientError::BadAnswer);
        }
        Ok(info)
    }

    /// `POST /api/account`: a new account with this instance as its first device.
    pub fn create_account(
        &self,
        invite: Option<&str>,
        recovery: &Secret,
        device_name: &[u8],
    ) -> Result<Issued, ClientError> {
        let body = json!({
            "invite": invite,
            "recovery_hash": URL_SAFE_NO_PAD.encode(recovery.hash()),
            "device_name": URL_SAFE_NO_PAD.encode(device_name),
        });
        self.issue("account", body)
    }

    /// `POST /api/recover`: a device token for an existing account.
    pub fn recover(
        &self,
        account: AccountId,
        secret: &Secret,
        device_name: &[u8],
    ) -> Result<Issued, ClientError> {
        let body = json!({
            "account": account.to_string(),
            "secret": secret.as_str(),
            "device_name": URL_SAFE_NO_PAD.encode(device_name),
        });
        self.issue("recover", body)
    }

    fn issue(&self, route: &str, body: serde_json::Value) -> Result<Issued, ClientError> {
        let response = self
            .agent
            .post(self.url.api(route))
            .send_json(body)
            .map_err(transport)?;
        let body: IssuedBody = answer(response)?;
        Ok(Issued {
            account: body.account.parse().map_err(|_| ClientError::BadAnswer)?,
            device: body.device,
            token: Secret::from_text(&body.token),
        })
    }

    /// `POST /api/pairings`: a one-time pairing secret.
    pub fn create_pairing(&self, token: &Secret) -> Result<Pairing, ClientError> {
        #[derive(Deserialize)]
        struct Body {
            secret: String,
            expires_at: i64,
        }
        let response = self
            .agent
            .post(self.url.api("pairings"))
            .header("Authorization", bearer(token))
            .send_empty()
            .map_err(transport)?;
        let body: Body = answer(response)?;
        Ok(Pairing {
            secret: Secret::from_text(&body.secret),
            expires_at: body.expires_at,
        })
    }

    /// `GET /api/devices`.
    pub fn devices(&self, token: &Secret) -> Result<Vec<Device>, ClientError> {
        #[derive(Deserialize)]
        struct Body {
            devices: Vec<DeviceBody>,
        }
        let response = self
            .agent
            .get(self.url.api("devices"))
            .header("Authorization", bearer(token))
            .call()
            .map_err(transport)?;
        let body: Body = answer(response)?;
        body.devices
            .into_iter()
            .map(|d| {
                let name = d
                    .name
                    .map(|n| URL_SAFE_NO_PAD.decode(n))
                    .transpose()
                    .map_err(|_| ClientError::BadAnswer)?;
                Ok(Device {
                    id: d.id,
                    name,
                    created_at: d.created_at,
                    last_seen_at: d.last_seen_at,
                    current: d.current,
                })
            })
            .collect()
    }

    /// `DELETE /api/devices/{id}`.
    pub fn revoke_device(&self, token: &Secret, id: i64) -> Result<(), ClientError> {
        let response = self
            .agent
            .delete(self.url.api(&format!("devices/{id}")))
            .header("Authorization", bearer(token))
            .call()
            .map_err(transport)?;
        empty(response)
    }
}

impl HubClient {
    /// `PUT /api/pub/{id}/{path}`: uploads a sealed blob (the first one creates the
    /// publication).
    pub fn put_blob(
        &self,
        token: &Secret,
        id: &PublicationId,
        path: BlobPath,
        blob: &[u8],
    ) -> Result<(), ClientError> {
        let response = self
            .agent
            .put(self.url.api(&format!("pub/{id}/{path}")))
            .config()
            .timeout_global(Some(BLOB_TIMEOUT))
            .build()
            .header("Authorization", bearer(token))
            .content_type("application/octet-stream")
            .send(blob)
            .map_err(transport)?;
        empty(response)
    }

    /// `DELETE /api/pub/{id}`; a publication the hub no longer has counts as deleted.
    pub fn delete_publication(
        &self,
        token: &Secret,
        id: &PublicationId,
    ) -> Result<(), ClientError> {
        let response = self
            .agent
            .delete(self.url.api(&format!("pub/{id}")))
            .header("Authorization", bearer(token))
            .call()
            .map_err(transport)?;
        match empty(response) {
            Err(ClientError::Hub { status: 404, .. }) => Ok(()),
            other => other,
        }
    }

    /// `GET /api/pub`: the ids of the account's publications on the hub.
    pub fn publications(&self, token: &Secret) -> Result<Vec<PublicationId>, ClientError> {
        #[derive(Deserialize)]
        struct Item {
            id: String,
        }
        #[derive(Deserialize)]
        struct Body {
            publications: Vec<Item>,
        }
        let response = self
            .agent
            .get(self.url.api("pub"))
            .header("Authorization", bearer(token))
            .call()
            .map_err(transport)?;
        let body: Body = answer(response)?;
        body.publications
            .into_iter()
            .map(|p| p.id.parse().map_err(|_| ClientError::BadAnswer))
            .collect()
    }

    /// `POST /api/sync`: pushes records (`data: None` deletes one); answers the
    /// account's newest `seq`.
    pub fn push(&self, token: &Secret, records: &[OutRecord]) -> Result<i64, ClientError> {
        #[derive(Deserialize)]
        struct Body {
            seq: i64,
        }
        let records: Vec<_> = records
            .iter()
            .map(|r| {
                json!({
                    "kind": r.kind,
                    "id": r.id,
                    "data": r.data.as_deref().map(|d| URL_SAFE_NO_PAD.encode(d)),
                })
            })
            .collect();
        let response = self
            .agent
            .post(self.url.api("sync"))
            .header("Authorization", bearer(token))
            .send_json(json!({ "records": records }))
            .map_err(transport)?;
        let body: Body = answer(response)?;
        Ok(body.seq)
    }
}

impl HubClient {
    /// `GET /api/sync?since=N`: one page of records changed after `since`.
    pub fn pull(&self, token: &Secret, since: i64) -> Result<Pulled, ClientError> {
        #[derive(Deserialize)]
        struct Item {
            kind: String,
            id: String,
            data: Option<String>,
            updated_at: i64,
        }
        #[derive(Deserialize)]
        struct Body {
            records: Vec<Item>,
            seq: i64,
            more: bool,
        }
        let response = self
            .agent
            .get(self.url.api(&format!("sync?since={since}&limit=500")))
            .header("Authorization", bearer(token))
            .call()
            .map_err(transport)?;
        let body: Body = answer(response)?;
        let records = body
            .records
            .into_iter()
            .map(|r| {
                Ok(InRecord {
                    kind: r.kind,
                    id: r.id,
                    data: r
                        .data
                        .map(|d| URL_SAFE_NO_PAD.decode(d))
                        .transpose()
                        .map_err(|_| ClientError::BadAnswer)?,
                    updated_at: r.updated_at,
                })
            })
            .collect::<Result<_, ClientError>>()?;
        Ok(Pulled {
            records,
            seq: body.seq,
            more: body.more,
        })
    }
}

/// A page of pulled records.
#[derive(Debug, Clone)]
pub struct Pulled {
    /// The records in `seq` order.
    pub records: Vec<InRecord>,
    /// The cursor for the next pull.
    pub seq: i64,
    /// Whether more records follow.
    pub more: bool,
}

/// A pulled record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InRecord {
    /// The kind.
    pub kind: String,
    /// The record id.
    pub id: String,
    /// The sealed record; `None` when it was deleted.
    pub data: Option<Vec<u8>>,
    /// When the hub stored this state (the hub's clock, Unix seconds).
    pub updated_at: i64,
}

/// A record to push.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutRecord {
    /// `shelf_item`, `annotation` or `position`.
    pub kind: &'static str,
    /// The record id.
    pub id: String,
    /// The sealed record; `None` deletes it.
    pub data: Option<Vec<u8>>,
}

fn bearer(token: &Secret) -> String {
    format!("Bearer {}", token.as_str())
}

/// The error of a non-2xx answer.
fn refused(mut response: ureq::http::Response<ureq::Body>) -> ClientError {
    let status = response.status().as_u16();
    if response.status().is_redirection() {
        // Never followed (`max_redirects(0)`): it would carry the bearer token.
        return ClientError::Unreachable(
            "the hub redirects to another address (check the URL)".to_owned(),
        );
    }
    match response
        .body_mut()
        .with_config()
        .limit(64 * 1024)
        .read_json::<ErrorBody>()
    {
        Ok(body) => ClientError::Hub {
            status,
            code: body.error,
            message: body.message,
        },
        Err(_) => ClientError::Hub {
            status,
            code: String::new(),
            message: format!("HTTP status {status}"),
        },
    }
}

/// The JSON body of a successful answer.
fn answer<T: DeserializeOwned>(
    mut response: ureq::http::Response<ureq::Body>,
) -> Result<T, ClientError> {
    if !response.status().is_success() {
        return Err(refused(response));
    }
    response
        .body_mut()
        .with_config()
        .limit(MAX_ANSWER)
        .read_json()
        .map_err(|_| ClientError::BadAnswer)
}

/// A successful answer without a body.
fn empty(response: ureq::http::Response<ureq::Body>) -> Result<(), ClientError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(refused(response))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hub_error_codes_have_plain_texts() {
        let error = |status: u16, code: &str| ClientError::Hub {
            status,
            code: code.to_owned(),
            message: "the hub is full and accepts no new data for now".to_owned(),
        };
        assert!(
            error(507, "hub_full")
                .user_text()
                .starts_with("The hub is full")
        );
        assert!(error(429, "account_limit").user_text().contains("tomorrow"));
        assert!(error(429, "rate_limited").user_text().contains("busy"));
    }

    #[test]
    fn hub_urls_are_normalized() {
        for (text, expected) in [
            ("https://hub.example.org", "https://hub.example.org"),
            (
                "HTTPS://HUB.example.org:8443/",
                "https://hub.example.org:8443",
            ),
            (
                "https://example.org//sg//hub/",
                "https://example.org/sg/hub",
            ),
            ("http://localhost:8743", "http://localhost:8743"),
            ("http://127.0.0.2", "http://127.0.0.2"),
            ("http://[::1]:8743/x", "http://[::1]:8743/x"),
            ("https://[2001:db8::1]", "https://[2001:db8::1]"),
        ] {
            assert_eq!(HubUrl::parse(text).unwrap().to_string(), expected, "{text}");
        }
    }

    #[test]
    fn bad_hub_urls_are_refused() {
        for text in [
            "",
            "hub.example.org",
            "ftp://hub.example.org",
            "http://hub.example.org",
            "http://192.168.1.2:8743",
            "https://user@hub.example.org",
            "https://hub.example.org/?x=1",
            "https://hub.example.org/#frag",
            "https://hub.example.org:0",
            "https://hub.example.org:99999",
            "https://hub.example.org/a/../b",
            "https://hub example.org",
            "https://[::1",
            "https:///path",
            "https://hub.example.org/{x}",
        ] {
            assert!(HubUrl::parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn links_are_built_from_the_base() {
        let url = HubUrl::parse("https://example.org/hub").unwrap();
        assert_eq!(url.api("info"), "https://example.org/hub/api/info");
        let secret = Secret::from_text("s3cret");
        let key = AccountKey::new([0; 32]);
        assert_eq!(
            url.pair_link(&secret, &key),
            format!("https://example.org/hub/pair#s3cret.{}", "A".repeat(43))
        );
    }

    /// The HTTPS path against real servers (needs the internet; run with
    /// `--ignored`): a valid certificate passes TLS, bad ones are refused.
    #[test]
    #[ignore = "needs the internet"]
    fn https_certificates_are_checked() {
        let client = HubClient::new(HubUrl::parse("https://example.org").unwrap());
        match client.info() {
            Err(ClientError::BadAnswer | ClientError::Hub { .. }) => {}
            other => panic!("a valid certificate should pass TLS: {other:?}"),
        }
        for host in [
            "https://self-signed.badssl.com",
            "https://expired.badssl.com",
        ] {
            let client = HubClient::new(HubUrl::parse(host).unwrap());
            match client.info() {
                Err(ClientError::Unreachable(text)) if text.starts_with("TLS: invalid peer") => {
                    println!("{host}: {text}");
                }
                other => panic!("{host}: {other:?}"),
            }
        }
    }
}
