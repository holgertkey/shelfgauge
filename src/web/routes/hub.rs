//! My profile → Phone and other devices: the profile's hub account (concept §11,
//! §12). Registering, joining with a recovery code, the recovery code until it is
//! saved, pairing a device, the device list, revoking and leaving.
//!
//! Hub calls are blocking HTTP and run on a blocking thread. The recovery code and the
//! pairing link (which carries the account key in its fragment) appear only in page
//! bodies sent with `no-store`, never in a URL or a log; form bodies are never logged.

use askama::Template;
use axum::extract::rejection::{FormRejection, QueryRejection};
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::{date, date_time, render};
use crate::annotations::now;
use crate::hub_client::keys::{self, AccountKey, RecoveryCode};
use crate::hub_client::store::{self, HubAccount, NewHubAccount};
use crate::hub_client::{ClientError, HubClient, HubUrl};
use crate::profiles::ProfileId;
use crate::web::layout::{Layout, SidebarMode};
use crate::web::profile::CurrentProfile;
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState, blocking, qr};

/// The page's title.
const TITLE: &str = "Phone and other devices";

/// The hub account as the page shows it.
struct AccountView {
    hub_url: String,
    recovery_pending: bool,
    joined: String,
    synced: Option<String>,
    sync_error: Option<String>,
}

/// A device of the account as the page shows it.
struct DeviceView {
    id: i64,
    name: String,
    paired: String,
    last_seen: String,
    current: bool,
}

#[derive(Template)]
#[template(path = "hub.html")]
struct HubPage {
    layout: Layout,
    notice: Option<String>,
    error: Option<String>,
    /// The configured hub for a new account (`[hub] url`).
    configured: Option<String>,
    account: Option<AccountView>,
    /// Whether the registration form asks for an invite code.
    invite: bool,
    devices: Vec<DeviceView>,
    /// Why the device list is missing.
    devices_error: Option<String>,
    action: String,
    recovery_action: String,
    profile_action: String,
}

#[derive(Template)]
#[template(path = "hub_code.html")]
struct CodePage {
    layout: Layout,
    /// `recovery` or `pair`.
    mode: &'static str,
    /// The text to copy: the recovery code or the pairing link.
    code: String,
    /// The QR code of `code` (inline SVG).
    qr: String,
    /// When a pairing link expires.
    expires: Option<String>,
    action: String,
    back: String,
}

/// The query of `GET /profile/hub`: what was just done.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct HubDone {
    done: Option<String>,
}

fn done_text(done: Option<&str>) -> Option<String> {
    Some(
        match done? {
            "joined" => "This library joined your hub account.",
            "saved" => "The recovery code is saved; this library no longer keeps it.",
            "revoked" => "The device was removed from your hub account.",
            "left" => {
                "This library left the hub account. Your marks and positions here are \
                 unchanged."
            }
            _ => return None,
        }
        .to_owned(),
    )
}

/// The name of this library on the account's devices.
fn instance_name(state: &AppState) -> String {
    let configured = state.config().hub.instance_name.clone();
    if !configured.is_empty() {
        return configured;
    }
    state
        .library()
        .root()
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "shelfgauge".to_owned())
}

/// The hub of `[hub] url`, if one is set.
fn configured_hub(state: &AppState) -> Option<HubUrl> {
    HubUrl::parse(&state.config().hub.url).ok()
}

async fn hub_account(state: &AppState, profile: ProfileId) -> Result<Option<HubAccount>, AppError> {
    super::profiles::user(state, move |_, conn| Ok(store::account(conn, profile)?)).await
}

/// Runs a hub call on a blocking thread.
async fn call<T: Send + 'static>(
    url: HubUrl,
    f: impl FnOnce(&HubClient) -> Result<T, ClientError> + Send + 'static,
) -> Result<Result<T, ClientError>, AppError> {
    blocking(move || f(&HubClient::new(url))).await
}

async fn layout(
    state: &AppState,
    current: &CurrentProfile,
    mode: SidebarMode,
) -> Result<Layout, AppError> {
    let s = state.clone();
    let current = current.clone();
    state
        .db(move |conn| {
            Ok(Layout::new(
                &s,
                &current,
                TITLE,
                sidebar::build(conn, &s, mode, &Focus::None)?,
            ))
        })
        .await
}

/// The page with `notice` or `error`, answered with `status`.
async fn page_response(
    state: &AppState,
    current: &CurrentProfile,
    mode: SidebarMode,
    notice: Option<String>,
    error: Option<String>,
    status: StatusCode,
) -> Result<Response, AppError> {
    let account = hub_account(state, current.id()).await?;
    let configured = configured_hub(state);
    let mut invite = true;
    let mut devices = Vec::new();
    let mut devices_error = None;
    let mut error = error;
    match &account {
        Some(account) => {
            let token = account.token.clone();
            let key = account.key.clone();
            match call(account.hub_url.clone(), move |c| c.devices(&token)).await? {
                Ok(list) => {
                    devices = list
                        .into_iter()
                        .map(|d| DeviceView {
                            id: d.id,
                            name: d
                                .name
                                .as_deref()
                                .and_then(|n| keys::open_device_name(&key, n))
                                .unwrap_or_else(|| "Unnamed device".to_owned()),
                            paired: date(d.created_at),
                            last_seen: date_time(d.last_seen_at),
                            current: d.current,
                        })
                        .collect();
                }
                Err(e) => devices_error = Some(e.user_text()),
            }
        }
        None => {
            if let Some(url) = &configured {
                match call(url.clone(), |c| c.info()).await? {
                    Ok(info) => invite = info.registration != "open",
                    Err(e) => error = error.or(Some(e.user_text())),
                }
            }
        }
    }
    let urls = state.urls();
    let page = HubPage {
        layout: layout(state, current, mode).await?,
        notice,
        error,
        configured: configured.map(|u| u.to_string()),
        account: account.map(|a| AccountView {
            hub_url: a.hub_url.to_string(),
            recovery_pending: a.recovery_pending,
            joined: date(a.created_at),
            synced: a.synced_at.map(date_time),
            sync_error: a.sync_error,
        }),
        invite,
        devices,
        devices_error,
        action: urls.profile_hub(),
        recovery_action: urls.profile_hub_recovery(),
        profile_action: urls.my_profile(),
    };
    Ok((
        status,
        [(header::CACHE_CONTROL, "no-store")],
        render(&page)?,
    )
        .into_response())
}

/// `GET /profile/hub`: Phone and other devices.
pub(crate) async fn page(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    query: Result<Query<HubDone>, QueryRejection>,
) -> Result<Response, AppError> {
    let done = query.map(|Query(d)| d).unwrap_or_default();
    let mode = SidebarMode::from_headers(&headers);
    page_response(
        &state,
        &current,
        mode,
        done_text(done.done.as_deref()),
        None,
        StatusCode::OK,
    )
    .await
}

/// `GET /profile/hub/recovery`: the recovery code until "I saved it" (`404` after).
pub(crate) async fn recovery(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let id = current.id();
    let code = super::profiles::user(&state, move |_, conn| {
        Ok(store::pending_recovery_code(conn, id)?)
    })
    .await?
    .ok_or(AppError::NotFound)?;
    let mode = SidebarMode::from_headers(&headers);
    code_page(&state, &current, mode, "recovery", code, None).await
}

async fn code_page(
    state: &AppState,
    current: &CurrentProfile,
    mode: SidebarMode,
    kind: &'static str,
    code: String,
    expires: Option<String>,
) -> Result<Response, AppError> {
    let label = if kind == "recovery" {
        "QR code of the recovery code"
    } else {
        "QR code of the pairing link"
    };
    let qr = qr::svg(&code, label)
        .ok_or_else(|| AppError::Internal("the code is too long for a QR code".to_owned()))?;
    let urls = state.urls();
    let page = CodePage {
        layout: layout(state, current, mode).await?,
        mode: kind,
        code,
        qr,
        expires,
        action: urls.profile_hub(),
        back: urls.profile_hub(),
    };
    Ok(([(header::CACHE_CONTROL, "no-store")], render(&page)?).into_response())
}

/// The form of `POST /profile/hub`; `action` says what to do. Never logged.
#[derive(Default, Deserialize)]
pub(crate) struct HubForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    invite: String,
    #[serde(default)]
    code: String,
    #[serde(default)]
    device: String,
}

/// `POST /profile/hub` (form): `register` (a new account on `[hub] url`; then the
/// recovery code), `join` (with a recovery code), `saved` (the recovery code is
/// saved), `pair` (shows a pairing link and its QR code), `revoke` (a device) or
/// `leave` (this library forgets the account). Refusals show the page again with the
/// reason.
pub(crate) async fn save(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    form: Result<Form<HubForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Form(form) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let mode = SidebarMode::from_headers(&headers);
    let id = current.id();
    let account = hub_account(&state, id).await?;
    let outcome = match (form.action.as_str(), account) {
        ("register", None) => register(&state, id, form.invite.trim()).await?,
        ("join", None) => join(&state, id, &form.code).await?,
        ("register" | "join", Some(_)) => {
            Outcome::Refused("This profile already has a hub account; leave it first.".to_owned())
        }
        ("saved", Some(_)) => {
            super::profiles::user(&state, move |_, conn| {
                Ok(store::confirm_recovery_code(conn, id)?)
            })
            .await?;
            tracing::info!(profile = id.0, "hub recovery code saved");
            Outcome::Done("saved")
        }
        ("pair", Some(account)) => {
            let token = account.token.clone();
            match call(account.hub_url.clone(), move |c| c.create_pairing(&token)).await? {
                Ok(pairing) => {
                    tracing::info!(profile = id.0, "hub pairing link shown");
                    let link = account.hub_url.pair_link(&pairing.secret, &account.key);
                    return code_page(
                        &state,
                        &current,
                        mode,
                        "pair",
                        link,
                        Some(date_time(pairing.expires_at)),
                    )
                    .await;
                }
                Err(e) => Outcome::Refused(e.user_text()),
            }
        }
        ("revoke", Some(account)) => {
            let device: i64 = form
                .device
                .trim()
                .parse()
                .map_err(|_| AppError::BadRequest("invalid device".to_owned()))?;
            let token = account.token.clone();
            match call(account.hub_url.clone(), move |c| {
                c.revoke_device(&token, device)
            })
            .await?
            {
                Ok(()) => {
                    tracing::info!(profile = id.0, device, "hub device revoked");
                    Outcome::Done("revoked")
                }
                Err(e) => Outcome::Refused(e.user_text()),
            }
        }
        // The stored recovery code is the only copy until it is saved: leaving would
        // lose the account if no device is paired.
        ("leave", Some(account)) if account.recovery_pending => Outcome::Refused(
            "Save the recovery code first (Show the recovery code, then I saved it): \
             without it this library could not join the account again."
                .to_owned(),
        ),
        ("leave", Some(account)) => {
            // Best effort: the hub forgets this library's token; leaving works offline.
            let token = account.token.clone();
            let own = account.device_id;
            if let Err(e) = call(account.hub_url.clone(), move |c| {
                c.revoke_device(&token, own)
            })
            .await?
            {
                tracing::info!(profile = id.0, error = %e, "the hub did not revoke this library");
            }
            super::profiles::user(&state, move |_, conn| Ok(store::forget_account(conn, id)?))
                .await?;
            tracing::info!(profile = id.0, "hub account left");
            Outcome::Done("left")
        }
        ("saved" | "pair" | "revoke" | "leave", None) => {
            Outcome::Refused("This profile has no hub account.".to_owned())
        }
        _ => return Err(AppError::BadRequest("unknown action".to_owned())),
    };
    match outcome {
        Outcome::Done(done) => Ok(Redirect::to(&format!(
            "{}?done={done}",
            state.urls().profile_hub()
        ))
        .into_response()),
        Outcome::Recovery => Ok(Redirect::to(&state.urls().profile_hub_recovery()).into_response()),
        Outcome::Refused(error) => {
            page_response(
                &state,
                &current,
                mode,
                None,
                Some(error),
                StatusCode::UNPROCESSABLE_ENTITY,
            )
            .await
        }
    }
}

/// What a post did.
enum Outcome {
    /// Done: `303` to the page with this `done` value.
    Done(&'static str),
    /// Registered: `303` to the recovery code.
    Recovery,
    /// Refused: the page again with this reason.
    Refused(String),
}

fn random_error(e: getrandom::Error) -> AppError {
    AppError::Internal(format!("no random numbers: {e}"))
}

async fn register(state: &AppState, id: ProfileId, invite: &str) -> Result<Outcome, AppError> {
    let Some(url) = configured_hub(state) else {
        return Ok(Outcome::Refused(
            "No hub is set: add its address as url in the [hub] section of config.toml.".to_owned(),
        ));
    };
    let key = AccountKey::generate().map_err(random_error)?;
    let mut secret = [0u8; 32];
    getrandom::fill(&mut secret).map_err(random_error)?;
    let name = keys::seal_device_name(&key, &instance_name(state)).map_err(random_error)?;
    let invite = (!invite.is_empty()).then(|| invite.to_owned());
    let recovery_secret = keys::recovery_secret(&secret);
    let issued = match call(url.clone(), move |c| {
        c.create_account(invite.as_deref(), &recovery_secret, &name)
    })
    .await?
    {
        Ok(issued) => issued,
        Err(e) => return Ok(Outcome::Refused(e.user_text())),
    };
    let code = RecoveryCode::new(issued.account, key.clone(), secret).to_text();
    let saved = super::profiles::user(state, move |_, conn| {
        let new = NewHubAccount {
            hub_url: &url,
            account: issued.account,
            key: &key,
            device_id: issued.device,
            token: &issued.token,
            recovery_code: Some(&code),
        };
        Ok(store::save_account(conn, id, &new, now())?)
    })
    .await?;
    if !saved {
        return Ok(Outcome::Refused(
            "This profile already has a hub account; leave it first.".to_owned(),
        ));
    }
    tracing::info!(profile = id.0, "hub account registered");
    Ok(Outcome::Recovery)
}

async fn join(state: &AppState, id: ProfileId, text: &str) -> Result<Outcome, AppError> {
    let Some(url) = configured_hub(state) else {
        return Ok(Outcome::Refused(
            "No hub is set: add its address as url in the [hub] section of config.toml.".to_owned(),
        ));
    };
    let code = match RecoveryCode::parse(text) {
        Ok(code) => code,
        Err(e) => {
            return Ok(Outcome::Refused(format!(
                "{}.",
                capitalized(&e.to_string())
            )));
        }
    };
    let key = code.key().clone();
    let name = keys::seal_device_name(&key, &instance_name(state)).map_err(random_error)?;
    let account = code.account();
    let secret = code.secret();
    let issued = match call(url.clone(), move |c| c.recover(account, &secret, &name)).await? {
        Ok(issued) => issued,
        Err(e) => return Ok(Outcome::Refused(e.user_text())),
    };
    if issued.account != account {
        return Err(AppError::Internal(
            "the hub answered for another account".to_owned(),
        ));
    }
    let saved = super::profiles::user(state, move |_, conn| {
        let new = NewHubAccount {
            hub_url: &url,
            account,
            key: &key,
            device_id: issued.device,
            token: &issued.token,
            recovery_code: None,
        };
        Ok(store::save_account(conn, id, &new, now())?)
    })
    .await?;
    if !saved {
        return Ok(Outcome::Refused(
            "This profile already has a hub account; leave it first.".to_owned(),
        ));
    }
    tracing::info!(profile = id.0, "hub account joined");
    Ok(Outcome::Done("joined"))
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}
