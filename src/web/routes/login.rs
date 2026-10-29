//! The login from other devices: `GET /login` lists the profiles, `GET`/`POST
//! /login/{id}` asks for a profile's password.
//!
//! These routes sit outside the auth slot (they are where a request from another
//! device gets its session) and send requests from this computer to the picker, where
//! a PIN, not a password, is asked. A right password starts a password session (see
//! [`Level::Password`]) with a new token; its cookie is kept for about 400 days with
//! "Remember this device", else until the browser closes.
//!
//! Wrong passwords lock the profile out like wrong PINs, and a client with a
//! non-loopback address (not the reverse proxy on this computer) also has a budget of
//! failed logins across all profiles. Both are counted before the password is hashed.
//! The login list shows which profiles have a password. Passwords are never
//! logged.

use std::time::Instant;

use askama::Template;
use axum::extract::rejection::{FormRejection, QueryRejection};
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::profiles::{
    CheckedPassword, Next, ProfileView, check_password, new_session, user, wait_text,
};
use super::{parse_id, render};
use crate::profiles::session::Level;
use crate::profiles::{ProfileId, store};
use crate::web::layout::Layout;
use crate::web::profile::{Keep, local_path, set_cookie};
use crate::web::security::Peer;
use crate::web::{AppError, AppState};

#[derive(Template)]
#[template(path = "login.html")]
struct PickerPage {
    layout: Layout,
    profiles: Vec<ProfileView>,
    next: String,
}

/// `GET /login`: the profiles to log into (those without a password are shown
/// disabled); with a single profile, its password form. From this computer: the
/// picker.
pub(crate) async fn picker(
    State(state): State<AppState>,
    peer: Peer,
    query: Result<Query<Next>, QueryRejection>,
) -> Result<Response, AppError> {
    let urls = state.urls();
    let next = Next::from(query).path(urls);
    if !peer.is_remote() {
        return Ok(Redirect::to(&urls.profiles(Some(&next))).into_response());
    }
    let profiles = user(&state, |_, conn| Ok(store::list_local(conn)?)).await?;
    if let [only] = profiles.as_slice() {
        return Ok(Redirect::to(&urls.login_profile(only.id.0, Some(&next))).into_response());
    }
    let page = PickerPage {
        layout: Layout::without_profile(&state, "Log in"),
        profiles: profiles.iter().map(ProfileView::new).collect(),
        next,
    };
    Ok(([(header::CACHE_CONTROL, "no-store")], render(&page)?).into_response())
}

#[derive(Template)]
#[template(path = "login_form.html")]
struct FormPage {
    layout: Layout,
    profile: ProfileView,
    action: String,
    next: String,
    /// Whether there are several profiles (a link back to the list).
    several: bool,
    /// The error under the field.
    error: Option<String>,
    /// Locked, or no password: the field is disabled.
    disabled: bool,
    /// Whether "Remember this device" is checked.
    remember: bool,
}

async fn form_page(
    state: &AppState,
    id: ProfileId,
    next: String,
    error: Option<String>,
    locked: bool,
    remember: bool,
) -> Result<FormPage, AppError> {
    let (profile, count) = user(state, move |_, conn| {
        Ok((store::get(conn, id)?, store::count_local(conn)?))
    })
    .await?;
    let profile = profile.ok_or(AppError::NotFound)?;
    Ok(FormPage {
        layout: Layout::without_profile(state, "Log in"),
        action: state.urls().login_profile(id.0, None),
        disabled: locked || !profile.has_password,
        profile: ProfileView::new(&profile),
        next,
        several: count > 1,
        error,
        remember,
    })
}

/// `GET /login/{id}`: the password form of a profile. From this computer: the picker.
pub(crate) async fn form(
    State(state): State<AppState>,
    peer: Peer,
    Path(id): Path<String>,
    query: Result<Query<Next>, QueryRejection>,
) -> Result<Response, AppError> {
    let id = ProfileId(parse_id(&id)?);
    let next = Next::from(query).path(state.urls());
    if !peer.is_remote() {
        return Ok(Redirect::to(&state.urls().profiles(Some(&next))).into_response());
    }
    let wait = state.passwords().locked(id, Instant::now());
    let page = form_page(
        &state,
        id,
        next,
        wait.map(super::profiles::password_locked_text),
        wait.is_some(),
        true,
    )
    .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], render(&page)?).into_response())
}

/// The form of `POST /login/{id}`. The password is never logged.
#[derive(Default, Deserialize)]
pub(crate) struct LoginForm {
    #[serde(default)]
    password: String,
    remember: Option<String>,
    #[serde(default)]
    next: String,
}

/// `POST /login/{id}`: checks the password; right: a password session (cookie, `303`
/// to `next`); wrong: `422` with the tries left; locked: `429` with the wait. From this
/// computer: the picker.
pub(crate) async fn login(
    State(state): State<AppState>,
    peer: Peer,
    headers: HeaderMap,
    Path(id): Path<String>,
    form: Result<Form<LoginForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = ProfileId(parse_id(&id)?);
    let form = form.map(|Form(f)| f).unwrap_or_default();
    let next = local_path(state.urls(), Some(&form.next));
    if !peer.is_remote() {
        return Ok(Redirect::to(&state.urls().profiles(Some(&next))).into_response());
    }
    let remember = form.remember.is_some();
    if user(&state, move |_, conn| Ok(store::get(conn, id)?))
        .await?
        .is_none()
    {
        return Err(AppError::NotFound);
    }
    if let Some(address) = peer.direct
        && let Err(wait) = state.addresses().begin(address, Instant::now())
    {
        tracing::warn!(
            profile = id.0,
            "login refused: too many failures from one address"
        );
        let error = format!(
            "Too many failed logins from this device. Try again in {}.",
            wait_text(wait)
        );
        let page = form_page(&state, id, next, Some(error), true, remember).await?;
        return Ok((StatusCode::TOO_MANY_REQUESTS, render(&page)?).into_response());
    }
    match check_password(&state, id, form.password).await? {
        CheckedPassword::Ok => {
            if let Some(address) = peer.direct {
                state.addresses().success(address);
            }
            let token = new_session(&state, &headers, id, Level::Password, true).await?;
            tracing::info!(profile = id.0, "logged in");
            let keep = if remember {
                Keep::Remember
            } else {
                Keep::BrowserSession
            };
            Ok((
                [(
                    header::SET_COOKIE,
                    set_cookie(&state, &headers, &token, keep),
                )],
                Redirect::to(&next),
            )
                .into_response())
        }
        CheckedPassword::Wrong(status, message) => {
            let locked = status == StatusCode::TOO_MANY_REQUESTS;
            let page = form_page(&state, id, next, Some(message), locked, remember).await?;
            Ok((status, render(&page)?).into_response())
        }
    }
}
