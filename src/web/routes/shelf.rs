//! A book on the profile's hub shelf (concept §11): "Open on phone" on the book card
//! publishes it (build, seal, upload, push the shelf item with the book's annotations
//! and position), "Remove from shelf" deletes the publication and the book's records
//! on the hub (local data stays), an expired publication can be published again.
//!
//! Publication ids and book keys are never logged; only profile and book ids.

use axum::extract::rejection::FormRejection;
use axum::extract::{Form, Path, State};
use axum::response::{IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::hub::call;
use super::parse_id;
use crate::annotations::now;
use crate::cache::covers;
use crate::format::model::Image;
use crate::hub_client::store::{self, HubAccount, ShelfEntry, ShelfState};
use crate::hub_client::{ClientError, HubClient, sync};
use crate::profiles::ProfileId;
use crate::publication::{self, BookInfo, BookKey, Publication, generate_publication_id};
use crate::render::load::content_key;
use crate::storage::books::{BookDetails, BookStatus, book_by_id};
use crate::web::profile::CurrentProfile;
use crate::web::{AppError, AppState, qr};

/// The book's place on the shelf, for the book card.
pub(crate) struct ShelfView {
    /// `none`, `uploading`, `published`, `expired`, `removing` or `protected`.
    pub(crate) state: &'static str,
    /// The library that published it, when it is another one.
    pub(crate) other: Option<String>,
    /// The book link and its QR code, when it is published.
    pub(crate) link: Option<(String, String)>,
    pub(crate) action: String,
}

/// What the book card shows about the shelf: `None` without a hub account.
pub(crate) fn view(
    state: &AppState,
    profile: ProfileId,
    book: &BookDetails,
    readable: bool,
) -> Result<Option<ShelfView>, AppError> {
    let Some(key) = content_key(book) else {
        return Ok(None);
    };
    let (account, entry) = {
        let conn = state.userdata().lock();
        let Some(account) = store::account(&conn, profile)? else {
            return Ok(None);
        };
        (account, store::shelf_entry(&conn, profile, &key)?)
    };
    let action = state.urls().book_hub(book.id);
    let (state_name, other, link) = match &entry {
        None if book.status == BookStatus::Protected => ("protected", None, None),
        None if !readable => return Ok(None),
        None => ("none", None, None),
        Some(entry) => {
            let link = match (entry.state, entry.publication) {
                (ShelfState::Published, Some(id)) => {
                    let link = account.hub_url.book_link(&id);
                    qr::svg(&link, "QR code of the book link").map(|svg| (link, svg))
                }
                _ => None,
            };
            let other = (!entry.own).then(|| entry.instance.clone());
            let name = match entry.state {
                ShelfState::Uploading => "uploading",
                ShelfState::Published => "published",
                ShelfState::Expired => "expired",
                ShelfState::Removing => "removing",
            };
            (name, other, link)
        }
    };
    Ok(Some(ShelfView {
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
            "The book is on your shelf: open it on a paired phone or other device.",
        ),
        "removed" => (
            "ok",
            "The book is off your shelf: the hub's copy and its marks there are deleted. \
             Your marks in this library stay.",
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
        "quota" => ("error", "The hub refused: your account's quota is used up."),
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
            "This library is no longer paired with your hub account; see My profile → \
             Phone and other devices.",
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

/// The form of `POST /book/{id}/hub`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ShelfForm {
    #[serde(default)]
    action: String,
}

/// `POST /book/{id}/hub` (form): `publish` (also again, after expiry or an
/// interrupted upload) or `remove`; `303` back to the book card with a `hub` notice.
pub(crate) async fn save(
    State(state): State<AppState>,
    current: CurrentProfile,
    Path(id): Path<String>,
    form: Result<Form<ShelfForm>, FormRejection>,
) -> Result<Response, AppError> {
    let Form(form) = form.map_err(|e| AppError::BadRequest(e.body_text()))?;
    let id = parse_id(&id)?;
    let profile = current.id();
    let account = super::hub::hub_account(&state, profile)
        .await?
        .ok_or_else(|| AppError::BadRequest("this profile has no hub account".to_owned()))?;
    let code = match form.action.as_str() {
        "publish" => publish(&state, account, id).await?,
        "remove" => remove(&state, account, id).await?,
        _ => return Err(AppError::BadRequest("unknown action".to_owned())),
    };
    Ok(Redirect::to(&format!("{}?hub={code}#shelf", state.urls().book(id))).into_response())
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

async fn publish(state: &AppState, account: HubAccount, id: i64) -> Result<&'static str, AppError> {
    let profile = account.profile;
    let built = state
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
        .await?;
    let Some((info, publication)) = built else {
        return Ok("protected");
    };
    // A second click (or tab) while it uploads, or on a published book, changes nothing.
    let Some(_publishing) = state.begin_publishing(profile, &info.content_key) else {
        return Ok("busy");
    };
    let entry = {
        let conn = state.userdata().lock();
        store::shelf_entry(&conn, profile, &info.content_key)?
    };
    if entry.is_some_and(|e| e.state == ShelfState::Published) {
        return Ok("published");
    }
    let instance = super::hub::instance_name(state);
    let userdata = state.userdata_handle();
    let url = account.hub_url.clone();
    let code = call(url, move |client| {
        Ok(upload(
            &userdata,
            &account,
            client,
            &info,
            &publication,
            &instance,
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

/// Seals and uploads `publication`, puts it on the shelf and pushes its records; the
/// `hub` code of the outcome. Runs blocking HTTP.
fn upload(
    userdata: &crate::annotations::store::UserData,
    account: &HubAccount,
    client: &HubClient,
    info: &BookInfo,
    publication: &Publication,
    instance: &str,
) -> &'static str {
    let profile = account.profile;
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
    let Ok(sealed) = publication.seal(&pub_id, &key) else {
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
    // An earlier copy (expired, or an interrupted upload) is not reused.
    if let Some(old) = previous
        .as_ref()
        .and_then(|p| p.publication.filter(|_| p.own))
        && let Err(e) = client.delete_publication(&account.token, &old)
    {
        tracing::info!(error = %e, "the hub did not delete an earlier copy");
    }
    // The manifest last: a device never sees a publication with missing parts.
    let mut blobs = sealed;
    blobs.rotate_left(1);
    for (path, blob) in &blobs {
        if let Err(e) = client.put_blob(&account.token, &pub_id, *path, blob) {
            tracing::info!(profile = profile.0, error = %e, "publishing failed");
            if let Err(e) = client.delete_publication(&account.token, &pub_id) {
                tracing::info!(error = %e, "the hub did not delete the partial copy");
            }
            restore(userdata, profile, &info.content_key, previous.as_ref());
            return failure(&e);
        }
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
    if let Err(e) = sync::push(userdata, account, client) {
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

async fn remove(state: &AppState, account: HubAccount, id: i64) -> Result<&'static str, AppError> {
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
    let done = call(account.hub_url.clone(), move |client| {
        Ok(sync::take_down(&userdata, &account, client, &entry))
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
