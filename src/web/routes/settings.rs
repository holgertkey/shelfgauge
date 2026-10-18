//! The settings page: the `config.toml` form, the read-only server settings, this
//! browser's settings, maintenance actions and facts about the library.
//!
//! Saving edits the file (see [`crate::config::edit`]) and swaps the
//! [`LiveConfig`](crate::config::LiveConfig): `[library]`, `[reader]` and
//! `[annotations]` apply on the next request, `[scan]` at the next scan. `[server]` is
//! never changed from the browser: fields of it in a form are ignored.

use std::collections::HashMap;
use std::path::Path;

use askama::Template;
use axum::Form;
use axum::extract::rejection::{FormRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use rusqlite::Connection;
use serde::Deserialize;

use super::date_time;
use super::figures::{count, duration, file_size, group_digits};
use crate::annotations::attach;
use crate::config::edit::{self, FIELDS, FieldKind, FieldValue, field, file_hash};
use crate::config::{Config, ServerConfig};
use crate::storage::scans::last_scan;
use crate::storage::stats::{LeavingBooks, leaving_books};
use crate::web::layout::{Layout, SidebarMode};
use crate::web::sidebar::{self, Focus};
use crate::web::{AppError, AppState};

/// The sections of the form, in page order: (section, title, when changes apply).
const SECTIONS: &[(&str, &str, &str)] = &[
    ("library", "Library", "Changes apply at once."),
    (
        "reader",
        "Reading",
        "Changes apply at once: parts are made when a page is opened, and links to places \
         in books keep working.",
    ),
    (
        "annotations",
        "Annotations",
        "Changes apply at once: the limits to the next mark or note, the retention to the \
         next cleanup after a scan. Stored marks are never changed.",
    ),
    ("scan", "Scanning", "Changes apply at the next scan."),
];

/// The book formats as the form lists them: (name, label, note).
const FORMAT_OPTIONS: &[(&str, &str, &str)] = &[
    ("fb2", "FB2 (.fb2, .fbz)", ""),
    ("epub", "EPUB", ""),
    ("txt", "Plain text (.txt)", ""),
    (
        "html",
        "HTML (.html, .htm, .xhtml)",
        "most HTML files are not books",
    ),
];

/// A message at the top of the page.
struct Notice {
    /// `ok`, `info` or `error` (a CSS class).
    kind: &'static str,
    text: String,
    /// Whether to offer **Rescan now**.
    rescan: bool,
}

impl Notice {
    fn new(kind: &'static str, text: impl Into<String>) -> Self {
        Self {
            kind,
            text: text.into(),
            rescan: false,
        }
    }
}

/// A choice of a radio group or a checkbox list.
struct OptionView {
    value: &'static str,
    label: &'static str,
    note: &'static str,
    checked: bool,
}

/// A field of the config form.
struct FieldView {
    /// The form field name (the dotted key).
    name: &'static str,
    /// The element id.
    id: String,
    label: &'static str,
    hint: &'static str,
    /// "Default: 100".
    default: String,
    /// `number`, `bool`, `choice`, `formats` or `lines`.
    kind: &'static str,
    /// The text of a number field or the text area.
    value: String,
    min: u32,
    max: Option<u32>,
    checked: bool,
    options: Vec<OptionView>,
    error: Option<String>,
}

/// A section of the config form.
struct SectionView {
    id: &'static str,
    title: &'static str,
    effect: &'static str,
    fields: Vec<FieldView>,
}

/// A read-only value (server settings, facts about the library).
struct Row {
    label: &'static str,
    value: String,
    note: String,
    /// A link after the value (to measure the cache sizes).
    link: Option<(String, &'static str)>,
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    layout: Layout,
    notices: Vec<Notice>,
    /// The hash of the file the form was rendered from.
    rev: String,
    can_save: bool,
    sections: Vec<SectionView>,
    server: Vec<Row>,
    allowed_hosts: Vec<String>,
    scan_running: bool,
    reread_action: String,
    cache_action: String,
    export_action: String,
    clear_reading_action: String,
    about: Vec<Row>,
    unknown_keys: Vec<String>,
}

/// `config.toml` as read for the page.
struct Loaded {
    /// The file text; `None` if it cannot be read.
    text: Option<String>,
    rev: String,
    /// The file's values (not validated); `None` if it cannot be read or parsed.
    file: Option<Config>,
    unknown_keys: Vec<String>,
    /// What keeps the file from being edited here (shown at the top).
    error: Option<String>,
    /// An invalid value of an editable key (shown at its field).
    field_error: Option<(&'static str, String)>,
}

impl Loaded {
    fn read(state: &AppState) -> Self {
        let mut loaded = Self {
            text: None,
            rev: String::new(),
            file: None,
            unknown_keys: Vec::new(),
            error: None,
            field_error: None,
        };
        let text = match edit::read(&state.library().config_path()) {
            Ok(text) => text,
            Err(e) => {
                loaded.error = Some(format!("{e}. The form shows the running settings."));
                return loaded;
            }
        };
        loaded.rev = file_hash(text.as_bytes());
        match Config::parse_unvalidated(&text) {
            Ok((config, unknown)) => {
                let mut valid = config;
                match valid.validate() {
                    Ok(()) => {}
                    Err(e) if field(e.key).is_some() => {
                        loaded.field_error = Some((e.key, e.message));
                    }
                    Err(e) => {
                        loaded.error = Some(format!(
                            "config.toml has an invalid value: `{}` {}. Fix this in \
                             config.toml; until then the settings cannot be saved here.",
                            e.key, e.message
                        ));
                    }
                }
                // Normalized as far as validation got (`formats` lowercase, without
                // repeats), so the form and the comparisons see what the program uses.
                loaded.file = Some(valid);
                loaded.unknown_keys = unknown;
            }
            Err(message) => {
                loaded.error = Some(format!(
                    "config.toml is not valid TOML ({message}). Fix it in config.toml; until \
                     then the settings cannot be saved here."
                ));
            }
        }
        loaded.text = Some(text);
        loaded
    }

    /// Whether a save may write the file.
    fn can_save(&self) -> bool {
        self.text.is_some() && self.file.is_some() && self.error.is_none()
    }

    /// The file's `[server]` settings, normalized, if the file is valid.
    fn server(&self) -> Option<ServerConfig> {
        let mut config = self.file.clone()?;
        config.validate().ok()?;
        Some(config.server)
    }
}

/// What the form shows: the values of a config or what was submitted.
enum Values<'a> {
    Config(&'a Config),
    Submitted(&'a [(String, String)]),
}

impl Values<'_> {
    fn submitted<'a>(pairs: &'a [(String, String)], key: &str) -> Vec<&'a str> {
        pairs
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }
}

/// The query of `GET /settings`: what was just done.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct Done {
    /// The config was saved.
    saved: Option<String>,
    /// The save may add books at the next scan.
    more: Option<String>,
    /// A maintenance action: `reread`, `busy`, `cache` or `export`.
    done: Option<String>,
    /// Its count (files removed, annotations exported).
    n: Option<u64>,
    /// Its second count (export files).
    books: Option<u64>,
    /// Measure the cache sizes (a directory walk: slow on big libraries).
    sizes: Option<String>,
}

fn sections(values: &Values, errors: &HashMap<&'static str, String>) -> Vec<SectionView> {
    SECTIONS
        .iter()
        .map(|&(id, title, effect)| SectionView {
            id,
            title,
            effect,
            fields: FIELDS
                .iter()
                .filter(|f| f.section() == id)
                .map(|f| field_view(f, values, errors.get(f.key).cloned()))
                .collect(),
        })
        .collect()
}

fn field_view(f: &edit::Field, values: &Values, error: Option<String>) -> FieldView {
    let default = f.default_value();
    let mut view = FieldView {
        name: f.key,
        id: format!("f-{}", f.key.replace(['.', '_'], "-")),
        label: f.label,
        hint: f.hint(),
        default: match (&default, f.kind) {
            (FieldValue::List(list), _) if list.is_empty() => "Default: none".to_owned(),
            (FieldValue::List(list), _) => format!("Default: {}", list.join(", ")),
            (value, _) => format!("Default: {value}"),
        },
        kind: "",
        value: String::new(),
        min: 0,
        max: None,
        checked: false,
        options: Vec::new(),
        error,
    };
    let (raw, current) = match values {
        Values::Config(config) => (Vec::new(), Some(f.get(config))),
        Values::Submitted(pairs) => (Values::submitted(pairs, f.key), None),
    };
    match f.kind {
        FieldKind::Number { min, max } => {
            view.kind = "number";
            view.min = min;
            view.max = (max < u32::MAX).then_some(max);
            view.value = match current {
                Some(value) => value.to_string(),
                None => raw
                    .first()
                    .map_or_else(String::new, |v| v.trim().to_owned()),
            };
        }
        FieldKind::Bool => {
            view.kind = "bool";
            view.checked = match current {
                Some(value) => value == FieldValue::Bool(true),
                None => f.parse(&raw) == Ok(FieldValue::Bool(true)),
            };
        }
        FieldKind::Choice(choices) => {
            view.kind = "choice";
            let selected = match current {
                Some(value) => value.to_string(),
                None => raw.first().map_or_else(String::new, |v| v.to_string()),
            };
            view.options = choices
                .iter()
                .map(|&c| OptionView {
                    value: c,
                    label: choice_label(c),
                    note: "",
                    checked: c == selected,
                })
                .collect();
        }
        FieldKind::Formats => {
            view.kind = "formats";
            let selected: Vec<String> = match current {
                Some(FieldValue::List(list)) => list,
                _ => raw.iter().map(|v| v.trim().to_ascii_lowercase()).collect(),
            };
            view.options = FORMAT_OPTIONS
                .iter()
                .map(|&(value, label, note)| OptionView {
                    value,
                    label,
                    note,
                    checked: selected.iter().any(|s| s == value),
                })
                .collect();
        }
        FieldKind::Lines => {
            view.kind = "lines";
            view.value = match current {
                Some(value) => value.to_string(),
                None => raw.join("\n").replace("\r\n", "\n"),
            };
        }
    }
    view
}

fn choice_label(choice: &str) -> &'static str {
    match choice {
        "none" => "Whole book on one page",
        "section" => "By sections",
        _ => "By size",
    }
}

/// `GET /settings` (admin): the settings page.
pub(crate) async fn page(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<Done>, QueryRejection>,
) -> Result<Html<String>, AppError> {
    let done = query.map(|Query(done)| done).unwrap_or_default();
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let page = state
        .db(move |conn| {
            let loaded = Loaded::read(&s);
            let mut notices = done_notices(&done);
            let mut page = build(&s, conn, mode, &loaded, None, HashMap::new(), &mut notices)?;
            if done.sizes.is_some() {
                page.about = about(&s, conn, true)?;
            }
            Ok((page, notices))
        })
        .await?;
    let (mut page, notices) = page;
    page.notices = notices;
    super::render(&page)
}

/// The notices for what was just done (the query of a redirect).
fn done_notices(done: &Done) -> Vec<Notice> {
    let mut notices = Vec::new();
    if done.saved.is_some() {
        notices.push(Notice::new("ok", "Settings saved."));
        if done.more.is_some() {
            notices.push(Notice {
                rescan: true,
                ..Notice::new("info", "New books are indexed at the next scan.")
            });
        }
    }
    let n = done.n.unwrap_or(0);
    match done.done.as_deref() {
        Some("reread") => notices.push(Notice::new(
            "ok",
            "Re-reading all books: the progress is shown above.",
        )),
        Some("busy") => notices.push(Notice::new(
            "info",
            "A scan is already running; try again when it has finished.",
        )),
        Some("cache") => notices.push(Notice::new(
            "ok",
            format!(
                "The content cache was cleared ({} removed). Books are parsed again when \
                 they are opened.",
                count(n, "file", "files")
            ),
        )),
        Some("export") => notices.push(Notice::new(
            "ok",
            format!(
                "Exported {} of {} to .shelfgauge/annotations/.",
                count(n, "annotation", "annotations"),
                count(done.books.unwrap_or(0), "book", "books")
            ),
        )),
        _ => {}
    }
    notices
}

/// Builds the page; the form shows `submitted` values if given, otherwise the file's.
/// Adds the notices about the file and the index to `notices`.
fn build(
    state: &AppState,
    conn: &Connection,
    mode: SidebarMode,
    loaded: &Loaded,
    submitted: Option<&[(String, String)]>,
    mut errors: HashMap<&'static str, String>,
    notices: &mut Vec<Notice>,
) -> Result<SettingsPage, AppError> {
    let running = state.config();
    if let Some(error) = &loaded.error {
        notices.push(Notice::new("error", error.clone()));
    }
    if let Some((key, message)) = &loaded.field_error
        && submitted.is_none()
    {
        errors.entry(key).or_insert_with(|| {
            format!("{message} (the value in config.toml; correct it here or in the file)")
        });
    }
    if let Some(file) = &loaded.file
        && loaded.can_save()
        && submitted.is_none()
        && FIELDS
            .iter()
            .any(|f| !f.get(file).same(&f.get(&running), f.kind))
    {
        notices.push(Notice::new(
            "info",
            "config.toml was changed outside this page: the running settings differ from \
             the form. Saving applies the file as a whole.",
        ));
    }
    let leaving = leaving(state, conn, &running)?;
    if leaving.total() > 0 {
        let retention = running.annotations.removed_book_retention_days;
        let mut parts: Vec<String> = leaving
            .by_format
            .iter()
            .map(|(format, n)| format!("{} in {} format", group_digits(*n), format.to_uppercase()))
            .collect();
        if leaving.excluded > 0 {
            parts.push(format!(
                "{} matching an exclude pattern",
                group_digits(leaving.excluded)
            ));
        }
        notices.push(Notice {
            rescan: true,
            ..Notice::new(
                "info",
                format!(
                    "{} ({}) will be removed from the index at the next scan; their \
                     bookmarks and quotes are {}.",
                    count(leaving.total(), "book", "books"),
                    parts.join(", "),
                    match retention {
                        0 => "deleted at once".to_owned(),
                        days => format!("kept for {}", count(days.into(), "day", "days")),
                    }
                ),
            )
        });
    }

    let values = match (submitted, &loaded.file) {
        (Some(pairs), _) => Values::Submitted(pairs),
        (None, Some(file)) => Values::Config(file),
        (None, None) => Values::Config(&running),
    };
    let urls = state.urls();
    Ok(SettingsPage {
        layout: Layout::new(
            state,
            "Settings",
            sidebar::build(conn, state, mode, &Focus::None)?,
        ),
        notices: Vec::new(),
        rev: loaded.rev.clone(),
        can_save: loaded.can_save(),
        sections: sections(&values, &errors),
        server: server_rows(state, loaded.server().as_ref()),
        allowed_hosts: state.server().allowed_hosts.clone(),
        scan_running: state.scan().is_running(),
        reread_action: urls.settings_action("reread"),
        cache_action: urls.settings_action("content-cache"),
        export_action: urls.settings_action("export"),
        clear_reading_action: urls.clear_reading(),
        about: about(state, conn, false)?,
        unknown_keys: loaded.unknown_keys.clone(),
    })
}

/// The books the next scan would remove under the running `[scan]` settings.
fn leaving(state: &AppState, conn: &Connection, config: &Config) -> Result<LeavingBooks, AppError> {
    let root = state.library().root();
    let excludes = crate::scanner::excludes(root, &config.scan.exclude).ok();
    let check_paths = excludes.as_ref().is_some_and(|e| !e.is_empty());
    Ok(leaving_books(
        conn,
        &config.scan.formats,
        check_paths,
        |path| {
            excludes.as_ref().is_some_and(|e| {
                e.matched_path_or_any_parents(root.join(path), false)
                    .is_ignore()
            })
        },
    )?)
}

/// The `[server]` rows: the running values, with what overrides them or differs in
/// the file.
fn server_rows(state: &AppState, file: Option<&ServerConfig>) -> Vec<Row> {
    let running = state.server();
    let overrides = state.overrides();
    let restart = |file_value: Option<String>, running_value: &str| match file_value {
        Some(v) if v != running_value => {
            format!("config.toml now says {v}; restart shelfgauge to apply it")
        }
        _ => String::new(),
    };
    let bind = running.bind.to_string();
    let port = running.port.to_string();
    let hosts = if running.allowed_hosts.is_empty() {
        "none".to_owned()
    } else {
        running.allowed_hosts.join(", ")
    };
    let file_hosts = file.map(|f| {
        if f.allowed_hosts.is_empty() {
            "none".to_owned()
        } else {
            f.allowed_hosts.join(", ")
        }
    });
    vec![
        Row {
            link: None,
            label: "Address",
            note: match overrides.bind {
                Some(_) => format!(
                    "--bind overrides {} from config.toml",
                    file.map_or_else(|| "the value".to_owned(), |f| f.bind.to_string())
                ),
                None => restart(file.map(|f| f.bind.to_string()), &bind),
            },
            value: bind,
        },
        Row {
            link: None,
            label: "Port",
            note: match overrides.port {
                Some(_) => format!(
                    "--port overrides {} from config.toml",
                    file.map_or_else(|| "the value".to_owned(), |f| f.port.to_string())
                ),
                None => restart(file.map(|f| f.port.to_string()), &port),
            },
            value: port,
        },
        Row {
            link: None,
            label: "Allowed hosts",
            note: restart(file_hosts, &hosts),
            value: hosts,
        },
        Row {
            link: None,
            label: "URL prefix",
            note: restart(file.map(|f| f.base_path.clone()), &running.base_path),
            value: running.base_path.clone(),
        },
    ]
}

/// Facts about the library: version, paths, sizes and the last scan. The cache sizes
/// need a walk of their directories (about 0.1 s per 100 000 files, more on a cold
/// disk), so they are measured only with `cache_sizes`.
fn about(state: &AppState, conn: &Connection, cache_sizes: bool) -> Result<Vec<Row>, AppError> {
    let library = state.library();
    let row = |label, value: String| Row {
        label,
        value,
        note: String::new(),
        link: None,
    };
    let measure = format!("{}?sizes=1#about", state.urls().settings());
    let database = |path: &Path| {
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        file_size(file_len(path) + file_len(Path::new(&wal)))
    };
    let cache = |label, dir: &Path| {
        if cache_sizes {
            let (files, bytes) = dir_size(dir);
            row(
                label,
                format!("{} in {}", file_size(bytes), count(files, "file", "files")),
            )
        } else {
            Row {
                link: Some((measure.clone(), "Measure")),
                ..row(label, "not measured".to_owned())
            }
        }
    };
    let last = match last_scan(conn)? {
        Some(scan) => format!(
            "{}: {} added, {} updated, {} moved, {} removed, {} in {}{}",
            date_time(scan.finished_at),
            group_digits(scan.added),
            group_digits(scan.updated),
            group_digits(scan.moved),
            group_digits(scan.removed),
            count(scan.errors, "error", "errors"),
            duration(scan.finished_at - scan.started_at),
            if scan.cancelled { " (cancelled)" } else { "" }
        ),
        None => "never".to_owned(),
    };
    Ok(vec![
        row("Version", crate::VERSION.to_owned()),
        row("Library", library.root().display().to_string()),
        row("Config file", library.config_path().display().to_string()),
        row("Index", database(&library.db_path())),
        row("Annotations database", database(&library.userdata_path())),
        cache("Cover cache", &library.covers_dir()),
        cache("Content cache", &library.content_dir()),
        row("Last scan", last),
    ])
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

/// The number of files below `dir` and their total size (0 if it does not exist).
fn dir_size(dir: &Path) -> (u64, u64) {
    walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .fold((0, 0), |(files, bytes), e| {
            (files + 1, bytes + e.metadata().map_or(0, |m| m.len()))
        })
}

/// The result of a save.
enum Saved {
    /// Written; whether books may be added at the next scan.
    Done { more: bool },
    /// Not written: the page with the reason.
    Refused(StatusCode, Box<SettingsPage>),
}

/// `POST /settings` (admin, form): checks and writes `config.toml` and applies it;
/// back to the page with the notices. Invalid values: the form again with status 422;
/// a file changed since the form was rendered: 409.
pub(crate) async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    form: Result<Form<Vec<(String, String)>>, FormRejection>,
) -> Result<Response, AppError> {
    let Form(pairs) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let mode = SidebarMode::from_headers(&headers);
    let s = state.clone();
    let saved = state
        .db(move |conn| save_form(&s, conn, mode, &pairs))
        .await?;
    match saved {
        Saved::Done { more } => {
            let mut url = format!("{}?saved=1", state.urls().settings());
            if more {
                url.push_str("&more=1");
            }
            Ok(Redirect::to(&url).into_response())
        }
        Saved::Refused(status, page) => Ok((status, super::render(page.as_ref())?).into_response()),
    }
}

fn save_form(
    state: &AppState,
    conn: &Connection,
    mode: SidebarMode,
    pairs: &[(String, String)],
) -> Result<Saved, AppError> {
    let _lock = state.save_lock();
    let loaded = Loaded::read(state);
    let refuse = |status: StatusCode,
                  notice: Option<Notice>,
                  submitted: Option<&[(String, String)]>,
                  errors: HashMap<&'static str, String>|
     -> Result<Saved, AppError> {
        let mut notices: Vec<Notice> = notice.into_iter().collect();
        let mut page = build(state, conn, mode, &loaded, submitted, errors, &mut notices)?;
        page.notices = notices;
        Ok(Saved::Refused(status, Box::new(page)))
    };
    let rev = Values::submitted(pairs, "rev");
    let (Some(text), Some(file)) = (&loaded.text, &loaded.file) else {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            None,
            Some(pairs),
            HashMap::new(),
        );
    };
    if rev.first().copied() != Some(loaded.rev.as_str()) {
        return refuse(
            StatusCode::CONFLICT,
            Some(Notice::new(
                "error",
                "config.toml was changed since this page was opened. The form now shows \
                 its current values; make your changes again.",
            )),
            None,
            HashMap::new(),
        );
    }
    if !loaded.can_save() {
        return refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            None,
            Some(pairs),
            HashMap::new(),
        );
    }

    let mut new = file.clone();
    let mut errors = HashMap::new();
    for f in FIELDS {
        match f.parse(&Values::submitted(pairs, f.key)) {
            Ok(value) => f.set(&mut new, value),
            Err(message) => {
                errors.insert(f.key, message);
            }
        }
    }
    let invalid = |errors| {
        refuse(
            StatusCode::UNPROCESSABLE_ENTITY,
            Some(Notice::new(
                "error",
                "Some values are invalid; nothing was saved.",
            )),
            Some(pairs),
            errors,
        )
    };
    if !errors.is_empty() {
        return invalid(errors);
    }
    let failed = |message: String| {
        refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            Some(Notice::new(
                "error",
                format!("{message}; nothing was saved."),
            )),
            Some(pairs),
            HashMap::new(),
        )
    };
    let new_text = match edit::edit(text, &new) {
        Ok(new_text) => new_text,
        Err(e) => return failed(e.to_string()),
    };
    // The same check as at startup: the next start never rejects the file.
    let mut config = match Config::from_toml(&new_text, Path::new("config.toml")) {
        Ok((config, _)) => config,
        Err(crate::Error::InvalidConfig { error, .. }) if field(error.key).is_some() => {
            return invalid(HashMap::from([(error.key, error.message)]));
        }
        Err(crate::Error::InvalidConfig { error, .. }) => {
            return failed(format!("`{}` {}", error.key, error.message));
        }
        Err(e) => return failed(e.to_string()),
    };
    if new_text != *text
        && let Err(e) = edit::write(&state.library().config_path(), &new_text)
    {
        return failed(e.to_string());
    }
    let running = state.config();
    let more = config
        .scan
        .formats
        .iter()
        .any(|f| !running.scan.formats.contains(f))
        || running
            .scan
            .exclude
            .iter()
            .any(|p| !config.scan.exclude.contains(p));
    // `[server]` stays as started (with the command-line overrides).
    config.server = state.server().clone();
    state.live_config().set(config);
    tracing::info!("settings saved");
    Ok(Saved::Done { more })
}

/// `POST /settings/reread` (admin): starts a scan that parses every book again.
pub(crate) async fn reread(State(state): State<AppState>) -> Redirect {
    let done = if state.scan().start_reread() {
        tracing::info!("re-reading all books");
        "reread"
    } else {
        "busy"
    };
    Redirect::to(&format!("{}?done={done}", state.urls().settings()))
}

/// `POST /settings/content-cache` (admin): removes the cached book content (covers
/// are kept); books are parsed again when they are opened.
pub(crate) async fn clear_content_cache(
    State(state): State<AppState>,
) -> Result<Redirect, AppError> {
    let dir = state.library().content_dir();
    let removed = crate::web::blocking(move || clear_dir(&dir)).await?;
    state.forget_loaded_books();
    tracing::info!(removed, "content cache cleared");
    Ok(Redirect::to(&format!(
        "{}?done=cache&n={removed}",
        state.urls().settings()
    )))
}

/// Removes the files below `dir`, leaving the directories (a book opened meanwhile may
/// be storing its content); returns the number removed.
fn clear_dir(dir: &Path) -> u64 {
    let mut removed = 0;
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if entry.file_type().is_file() {
            match std::fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(
                    file = ?entry.file_name(),
                    error = %e,
                    "cannot remove a content cache file"
                ),
            }
        }
    }
    removed
}

/// `POST /settings/export` (admin): writes the Markdown export of all annotations
/// (`shelfgauge annotations export`).
pub(crate) async fn export(State(state): State<AppState>) -> Result<Redirect, AppError> {
    let summary = state
        .user_db(|state, index| {
            Ok(attach::export_all(
                index,
                &state.userdata().lock(),
                state.exporter(),
            )?)
        })
        .await?;
    tracing::info!(
        annotations = summary.annotations,
        books = summary.files,
        "exported the annotations"
    );
    Ok(Redirect::to(&format!(
        "{}?done=export&n={}&books={}",
        state.urls().settings(),
        summary.annotations,
        summary.files
    )))
}
