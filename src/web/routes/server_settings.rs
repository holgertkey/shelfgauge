//! Settings → Server and hub (Phase 30, decision 12): `[server]` and `[hub]` of
//! `config.toml`, editable on this computer only (other devices still cannot change
//! them: a remote change of `bind` or `auth` could lock the library out), applied by a
//! restart from inside; and the library's handle on its hub with **Change handle**.
//!
//! `hub.url` cannot change while the library is on its hub: the page points to
//! **Delete the library on this hub** first (moving between hubs is not in Phase 30).
//! The file is edited like the rest of the settings page ([`edit_restart_keys`]),
//! validated before it is written, and a form rendered before another edit of the
//! file is refused.

use std::net::IpAddr;

use askama::Template;
use axum::extract::rejection::FormRejection;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::render;
use crate::config::edit::{edit_restart_keys, file_hash, read, write};
use crate::config::{AuthMode, Config};
use crate::hub::address::normalize_label;
use crate::hub_client::store;
use crate::web::layout::{Layout, SidebarMode};
use crate::web::profile::CurrentProfile;
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState};

const TITLE: &str = "Server and hub";

#[derive(Template)]
#[template(path = "settings_server.html")]
struct ServerPage {
    layout: Layout,
    notice: Option<String>,
    error: Option<String>,
    rev: String,
    bind: String,
    /// Whether `bind` is one of the two choices (else it is shown as it is).
    bind_other: bool,
    port: u16,
    /// The command-line options that override the file (`--port 9000`), which the
    /// restart keeps.
    overridden: Vec<String>,
    allowed_hosts: String,
    auth_password: bool,
    remote_admin: bool,
    hub_url: String,
    /// The library is on its hub: `hub.url` is locked.
    hub_locked: bool,
    instance_name: String,
    sync_interval: u32,
    handle: Option<String>,
    action: String,
    restart_action: String,
    hub_action: String,
    settings_action: String,
}

/// The query of `GET /settings/server`: what was just done.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ServerDone {
    done: Option<String>,
}

fn done_text(done: Option<&str>) -> Option<String> {
    Some(
        match done? {
            "saved" => "Saved in config.toml. Restart shelfgauge to apply the changes.",
            "unchanged" => "Nothing changed.",
            "handle" => {
                "The handle changed: every member's address ends in the new one now. Tell \
                 them; their devices stay signed in."
            }
            _ => return None,
        }
        .to_owned(),
    )
}

/// Only on this computer.
fn local_only(current: &CurrentProfile) -> Result<(), AppError> {
    if current.remote {
        return Err(AppError::Forbidden);
    }
    Ok(())
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

/// The file's config (the running one when the file cannot be read or parsed) and
/// its hash.
fn file_config(state: &AppState) -> (Config, String, Option<String>) {
    match read(&state.library().config_path()) {
        Ok(text) => {
            let rev = file_hash(text.as_bytes());
            match Config::parse_unvalidated(&text) {
                Ok((config, _)) => (config, rev, None),
                Err(e) => (
                    (*state.config()).clone(),
                    rev,
                    Some(format!(
                        "config.toml is not valid TOML ({e}): fix it there first."
                    )),
                ),
            }
        }
        Err(e) => (
            (*state.config()).clone(),
            String::new(),
            Some(e.to_string()),
        ),
    }
}

/// Whether logins with passwords are on now or will be at the next start (config.toml):
/// then an admin must keep a password, or shelfgauge would not start.
pub(crate) fn password_login(state: &AppState) -> bool {
    state.auth().is_configured() || file_config(state).0.server.auth == AuthMode::Password
}

/// Why the last admin password stays (Settings → Profiles and My profile).
pub(crate) const LAST_ADMIN_PASSWORD: &str = "That would leave no admin with a password \
    while logins with passwords are on, and shelfgauge would not start. Give another admin \
    a password first, or turn logins with passwords off (Settings → Server and hub).";

async fn page_response(
    state: &AppState,
    current: &CurrentProfile,
    mode: SidebarMode,
    notice: Option<String>,
    error: Option<String>,
    status: StatusCode,
) -> Result<Response, AppError> {
    let (config, rev, file_error) = file_config(state);
    let library = super::hub::hub_library(state).await?;
    let bind = config.server.bind.to_string();
    let urls = state.urls();
    let page = ServerPage {
        layout: layout(state, current, mode).await?,
        notice,
        error: error.or(file_error),
        rev,
        bind_other: bind != "127.0.0.1" && bind != "0.0.0.0",
        bind,
        port: config.server.port,
        overridden: overridden(state),
        allowed_hosts: config.server.allowed_hosts.join("\n"),
        auth_password: config.server.auth == AuthMode::Password,
        remote_admin: config.server.remote_admin,
        hub_url: library
            .as_ref()
            .map_or(config.hub.url.clone(), |l| l.hub_url.to_string()),
        hub_locked: library.is_some(),
        instance_name: config.hub.instance_name.clone(),
        sync_interval: config.hub.sync_interval_minutes,
        handle: library.map(|l| l.handle),
        action: urls.settings_action("server"),
        restart_action: urls.settings_action("restart"),
        hub_action: urls.profile_hub(),
        settings_action: urls.settings(),
    };
    Ok((
        status,
        [(header::CACHE_CONTROL, "no-store")],
        render(&page)?,
    )
        .into_response())
}

/// `GET /settings/server` (admin, this computer only).
pub(crate) async fn page(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    query: Query<ServerDone>,
) -> Result<Response, AppError> {
    local_only(&current)?;
    let mode = SidebarMode::from_headers(&headers);
    page_response(
        &state,
        &current,
        mode,
        done_text(query.done.as_deref()),
        None,
        StatusCode::OK,
    )
    .await
}

/// The form of `POST /settings/server`. Never logged.
#[derive(Default, Deserialize)]
pub(crate) struct ServerForm {
    #[serde(default)]
    action: String,
    #[serde(default)]
    rev: String,
    #[serde(default)]
    bind: String,
    #[serde(default)]
    port: String,
    #[serde(default)]
    allowed_hosts: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    remote_admin: Option<String>,
    #[serde(default)]
    hub_url: String,
    #[serde(default)]
    instance_name: String,
    #[serde(default)]
    sync_interval: String,
    #[serde(default)]
    handle: String,
}

/// `POST /settings/server` (admin, this computer only): `save` writes `[server]` and
/// `[hub]` to `config.toml` (applied at the next start), `handle` changes the
/// library's handle on its hub.
pub(crate) async fn save(
    State(state): State<AppState>,
    current: CurrentProfile,
    headers: HeaderMap,
    form: Result<Form<ServerForm>, FormRejection>,
) -> Result<Response, AppError> {
    local_only(&current)?;
    let Form(form) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let mode = SidebarMode::from_headers(&headers);
    let refused = |error: String| {
        let state = state.clone();
        let current = current.clone();
        async move {
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
    };
    match form.action.as_str() {
        "save" => match save_config(&state, &form).await? {
            Ok(changed) => Ok(Redirect::to(&format!(
                "{}?done={}",
                state.urls().settings_action("server"),
                if changed { "saved" } else { "unchanged" }
            ))
            .into_response()),
            Err(error) => refused(error).await,
        },
        "handle" => {
            let Some(library) = super::hub::hub_library(&state).await? else {
                return refused("The library is not on a hub.".to_owned()).await;
            };
            let Some(handle) = normalize_label(&form.handle) else {
                return refused(super::hub::NAME_RULES.to_owned()).await;
            };
            let token = library.token.clone();
            let sent = handle.clone();
            match super::hub::call(library.hub_url.clone(), move |c| {
                c.set_handle(&token, &sent)
            })
            .await?
            {
                Ok(()) => {
                    super::profiles::user(&state, move |_, conn| {
                        Ok(store::set_handle(conn, &handle)?)
                    })
                    .await?;
                    tracing::info!("library handle changed");
                    Ok(Redirect::to(&format!(
                        "{}?done=handle",
                        state.urls().settings_action("server")
                    ))
                    .into_response())
                }
                Err(e) => refused(e.user_text()).await,
            }
        }
        _ => Err(AppError::BadRequest("unknown action".to_owned())),
    }
}

/// The command-line options overriding `[server]`, as typed (`--port 9000`).
fn overridden(state: &AppState) -> Vec<String> {
    let o = state.overrides();
    let port = o.port.map(|p| format!("--port {p}"));
    let bind = o.bind.map(|b| format!("--bind {b}"));
    port.into_iter().chain(bind).collect()
}

/// Writes the form's `[server]` and `[hub]` values; `Ok(changed)` or a refusal.
async fn save_config(
    state: &AppState,
    form: &ServerForm,
) -> Result<Result<bool, String>, AppError> {
    let path = state.library().config_path();
    let text = read(&path).map_err(|e| AppError::Internal(e.to_string()))?;
    if file_hash(text.as_bytes()) != form.rev {
        return Ok(Err(
            "config.toml was changed since this page was shown: look at it again.".to_owned(),
        ));
    }
    let (mut new, _) = match Config::parse_unvalidated(&text) {
        Ok(parsed) => parsed,
        Err(e) => {
            return Ok(Err(
                format!("config.toml is not valid TOML ({e}).").to_owned()
            ));
        }
    };
    let Ok(bind) = form.bind.trim().parse::<IpAddr>() else {
        return Ok(Err(
            "The address to listen on is not an IP address.".to_owned()
        ));
    };
    let Ok(port) = form.port.trim().parse::<u16>() else {
        return Ok(Err("The port is a number from 1 to 65535.".to_owned()));
    };
    let Ok(sync) = form.sync_interval.trim().parse::<u32>() else {
        return Ok(Err("The sync interval is a number of minutes.".to_owned()));
    };
    new.server.bind = bind;
    new.server.port = port;
    new.server.allowed_hosts = form
        .allowed_hosts
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    new.server.auth = if form.auth == "password" {
        AuthMode::Password
    } else {
        AuthMode::None
    };
    new.server.remote_admin = form.remote_admin.is_some();
    if super::hub::hub_library(state).await?.is_none() {
        new.hub.url = form.hub_url.trim().to_owned();
    }
    new.hub.instance_name = form.instance_name.trim().to_owned();
    new.hub.sync_interval_minutes = sync;
    // Normalized as the program reads them (the hub's address in its form), so the
    // file holds what is used and a save of the same values changes nothing.
    if let Err(e) = new.validate() {
        return Ok(Err(format!("`{}` {}", e.key, e.message)));
    }
    let edited = match edit_restart_keys(&text, &new) {
        Ok(edited) => edited,
        Err(e) => return Ok(Err(e.to_string())),
    };
    if edited == text {
        return Ok(Ok(false));
    }
    if let Err(e) = Config::from_toml(&edited, &path) {
        return Ok(Err(e
            .to_string()
            .replace(&path.display().to_string(), "config.toml")));
    }
    // A password login needs an admin with a password, or the next start fails.
    if new.server.auth == AuthMode::Password {
        let has = super::profiles::user(state, |_, conn| {
            Ok(crate::profiles::store::admin_has_password(conn)?)
        })
        .await?;
        if !has {
            return Ok(Err(
                "Logins with passwords need an admin with a password first (My profile → \
                 Password), or shelfgauge would not start."
                    .to_owned(),
            ));
        }
    }
    write(&path, &edited).map_err(|e| AppError::Internal(e.to_string()))?;
    tracing::info!("server and hub settings saved");
    Ok(Ok(true))
}

/// `POST /settings/restart` (admin, this computer only): restarts shelfgauge; the page
/// tells the browser to come back in a few seconds.
pub(crate) async fn restart(
    State(state): State<AppState>,
    current: CurrentProfile,
) -> Result<Response, AppError> {
    local_only(&current)?;
    tracing::info!("restart asked for on the settings page");
    let back = state.urls().settings_action("server");
    // After the answer is sent.
    let restarting = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        restarting.request_restart();
    });
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>Restarting</title>\
         <p>shelfgauge is restarting. <a href=\"{back}\">Back to the settings</a> in a few \
         seconds (the address may have changed if you changed the port or the address it \
         listens on).</p>"
    );
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        axum::response::Html(body),
    )
        .into_response())
}
