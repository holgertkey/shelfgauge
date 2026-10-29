//! The library's members on the hub, for the admin in Settings → Profiles (Phase 30):
//! each profile's account status, **Invite someone** and **Invite again** (a new
//! account), **Reset password** (a new password for an existing account), **Sign out
//! all devices**, and the removal of a deleted profile's account.
//!
//! An invite link is `<hub>/invite#<secret>.<one-time key>`: the hub keeps, with the
//! invite, a payload sealed under the one-time key (the library, the profile's name,
//! the account key and the current library key). The link is shown once, in a page
//! sent with `no-store`, never in a URL or a log.

use askama::Template;
use axum::http::header;
use axum::response::{IntoResponse, Redirect, Response};

use super::hub::{call, hub_account, hub_library, instance_name};
use super::{date, date_time, render};
use crate::annotations::now;
use crate::hub_client::keys::{self, AccountKey, InviteLibraryKey, InvitePayload};
use crate::hub_client::store::{self, HubAccount, InviteKind, PendingInvite, Removal, Role};
use crate::profiles::ProfileId;
use crate::web::layout::Layout;
use crate::web::profile::CurrentProfile;
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState, qr};

/// A profile's account as the admin's list shows it.
pub(crate) struct HubStatus {
    /// What the account is: on the hub with devices and last sync, invited, gone.
    pub(crate) text: String,
    /// Whether **Invite** (a first invite, or **Invite again**) applies.
    pub(crate) can_invite: bool,
    /// Whether **Reset password** and **Sign out all devices** apply.
    pub(crate) on_hub: bool,
    /// Whether the account is gone (left or expired).
    pub(crate) gone: bool,
}

/// The status of `account` (`None`: the profile has no account and no invite) of a
/// library with `handle`.
pub(crate) fn status(account: Option<&HubAccount>, handle: &str) -> HubStatus {
    let Some(account) = account else {
        return HubStatus {
            text: "Not on the hub".to_owned(),
            can_invite: true,
            on_hub: false,
            gone: false,
        };
    };
    let address = account.name.as_ref().map(|name| format!("{name}@{handle}"));
    let last_sync = account
        .last_seen_at
        .map_or_else(|| "never".to_owned(), date_time);
    if account.gone_at.is_some() {
        return HubStatus {
            text: format!("No longer on the hub: left or expired; last sync {last_sync}"),
            can_invite: account.role == Role::Reader,
            on_hub: false,
            gone: true,
        };
    }
    if let Some(invite) = &account.invite
        && invite.kind == InviteKind::New
    {
        return HubStatus {
            text: if invite.expires_at > now() {
                format!("Invited, until {}", date(invite.expires_at))
            } else {
                "The invite expired".to_owned()
            },
            can_invite: true,
            on_hub: false,
            gone: false,
        };
    }
    let devices = match account.devices {
        1 => "1 device".to_owned(),
        n => format!("{n} devices"),
    };
    let mut text = match address {
        Some(address) => format!("{address} · {devices} · last sync {last_sync}"),
        None => format!("On the hub · {devices} · last sync {last_sync}"),
    };
    if let Some(invite) = &account.invite {
        text.push_str(&format!(
            " · password reset sent, until {}",
            date(invite.expires_at)
        ));
    }
    HubStatus {
        text,
        can_invite: false,
        on_hub: account.role == Role::Reader,
        gone: false,
    }
}

#[derive(Template)]
#[template(path = "hub_invite.html")]
struct InvitePage {
    layout: Layout,
    /// The profile's name.
    name: String,
    /// Whether it resets a password (else it is an invite).
    reset: bool,
    link: String,
    qr: String,
    expires: String,
    back: String,
}

/// Why an invite could not be made, as a `profile` code of the settings page.
pub(crate) enum Refusal {
    /// The library is not on the hub yet.
    NoLibrary,
    /// The profile has an account on the hub already (use Reset password).
    HasAccount,
    /// The profile has no account on the hub to reset.
    NoAccount,
    /// The hub refused or could not be reached.
    Hub(String),
}

/// Makes a member invite of `profile` (named `name`): a new account (`kind` New; the
/// profile must not be on the hub) or a password reset of its account; answers the
/// page with the link, or why there is none.
pub(crate) async fn invite(
    state: &AppState,
    current: &CurrentProfile,
    profile: ProfileId,
    name: String,
    kind: InviteKind,
) -> Result<Result<Response, Refusal>, AppError> {
    let Some(library) = hub_library(state).await? else {
        return Ok(Err(Refusal::NoLibrary));
    };
    let account = hub_account(state, profile).await?;
    let on_hub = account.as_ref().is_some_and(HubAccount::on_hub);
    let reset = match kind {
        InviteKind::New if on_hub => return Ok(Err(Refusal::HasAccount)),
        InviteKind::Reset if !on_hub => return Ok(Err(Refusal::NoAccount)),
        InviteKind::New => None,
        InviteKind::Reset => account.as_ref().and_then(|a| a.account),
    };
    // Only a reader account is reset by a link: the owner's password is set here.
    if reset.is_some() && account.as_ref().is_some_and(|a| a.role == Role::Owner) {
        return Ok(Err(Refusal::HasAccount));
    }
    let key = match (&account, kind) {
        (Some(account), InviteKind::Reset) => account.key.clone(),
        _ => AccountKey::generate().map_err(random)?,
    };
    let (key_id, library_key) =
        super::profiles::user(state, |_, conn| Ok(store::current_library_key(conn)?))
            .await?
            .ok_or_else(|| AppError::Internal("the library has no key".to_owned()))?;
    let one_time = AccountKey::generate().map_err(random)?;
    let payload = InvitePayload {
        library_id: library.library_id.to_string(),
        library_name: instance_name(state),
        profile_name: name.clone(),
        account_key: key.to_text(),
        library_key: InviteLibraryKey {
            key_id,
            key: library_key.to_text(),
        },
    };
    let sealed = keys::seal_invite(&one_time, &payload).map_err(random)?;
    // A new invite replaces a pending one.
    let replaced = account
        .as_ref()
        .and_then(|a| a.invite.as_ref())
        .map(|i| i.id);
    let token = library.token.clone();
    let created = call(library.hub_url.clone(), move |c| {
        if let Some(old) = replaced {
            c.withdraw_invite(&token, &old)?;
        }
        c.create_invite(&token, reset.as_ref(), &sealed)
    })
    .await?;
    let created = match created {
        Ok(created) => created,
        Err(e) => return Ok(Err(Refusal::Hub(e.user_text()))),
    };
    let pending = PendingInvite {
        id: created.id,
        kind,
        expires_at: created.expires_at,
    };
    super::profiles::user(state, move |_, conn| {
        Ok(store::save_invite(conn, profile, &key, &pending, now())?)
    })
    .await?;
    tracing::info!(
        profile = profile.0,
        reset = kind == InviteKind::Reset,
        "hub invite created"
    );
    let link = library.hub_url.invite_link(&created.secret, &one_time);
    let qr = qr::svg(&link, "QR code of the invite link")
        .ok_or_else(|| AppError::Internal("the link is too long for a QR code".to_owned()))?;
    let s = state.clone();
    let current = current.clone();
    let sidebar_current = current.clone();
    let layout = state
        .db(move |conn| {
            let mode = crate::web::layout::SidebarMode::default();
            Ok(Layout::new(
                &s,
                &sidebar_current,
                "Invite to the hub",
                sidebar::build(conn, &s, mode, &Focus::None)?,
            ))
        })
        .await?;
    let page = InvitePage {
        layout,
        name,
        reset: kind == InviteKind::Reset,
        link,
        qr,
        expires: date_time(created.expires_at),
        back: format!("{}#profiles", state.urls().settings()),
    };
    Ok(Ok(
        ([(header::CACHE_CONTROL, "no-store")], render(&page)?).into_response()
    ))
}

/// **Sign out all devices** of `profile`'s account.
pub(crate) async fn sign_out(
    state: &AppState,
    profile: ProfileId,
) -> Result<Result<u32, Refusal>, AppError> {
    let Some(library) = hub_library(state).await? else {
        return Ok(Err(Refusal::NoLibrary));
    };
    let Some(id) = hub_account(state, profile)
        .await?
        .filter(HubAccount::on_hub)
        .and_then(|a| a.account)
    else {
        return Ok(Err(Refusal::NoAccount));
    };
    let token = library.token.clone();
    match call(library.hub_url.clone(), move |c| {
        c.sign_out_member(&token, &id)
    })
    .await?
    {
        Ok(removed) => {
            tracing::info!(
                profile = profile.0,
                count = removed,
                "hub devices signed out"
            );
            Ok(Ok(removed))
        }
        Err(e) => Ok(Err(Refusal::Hub(e.user_text()))),
    }
}

/// Queues the deletion on the hub of `profile`'s account and pending invite, before
/// the profile is deleted here; the sync worker carries it out (also later, when the
/// hub cannot be reached now).
pub(crate) fn queue_removal(
    conn: &rusqlite::Connection,
    profile: ProfileId,
) -> Result<(), AppError> {
    if let Some(account) = store::account(conn, profile)? {
        if let Some(id) = account.account.filter(|_| account.gone_at.is_none()) {
            store::queue_removal(conn, Removal::Account(id))?;
        }
        if let Some(invite) = &account.invite {
            store::queue_removal(conn, Removal::Invite(invite.id))?;
        }
    }
    Ok(())
}

/// Carries out the queued removals now, if the hub can be reached (else the sync
/// worker does later).
pub(crate) async fn remove_now(state: &AppState) {
    let state = state.clone();
    let done = crate::web::blocking(move || {
        let library = store::library(&state.userdata().lock())?;
        match library {
            Some(library) => crate::web::hub_sync::refresh_library(&state, &library)
                .map_err(|e| AppError::Internal(e.to_string())),
            None => Ok(()),
        }
    })
    .await;
    if !matches!(done, Ok(Ok(()))) {
        tracing::info!("the hub removal waits for the next sync");
    }
}

fn random(e: getrandom::Error) -> AppError {
    AppError::Internal(format!("no random numbers: {e}"))
}

/// The `profile` code of a refusal.
pub(crate) fn refusal_code(refusal: &Refusal) -> &'static str {
    match refusal {
        Refusal::NoLibrary => "hub-missing",
        Refusal::HasAccount => "hub-has-account",
        Refusal::NoAccount => "hub-no-account",
        Refusal::Hub(_) => "hub-refused",
    }
}

/// Back to the profiles section with the refusal's code.
pub(crate) fn refused(state: &AppState, refusal: &Refusal) -> Response {
    if let Refusal::Hub(text) = refusal {
        tracing::info!(error = %text, "the hub refused an invite");
    }
    Redirect::to(&format!(
        "{}?profile={}#profiles",
        state.urls().settings(),
        refusal_code(refusal)
    ))
    .into_response()
}
