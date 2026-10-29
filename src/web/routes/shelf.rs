//! A book on the profile's hub shelf (concept §11, Phase 30): "Read on phone" on the
//! book card publishes it for the library (build, seal with the book key in the
//! manifest's header under the current library key, upload with the library's token,
//! push the shelf item with the book's annotations and position), "Remove from shelf"
//! deletes the publication and the book's records on the hub (local data stays), an
//! expired publication can be published again. "Download book file" exports the book
//! as a `.sgbook` file (concept §11 "Book files"): always a new publication, nothing
//! uploaded; a book not yet on the shelf goes on it as a `file` item. Only admins
//! publish: a `reader` profile's card has none of this.
//!
//! Publication ids and book keys are never logged; only profile and book ids.

use std::sync::Arc;

use axum::extract::rejection::FormRejection;
use axum::extract::{Form, Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::hub::call;
use super::parse_id;
use crate::annotations::now;
use crate::cache::covers;
use crate::format::model::Image;
use crate::hub_client::book_file;
use crate::hub_client::keys;
use crate::hub_client::store::{self, HubAccount, HubLibrary, ShelfEntry, ShelfKind, ShelfState};
use crate::hub_client::sync::Target;
use crate::hub_client::{ClientError, HubClient, sync};
use crate::profiles::ProfileId;
use crate::publication::{self, BookInfo, BookKey, Publication, generate_publication_id};
use crate::render::load::content_key;
use crate::storage::books::{BookDetails, BookStatus, book_by_id};
use crate::web::profile::CurrentProfile;
use crate::web::{AppError, AppState, blocking, qr};

/// The book's place on the shelf, for the book card.
pub(crate) struct ShelfView {
    /// `none`, `uploading`, `published`, `file` (on the shelf as book files),
    /// `expired`, `removing` or `protected`.
    pub(crate) state: &'static str,
    /// The library that published it, when it is another one.
    pub(crate) other: Option<String>,
    /// The book link and its QR code, when it is published.
    pub(crate) link: Option<(String, String)>,
    /// Whether the profile is the owner (it may pair a phone with the book).
    pub(crate) owner: bool,
    pub(crate) action: String,
}

/// What the book card shows about the shelf: `None` for a profile that is not an
/// admin, and while the library is not on the hub or the profile has no account there.
pub(crate) fn view(
    state: &AppState,
    profile: ProfileId,
    admin: bool,
    book: &BookDetails,
    readable: bool,
) -> Result<Option<ShelfView>, AppError> {
    let Some(key) = content_key(book).filter(|_| admin) else {
        return Ok(None);
    };
    let (library, entry) = {
        let conn = state.userdata().lock();
        let Some(library) = store::library(&conn)? else {
            return Ok(None);
        };
        if !store::account(&conn, profile)?.is_some_and(|a| a.on_hub()) {
            return Ok(None);
        }
        (library, store::shelf_entry(&conn, profile, &key)?)
    };
    let action = state.urls().book_hub(book.id);
    let (state_name, other, link) = match &entry {
        None if book.status == BookStatus::Protected => ("protected", None, None),
        None if !readable => return Ok(None),
        None => ("none", None, None),
        Some(entry) => {
            let link = match (entry.state, entry.publication) {
                (ShelfState::Published, Some(id)) if entry.kind == ShelfKind::Upload => {
                    let link = library.hub_url.book_link(&id);
                    qr::svg(&link, "QR code of the book link").map(|svg| (link, svg))
                }
                _ => None,
            };
            let other = (!entry.own).then(|| entry.instance.clone());
            let name = match entry.state {
                ShelfState::Uploading => "uploading",
                ShelfState::Published if entry.kind == ShelfKind::File => "file",
                ShelfState::Published => "published",
                ShelfState::Expired => "expired",
                ShelfState::Removing => "removing",
            };
            (name, other, link)
        }
    };
    Ok(Some(ShelfView {
        owner: profile.is_owner(),
        state: state_name,
        other,
        link,
        action,
    }))
}

/// The text of the `hub` query of the book card: `(class, text)`.
pub(crate) fn notice(code: &str) -> Option<(&'static str, &'static str)> {
    Some(match code {
        "published" => (
            "ok",
            "The book is on the hub: scan the code with your phone, or send the link to \
             anyone in the library.",
        ),
        "removed" => (
            "ok",
            "The book is deleted on the hub, for every member: its copy and your marks \
             there are gone. Your marks in this library stay.",
        ),
        "removing" => (
            "error",
            "The book is off your shelf here; the hub could not be reached, so its copy \
             there is deleted at the next sync.",
        ),
        "busy" => (
            "ok",
            "The book is being uploaded; reload this page in a moment.",
        ),
        "unreachable" => ("error", "The hub cannot be reached; try again later."),
        "quota" => ("error", "The hub refused: the library's quota is used up."),
        "too-large" => (
            "error",
            "A part or an image of this book is larger than the hub accepts.",
        ),
        "format" => (
            "error",
            "The hub does not accept this library's book format: it needs an update.",
        ),
        "unpaired" => (
            "error",
            "The hub no longer knows this library; see My profile → Phone and reading \
             through the hub.",
        ),
        "protected" => ("error", "Books protected by DRM never leave this library."),
        "failed" => ("error", "Publishing failed; the log has the reason."),
        _ => return None,
    })
}

/// The `hub` code of a failed hub call.
fn failure(e: &ClientError) -> &'static str {
    match e {
        ClientError::Unreachable(_) => "unreachable",
        ClientError::Hub { status: 401, .. } => "unpaired",
        ClientError::Hub { status: 413, .. } => "too-large",
        e if e.code() == Some("quota_exceeded") => "quota",
        _ => "failed",
    }
}

/// `GET /book/{id}/hub`: the progress of the book's upload by this profile, as JSON
/// `{uploading, done, total}` (blobs), for the book card while it publishes.
pub(crate) async fn progress(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
) -> Result<Response, AppError> {
    let id = parse_id(&id)?;
    let key = state
        .db(move |conn| {
            let book = book_by_id(conn, id)?.ok_or(AppError::NotFound)?;
            content_key(&book).ok_or(AppError::NotFound)
        })
        .await?;
    let progress = state.upload_progress(current.id(), &key);
    let (done, total) = progress.unwrap_or((0, 0));
    Ok((
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        axum::Json(serde_json::json!({
            "uploading": progress.is_some(),
            "done": done,
            "total": total,
        })),
    )
        .into_response())
}

/// The form of `POST /book/{id}/hub`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ShelfForm {
    #[serde(default)]
    action: String,
}

/// `POST /book/{id}/hub` (form): `publish` (also again, after expiry or an
/// interrupted upload) or `remove`, `303` back to the book card with a `hub` notice;
/// `export` answers with the book file (or `303` with a notice when it cannot be made).
pub(crate) async fn save(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
    form: Result<Form<ShelfForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Form(form) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let id = parse_id(&id)?;
    let profile = current.id();
    if !current.is_admin() {
        return Err(AppError::Forbidden);
    }
    let library = super::hub::hub_library(&state)
        .await?
        .ok_or_else(|| AppError::BadRequest("the library is not on the hub".to_owned()))?;
    let account = super::hub::hub_account(&state, profile)
        .await?
        .filter(HubAccount::on_hub)
        .ok_or_else(|| AppError::BadRequest("this profile has no hub account".to_owned()))?;
    let code = match form.action.as_str() {
        "export" => match export(&state, library, account, id).await? {
            Ok(file) => return Ok(file),
            Err(code) => code,
        },
        "publish" => publish(&state, library, account, id).await?,
        "pair" if profile.is_owner() => {
            return pair_with_book(&state, &current, library, account, id).await;
        }
        "remove" => remove(&state, library, account, id).await?,
        _ => return Err(AppError::BadRequest("unknown action".to_owned())),
    };
    Ok(Redirect::to(&format!("{}?hub={code}#shelf", state.urls().book(id))).into_response())
}

/// The owner's phone paired with the book open: a pairing secret and the book link
/// that carries it ([`HubUrl::book_pair_link`](crate::hub_client::HubUrl::book_pair_link)).
async fn pair_with_book(
    state: &AppState,
    current: &CurrentProfile,
    library: HubLibrary,
    account: HubAccount,
    id: i64,
) -> Result<Response, AppError> {
    let content = state
        .db(move |conn| {
            let book = book_by_id(conn, id)?.ok_or(AppError::NotFound)?;
            content_key(&book).ok_or(AppError::NotFound)
        })
        .await?;
    let publication = {
        let conn = state.userdata().lock();
        store::shelf_entry(&conn, account.profile, &content)?
            .filter(|e| e.state == ShelfState::Published && e.kind == ShelfKind::Upload)
            .and_then(|e| e.publication)
            .ok_or(AppError::NotFound)?
    };
    let token = library.token.clone();
    let pairing = match call(library.hub_url.clone(), move |c| c.create_pairing(&token)).await? {
        Ok(pairing) => pairing,
        Err(e) => {
            let code = failure(&e);
            return Ok(
                Redirect::to(&format!("{}?hub={code}#shelf", state.urls().book(id)))
                    .into_response(),
            );
        }
    };
    tracing::info!(
        profile = account.profile.0,
        book_id = id,
        "hub pairing link shown"
    );
    let link = library
        .hub_url
        .book_pair_link(&publication, &pairing.secret, &account.key);
    super::hub::pair_page(
        state,
        current,
        crate::web::layout::SidebarMode::default(),
        link,
        Some(super::date_time(pairing.expires_at)),
        state.urls().book(id),
    )
    .await
}

/// The cover of `book` from the cover cache, if any.
fn cover(state: &AppState, book: &BookDetails) -> Option<Image> {
    let mime = book.cover_mime.as_deref().and_then(covers::known_mime)?;
    let data = std::fs::read(covers::path(&state.library().covers_dir(), book.id, mime)).ok()?;
    Some(Image {
        mime: mime.to_owned(),
        data,
    })
}

/// The publication of library book `id` as the reader splits it now; `None` for a
/// DRM-protected book.
async fn build(state: &AppState, id: i64) -> Result<Option<(BookInfo, Publication)>, AppError> {
    state
        .user_db(move |s, conn| {
            let book = book_by_id(conn, id)?.ok_or(AppError::NotFound)?;
            if book.status == BookStatus::Protected {
                return Ok(None);
            }
            let info = BookInfo::of(&book).ok_or(AppError::NotFound)?;
            let content = s.content(&book)?;
            let cover = cover(s, &book);
            let reader = s.config().reader.clone();
            match publication::build(&info, &content, cover.as_ref(), &reader) {
                Ok(publication) => Ok(Some((info, publication))),
                Err(publication::PublishError::Protected(_)) => Ok(None),
            }
        })
        .await
}

async fn publish(
    state: &AppState,
    library: HubLibrary,
    account: HubAccount,
    id: i64,
) -> Result<&'static str, AppError> {
    let profile = account.profile;
    let Some((info, publication)) = build(state, id).await? else {
        return Ok("protected");
    };
    // A second click (or tab) while it uploads, or on a published book, changes nothing.
    let Some(publishing) = state.begin_publishing(profile, &info.content_key) else {
        return Ok("busy");
    };
    let entry = {
        let conn = state.userdata().lock();
        store::shelf_entry(&conn, profile, &info.content_key)?
    };
    if entry.is_some_and(|e| e.state == ShelfState::Published && e.kind == ShelfKind::Upload) {
        return Ok("published");
    }
    let instance = super::hub::instance_name(state);
    let userdata = state.userdata_handle();
    let url = library.hub_url.clone();
    let progress = Arc::clone(&publishing.progress);
    let code = call(url, move |client| {
        Ok(upload(
            &userdata,
            &library,
            &account,
            client,
            &info,
            &publication,
            &instance,
            &progress,
        ))
    })
    .await?
    .unwrap_or("failed");
    if code == "published" {
        tracing::info!(
            profile = profile.0,
            book_id = id,
            "book published to the hub"
        );
    }
    Ok(code)
}

/// Seals and uploads `publication`, puts it on the shelf and pushes its records,
/// counting the blobs sent in `progress`; the `hub` code of the outcome. Runs
/// blocking HTTP.
#[allow(clippy::too_many_arguments)]
fn upload(
    userdata: &crate::annotations::store::UserData,
    library: &HubLibrary,
    account: &HubAccount,
    client: &HubClient,
    info: &BookInfo,
    publication: &Publication,
    instance: &str,
    progress: &crate::web::UploadProgress,
) -> &'static str {
    let profile = account.profile;
    let Some(target) = Target::of(library, account) else {
        return "failed";
    };
    let hub = match client.info() {
        Ok(hub) => hub,
        Err(e) => return failure(&e),
    };
    if !hub.formats.contains(publication.manifest.format_version) {
        return "format";
    }
    let (Ok(pub_id), Ok(key)) = (generate_publication_id(), BookKey::generate()) else {
        return "failed";
    };
    let header = match store::current_library_key(&userdata.lock()) {
        Ok(Some((key_id, library_key))) => {
            keys::key_header(&library.library_id, key_id, &library_key, &pub_id, &key)
        }
        Ok(None) => {
            tracing::error!("the library has no key");
            return "failed";
        }
        Err(e) => {
            tracing::error!(error = %e, "cannot read the library key");
            return "failed";
        }
    };
    let Ok(header) = header else {
        return "failed";
    };
    let Ok(sealed) = publication.seal(&pub_id, &key, &header) else {
        return "failed";
    };
    if sealed
        .iter()
        .any(|(_, blob)| blob.len() as u64 > hub.limits.max_blob_bytes)
    {
        return "too-large";
    }
    let previous = match store::shelf_entry(&userdata.lock(), profile, &info.content_key) {
        Ok(previous) => previous,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the hub shelf");
            return "failed";
        }
    };
    let now = now();
    let entry = ShelfEntry {
        content_key: info.content_key.clone(),
        publication: Some(pub_id),
        book_key: Some(key),
        own: true,
        kind: ShelfKind::Upload,
        state: ShelfState::Uploading,
        instance: instance.to_owned(),
        title: info.title.clone(),
        authors: info.authors.join(", "),
        published_at: Some(now),
        updated_at: now,
    };
    if let Err(e) = store::begin_publish(&userdata.lock(), profile, &entry) {
        tracing::error!(error = %e, "cannot write the hub shelf");
        return "failed";
    }
    // An earlier copy (expired, or an interrupted upload) is not reused; a book on
    // the shelf as files has none on the hub.
    if let Some(old) = previous.as_ref().and_then(|p| {
        p.publication
            .filter(|_| p.own && p.kind == ShelfKind::Upload)
    }) && let Err(e) = client.delete_publication(&library.token, &old)
    {
        tracing::info!(error = %e, "the hub did not delete an earlier copy");
    }
    // The manifest last: a device never sees a publication with missing parts.
    let mut blobs = sealed;
    blobs.rotate_left(1);
    progress.start(blobs.len());
    for (path, blob) in &blobs {
        if let Err(e) = client.put_blob(&library.token, &pub_id, *path, blob) {
            tracing::info!(profile = profile.0, error = %e, "publishing failed");
            if let Err(e) = client.delete_publication(&library.token, &pub_id) {
                tracing::info!(error = %e, "the hub did not delete the partial copy");
            }
            restore(userdata, profile, &info.content_key, previous.as_ref());
            return failure(&e);
        }
        progress.advance();
    }
    {
        let conn = userdata.lock();
        let done = store::set_state(
            &conn,
            profile,
            &info.content_key,
            ShelfState::Published,
            Some(now),
        )
        .and_then(|()| store::enqueue_book(&conn, profile, &info.content_key));
        if let Err(e) = done {
            tracing::error!(error = %e, "cannot write the hub shelf");
            return "failed";
        }
    }
    if let Err(e) = sync::push(userdata, target, account, client) {
        // The worker pushes the rest later.
        tracing::info!(profile = profile.0, error = %e, "push after publishing failed");
    }
    "published"
}

/// Puts the shelf entry back as it was before a failed upload.
fn restore(
    userdata: &crate::annotations::store::UserData,
    profile: ProfileId,
    content_key: &[u8],
    previous: Option<&ShelfEntry>,
) {
    let conn = userdata.lock();
    let result = match previous {
        None => store::delete_entry(&conn, profile, content_key),
        Some(previous) => store::begin_publish(&conn, profile, previous)
            .and_then(|()| store::set_state(&conn, profile, content_key, previous.state, None)),
    };
    if let Err(e) = result {
        tracing::error!(error = %e, "cannot write the hub shelf");
    }
}

/// Exports library book `id` as a book file: a new publication sealed under the
/// current library key, with a snapshot of the profile's records of the book. A book not on the shelf (or being taken
/// off it) goes on it as a `file` item, a `file` item points to the newest file; an
/// uploaded book's shelf item stays as it is. Nothing is sent to the hub here (the
/// shelf item and records go out with the next push).
async fn export(
    state: &AppState,
    library: HubLibrary,
    account: HubAccount,
    id: i64,
) -> Result<Result<Response, &'static str>, AppError> {
    let profile = account.profile;
    let Some((info, publication)) = build(state, id).await? else {
        return Ok(Err("protected"));
    };
    let instance = super::hub::instance_name(state);
    let userdata = state.userdata_handle();
    let made = blocking(move || -> Result<(Vec<u8>, String), AppError> {
        let pub_id = generate_publication_id().map_err(random)?;
        let key = BookKey::generate().map_err(random)?;
        let (key_id, library_key) = store::current_library_key(&userdata.lock())?
            .ok_or_else(|| AppError::Internal("the library has no key".to_owned()))?;
        let header = keys::key_header(&library.library_id, key_id, &library_key, &pub_id, &key)
            .map_err(random)?;
        let sealed = publication.seal(&pub_id, &key, &header).map_err(random)?;
        let snapshot = {
            let conn = userdata.lock();
            let entry = store::shelf_entry(&conn, profile, &info.content_key)?;
            let replace = match &entry {
                None => true,
                Some(e) => e.state == ShelfState::Removing || (e.own && e.kind == ShelfKind::File),
            };
            if replace {
                let now = now();
                store::put_file(
                    &conn,
                    profile,
                    &ShelfEntry {
                        content_key: info.content_key.clone(),
                        publication: Some(pub_id),
                        book_key: Some(key.clone()),
                        own: true,
                        kind: ShelfKind::File,
                        state: ShelfState::Published,
                        instance,
                        title: info.title.clone(),
                        authors: info.authors.join(", "),
                        published_at: Some(now),
                        updated_at: now,
                    },
                )?;
                store::enqueue_book(&conn, profile, &info.content_key)?;
            }
            book_file::snapshot(&conn, profile, &account.key, &info.content_key).map_err(|e| {
                match e {
                    book_file::SnapshotError::Storage(e) => AppError::from(e),
                    book_file::SnapshotError::Random(e) => random(e),
                }
            })?
        };
        let account_id = account
            .account
            .ok_or_else(|| AppError::Internal("the account has no id".to_owned()))?;
        let sg = book_file::sg_json(
            &library.library_id,
            key_id,
            &account_id,
            &pub_id,
            publication.manifest.format_version,
            snapshot,
        );
        let name = book_file::file_name(&info.title, &info.authors.join(", "));
        Ok((book_file::write(&sg, &sealed), name))
    })
    .await??;
    let (file, name) = made;
    tracing::info!(
        profile = profile.0,
        book_id = id,
        bytes = file.len(),
        "book file exported"
    );
    Ok(Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static(publication::format::container::MIME),
            ),
            (header::CONTENT_DISPOSITION, attachment(&name)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        file,
    )
        .into_response()))
}

fn random(e: getrandom::Error) -> AppError {
    AppError::Internal(format!("no randomness: {e}"))
}

/// `Content-Disposition: attachment` with `name`: an ASCII fallback and the UTF-8 name
/// (RFC 6266, RFC 8187).
///
/// The fallback keeps letters, digits and `-_.,()` and a space; everything else
/// becomes `_`.
fn attachment(name: &str) -> HeaderValue {
    let fallback: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " -_.,()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encoded = String::with_capacity(name.len() * 3);
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.!~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    HeaderValue::from_str(&format!(
        "attachment; filename=\"{fallback}\"; filename*=UTF-8''{encoded}"
    ))
    .expect("only visible ASCII")
}

async fn remove(
    state: &AppState,
    library: HubLibrary,
    account: HubAccount,
    id: i64,
) -> Result<&'static str, AppError> {
    let profile = account.profile;
    let key = state
        .db(move |conn| {
            let book = book_by_id(conn, id)?.ok_or(AppError::NotFound)?;
            content_key(&book).ok_or(AppError::NotFound)
        })
        .await?;
    let entry = {
        let conn = state.userdata().lock();
        let entry = store::shelf_entry(&conn, profile, &key)?.ok_or(AppError::NotFound)?;
        store::set_state(&conn, profile, &key, ShelfState::Removing, Some(now()))?;
        store::enqueue_book(&conn, profile, &key)?;
        entry
    };
    let userdata = state.userdata_handle();
    let done = call(library.hub_url.clone(), move |client| {
        Ok(Target::of(&library, &account)
            .is_some_and(|target| sync::take_down(&userdata, target, &account, client, &entry)))
    })
    .await?
    .unwrap_or(false);
    tracing::info!(
        profile = profile.0,
        book_id = id,
        done,
        "book taken off the hub shelf"
    );
    Ok(if done { "removed" } else { "removing" })
}
