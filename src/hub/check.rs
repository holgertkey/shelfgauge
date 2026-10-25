//! `shelfgauge hub check <url>`: whether a hub serves exactly this build's reader app.
//!
//! The binary embeds every file of the reader app, so a binary of the same version is
//! the reference: [`check`] renders the shell pages, the Service Worker and the
//! manifest with the hub's base path, fetches each file from the hub and compares
//! the bytes, and checks the `Content-Security-Policy` of the start page. The
//! operator's own pages (`/terms`) and the API are not part of the app.
//!
//! What it cannot show: what the hub serves to other clients or at another time, or
//! which app a device has already cached.

use std::time::Duration;

use crate::hub_client::{ClientError, HubClient, HubUrl};

use super::CONTENT_SECURITY_POLICY;

/// The largest file read from the hub.
const MAX_FILE: u64 = 64 * 1024 * 1024;

/// The time limit of one request.
const TIMEOUT: Duration = Duration::from_secs(60);

/// The result of [`check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckReport {
    /// The program version the hub reports.
    pub hub_version: String,
    /// The reader app version of this build (the Service Worker's `VERSION`).
    pub app_version: String,
    /// How many files were compared.
    pub checked: usize,
    /// Files whose bytes differ from this build's.
    pub differing: Vec<String>,
    /// Files the hub does not serve, with the HTTP status.
    pub missing: Vec<(String, u16)>,
    /// Whether the start page has this build's `Content-Security-Policy`.
    pub csp_matches: bool,
}

impl CheckReport {
    /// Whether the hub serves exactly this build's reader app.
    pub fn matches(&self) -> bool {
        self.differing.is_empty() && self.missing.is_empty() && self.csp_matches
    }

    /// Whether the hub runs the same version as this binary.
    pub fn same_version(&self) -> bool {
        self.hub_version == crate::VERSION
    }
}

/// Compares the reader app the hub at `url` serves with this build's.
pub fn check(url: &HubUrl) -> Result<CheckReport, ClientError> {
    let info = HubClient::new(url.clone()).info()?;
    let tls = ureq::tls::TlsConfig::builder()
        .root_certs(ureq::tls::RootCerts::PlatformVerifier)
        .build();
    let agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(TIMEOUT))
        .user_agent(format!("shelfgauge/{}", crate::VERSION))
        .build()
        .new_agent();
    let mut report = CheckReport {
        hub_version: info.version,
        app_version: super::shell::app_version().to_owned(),
        checked: 0,
        differing: Vec::new(),
        missing: Vec::new(),
        csp_matches: false,
    };
    for (path, expected) in super::shell::expected_files(url.base_path()) {
        let mut response = agent
            .get(url.page(&path))
            .call()
            .map_err(|e| ClientError::Unreachable(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            report.missing.push((path, status.as_u16()));
            continue;
        }
        if path == "/" {
            report.csp_matches = response
                .headers()
                .get("content-security-policy")
                .and_then(|v| v.to_str().ok())
                == Some(CONTENT_SECURITY_POLICY);
        }
        let body = response
            .body_mut()
            .with_config()
            .limit(MAX_FILE)
            .read_to_vec()
            .map_err(|e| ClientError::Unreachable(e.to_string()))?;
        report.checked += 1;
        if body != expected {
            report.differing.push(path);
        }
    }
    Ok(report)
}
