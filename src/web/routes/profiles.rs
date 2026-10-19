//! Profiles in the browser: the picker and the PIN page, My profile, the theme and
//! reader settings, and the admin's profile management (on the settings page).
//!
//! The picker routes need no profile (they are where one is chosen); they sit behind
//! the Host and Origin checks and the auth slot like every other route. Every choice
//! starts a new session with a new token (no session fixation); a PIN set, change or
//! removal ends the profile's other sessions and renews the current token. PINs, names
//! and tokens are never logged, only profile ids.

use std::time::Instant;

use askama::Template;
use axum::Json;
use axum::extract::rejection::{FormRejection, JsonRejection, QueryRejection};
use axum::extract::{Form, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Redirect, Response};
use rusqlite::Connection;
use serde::Deserialize;

use super::annotations::ApiError;
use super::{parse_id, render};
use crate::annotations::attach;
use crate::annotations::now;
use crate::profiles::pin::{self, Failure};
use crate::profiles::session::{self, Token};
use crate::profiles::store::{self, ProfileError};
use crate::profiles::{COLOR_NAMES, COLORS, Profile, ProfileId, ProfileSettings, Role};
use crate::web::layout::{Layout, SidebarMode};
use crate::web::profile::{CurrentProfile, clear_cookie, cookie_tokens, local_path, set_cookie};
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState, Urls};

/// The `next` parameter of the picker routes.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Next {
    next: Option<String>,
}

impl Next {
    fn from(query: Result<Query<Next>, QueryRejection>) -> Self {
        query.map(|Query(next)| next).unwrap_or_default()
    }

    /// The local page to go to (the home page if missing or not local).
    fn path(&self, urls: &Urls) -> String {
        local_path(urls, self.next.as_deref())
    }
}

/// A profile as a tile or a row.
pub(crate) struct ProfileView {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) initials: String,
    pub(crate) color: u8,
    pub(crate) admin: bool,
    pub(crate) owner: bool,
    pub(crate) has_pin: bool,
    /// "2026-10-19" or "never".
    pub(crate) last_seen: String,
}

impl ProfileView {
    pub(crate) fn new(profile: &Profile) -> Self {
        Self {
            id: profile.id.0,
            name: profile.name.clone(),
            initials: profile.initials(),
            color: profile.color,
            admin: profile.is_admin(),
            owner: profile.id.is_owner(),
            has_pin: profile.has_pin,
            last_seen: profile
                .last_seen_at
                .map_or_else(|| "never".to_owned(), super::date),
        }
    }
}

/// A color choice of a form.
pub(crate) struct ColorChoice {
    pub(crate) index: u8,
    pub(crate) name: &'static str,
    pub(crate) checked: bool,
}

/// The color choices with `selected` checked.
pub(crate) fn colors(selected: u8) -> Vec<ColorChoice> {
    (0..COLORS)
        .map(|index| ColorChoice {
            index,
            name: COLOR_NAMES[usize::from(index)],
            checked: index == selected,
        })
        .collect()
}

#[derive(Template)]
#[template(path = "profiles.html")]
struct PickerPage {
    layout: Layout,
    profiles: Vec<ProfileView>,
    next: String,
}

/// `GET /profiles`: the picker. With a single profile there is nothing to choose: back
/// to `next`.
pub(crate) async fn picker(
    State(state): State<AppState>,
    query: Result<Query<Next>, QueryRejection>,
) -> Result<Response, AppError> {
    let next = Next::from(query).path(state.urls());
    let profiles = user(&state, |_, conn| Ok(store::list(conn)?)).await?;
    if profiles.len() <= 1 {
        return Ok(Redirect::to(&next).into_response());
    }
    let page = PickerPage {
        layout: Layout::without_profile(&state, "Who is reading?"),
        profiles: profiles.iter().map(ProfileView::new).collect(),
        next,
    };
    Ok(([(header::CACHE_CONTROL, "no-store")], render(&page)?).into_response())
}

/// Runs `f` with the locked user-data connection on a blocking thread.
async fn user<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&AppState, &Connection) -> Result<T, AppError> + Send + 'static,
) -> Result<T, AppError> {
    let state = state.clone();
    crate::web::blocking(move || f(&state, &state.userdata().lock())).await?
}

/// The form of `POST /profiles/{id}/select`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct SelectForm {
    #[serde(default)]
    next: String,
}

/// `POST /profiles/{id}/select`: a profile without a PIN starts a session (cookie,
/// `303` to `next`); one with a PIN goes to its PIN page.
pub(crate) async fn select(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    form: Result<Form<SelectForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = ProfileId(parse_id(&id)?);
    let next = local_path(state.urls(), form.ok().map(|Form(f)| f.next).as_deref());
    let profile = user(&state, move |_, conn| Ok(store::get(conn, id)?))
        .await?
        .ok_or(AppError::NotFound)?;
    if profile.has_pin {
        let url = format!(
            "{}?next={}",
            state.urls().profile_action(id.0, "pin"),
            percent_encoding::utf8_percent_encode(&next, percent_encoding::NON_ALPHANUMERIC)
        );
        return Ok(Redirect::to(&url).into_response());
    }
    start_session(&state, &headers, id, &next).await
}

/// Starts a session of `profile` (ending the sessions in the request's cookies) and
/// redirects to `next` with the cookie.
async fn start_session(
    state: &AppState,
    headers: &HeaderMap,
    profile: ProfileId,
    next: &str,
) -> Result<Response, AppError> {
    let old = cookie_tokens(headers);
    let token = user(state, move |_, conn| {
        for token in &old {
            session::end(conn, token)?;
        }
        Ok(session::create(conn, profile, now())?)
    })
    .await?;
    tracing::info!(profile = profile.0, "profile selected");
    Ok((
        [(header::SET_COOKIE, set_cookie(state, headers, &token))],
        Redirect::to(next),
    )
        .into_response())
}

#[derive(Template)]
#[template(path = "profile_pin.html")]
struct PinPage {
    layout: Layout,
    profile: ProfileView,
    action: String,
    next: String,
    /// The error under the field.
    error: Option<String>,
    /// Locked: the field is disabled.
    locked: bool,
}

/// "30 seconds", "2 minutes".
fn wait_text(wait: std::time::Duration) -> String {
    let seconds = wait.as_secs().max(1);
    if seconds < 90 {
        super::figures::count(seconds, "second", "seconds")
    } else {
        super::figures::count(seconds.div_ceil(60), "minute", "minutes")
    }
}

fn locked_text(wait: std::time::Duration) -> String {
    format!("Too many wrong PINs. Try again in {}.", wait_text(wait))
}

/// `GET /profiles/{id}/pin`: the PIN form of a profile.
pub(crate) async fn pin_page(
    State(state): State<AppState>,
    Path(id): Path<String>,
    query: Result<Query<Next>, QueryRejection>,
) -> Result<Response, AppError> {
    let id = ProfileId(parse_id(&id)?);
    let next = Next::from(query).path(state.urls());
    let profile = user(&state, move |_, conn| Ok(store::get(conn, id)?))
        .await?
        .ok_or(AppError::NotFound)?;
    let wait = state.pins().locked(id, Instant::now());
    let page = pin_view(
        &state,
        &profile,
        next,
        wait.map(locked_text),
        wait.is_some(),
    );
    Ok(([(header::CACHE_CONTROL, "no-store")], render(&page)?).into_response())
}

fn pin_view(
    state: &AppState,
    profile: &Profile,
    next: String,
    error: Option<String>,
    locked: bool,
) -> PinPage {
    PinPage {
        layout: Layout::without_profile(state, "Enter the PIN"),
        action: state.urls().profile_action(profile.id.0, "pin"),
        profile: ProfileView::new(profile),
        next,
        error,
        locked,
    }
}

/// The form of `POST /profiles/{id}/pin`. The PIN is never logged.
#[derive(Default, Deserialize)]
pub(crate) struct PinForm {
    #[serde(default)]
    pin: String,
    #[serde(default)]
    next: String,
}

/// What checking a PIN gave.
enum Checked {
    Ok,
    Wrong(Failure),
    Locked(std::time::Duration),
}

/// Checks `entered` against the PIN of `profile` with the lockout: the try is counted
/// first (so parallel requests get no more guesses, and a locked profile costs no
/// hashing; malformed PINs count too), the hash is read under the user-data lock and
/// verified after it is released, one argon2 run at a time.
async fn check_pin(
    state: &AppState,
    profile: ProfileId,
    entered: String,
) -> Result<Checked, AppError> {
    let wrong = match state.pins().begin(profile, Instant::now()) {
        Ok(wrong) => wrong,
        Err(wait) => return Ok(Checked::Locked(wait)),
    };
    let hash = user(state, move |_, conn| Ok(store::pin_hash(conn, profile)?)).await?;
    let s = state.clone();
    let ok = crate::web::blocking(move || match hash {
        Some(hash) => {
            let _one = s.pins().hashing();
            pin::check_format(&entered).is_ok() && pin::verify(&entered, &hash)
        }
        None => true,
    })
    .await?;
    if ok {
        state.pins().success(profile);
        return Ok(Checked::Ok);
    }
    match wrong {
        Failure::TriesLeft(left) => {
            tracing::info!(profile = profile.0, tries_left = left, "pin rejected");
        }
        Failure::Locked(wait) => {
            tracing::warn!(profile = profile.0, seconds = wait.as_secs(), "pin locked");
        }
    }
    Ok(Checked::Wrong(wrong))
}

/// Hashes a new PIN on a blocking thread, one argon2 run at a time.
async fn hash_pin(state: &AppState, pin: String) -> Result<String, AppError> {
    let s = state.clone();
    crate::web::blocking(move || {
        let _one = s.pins().hashing();
        pin::hash(&pin)
    })
    .await?
    .map_err(|e| AppError::Internal(e.to_string()))
}

/// The message and status of a wrong PIN.
fn wrong_pin(failure: Failure) -> (StatusCode, String, bool) {
    match failure {
        Failure::TriesLeft(left) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            format!(
                "Wrong PIN. {} left.",
                super::figures::count(left.into(), "try", "tries")
            ),
            false,
        ),
        Failure::Locked(wait) => (StatusCode::TOO_MANY_REQUESTS, locked_text(wait), true),
    }
}

/// `POST /profiles/{id}/pin`: checks the PIN; right: a session (cookie, `303` to
/// `next`); wrong: `422` with the tries left; locked: `429` with the wait.
pub(crate) async fn pin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    form: Result<Form<PinForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = ProfileId(parse_id(&id)?);
    let form = form.map(|Form(f)| f).unwrap_or_default();
    let next = local_path(state.urls(), Some(&form.next));
    let profile = user(&state, move |_, conn| Ok(store::get(conn, id)?))
        .await?
        .ok_or(AppError::NotFound)?;
    let (status, error, locked) = match check_pin(&state, id, form.pin).await? {
        Checked::Ok => return start_session(&state, &headers, id, &next).await,
        Checked::Locked(wait) => (StatusCode::TOO_MANY_REQUESTS, locked_text(wait), true),
        Checked::Wrong(failure) => wrong_pin(failure),
    };
    let page = pin_view(&state, &profile, next, Some(error), locked);
    Ok((status, render(&page)?).into_response())
}

/// `POST /profiles/leave`: ends the session (row and cookie) and goes to the picker.
pub(crate) async fn leave(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let tokens = cookie_tokens(&headers);
    user(&state, move |_, conn| {
        for token in &tokens {
            session::end(conn, token)?;
        }
        Ok(())
    })
    .await?;
    Ok((
        [(header::SET_COOKIE, clear_cookie(&state))],
        Redirect::to(&state.urls().profiles(None)),
    )
        .into_response())
}

/// `POST /profile/settings` (JSON): replaces the profile's theme and reader settings
/// (only known names with allowed values are kept); `204`.
pub(crate) async fn save_settings(
    State(state): State<AppState>,
    current: CurrentProfile,
    body: Result<Json<serde_json::Value>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(value) = body?;
    let settings = ProfileSettings::from_json(&value);
    let id = current.id();
    user(&state, move |_, conn| {
        store::save_settings(conn, id, &settings).map_err(profile_error)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn profile_error(e: ProfileError) -> AppError {
    match e {
        ProfileError::NotFound => AppError::NotFound,
        ProfileError::Storage(e) => e.into(),
        e => AppError::BadRequest(e.to_string()),
    }
}

// ---------- My profile ----------

/// A theme choice.
pub(crate) struct ThemeChoice {
    pub(crate) value: &'static str,
    pub(crate) label: &'static str,
    pub(crate) selected: bool,
}

const THEMES: &[(&str, &str)] = &[
    ("", "Auto"),
    ("light", "Light"),
    ("dark", "Dark"),
    ("sepia", "Sepia"),
    ("navy", "Navy"),
    ("black", "Black"),
];

/// The reader settings that differ from the defaults, e.g. "serif font, text size 5".
fn reader_summary(settings: &ProfileSettings) -> String {
    let parts: Vec<String> = settings
        .iter()
        .filter_map(|(name, value)| match name {
            "font" => Some(format!("{value} font")),
            "size" => Some(format!("text size {value}")),
            "leading" => Some(format!("{value} line spacing")),
            "width" => Some(format!("{value} text width")),
            "align" => Some(
                if value == "justify" {
                    "justified"
                } else {
                    "left-aligned"
                }
                .to_owned(),
            ),
            "status" => Some(format!("status line {value}")),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        "default".to_owned()
    } else {
        parts.join(", ")
    }
}

#[derive(Template)]
#[template(path = "profile.html")]
struct MyProfilePage {
    layout: Layout,
    profile: ProfileView,
    several: bool,
    share: bool,
    colors: Vec<ColorChoice>,
    themes: Vec<ThemeChoice>,
    reader_summary: String,
    reader_default: bool,
    /// The notice at the top (what was just done).
    notice: Option<String>,
    /// Field errors: `(field, message)`.
    errors: Vec<(&'static str, String)>,
    /// The submitted name (after an error) or the profile's.
    name: String,
    action: String,
    leave_action: String,
}

impl MyProfilePage {
    fn error(&self, field: &str) -> Option<&str> {
        self.errors
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, m)| m.as_str())
    }
}

/// The query of `GET /profile`: what was just done.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ProfileDone {
    done: Option<String>,
}

fn done_text(done: Option<&str>) -> Option<String> {
    Some(
        match done? {
            "saved" => "Your profile was saved.",
            "pin-set" => "Your PIN was saved. Your other devices were signed out.",
            "pin-removed" => "Your PIN was removed. Your other devices were signed out.",
            "reading" => "The reading settings are back to their defaults.",
            "signed-out" => "Your other devices were signed out.",
            _ => return None,
        }
        .to_owned(),
    )
}

fn my_profile_page(
    state: &AppState,
    current: &CurrentProfile,
    conn: &Connection,
    mode: SidebarMode,
    notice: Option<String>,
    errors: Vec<(&'static str, String)>,
    name: Option<String>,
) -> Result<MyProfilePage, AppError> {
    let profile = &current.profile;
    let settings = profile.settings.clone().unwrap_or_default();
    let theme = settings.get("theme").unwrap_or("");
    let summary = reader_summary(&settings);
    let urls = state.urls();
    Ok(MyProfilePage {
        layout: Layout::new(
            state,
            current,
            "My profile",
            sidebar::build(conn, state, mode, &Focus::None)?,
        ),
        profile: ProfileView::new(profile),
        several: current.several(),
        share: profile.share_activity,
        colors: colors(profile.color),
        themes: THEMES
            .iter()
            .map(|(value, label)| ThemeChoice {
                value,
                label,
                selected: *value == theme,
            })
            .collect(),
        reader_default: summary == "default",
        reader_summary: summary,
        notice,
        errors,
        name: name.unwrap_or_else(|| profile.name.clone()),
        action: urls.my_profile(),
        leave_action: urls.leave_profile(),
    })
}

/// `GET /profile`: My profile.
pub(crate) async fn my_profile(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    query: Result<Query<ProfileDone>, QueryRejection>,
) -> Result<Response, AppError> {
    let done = query.map(|Query(d)| d).unwrap_or_default();
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let page = state
        .db(move |conn| {
            let notice = done_text(done.done.as_deref());
            my_profile_page(&s, &current, conn, mode, notice, Vec::new(), None)
        })
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], render(&page)?).into_response())
}

/// The form of `POST /profile`; `action` says which part. PINs are never logged.
#[derive(Default, Deserialize)]
pub(crate) struct MyProfileForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    name: String,
    color: Option<String>,
    share: Option<String>,
    theme: Option<String>,
    #[serde(default)]
    current_pin: String,
    #[serde(default)]
    new_pin: String,
    #[serde(default)]
    confirm_pin: String,
}

/// What a My profile post did.
enum Saved {
    /// Done: `303` to the page with this `done` value, with a renewed cookie.
    Done(&'static str, Option<Token>),
    /// Refused: the page again with these field errors.
    Refused(StatusCode, Vec<(&'static str, String)>),
}

/// `POST /profile` (form): saves the name, color, sharing and theme
/// (`action=details`), sets, changes or removes the PIN (`set-pin`, `remove-pin`; the
/// current PIN is needed when there is one), resets the reading settings
/// (`reset-reading`) or ends the other sessions (`sign-out-others`).
/// Post/redirect/get; `422` with field errors.
pub(crate) async fn save_my_profile(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    form: Result<Form<MyProfileForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Form(form) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let id = current.id();
    let saved = match form.action.as_str() {
        "details" => save_details(&state, &current, &form).await?,
        "set-pin" | "remove-pin" => save_pin(&state, &current, &form).await?,
        "reset-reading" => {
            let mut settings = ProfileSettings::default();
            if let Some(theme) = current
                .profile
                .settings
                .as_ref()
                .and_then(|s| s.get("theme"))
            {
                settings = ProfileSettings::from_json(&serde_json::json!({ "theme": theme }));
            }
            user(&state, move |_, conn| {
                store::save_settings(conn, id, &settings).map_err(profile_error)
            })
            .await?;
            Saved::Done("reading", None)
        }
        "sign-out-others" => {
            let keep = current.token.clone();
            user(&state, move |_, conn| {
                Ok(session::end_others(conn, id, keep.as_ref())?)
            })
            .await?;
            Saved::Done("signed-out", None)
        }
        _ => return Err(AppError::BadRequest("unknown action".to_owned())),
    };
    match saved {
        Saved::Done(done, token) => {
            let url = format!("{}?done={done}", state.urls().my_profile());
            match token {
                Some(token) => Ok((
                    [(header::SET_COOKIE, set_cookie(&state, &headers, &token))],
                    Redirect::to(&url),
                )
                    .into_response()),
                None => Ok(Redirect::to(&url).into_response()),
            }
        }
        Saved::Refused(status, errors) => {
            let mode = SidebarMode::from_headers(&headers);
            let s = state.clone();
            // The profile as it is now (a rename may have failed after nothing changed).
            let name = form.name.clone();
            let page = state
                .db(move |conn| my_profile_page(&s, &current, conn, mode, None, errors, Some(name)))
                .await?;
            Ok((status, render(&page)?).into_response())
        }
    }
}

/// The color index of a form value.
fn parse_color(value: Option<&str>) -> Option<u8> {
    value
        .and_then(|v| v.trim().parse::<u8>().ok())
        .filter(|c| *c < COLORS)
}

async fn save_details(
    state: &AppState,
    current: &CurrentProfile,
    form: &MyProfileForm,
) -> Result<Saved, AppError> {
    let id = current.id();
    let old_name = current.profile.name.clone();
    let name = form.name.clone();
    let color = parse_color(form.color.as_deref());
    let share = form.share.is_some();
    let theme = form.theme.clone();
    let mut settings = current.profile.settings.clone().unwrap_or_default();
    if let Some(theme) = &theme {
        let mut value = serde_json::Map::new();
        for (k, v) in settings.iter() {
            value.insert(k.to_owned(), v.into());
        }
        value.insert("theme".to_owned(), theme.as_str().into());
        settings = ProfileSettings::from_json(&serde_json::Value::Object(value));
    }
    let several = current.several();
    let result = user(state, move |state, conn| {
        let mut errors = Vec::new();
        let new_name = match store::rename(conn, id, &name) {
            Ok(new_name) => Some(new_name),
            Err(e @ (ProfileError::InvalidName(_) | ProfileError::DuplicateName)) => {
                errors.push(("name", e.to_string()));
                None
            }
            Err(e) => return Err(profile_error(e)),
        };
        if errors.is_empty() {
            if let Some(color) = color {
                store::set_color(conn, id, color).map_err(profile_error)?;
            }
            if several {
                store::set_sharing(conn, id, share).map_err(profile_error)?;
            }
            if theme.is_some() {
                store::save_settings(conn, id, &settings).map_err(profile_error)?;
            }
            if let Some(new_name) = &new_name {
                state.exporter().rename_profile(id, &old_name, new_name);
            }
        }
        Ok(errors)
    })
    .await?;
    Ok(if result.is_empty() {
        Saved::Done("saved", None)
    } else {
        Saved::Refused(StatusCode::UNPROCESSABLE_ENTITY, result)
    })
}

async fn save_pin(
    state: &AppState,
    current: &CurrentProfile,
    form: &MyProfileForm,
) -> Result<Saved, AppError> {
    let id = current.id();
    let remove = form.action == "remove-pin";
    if current.profile.has_pin {
        match check_pin(state, id, form.current_pin.clone()).await? {
            Checked::Ok => {}
            Checked::Locked(wait) => {
                return Ok(Saved::Refused(
                    StatusCode::TOO_MANY_REQUESTS,
                    vec![("current_pin", locked_text(wait))],
                ));
            }
            Checked::Wrong(failure) => {
                let (status, message, _) = wrong_pin(failure);
                return Ok(Saved::Refused(status, vec![("current_pin", message)]));
            }
        }
    } else if remove {
        return Ok(Saved::Done("pin-removed", None));
    }
    let hash = if remove {
        None
    } else {
        if pin::check_format(&form.new_pin).is_err() {
            return Ok(Saved::Refused(
                StatusCode::UNPROCESSABLE_ENTITY,
                vec![("new_pin", "A PIN has 4 to 8 digits.".to_owned())],
            ));
        }
        if form.new_pin != form.confirm_pin {
            return Ok(Saved::Refused(
                StatusCode::UNPROCESSABLE_ENTITY,
                vec![("confirm_pin", "The two PINs differ.".to_owned())],
            ));
        }
        Some(hash_pin(state, form.new_pin.clone()).await?)
    };
    let token = current.token.clone();
    let renewed = user(state, move |_, conn| {
        store::set_pin_hash(conn, id, hash.as_deref()).map_err(profile_error)?;
        session::end_others(conn, id, token.as_ref())?;
        Ok(match &token {
            Some(token) => session::renew(conn, token, now())?,
            None => None,
        })
    })
    .await?;
    state.pins().success(id);
    tracing::info!(profile = id.0, removed = remove, "pin changed");
    Ok(Saved::Done(
        if remove { "pin-removed" } else { "pin-set" },
        renewed,
    ))
}

// ---------- Settings → Profiles (admin) ----------

/// The admin's section of the settings page.
pub(crate) struct ProfilesSection {
    pub(crate) profiles: Vec<ProfileView>,
    pub(crate) colors: Vec<ColorChoice>,
    pub(crate) can_add: bool,
    pub(crate) add_action: String,
    /// The notice of the last action, if any.
    pub(crate) notice: Option<(&'static str, String)>,
}

impl ProfilesSection {
    /// The section with the notice for the settings page's `profile` query value.
    pub(crate) fn load(state: &AppState, done: Option<&str>) -> Result<Self, AppError> {
        let profiles = store::list(&state.userdata().lock())?;
        let notice = done.and_then(|done| {
            Some(match done {
                "added" => ("ok", "The profile was added.".to_owned()),
                "renamed" => ("ok", "The profile was renamed.".to_owned()),
                "color" => ("ok", "The color was changed.".to_owned()),
                "role" => ("ok", "The role was changed.".to_owned()),
                "pin-removed" => (
                    "ok",
                    "The PIN was removed; the profile's devices were signed out.".to_owned(),
                ),
                "deleted" => ("ok", "The profile was deleted.".to_owned()),
                "duplicate" => ("error", "Another profile already has this name.".to_owned()),
                "name" => (
                    "error",
                    "A name has 1 to 32 characters and no control characters.".to_owned(),
                ),
                "too-many" => (
                    "error",
                    format!(
                        "There can be at most {} profiles.",
                        crate::profiles::MAX_PROFILES
                    ),
                ),
                "pin" => ("error", "A PIN has 4 to 8 digits.".to_owned()),
                "owner" => (
                    "error",
                    "The owner profile is always an admin and cannot be deleted.".to_owned(),
                ),
                _ => return None,
            })
        });
        Ok(Self {
            can_add: profiles.len() < crate::profiles::MAX_PROFILES,
            profiles: profiles.iter().map(ProfileView::new).collect(),
            colors: colors(0),
            add_action: state.urls().manage_profiles(),
            notice,
        })
    }
}

/// Back to the profiles section of the settings page with `done`.
fn back_to_profiles(urls: &Urls, done: &str) -> Redirect {
    Redirect::to(&format!("{}?profile={done}#profiles", urls.settings()))
}

/// The code of a refused change for the settings page.
fn refused(e: ProfileError) -> Result<&'static str, AppError> {
    Ok(match e {
        ProfileError::InvalidName(_) => "name",
        ProfileError::DuplicateName => "duplicate",
        ProfileError::TooMany => "too-many",
        ProfileError::Owner(_) => "owner",
        ProfileError::InvalidColor => return Err(AppError::BadRequest("unknown color".into())),
        ProfileError::NotFound => return Err(AppError::NotFound),
        ProfileError::Storage(e) => return Err(e.into()),
    })
}

/// The form of `POST /settings/profiles`. The PIN is never logged.
#[derive(Default, Deserialize)]
pub(crate) struct AddForm {
    #[serde(default)]
    name: String,
    color: Option<String>,
    role: Option<String>,
    #[serde(default)]
    pin: String,
}

/// `POST /settings/profiles` (admin): adds a profile (with an optional PIN). Adding
/// the second profile also starts an owner session in this browser, so the admin is
/// not sent to the picker.
pub(crate) async fn add(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    form: Result<Form<AddForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Form(form) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let urls = state.urls().clone();
    let pin = form.pin.trim().to_owned();
    if !pin.is_empty() && pin::check_format(&pin).is_err() {
        return Ok(back_to_profiles(&urls, "pin").into_response());
    }
    let hash = if pin.is_empty() {
        None
    } else {
        Some(hash_pin(&state, pin).await?)
    };
    let color = parse_color(form.color.as_deref()).unwrap_or(0);
    let role = match form.role.as_deref() {
        Some("admin") => Role::Admin,
        _ => Role::Reader,
    };
    let first_other = !current.several();
    let admin = current.id();
    let outcome = user(&state, move |_, conn| {
        let profile = match store::create(conn, &form.name, color, role, now()) {
            Ok(profile) => profile,
            Err(e) => return Ok(Err(refused(e)?)),
        };
        if let Some(hash) = &hash {
            store::set_pin_hash(conn, profile.id, Some(hash)).map_err(profile_error)?;
        }
        tracing::info!(profile = profile.id.0, "profile added");
        // The admin keeps working without choosing a profile.
        let token = if first_other {
            Some(session::create(conn, admin, now())?)
        } else {
            None
        };
        Ok(Ok(token))
    })
    .await?;
    match outcome {
        Err(code) => Ok(back_to_profiles(&urls, code).into_response()),
        Ok(Some(token)) => Ok((
            [(header::SET_COOKIE, set_cookie(&state, &headers, &token))],
            back_to_profiles(&urls, "added"),
        )
            .into_response()),
        Ok(None) => Ok(back_to_profiles(&urls, "added").into_response()),
    }
}

/// The form of the admin's profile actions.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ManageForm {
    #[serde(default)]
    name: String,
    color: Option<String>,
    role: Option<String>,
}

/// `POST /settings/profiles/{id}/{action}` (admin): `rename`, `color`, `role`,
/// `remove-pin`, `export` (the profile's Markdown export) or `delete` (needs the
/// profile's name typed in `name`).
pub(crate) async fn manage(
    State(state): State<AppState>,
    Path((id, action)): Path<(String, String)>,
    form: Result<Form<ManageForm>, FormRejection>,
) -> Result<Response, AppError> {
    let id = ProfileId(parse_id(&id)?);
    let form = form.map(|Form(f)| f).unwrap_or_default();
    let urls = state.urls().clone();
    let profile = user(&state, move |_, conn| Ok(store::get(conn, id)?))
        .await?
        .ok_or(AppError::NotFound)?;
    let done: &'static str = match action.as_str() {
        "rename" => {
            let old = profile.name.clone();
            let name = form.name.clone();
            user(&state, move |state, conn| {
                match store::rename(conn, id, &name) {
                    Ok(new) => {
                        state.exporter().rename_profile(id, &old, &new);
                        Ok("renamed")
                    }
                    Err(e) => refused(e),
                }
            })
            .await?
        }
        "color" => {
            let color = parse_color(form.color.as_deref())
                .ok_or_else(|| AppError::BadRequest("unknown color".into()))?;
            user(&state, move |_, conn| {
                store::set_color(conn, id, color).map_err(profile_error)?;
                Ok("color")
            })
            .await?
        }
        "role" => {
            let role = form
                .role
                .as_deref()
                .and_then(Role::parse)
                .ok_or_else(|| AppError::BadRequest("unknown role".into()))?;
            user(&state, move |_, conn| {
                match store::set_role(conn, id, role) {
                    Ok(()) => Ok("role"),
                    Err(e) => refused(e),
                }
            })
            .await?
        }
        "remove-pin" => {
            user(&state, move |_, conn| {
                store::set_pin_hash(conn, id, None).map_err(profile_error)?;
                session::end_others(conn, id, None)?;
                Ok(())
            })
            .await?;
            state.pins().success(id);
            tracing::info!(profile = id.0, "pin removed by an admin");
            "pin-removed"
        }
        "export" => {
            let summary = state
                .user_db(move |state, index| {
                    Ok(attach::export_all(
                        index,
                        &state.userdata().lock(),
                        state.exporter(),
                        Some(id),
                    )?)
                })
                .await?;
            let url = format!(
                "{}?exported={}",
                urls.manage_profile(id.0, "delete"),
                summary.annotations
            );
            return Ok(Redirect::to(&url).into_response());
        }
        "delete" => {
            if id.is_owner() {
                "owner"
            } else if form.name.trim().to_lowercase() != profile.name.to_lowercase() {
                let url = format!("{}?mismatch=1", urls.manage_profile(id.0, "delete"));
                return Ok(Redirect::to(&url).into_response());
            } else {
                user(&state, move |_, conn| {
                    store::delete(conn, id).map_err(profile_error)
                })
                .await?;
                state.pins().success(id);
                tracing::info!(profile = id.0, "profile deleted");
                "deleted"
            }
        }
        _ => return Err(AppError::NotFound),
    };
    Ok(back_to_profiles(&urls, done).into_response())
}

#[derive(Template)]
#[template(path = "profile_delete.html")]
struct DeletePage {
    layout: Layout,
    profile: ProfileView,
    action: String,
    export_action: String,
    folder: String,
    exported: Option<u64>,
    mismatch: bool,
}

/// The query of the delete confirmation.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct DeleteQuery {
    exported: Option<u64>,
    mismatch: Option<String>,
}

/// `GET /settings/profiles/{id}/delete` (admin): the delete confirmation (type the
/// name; export first; the export folder is kept).
pub(crate) async fn confirm_delete(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    Path((id, action)): Path<(String, String)>,
    query: Result<Query<DeleteQuery>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let id = ProfileId(parse_id(&id)?);
    if action != "delete" || id.is_owner() {
        return Err(AppError::NotFound);
    }
    let query = query.map(|Query(q)| q).unwrap_or_default();
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let (profile, sidebar) = state
        .db(move |conn| {
            let profile = store::get(&s.userdata().lock(), id)?.ok_or(AppError::NotFound)?;
            Ok((profile, sidebar::build(conn, &s, mode, &Focus::None)?))
        })
        .await?;
    let urls = state.urls();
    let folder = state
        .exporter()
        .profile_dir(id, &profile.name)
        .strip_prefix(state.library().root())
        .map_or_else(
            |_| state.exporter().profile_dir(id, &profile.name),
            std::path::Path::to_path_buf,
        )
        .display()
        .to_string();
    render(&DeletePage {
        layout: Layout::new(&state, &current, "Delete a profile", sidebar),
        action: urls.manage_profile(id.0, "delete"),
        export_action: urls.manage_profile(id.0, "export"),
        profile: ProfileView::new(&profile),
        folder,
        exported: query.exported,
        mismatch: query.mismatch.is_some(),
    })
}
