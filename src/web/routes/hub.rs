//! My profile → Phone and reading through the hub: the library on its hub and this
//! profile's account in it (Phase 30).
//!
//! An admin creates the library on the hub here (its handle, the owner's name, a hub
//! invite code when the hub asks for one), after a version check and the reader app
//! check of `shelfgauge hub check`. The owner profile pairs its phone, sees and
//! removes its devices, changes the name part of its address and sets an optional hub
//! password (derived on this computer, [`shelfgauge_core::password`]). Any profile
//! with an account sees its address and the last sync.
//!
//! Hub calls are blocking HTTP and run on a blocking thread. The pairing link (which
//! carries the account key in its fragment) appears only in page bodies sent with
//! `no-store`, never in a URL or a log; form bodies (passwords) are never logged.

use askama::Template;
use axum::extract::rejection::{FormRejection, QueryRejection};
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::{date, date_time, render};
use crate::annotations::now;
use crate::hub::address::normalize_label;
use crate::hub_client::keys::{self, AccountKey};
use crate::hub_client::store::{self, HubAccount, HubLibrary, Role};
use crate::hub_client::{ClientError, HubClient, HubInfo, HubUrl, PasswordUpload};
use crate::profiles::ProfileId;
use crate::web::layout::{Layout, SidebarMode};
use crate::web::profile::CurrentProfile;
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState, blocking, qr};

/// The page's title.
const TITLE: &str = "Phone and reading through the hub";

/// The library on its hub as the page shows it.
struct LibraryView {
    hub_url: String,
    handle: String,
    created: String,
}

/// This profile's account as the page shows it.
struct AccountView {
    /// `name@handle`, once the name is known.
    address: Option<String>,
    owner: bool,
    has_password: bool,
    /// A pending invite: "invited until <date>" or "password reset until <date>".
    pending: Option<String>,
    gone: bool,
    synced: Option<String>,
    sync_error: Option<String>,
}

/// A device of the owner account as the page shows it.
struct DeviceView {
    id: i64,
    name: String,
    paired: String,
    last_seen: String,
}

/// What the form that creates the library needs.
struct CreateView {
    hub_url: String,
    handle: String,
    owner_name: String,
    /// Whether the hub asks for a hub invite code.
    invite: bool,
    /// The operator's name and contact, if the hub says them.
    operator: Option<String>,
    /// The hub's terms page, if it has terms.
    terms: Option<String>,
}

#[derive(Template)]
#[template(path = "hub.html")]
struct HubPage {
    layout: Layout,
    notice: Option<String>,
    error: Option<String>,
    /// The configured hub (`[hub] url`).
    configured: Option<String>,
    admin: bool,
    library: Option<LibraryView>,
    create: Option<CreateView>,
    account: Option<AccountView>,
    devices: Vec<DeviceView>,
    /// Why the device list is missing.
    devices_error: Option<String>,
    min_password: usize,
    action: String,
    profile_action: String,
    profiles_action: String,
}

#[derive(Template)]
#[template(path = "hub_code.html")]
struct CodePage {
    layout: Layout,
    /// The pairing link.
    code: String,
    /// The QR code of `code` (inline SVG).
    qr: String,
    /// When the link expires.
    expires: Option<String>,
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
            "created" => {
                "The library is on the hub. Read on phone on a book’s page sends it to your \
                 phone; Settings → Profiles invites the others."
            }
            "synced" => "Synced with the hub.",
            "removed" => "The device was removed from your hub account.",
            "renamed" => "Your address on the hub changed.",
            "password" => "Your hub password is set: sign in with it in the hub reader.",
            "deleted" => {
                "The library was deleted on the hub. Your marks and positions here are \
                 unchanged."
            }
            "rotated" => {
                "The library has a new key: books sent from now on use it, and it reaches \
                 every member’s devices with the next sync. Books sent before still open."
            }
            _ => return None,
        }
        .to_owned(),
    )
}

/// The name of this library on the members' devices.
pub(crate) fn instance_name(state: &AppState) -> String {
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

/// A handle or name suggested from `text`: lower case ASCII letters and digits, other
/// characters as `-`, at least the format's minimum, `fallback` if nothing is left.
pub(crate) fn suggest_label(text: &str, fallback: &str) -> String {
    let mut label = String::new();
    for c in text.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            label.push(c);
        } else if !label.ends_with('-') && !label.is_empty() {
            label.push('-');
        }
    }
    let mut label: String = label.trim_matches('-').chars().take(32).collect();
    label = label.trim_end_matches('-').to_owned();
    normalize_label(&label).unwrap_or_else(|| fallback.to_owned())
}

/// The hub of `[hub] url`, if one is set.
pub(crate) fn configured_hub(state: &AppState) -> Option<HubUrl> {
    HubUrl::parse(&state.config().hub.url).ok()
}

/// The library on its hub, if it was created there.
pub(crate) async fn hub_library(state: &AppState) -> Result<Option<HubLibrary>, AppError> {
    super::profiles::user(state, move |_, conn| Ok(store::library(conn)?)).await
}

pub(super) async fn hub_account(
    state: &AppState,
    profile: ProfileId,
) -> Result<Option<HubAccount>, AppError> {
    super::profiles::user(state, move |_, conn| Ok(store::account(conn, profile)?)).await
}

/// Runs a hub call on a blocking thread.
pub(super) async fn call<T: Send + 'static>(
    url: HubUrl,
    f: impl FnOnce(&HubClient) -> Result<T, ClientError> + Send + 'static,
) -> Result<Result<T, ClientError>, AppError> {
    blocking(move || f(&HubClient::new(url))).await
}

/// `(major, minor)` of a version text `MAJOR.MINOR.PATCH[+BUILD]`.
fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split(['.', '+', '-']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

/// The refusal of a hub older than this library (its API is another), if it is.
pub(crate) fn version_refusal(info: &HubInfo) -> Option<String> {
    let ours = major_minor(crate::VERSION)?;
    let theirs = major_minor(&info.version)?;
    (theirs < ours).then(|| {
        let base = crate::VERSION.split('+').next().unwrap_or(crate::VERSION);
        format!(
            "This hub runs shelfgauge {}; the library needs {base} or newer. Ask the hub’s \
             operator to upgrade it.",
            info.version
        )
    })
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

fn pending_text(account: &HubAccount) -> Option<String> {
    let invite = account.invite.as_ref()?;
    Some(match invite.kind {
        store::InviteKind::New => format!("Invited, until {}", date(invite.expires_at)),
        store::InviteKind::Reset => {
            format!("Password reset sent, until {}", date(invite.expires_at))
        }
    })
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
    let library = hub_library(state).await?;
    let account = hub_account(state, current.id()).await?;
    let configured = configured_hub(state);
    let admin = current.is_admin();
    let mut error = error;
    let mut create = None;
    let mut devices = Vec::new();
    let mut devices_error = None;
    let mut has_password = false;
    match (&library, &configured) {
        (None, Some(url)) if admin => match call(url.clone(), |c| c.info()).await? {
            Ok(info) => {
                let operator = info.operator.as_ref().and_then(|o| {
                    let text = [o.name.as_str(), o.contact.as_str()]
                        .iter()
                        .filter(|t| !t.is_empty())
                        .copied()
                        .collect::<Vec<_>>()
                        .join(", ");
                    (!text.is_empty()).then_some(text)
                });
                let terms = info
                    .operator
                    .as_ref()
                    .filter(|o| o.terms)
                    .map(|_| url.page("/terms"));
                let owner_name = super::profiles::user(state, |_, conn| {
                    Ok(crate::profiles::store::get(conn, ProfileId::OWNER)?.map(|p| p.name))
                })
                .await?
                .unwrap_or_default();
                create = Some(CreateView {
                    hub_url: url.to_string(),
                    handle: suggest_label(&instance_name(state), "library"),
                    owner_name: suggest_label(&owner_name, "admin"),
                    invite: info.registration != "open",
                    operator,
                    terms,
                });
                if let Some(refusal) = version_refusal(&info) {
                    error = error.or(Some(refusal));
                }
            }
            Err(e) => error = error.or(Some(e.user_text())),
        },
        (Some(library), _) if account.as_ref().is_some_and(|a| a.role == Role::Owner) => {
            let (url, token) = (library.hub_url.clone(), library.token.clone());
            let key = account.as_ref().map(|a| a.key.clone()).expect("checked");
            let listed = call(url, move |c| {
                let devices = c.owner_devices(&token)?;
                let members = c.members(&token)?;
                Ok((devices, members))
            })
            .await?;
            match listed {
                Ok((list, members)) => {
                    has_password = members.iter().any(|m| m.owner && m.has_password);
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
                        })
                        .collect();
                }
                Err(e) => devices_error = Some(e.user_text()),
            }
        }
        _ => {}
    }
    let urls = state.urls();
    let page = HubPage {
        layout: layout(state, current, mode).await?,
        notice,
        error,
        configured: configured.map(|u| u.to_string()),
        admin,
        account: account.map(|a| AccountView {
            address: (a.name.as_ref())
                .zip(library.as_ref())
                .map(|(name, l)| format!("{name}@{}", l.handle)),
            owner: a.role == Role::Owner,
            has_password,
            pending: pending_text(&a),
            gone: a.gone_at.is_some(),
            synced: a.synced_at.map(date_time),
            sync_error: a.sync_error,
        }),
        library: library.map(|l| LibraryView {
            hub_url: l.hub_url.to_string(),
            handle: l.handle,
            created: date(l.created_at),
        }),
        create,
        devices,
        devices_error,
        min_password: shelfgauge_core::password::MIN_PASSWORD_CHARS,
        action: urls.profile_hub(),
        profile_action: urls.my_profile(),
        profiles_action: urls.manage_profiles(),
    };
    Ok((
        status,
        [(header::CACHE_CONTROL, "no-store")],
        render(&page)?,
    )
        .into_response())
}

/// `GET /profile/hub`: Phone and reading through the hub.
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

/// The pairing link of the owner's phone with its QR code.
pub(crate) async fn pair_page(
    state: &AppState,
    current: &CurrentProfile,
    mode: SidebarMode,
    link: String,
    expires: Option<String>,
    back: String,
) -> Result<Response, AppError> {
    let qr = qr::svg(&link, "QR code of the pairing link")
        .ok_or_else(|| AppError::Internal("the link is too long for a QR code".to_owned()))?;
    let page = CodePage {
        layout: layout(state, current, mode).await?,
        code: link,
        qr,
        expires,
        back,
    };
    Ok(([(header::CACHE_CONTROL, "no-store")], render(&page)?).into_response())
}

/// The form of `POST /profile/hub`; `action` says what to do. Never logged.
#[derive(Default, Deserialize)]
pub(crate) struct HubForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    handle: String,
    #[serde(default)]
    owner_name: String,
    #[serde(default)]
    invite: String,
    #[serde(default)]
    device: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    repeat: String,
    #[serde(default)]
    confirm: String,
}

/// `POST /profile/hub` (form): `create` (an admin creates the library on the hub),
/// `sync`, and for the owner profile `pair` (shows a pairing link and its QR code),
/// `remove` (a device), `name` (the address's name part), `password` (the hub
/// password), and for an admin `delete` (the library on the hub). Refusals show the
/// page again with the reason.
pub(crate) async fn save(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    form: Result<Form<HubForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Form(form) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let mode = SidebarMode::from_headers(&headers);
    let id = current.id();
    let library = hub_library(&state).await?;
    let account = hub_account(&state, id).await?;
    let owner = account.as_ref().is_some_and(|a| a.role == Role::Owner);
    let admin = current.is_admin();
    let outcome = match (form.action.as_str(), library) {
        ("create", None) if admin => {
            create_library(&state, &form.handle, &form.owner_name, form.invite.trim()).await?
        }
        ("create", Some(_)) => Outcome::Refused("The library is on the hub already.".to_owned()),
        ("sync", Some(library)) => match account {
            Some(account) => {
                // The account list first: invites redeemed, accounts gone.
                let refreshed = {
                    let (state, library) = (state.clone(), library.clone());
                    blocking(move || crate::web::hub_sync::refresh_library(&state, &library))
                        .await?
                };
                if let Err(e) = refreshed {
                    tracing::info!(error = %e, "cannot read the hub's account list");
                }
                let account = hub_account(&state, id).await?.unwrap_or(account);
                // An admin syncs every account of the library, the own one last.
                if admin {
                    let others =
                        super::profiles::user(&state, |_, conn| Ok(store::accounts(conn)?)).await?;
                    for other in others.into_iter().filter(|a| a.profile != id && a.on_hub()) {
                        if let Err(e) =
                            crate::web::hub_sync::sync_account(&state, library.clone(), other)
                                .await?
                        {
                            tracing::info!(error = %e, "a member's sync failed");
                        }
                    }
                }
                match crate::web::hub_sync::sync_account(&state, library, account).await? {
                    Ok(_) => Outcome::Done("synced"),
                    Err(crate::hub_client::sync::SyncError::Client(e)) => {
                        Outcome::Refused(e.user_text())
                    }
                    Err(e) => return Err(AppError::Internal(e.to_string())),
                }
            }
            None => Outcome::Refused("This profile has no account on the hub.".to_owned()),
        },
        ("pair", Some(library)) if owner => {
            let key = account.as_ref().map(|a| a.key.clone()).expect("checked");
            let token = library.token.clone();
            match call(library.hub_url.clone(), move |c| c.create_pairing(&token)).await? {
                Ok(pairing) => {
                    tracing::info!(profile = id.0, "hub pairing link shown");
                    let link = library.hub_url.pair_link(&pairing.secret, &key);
                    return pair_page(
                        &state,
                        &current,
                        mode,
                        link,
                        Some(date_time(pairing.expires_at)),
                        state.urls().profile_hub(),
                    )
                    .await;
                }
                Err(e) => Outcome::Refused(e.user_text()),
            }
        }
        ("remove", Some(library)) if owner => {
            let device: i64 = form
                .device
                .trim()
                .parse()
                .map_err(|_| AppError::BadRequest("invalid device".to_owned()))?;
            let token = library.token.clone();
            match call(library.hub_url.clone(), move |c| {
                c.remove_owner_device(&token, device)
            })
            .await?
            {
                Ok(()) => {
                    tracing::info!(profile = id.0, device, "hub device removed");
                    Outcome::Done("removed")
                }
                Err(e) => Outcome::Refused(e.user_text()),
            }
        }
        ("name", Some(library)) if owner => {
            let Some(name) = normalize_label(&form.name) else {
                return refused(&state, &current, mode, NAME_RULES.to_owned()).await;
            };
            let token = library.token.clone();
            let sent = name.clone();
            match call(library.hub_url.clone(), move |c| {
                c.set_owner_name(&token, &sent)
            })
            .await?
            {
                Ok(()) => {
                    super::profiles::user(&state, move |_, conn| {
                        Ok(store::set_name(conn, id, &name)?)
                    })
                    .await?;
                    tracing::info!(profile = id.0, "hub owner name changed");
                    Outcome::Done("renamed")
                }
                Err(e) => Outcome::Refused(e.user_text()),
            }
        }
        ("password", Some(library)) if owner => {
            let key = account.as_ref().map(|a| a.key.clone()).expect("checked");
            set_owner_password(&library, &key, &form.password, &form.repeat).await?
        }
        ("rotate", Some(_)) if admin => {
            let key = AccountKey::generate().map_err(random_error)?;
            let key_id = super::profiles::user(&state, move |_, conn| {
                Ok(store::add_library_key(conn, &key, now())?)
            })
            .await?;
            tracing::info!(key_id, "library key changed");
            Outcome::Done("rotated")
        }
        ("delete", Some(library)) if admin => {
            if form.confirm != library.handle {
                return refused(
                    &state,
                    &current,
                    mode,
                    "Type the library’s handle to confirm the deletion.".to_owned(),
                )
                .await;
            }
            delete_library(&state, library).await?
        }
        ("create", None) => return Err(AppError::Forbidden),
        ("sync" | "pair" | "remove" | "name" | "password" | "delete" | "rotate", None) => {
            Outcome::Refused("The library is not on the hub yet.".to_owned())
        }
        ("pair" | "remove" | "name" | "password" | "delete" | "rotate", Some(_)) => {
            return Err(AppError::Forbidden);
        }
        _ => return Err(AppError::BadRequest("unknown action".to_owned())),
    };
    match outcome {
        Outcome::Done(done) => Ok(Redirect::to(&format!(
            "{}?done={done}",
            state.urls().profile_hub()
        ))
        .into_response()),
        Outcome::Refused(error) => refused(&state, &current, mode, error).await,
    }
}

async fn refused(
    state: &AppState,
    current: &CurrentProfile,
    mode: SidebarMode,
    error: String,
) -> Result<Response, AppError> {
    page_response(
        state,
        current,
        mode,
        None,
        Some(error),
        StatusCode::UNPROCESSABLE_ENTITY,
    )
    .await
}

/// What a post did.
pub(crate) enum Outcome {
    /// Done: `303` to the page with this `done` value.
    Done(&'static str),
    /// Refused: the page again with this reason.
    Refused(String),
}

/// The rules of a name or handle, for refusals.
pub(crate) const NAME_RULES: &str = "Use 3 to 32 characters: Latin letters, digits, dots, \
     dashes and underscores, starting and ending with a letter or digit.";

fn random_error(e: getrandom::Error) -> AppError {
    AppError::Internal(format!("no random numbers: {e}"))
}

/// Creates the library on `[hub] url` with its handle, the owner profile's account and
/// the first library key, after the version check; warns when the hub serves another
/// reader app than this build.
async fn create_library(
    state: &AppState,
    handle: &str,
    owner_name: &str,
    invite: &str,
) -> Result<Outcome, AppError> {
    let Some(url) = configured_hub(state) else {
        return Ok(Outcome::Refused(
            "No hub is set: add its address as url in the [hub] section of config.toml.".to_owned(),
        ));
    };
    let (Some(handle), Some(owner_name)) = (normalize_label(handle), normalize_label(owner_name))
    else {
        return Ok(Outcome::Refused(NAME_RULES.to_owned()));
    };
    let info = match call(url.clone(), |c| c.info()).await? {
        Ok(info) => info,
        Err(e) => return Ok(Outcome::Refused(e.user_text())),
    };
    if let Some(refusal) = version_refusal(&info) {
        return Ok(Outcome::Refused(refusal));
    }
    if info.version.split('+').next() == crate::VERSION.split('+').next() {
        let checked = url.clone();
        match blocking(move || crate::hub::check::check(&checked)).await? {
            Ok(report) if !report.matches() => tracing::warn!(
                differing = report.differing.len(),
                missing = report.missing.len(),
                "the hub serves a reader app that differs from this build"
            ),
            Ok(_) => {}
            Err(e) => tracing::info!(error = %e, "cannot check the hub's reader app"),
        }
    }
    let invite = (!invite.is_empty()).then(|| invite.to_owned());
    let (sent_handle, sent_name) = (handle.clone(), owner_name.clone());
    let created = match call(url.clone(), move |c| {
        c.create_library(invite.as_deref(), &sent_handle, &sent_name)
    })
    .await?
    {
        Ok(created) => created,
        Err(e) => return Ok(Outcome::Refused(e.user_text())),
    };
    let library_key = AccountKey::generate().map_err(random_error)?;
    let owner_key = AccountKey::generate().map_err(random_error)?;
    super::profiles::user(state, move |_, conn| {
        Ok(store::save_library(
            conn,
            &url,
            created.library,
            &handle,
            &created.token,
            &library_key,
            ProfileId::OWNER,
            created.owner,
            &owner_key,
            &owner_name,
            now(),
        )?)
    })
    .await?;
    tracing::info!("library created on the hub");
    Ok(Outcome::Done("created"))
}

/// Sets the owner account's hub password: derived here with the hub's parameters
/// (never weaker than the built-in minimum), the account key sealed under the
/// wrapping key; the library never keeps the password.
async fn set_owner_password(
    library: &HubLibrary,
    key: &AccountKey,
    password: &str,
    repeat: &str,
) -> Result<Outcome, AppError> {
    use shelfgauge_core::password;
    if password.chars().count() < password::MIN_PASSWORD_CHARS {
        return Ok(Outcome::Refused(format!(
            "Use at least {} characters; a few words are easy to remember.",
            password::MIN_PASSWORD_CHARS
        )));
    }
    if password != repeat {
        return Ok(Outcome::Refused("The two passwords differ.".to_owned()));
    }
    let info = match call(library.hub_url.clone(), |c| c.info()).await? {
        Ok(info) => info,
        Err(e) => return Ok(Outcome::Refused(e.user_text())),
    };
    let (memory_kib, iterations) =
        info.passwords
            .map_or((password::MIN_MEMORY_KIB, password::MIN_ITERATIONS), |p| {
                (
                    p.memory_kib.max(password::MIN_MEMORY_KIB),
                    p.iterations.max(password::MIN_ITERATIONS),
                )
            });
    let mut salt = [0u8; password::SALT_LEN];
    getrandom::fill(&mut salt).map_err(random_error)?;
    let password = password.to_owned();
    let key = key.clone();
    let (url, token) = (library.hub_url.clone(), library.token.clone());
    let result = blocking(move || {
        let keys = password::derive(&password, &salt, memory_kib, iterations)
            .map_err(|e| AppError::Internal(e.to_string()))?;
        let bundle =
            keys::seal_bundle(&AccountKey::new(keys.wrapping_key), &key).map_err(random_error)?;
        let upload = PasswordUpload {
            login_key: &keys.login_key,
            salt: &salt,
            memory_kib,
            iterations,
            bundle: &bundle,
        };
        Ok::<_, AppError>(HubClient::new(url).set_owner_password(&token, &upload))
    })
    .await??;
    match result {
        Ok(()) => {
            tracing::info!("hub owner password set");
            Ok(Outcome::Done("password"))
        }
        Err(e) => Ok(Outcome::Refused(e.user_text())),
    }
}

/// Deletes the library on the hub after a last sync of every account, then forgets
/// it here (marks stay).
async fn delete_library(state: &AppState, library: HubLibrary) -> Result<Outcome, AppError> {
    let accounts = super::profiles::user(state, |_, conn| Ok(store::accounts(conn)?)).await?;
    for account in accounts.into_iter().filter(HubAccount::on_hub) {
        if let Err(e) = crate::web::hub_sync::sync_account(state, library.clone(), account).await? {
            tracing::info!(error = %e, "a last sync before deleting the library failed");
        }
    }
    let token = library.token.clone();
    match call(library.hub_url.clone(), move |c| c.delete_library(&token)).await? {
        Ok(()) | Err(ClientError::Hub { status: 401, .. }) => {}
        Err(e) => return Ok(Outcome::Refused(e.user_text())),
    }
    super::profiles::user(state, |_, conn| Ok(store::forget_library(conn)?)).await?;
    tracing::info!("library deleted on the hub");
    Ok(Outcome::Done("deleted"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_suggested_within_the_format() {
        assert_eq!(suggest_label("Home library", "x"), "home-library");
        assert_eq!(suggest_label("  Книги  ", "library"), "library");
        assert_eq!(suggest_label("Smith's books!", "x"), "smith-s-books");
        assert_eq!(suggest_label("ab", "admin"), "admin");
        assert_eq!(suggest_label(&"x".repeat(40), "y").len(), 32);
    }

    #[test]
    fn an_older_hub_is_refused_by_its_version() {
        let info = |version: &str| HubInfo {
            service: "shelfgauge-hub".into(),
            version: version.into(),
            formats: crate::publication::format::SUPPORTED_VERSIONS,
            registration: "open".into(),
            limits: crate::hub_client::HubLimits {
                max_blob_bytes: 1,
                max_record_bytes: 1,
                max_sync_records: 1,
                publication_expiry_days: 0,
            },
            passwords: None,
            operator: None,
        };
        // One minor version below this build.
        let (major, minor) = major_minor(crate::VERSION).unwrap();
        let older = if minor > 0 {
            format!("{major}.{}.3", minor - 1)
        } else {
            format!("{}.9.0", major - 1)
        };
        let refusal = version_refusal(&info(&older)).unwrap();
        assert!(
            refusal.contains(&format!("runs shelfgauge {older}")),
            "{refusal}"
        );
        assert_eq!(version_refusal(&info(crate::VERSION)), None);
        assert_eq!(version_refusal(&info("9.0.0")), None);
    }
}
