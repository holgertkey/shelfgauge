//! The Showcase marks (`hub_client::showcase`): which folders an admin opened for the
//! library's catalog on the hub. Set by a checkbox next to every folder in the
//! sidebar's folder mode (the page's script, with `GET` and `POST /showcase`) and on
//! the folder page (a form with Save, for use without JavaScript). Shown only to admins
//! and only while the library is on a hub; the marks are kept when it is not.

use axum::extract::rejection::FormRejection;
use axum::extract::{Form, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Json, Redirect, Response};
use serde::{Deserialize, Serialize};

use super::Params;
use crate::annotations::now;
use crate::hub_client::showcase::{self, MarkState};
use crate::hub_client::store;
use crate::storage::folders::FolderPath;
use crate::web::profile::CurrentProfile;
use crate::web::{AppError, AppState};

/// A mark as the page's script gets it: `{path}` or `{zip, in}`.
#[derive(Debug, Serialize)]
#[serde(untagged)]
enum MarkJson<'a> {
    Dir {
        path: &'a str,
    },
    Archive {
        zip: &'a str,
        #[serde(rename = "in")]
        inner: &'a str,
    },
}

impl<'a> From<&'a FolderPath> for MarkJson<'a> {
    fn from(folder: &'a FolderPath) -> Self {
        match folder {
            FolderPath::Dir(path) => Self::Dir { path },
            FolderPath::Archive { archive, inner } => Self::Archive {
                zip: archive,
                inner,
            },
        }
    }
}

/// What `GET /showcase` answers.
#[derive(Debug, Serialize)]
struct MarksJson<'a> {
    /// Whether the library is on a hub (the checkboxes are shown only then).
    enabled: bool,
    marks: Vec<MarkJson<'a>>,
}

fn marks_json(enabled: bool, marks: &[FolderPath]) -> Response {
    Json(MarksJson {
        enabled,
        marks: marks.iter().map(MarkJson::from).collect(),
    })
    .into_response()
}

/// Whether the library is on a hub, and the marks (`conn`: the user-data database).
fn read(conn: &rusqlite::Connection) -> Result<(bool, Vec<FolderPath>), AppError> {
    Ok((store::library(conn)?.is_some(), showcase::marks(conn)?))
}

/// `GET /showcase` (admins): JSON `{enabled, marks: [{path} | {zip, in}]}`.
pub(crate) async fn marks(State(state): State<AppState>) -> Result<Response, AppError> {
    let (enabled, marks) = super::profiles::user(&state, |_, conn| read(conn)).await?;
    Ok(marks_json(enabled, &marks))
}

/// The form of `POST /showcase`: the folder (`path`, or `zip` and `in`) and `open`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct MarkForm {
    path: Option<String>,
    zip: Option<String>,
    #[serde(rename = "in")]
    inner: Option<String>,
    /// `1` (a checked box) opens the folder; absent closes it.
    #[serde(default)]
    open: String,
}

/// `POST /showcase` (admins, the library on a hub): marks a folder open or forgets its
/// mark. A script asking for JSON (`Accept: application/json`) gets the marks as `GET`
/// does; a form goes back to the folder page (`303`).
pub(crate) async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<MarkForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Form(form) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let folder = Params {
        path: form.path,
        zip: form.zip,
        inner: form.inner,
        ..Params::default()
    }
    .folder();
    let open = form.open == "1";
    let target = folder.clone();
    let marks = super::profiles::user(&state, move |_, conn| {
        if store::library(conn)?.is_none() {
            return Err(AppError::BadRequest(
                "the library is not on a hub".to_owned(),
            ));
        }
        showcase::set_mark(conn, &target, open, now())?;
        Ok(showcase::marks(conn)?)
    })
    .await?;
    tracing::info!(open, "showcase folder changed");
    let wants_json = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.contains("application/json"));
    if wants_json {
        return Ok(marks_json(true, &marks));
    }
    let url = state.urls().folder(&folder, 1);
    let sep = if url.contains('?') { '&' } else { '?' };
    Ok(Redirect::to(&format!("{url}{sep}showcase=saved#showcase")).into_response())
}

/// The Showcase control of a folder page.
pub(crate) struct Control {
    /// Where the form posts.
    pub(crate) action: String,
    /// The hidden fields naming the folder.
    pub(crate) fields: Vec<(&'static str, String)>,
    /// `open`, `closed` or `parent` (open through a parent folder).
    pub(crate) state: &'static str,
    /// The open parent's name and page, for `parent`.
    pub(crate) parent: Option<(String, String)>,
    /// Just saved (the form's redirect).
    pub(crate) saved: bool,
    /// On the root page: marked folders that no longer exist, with their forms'
    /// fields.
    pub(crate) missing: Vec<(String, Vec<(&'static str, String)>)>,
}

fn fields(folder: &FolderPath) -> Vec<(&'static str, String)> {
    match folder {
        FolderPath::Dir(path) => vec![("path", path.clone())],
        FolderPath::Archive { archive, inner } => {
            vec![("zip", archive.clone()), ("in", inner.clone())]
        }
    }
}

/// A folder as the admin reads it in a note: its path, archives with `→`.
fn describe(folder: &FolderPath) -> String {
    match folder {
        FolderPath::Dir(path) if path.is_empty() => "Library".to_owned(),
        FolderPath::Dir(path) => path.clone(),
        FolderPath::Archive { archive, inner } if inner.is_empty() => archive.clone(),
        FolderPath::Archive { archive, inner } => format!("{archive} → {inner}"),
    }
}

/// The control for `folder`'s page, if `current` is an admin here and the library
/// is on a hub (`index`: the index connection, for the missing marks on the root
/// page). Locks the user-data database itself. The nearest open parent is named for
/// a folder open through one.
pub(crate) fn control(
    state: &AppState,
    current: &CurrentProfile,
    index: &rusqlite::Connection,
    folder: &FolderPath,
    saved: bool,
) -> Result<Option<Control>, AppError> {
    if !current.admin_here(state) {
        return Ok(None);
    }
    let (enabled, marks) = read(&state.userdata().lock())?;
    if !enabled {
        return Ok(None);
    }
    let urls = state.urls();
    let (state_name, parent) = match showcase::folder_state(&marks, folder) {
        MarkState::Open => ("open", None),
        MarkState::Closed => ("closed", None),
        MarkState::ThroughParent => {
            let parent = marks
                .iter()
                .filter(|m| *m != folder && showcase::is_under(m, folder))
                .max_by_key(|m| match m {
                    FolderPath::Dir(p) => (0, p.len()),
                    FolderPath::Archive { inner, .. } => (1, inner.len()),
                })
                .map(|m| (describe(m), urls.folder(m, 1)));
            ("parent", parent)
        }
    };
    let missing = if *folder == FolderPath::Dir(String::new()) {
        showcase::missing_marks(index, &marks)?
            .iter()
            .map(|m| (describe(m), fields(m)))
            .collect()
    } else {
        Vec::new()
    };
    Ok(Some(Control {
        action: urls.showcase(),
        fields: fields(folder),
        state: state_name,
        parent,
        saved,
        missing,
    }))
}
