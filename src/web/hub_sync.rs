//! The hub sync worker: one Tokio task for the library and all its accounts.
//!
//! Every [`TICK`] it pushes the profiles' queued changes (so a change reaches the hub
//! within about half a minute; a reading position changed on every page turn goes out
//! once per tick), and every `[hub] sync_interval_minutes` it runs a full round: the
//! last pull of any account of 0.18–0.20 ([`sync::pull_legacy`]), the hub's account
//! list (redeemed invites, devices, accounts gone) and the queued removals
//! (`refresh_library`), then a full sync of every account ([`sync::sync_once`]):
//! library keys, push, finish removals, pull and merge, expiry. The library's catalog
//! (its Showcase) is rebuilt and its changes uploaded at a tick after the Showcase, a
//! publication or the index changed (a scan), and at every full round
//! ([`catalog_sync::sync_catalog`]). At every tick it also asks the hub which accounts
//! have new requests (`GET /api/library/pending`), pulls those, and serves the
//! members' requests (`serve_requests`). Everything goes with
//! the library's token. An unreachable hub backs off (doubling, at most an hour).
//! Pulled annotations rewrite the Markdown export of their books. It logs counts and
//! profile ids only.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{AppState, blocking};
use crate::annotations::attach::export_key;
use crate::annotations::now;
use crate::hub_client::store::{self, HubAccount, HubLibrary, Removal};
use crate::hub_client::sync::{self, Report, SyncError, Target};
use crate::hub_client::{HubClient, catalog_sync};
use crate::profiles::ProfileId;
use crate::storage::books::live_books_by_key;

/// How often queued changes are pushed.
pub const TICK: Duration = Duration::from_secs(30);

/// The longest wait after failures.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// Exports the Markdown files of the books whose annotations a pull changed.
fn export_annotated(state: &AppState, report: &Report) {
    if report.annotated.is_empty() {
        return;
    }
    let Ok(index) = state.pool().get() else {
        return;
    };
    for key in &report.annotated {
        let user = state.userdata().lock();
        if let Err(e) = export_key(&index, &user, state.exporter(), key) {
            tracing::error!(error = %e, "cannot update the annotation export");
        }
    }
}

/// The library's book with a content key, for merging.
fn book_finder(state: &AppState) -> impl Fn(&[u8]) -> Option<i64> + use<> {
    let pool = state.pool().clone();
    move |key: &[u8]| {
        let conn = pool.get().ok()?;
        live_books_by_key(&conn, key).ok()?.first().copied()
    }
}

/// The last pull of every account of 0.18–0.20 still waiting for it (before anything
/// else talks to the hub: a 0.21 check would refuse the old hub). Runs blocking HTTP.
pub(crate) fn pull_legacy_accounts(state: &AppState) -> Result<(), SyncError> {
    let legacy = store::legacy_accounts(&state.userdata().lock())?;
    if legacy.is_empty() {
        return Ok(());
    }
    let book_id = book_finder(state);
    let instance = super::routes::hub::instance_name(state);
    let mut report = Report::default();
    let mut failed = None;
    for account in &legacy {
        if let Err(e) =
            sync::pull_legacy(state.userdata(), account, &book_id, &instance, &mut report)
        {
            tracing::info!(profile = account.profile.0, error = %e, "the last pull of an old hub account failed");
            failed = Some(e);
        }
    }
    export_annotated(state, &report);
    failed.map_or(Ok(()), Err)
}

/// Brings the accounts up to date with the hub's account list and carries out the
/// queued removals. Runs blocking HTTP.
pub(crate) fn refresh_library(state: &AppState, library: &HubLibrary) -> Result<(), SyncError> {
    let client = state.hub_client(&library.hub_url);
    // Read first: the lock is taken again for each removal done.
    let removals = store::removals(&state.userdata().lock())?;
    for removal in removals {
        match removal {
            Removal::Account(id) => client.delete_member(&library.token, &id)?,
            Removal::Invite(id) => client.withdraw_invite(&library.token, &id)?,
        }
        store::removal_done(&state.userdata().lock(), removal)?;
    }
    let listed: Vec<_> = client
        .members(&library.token)?
        .into_iter()
        .map(|m| m.listed)
        .collect();
    let changed = store::apply_account_list(&mut state.userdata().lock(), &listed, now())?;
    if !changed.is_empty() {
        tracing::info!(count = changed.len(), "hub accounts joined or left");
    }
    Ok(())
}

/// Runs one full sync of `account` and exports the books whose annotations changed;
/// an account without an id on the hub (a pending invite) or gone has nothing to sync.
pub(crate) async fn sync_account(
    state: &AppState,
    library: HubLibrary,
    account: HubAccount,
) -> Result<Result<Report, SyncError>, super::AppError> {
    let state = state.clone();
    blocking(move || {
        let Some(target) = Target::of(&library, &account) else {
            return Ok(Report::default());
        };
        let client = state.hub_client(&library.hub_url);
        let book_id = book_finder(&state);
        let instance = super::routes::hub::instance_name(&state);
        let report = sync::sync_once(
            state.userdata(),
            target,
            &account,
            &client,
            &book_id,
            &instance,
            now(),
        );
        if let Ok(report) = &report {
            export_annotated(&state, report);
        }
        if let Ok(report) = &report
            && report.pushed + report.merged > 0
        {
            tracing::info!(
                profile = account.profile.0,
                pushed = report.pushed,
                pulled = report.pulled,
                merged = report.merged,
                "synced with the hub"
            );
        }
        report
    })
    .await
}

/// Builds the library's catalog from the Showcase and uploads what changed
/// ([`catalog_sync::sync_catalog`]). Runs blocking HTTP.
pub(crate) fn upload_catalog(
    state: &AppState,
    library: &HubLibrary,
) -> Result<catalog_sync::Outcome, SyncError> {
    let index = state
        .pool()
        .get()
        .map_err(|e| SyncError::Storage(e.into()))?;
    let client = state.hub_client(&library.hub_url);
    catalog_sync::sync_catalog(state.userdata(), &index, library, &client)
}

/// What became of a request.
enum Answer {
    /// Served: the request record is deleted.
    Served,
    /// Refused for this reason: the record is rewritten as refused.
    Refused(&'static str),
    /// Not now (the book is being published, the hub could not be reached): asked
    /// again at the next round.
    Later,
}

/// Serves the members' requests kept from their accounts (`hub_client::requests`):
/// a book on the Showcase *now* (not as a member's catalog says: it may be stale or
/// forged), or one on the member's own shelf whose copy on the hub is gone, is
/// published to the person's shelf, anything else refused; thumbnails of books on the
/// Showcase are made and uploaded. Answers how many were answered.
pub(crate) async fn serve_requests(
    state: &AppState,
    library: &HubLibrary,
) -> Result<usize, super::AppError> {
    use crate::hub_client::requests;
    let mut pending = user(state, |conn| Ok(requests::pending(conn)?)).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    // Covers first: a large upload does not hold them back.
    pending.sort_by_key(|r| r.kind != requests::COVER_REQUEST_KIND);
    let (built, catalog_key) = showcase_built(state).await?;
    let mut answered = 0;
    for request in pending {
        let profile = request.profile;
        let account = user(state, move |conn| Ok(store::account(conn, profile)?))
            .await?
            .filter(HubAccount::on_hub);
        let Some(account) = account else {
            done_request(state, &request).await?;
            continue;
        };
        let (answer, record) = if request.kind == requests::BOOK_REQUEST_KIND {
            match serde_json::from_slice::<requests::BookRequest>(&request.plain) {
                Ok(asked) if asked.state == "pending" => {
                    let answer = serve_book(state, library, &account, &built, &asked).await?;
                    let record = match &answer {
                        Answer::Refused(reason) => Some(asked.refused(reason, now()).encode()),
                        _ => None,
                    };
                    (answer, record)
                }
                // The library's own refusal pulled back, or nothing readable.
                _ => {
                    done_request(state, &request).await?;
                    continue;
                }
            }
        } else {
            let covers = serde_json::from_slice::<requests::CoverRequest>(&request.plain)
                .map(|r| r.covers)
                .unwrap_or_default();
            let answer = match &catalog_key {
                Some(key) => {
                    let (state, library, key, built) =
                        (state.clone(), library.clone(), key.clone(), built.clone());
                    blocking(move || upload_thumbnails(&state, &library, &key, &built, &covers))
                        .await?
                }
                None => Answer::Served,
            };
            (answer, None)
        };
        if matches!(answer, Answer::Later) {
            continue;
        }
        let pushed = {
            let (library, account, request) = (library.clone(), account.clone(), request.clone());
            let client = state.hub_client(&library.hub_url);
            blocking(move || answer_request(&client, &library, &account, &request, record)).await?
        };
        match pushed {
            Ok(()) => {
                done_request(state, &request).await?;
                answered += 1;
            }
            Err(e) => {
                tracing::info!(error = %e, "a request could not be answered; retrying later");
                break;
            }
        }
    }
    if answered > 0 {
        tracing::info!(count = answered, "requests answered");
    }
    Ok(answered)
}

/// What a catalog build was made from: a build is used again only while all of it is
/// the same, so a request is always checked against the Showcase *now* (the marks
/// themselves are compared, besides the epoch every change of a mark moves).
#[derive(Debug, PartialEq, Eq)]
struct BuiltFrom {
    /// The index's scan generation.
    generation: i64,
    /// [`AppState::catalog_epoch`].
    epoch: u64,
    /// The Showcase marks.
    marks: Vec<crate::storage::folders::FolderPath>,
    /// The catalog key (thumbnail ids depend on it), if a catalog was built.
    key: Option<[u8; 32]>,
}

/// The last catalog build of [`showcase_built`] and what it was made from.
pub(crate) struct CachedCatalog {
    from: BuiltFrom,
    built: std::sync::Arc<catalog_sync::Built>,
}

/// The catalog as built from the Showcase now, with the catalog key: the cached
/// build while the index's scan generation, the catalog epoch, the marks and the key
/// are what it was made from, else a new build (kept for the next call).
async fn showcase_built(
    state: &AppState,
) -> Result<
    (
        std::sync::Arc<catalog_sync::Built>,
        Option<crate::hub_client::catalog::CatalogKey>,
    ),
    super::AppError,
> {
    state
        .user_db(|s, index| {
            // The epoch first: a change during the build moves it past the key kept.
            let epoch = s.catalog_epoch();
            let generation = crate::storage::scans::scan_generation(index)?;
            let (marks, key) = {
                let conn = s.userdata().lock();
                (
                    crate::hub_client::showcase::marks(&conn)?,
                    catalog_sync::catalog_key(&conn)?,
                )
            };
            let from = BuiltFrom {
                generation,
                epoch,
                marks,
                key: key.as_ref().map(|k| *k.as_bytes()),
            };
            if let Some(cached) = s.catalog_cache().as_ref()
                && cached.from == from
            {
                return Ok((std::sync::Arc::clone(&cached.built), key));
            }
            let published = catalog_sync::own_publications(&s.userdata().lock())?;
            // Book ids need no key; thumbnail ids are named only once a catalog was
            // built.
            let any = key
                .clone()
                .unwrap_or_else(|| crate::hub_client::catalog::CatalogKey::new([0; 32]));
            let built =
                std::sync::Arc::new(catalog_sync::build(index, &from.marks, &published, &any)?);
            *s.catalog_cache() = Some(CachedCatalog {
                from,
                built: std::sync::Arc::clone(&built),
            });
            Ok((built, key))
        })
        .await
}

/// Serves the requests already pulled, as the worker does at a tick between two full
/// rounds (no expiry check of the shelf first); how many were answered, `None` without
/// a hub library or when a step failed. For tests.
#[doc(hidden)]
pub async fn serve_pulled_requests(state: &AppState) -> Option<usize> {
    let userdata = state.userdata_handle();
    let library = blocking(move || store::library(&userdata.lock()))
        .await
        .ok()?
        .ok()??;
    serve_requests(state, &library).await.ok()
}

/// Runs `f` with the user-data database on a blocking thread.
async fn user<T: Send + 'static>(
    state: &AppState,
    f: impl FnOnce(&rusqlite::Connection) -> Result<T, super::AppError> + Send + 'static,
) -> Result<T, super::AppError> {
    super::routes::profiles::user(state, move |_, conn| f(conn)).await
}

async fn done_request(
    state: &AppState,
    request: &crate::hub_client::requests::Pending,
) -> Result<(), super::AppError> {
    let request = request.clone();
    user(state, move |conn| {
        Ok(crate::hub_client::requests::done(conn, &request)?)
    })
    .await
}

/// A book request: published if the book is on the Showcase now, or if it is on the
/// member's own shelf as an upload whose copy on the hub may be gone (an expired copy,
/// Phase 31.6: the shelf entry is the admin's earlier decision, even for a book in a
/// closed folder). Only the requesting member's shelf counts: another member's book is
/// refused as `closed`. An entry the member's device wrote (a book opened by a link)
/// counts only for a book this library itself sent to the hub, which every member
/// could open; a forged entry names nothing this library sent. A shelf entry still
/// marked published is checked against the hub first and marked expired when its copy
/// is gone, so the book is sent again instead of the request being answered with
/// nothing.
async fn serve_book(
    state: &AppState,
    library: &HubLibrary,
    account: &HubAccount,
    built: &catalog_sync::Built,
    asked: &crate::hub_client::requests::BookRequest,
) -> Result<Answer, super::AppError> {
    use crate::hub_client::store::{ShelfKind, ShelfState};
    let Ok(id) = asked.book.parse::<shelfgauge_core::catalog::BookId>() else {
        return Ok(Answer::Refused("closed"));
    };
    let profile = account.profile;
    let entry = user(state, move |conn| {
        let Some(entry) = store::shelf(conn, profile)?.into_iter().find(|e| {
            e.kind == ShelfKind::Upload
                && matches!(e.state, ShelfState::Published | ShelfState::Expired)
                && shelfgauge_core::catalog::BookId::from_content_key(&e.content_key) == id
        }) else {
            return Ok(None);
        };
        let sent = entry.own
            || conn
                .query_row(
                    "SELECT EXISTS (SELECT 1 FROM hub_shelf
                                WHERE book_key = ?1 AND own = 1 AND kind = 'upload')",
                    [&entry.content_key],
                    |r| r.get::<_, bool>(0),
                )
                .map_err(crate::storage::StorageError::from)?;
        Ok(Some((entry, sent)))
    })
    .await?;
    let book = match (built.books.get(&id).copied(), &entry) {
        (Some(book), _) => book,
        (None, Some((entry, true))) => {
            let key = entry.content_key.clone();
            let found = state
                .db(move |conn| Ok(live_books_by_key(conn, &key)?.first().copied()))
                .await?;
            match found {
                Some(book) => book,
                None => return Ok(Answer::Refused("missing")),
            }
        }
        (None, _) => return Ok(Answer::Refused("closed")),
    };
    if let Some((entry, _)) = entry.filter(|(e, _)| e.state == ShelfState::Published) {
        let listed = {
            let library = library.clone();
            let client = state.hub_client(&library.hub_url);
            blocking(move || client.publications(&library.token)).await?
        };
        let on_hub = match listed {
            Ok(on_hub) => on_hub,
            Err(e) => {
                tracing::info!(error = %e, "the hub's publications could not be read");
                return Ok(Answer::Later);
            }
        };
        if entry.publication.is_some_and(|p| !on_hub.contains(&p)) {
            let key = entry.content_key.clone();
            user(state, move |conn| {
                Ok(store::set_state(
                    conn,
                    profile,
                    &key,
                    ShelfState::Expired,
                    None,
                )?)
            })
            .await?;
        }
    }
    let code = super::routes::shelf::publish(state, library.clone(), account.clone(), book).await;
    Ok(match code {
        Ok("published") => {
            // The catalog entry names the publication.
            state.mark_catalog_dirty();
            Answer::Served
        }
        Ok("busy" | "unreachable" | "slow-down") => Answer::Later,
        Ok("protected") => Answer::Refused("drm"),
        Ok("quota") => Answer::Refused("quota"),
        Ok("too-large") => Answer::Refused("too-large"),
        Ok(_) => Answer::Refused("failed"),
        Err(super::AppError::NotFound) => Answer::Refused("missing"),
        Err(e) => {
            tracing::info!(error = ?e, "a requested book could not be published");
            Answer::Refused("failed")
        }
    })
}

/// Pushes the answer to a request: the refusal `record` (a plaintext), or a deletion.
/// Runs blocking HTTP.
fn answer_request(
    client: &HubClient,
    library: &HubLibrary,
    account: &HubAccount,
    request: &crate::hub_client::requests::Pending,
    record: Option<Vec<u8>>,
) -> Result<(), SyncError> {
    use crate::hub_client::requests::{BOOK_REQUEST_KIND, COVER_REQUEST_KIND};
    let Some(target) = Target::of(library, account) else {
        return Ok(());
    };
    let kind = if request.kind == BOOK_REQUEST_KIND {
        BOOK_REQUEST_KIND
    } else {
        COVER_REQUEST_KIND
    };
    let data = record
        .map(|plain| crate::hub_client::keys::seal_record(&account.key, kind, &request.id, &plain))
        .transpose()
        .map_err(crate::hub_client::ClientError::from)?;
    client.push(
        target.token,
        Some(target.id),
        &[crate::hub_client::OutRecord {
            kind,
            id: request.id.clone(),
            data,
        }],
    )?;
    Ok(())
}

/// The sealed thumbnail `id` of library book `book`, from its cached cover; `None`
/// when the book has no cover file or it makes no thumbnail.
fn seal_thumbnail(
    state: &AppState,
    index: &rusqlite::Connection,
    library: &HubLibrary,
    key: &crate::hub_client::catalog::CatalogKey,
    id: shelfgauge_core::catalog::CoverId,
    book: i64,
) -> Option<Vec<u8>> {
    use crate::cache::covers;
    use shelfgauge_core::catalog::CatalogPath;
    let details = crate::storage::books::book_by_id(index, book).ok()??;
    let mime = details.cover_mime.as_deref().and_then(covers::known_mime)?;
    let data = std::fs::read(covers::path(&state.library().covers_dir(), book, mime)).ok()?;
    let Some(jpeg) = crate::hub_client::requests::thumbnail(&data) else {
        tracing::debug!(book_id = book, "a cover does not make a thumbnail");
        return None;
    };
    let path = CatalogPath::Thumbnail(id);
    crate::hub_client::catalog::seal(&library.library_id, key, path, b"", &jpeg).ok()
}

/// Uploads the sealed thumbnails `blobs` a few at a time, noting each one uploaded;
/// how many went up, or the first failure.
fn put_thumbnails(
    state: &AppState,
    client: &HubClient,
    library: &HubLibrary,
    blobs: &[(shelfgauge_core::catalog::CoverId, Vec<u8>)],
) -> (usize, Result<(), crate::hub_client::ClientError>) {
    use shelfgauge_core::catalog::CatalogPath;
    let made = std::sync::atomic::AtomicUsize::new(0);
    let result = crate::hub_client::upload_at_once(blobs, |(id, blob)| {
        client.put_catalog_blob(&library.token, CatalogPath::Thumbnail(*id), blob)?;
        let noted =
            catalog_sync::cover_uploaded(&state.userdata().lock(), id, blob.len() as u64, now());
        if let Err(e) = noted {
            tracing::error!(error = %e, "cannot note an uploaded thumbnail");
        }
        made.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    });
    (made.into_inner(), result)
}

/// Makes and uploads the thumbnails among `covers` (hex ids) of books on the Showcase
/// that the hub does not have yet; [`Answer::Later`] when the hub cannot be reached.
/// Not bound by the cap of the thumbnails made ahead (the hub's quota bounds both).
/// Runs blocking HTTP.
fn upload_thumbnails(
    state: &AppState,
    library: &HubLibrary,
    key: &crate::hub_client::catalog::CatalogKey,
    built: &catalog_sync::Built,
    covers: &[String],
) -> Answer {
    use crate::hub_client::requests::MAX_COVERS;
    use shelfgauge_core::catalog::CoverId;
    let uploaded = match catalog_sync::uploaded_covers(&state.userdata().lock()) {
        Ok(uploaded) => uploaded,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the uploaded thumbnails");
            return Answer::Later;
        }
    };
    let Ok(index) = state.pool().get() else {
        return Answer::Later;
    };
    let mut blobs = Vec::new();
    for hex in covers.iter().take(MAX_COVERS) {
        let Ok(id) = hex.parse::<CoverId>() else {
            continue;
        };
        let Some(&book) = built.covers.get(&id).filter(|_| !uploaded.contains(&id)) else {
            continue;
        };
        if blobs.iter().any(|(made, _)| *made == id) {
            continue;
        }
        if let Some(blob) = seal_thumbnail(state, &index, library, key, id, book) {
            blobs.push((id, blob));
        }
    }
    drop(index);
    let client = state.hub_client(&library.hub_url);
    let (made, result) = put_thumbnails(state, &client, library, &blobs);
    if made > 0 {
        tracing::info!(count = made, "thumbnails uploaded");
    }
    match result {
        Ok(()) => Answer::Served,
        Err(e) => {
            tracing::info!(error = %e, "a thumbnail could not be uploaded");
            Answer::Later
        }
    }
}

/// The thumbnails made ahead at a tick: far under the token's 600 calls a minute,
/// so a tick stays short.
const AHEAD_BATCH: usize = 100;

/// The most thumbnails on the hub before the pass ahead stops (with those made on
/// request): about 30–40 MB.
pub const AHEAD_MAX_COUNT: u64 = 5000;

/// The share of the library's byte quota (`quotas.max_bytes` of `GET /api/info`)
/// thumbnails take at most before the pass ahead stops, in percent.
pub const AHEAD_MAX_PERCENT: u64 = 5;

/// The pass that uploads thumbnails ahead of the members' requests.
#[derive(Default)]
struct AheadSchedule {
    /// Not before (after failures).
    next_try: Option<Instant>,
    failures: u32,
    /// Stopped by the hub's quota until the next full round.
    stopped: bool,
    /// The library's byte quota from `GET /api/info` (0: none), read at a full round.
    max_bytes: Option<u64>,
    /// The Showcase's thumbnails in the order of the Explore page's first view
    /// (Recent: newest first, then by title), with the build they come from.
    order: Option<(
        std::sync::Arc<catalog_sync::Built>,
        Vec<shelfgauge_core::catalog::CoverId>,
    )>,
    /// Thumbnails of this build that could not be made (no cover file, not an image).
    unmade: std::collections::HashSet<shelfgauge_core::catalog::CoverId>,
}

/// The thumbnails of `built` in the order of the Explore page's first view: Recent,
/// the newest `added` first, then by title (as `Catalog::recent`).
fn recent_covers(built: &catalog_sync::Built) -> Vec<shelfgauge_core::catalog::CoverId> {
    use shelfgauge_core::fold::normalize_key;
    let mut entries: Vec<_> = (built.entries.iter())
        .filter_map(|e| {
            let title = if e.sort_as.is_empty() {
                &e.title
            } else {
                &e.sort_as
            };
            Some((e.added, normalize_key(title), e.id, e.cover?))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| (&a.1, a.2).cmp(&(&b.1, b.2))));
    entries.into_iter().map(|(.., cover)| cover).collect()
}

/// What a pass ahead came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ahead {
    /// This many thumbnails went up.
    Uploaded(usize),
    /// Nothing left to upload, or the cap is reached.
    Done,
    /// The hub refused for the quota: stopped until the next full round.
    Quota,
    /// The hub could not be reached or asked to slow down.
    Later,
}

/// How many more thumbnails, and how many more bytes of them (`None`: no byte cap),
/// the pass ahead may upload with `count` thumbnails of `bytes` on the hub and the
/// byte quota `max_bytes` (0: none): [`AHEAD_MAX_COUNT`] and [`AHEAD_MAX_PERCENT`]
/// of the quota, the lower.
fn ahead_room(count: u64, bytes: u64, max_bytes: u64) -> (u64, Option<u64>) {
    let byte_cap = (max_bytes > 0).then(|| max_bytes / 100 * AHEAD_MAX_PERCENT);
    (
        AHEAD_MAX_COUNT.saturating_sub(count),
        byte_cap.map(|cap| cap.saturating_sub(bytes)),
    )
}

/// One batch of the pass ahead: up to [`AHEAD_BATCH`] thumbnails of the Showcase
/// that the hub does not have, in [`recent_covers`] order, within the cap
/// ([`AHEAD_MAX_COUNT`], [`AHEAD_MAX_PERCENT`] of `max_bytes`). Runs blocking HTTP.
fn upload_ahead(
    state: &AppState,
    library: &HubLibrary,
    key: &crate::hub_client::catalog::CatalogKey,
    built: &catalog_sync::Built,
    order: &[shelfgauge_core::catalog::CoverId],
    max_bytes: u64,
    unmade: &mut std::collections::HashSet<shelfgauge_core::catalog::CoverId>,
) -> Ahead {
    let read = {
        let conn = state.userdata().lock();
        catalog_sync::uploaded_covers(&conn)
            .and_then(|up| Ok((up, catalog_sync::uploaded_cover_usage(&conn)?)))
    };
    let (uploaded, (count, bytes)) = match read {
        Ok(read) => read,
        Err(e) => {
            tracing::error!(error = %e, "cannot read the uploaded thumbnails");
            return Ahead::Later;
        }
    };
    let (mut room_count, mut room_bytes) = ahead_room(count, bytes, max_bytes);
    let Ok(index) = state.pool().get() else {
        return Ahead::Later;
    };
    let mut blobs = Vec::new();
    for id in order {
        if blobs.len() >= AHEAD_BATCH || room_count == 0 || room_bytes == Some(0) {
            break;
        }
        if uploaded.contains(id) || unmade.contains(id) {
            continue;
        }
        let Some(&book) = built.covers.get(id) else {
            continue;
        };
        let Some(blob) = seal_thumbnail(state, &index, library, key, *id, book) else {
            unmade.insert(*id);
            continue;
        };
        if let Some(room) = room_bytes.as_mut() {
            if (blob.len() as u64) > *room {
                break;
            }
            *room -= blob.len() as u64;
        }
        room_count -= 1;
        blobs.push((*id, blob));
    }
    drop(index);
    if blobs.is_empty() {
        return Ahead::Done;
    }
    let client = state.hub_client(&library.hub_url);
    let (made, result) = put_thumbnails(state, &client, library, &blobs);
    if made > 0 {
        tracing::info!(count = made, "thumbnails uploaded ahead");
    }
    match result {
        Ok(()) => Ahead::Uploaded(made),
        Err(e) if matches!(e.code(), Some("quota_exceeded" | "hub_full")) => {
            tracing::info!(
                "the hub has no room for more thumbnails; stopped until the next full round"
            );
            Ahead::Quota
        }
        Err(e) => {
            tracing::info!(error = %e, "thumbnails could not be uploaded ahead; retrying later");
            Ahead::Later
        }
    }
}

/// The pass ahead at a tick (after the catalog and the requests): a batch while
/// thumbnails are missing on the hub; a full round (`due`) lifts a stop for the quota
/// and reads the quota again.
async fn thumbnails_ahead(
    state: &AppState,
    library: &HubLibrary,
    due: bool,
    schedule: &mut AheadSchedule,
) -> Result<Option<Ahead>, super::AppError> {
    if due {
        schedule.stopped = false;
        schedule.max_bytes = None;
    }
    if schedule.stopped || schedule.next_try.is_some_and(|at| Instant::now() < at) {
        return Ok(None);
    }
    let max_bytes = match schedule.max_bytes {
        Some(max) => max,
        None => {
            let client = state.hub_client(&library.hub_url);
            match blocking(move || client.info()).await? {
                Ok(info) => {
                    let max = info.quotas.map_or(0, |q| q.max_bytes);
                    schedule.max_bytes = Some(max);
                    max
                }
                Err(e) => {
                    schedule.failures += 1;
                    schedule.next_try =
                        Some(Instant::now() + backoff(&e.into(), TICK, schedule.failures));
                    return Ok(Some(Ahead::Later));
                }
            }
        }
    };
    let (built, Some(key)) = showcase_built(state).await? else {
        return Ok(None);
    };
    let order = match &schedule.order {
        Some((from, order)) if std::sync::Arc::ptr_eq(from, &built) => order.clone(),
        _ => {
            let order = recent_covers(&built);
            schedule.order = Some((std::sync::Arc::clone(&built), order.clone()));
            schedule.unmade.clear();
            order
        }
    };
    let mut unmade = std::mem::take(&mut schedule.unmade);
    let (state2, library2) = (state.clone(), library.clone());
    let (outcome, unmade) = blocking(move || {
        let outcome = upload_ahead(
            &state2,
            &library2,
            &key,
            &built,
            &order,
            max_bytes,
            &mut unmade,
        );
        (outcome, unmade)
    })
    .await?;
    schedule.unmade = unmade;
    match outcome {
        Ahead::Uploaded(_) | Ahead::Done => schedule.failures = 0,
        Ahead::Quota => schedule.stopped = true,
        Ahead::Later => {
            schedule.failures += 1;
            let wait =
                (TICK * 2u32.saturating_pow(schedule.failures.saturating_sub(1))).min(MAX_BACKOFF);
            schedule.next_try = Some(Instant::now() + wait);
        }
    }
    Ok(Some(outcome))
}

/// One pass ahead as the worker makes it at a tick (with a schedule of its own: no
/// stop or backoff from earlier calls); `None` without a hub library or a catalog.
/// For tests.
#[doc(hidden)]
pub async fn thumbnails_ahead_once(state: &AppState) -> Option<Ahead> {
    let userdata = state.userdata_handle();
    let library = blocking(move || store::library(&userdata.lock()))
        .await
        .ok()?
        .ok()??;
    let mut schedule = AheadSchedule::default();
    thumbnails_ahead(state, &library, true, &mut schedule)
        .await
        .ok()
        .flatten()
}

/// The cheap check at every tick (`GET /api/library/pending`): pulls the accounts whose
/// request records moved past their sync cursor (not those waiting after failures),
/// so a request is seen within a tick. A hub without the endpoint is left alone.
async fn pull_requests(
    state: &AppState,
    library: &HubLibrary,
    skip: &[ProfileId],
) -> Result<(), super::AppError> {
    let pending = {
        let library = library.clone();
        let online = state.config().hub.show_online;
        let client = state.hub_client(&library.hub_url);
        blocking(move || client.pending_requests(&library.token, online)).await?
    };
    let pending = match pending {
        Ok(pending) => pending,
        Err(e) => {
            tracing::debug!(error = %e, "the hub's pending requests could not be read");
            return Ok(());
        }
    };
    if pending.is_empty() {
        return Ok(());
    }
    let skip = skip.to_vec();
    let due = user(state, move |conn| due_accounts(conn, &pending, &skip)).await?;
    for account in due {
        let (state, library) = (state.clone(), library.clone());
        let pulled = blocking(move || {
            let Some(target) = Target::of(&library, &account) else {
                return Ok(Report::default());
            };
            let client = state.hub_client(&library.hub_url);
            let book_id = book_finder(&state);
            let instance = super::routes::hub::instance_name(&state);
            let mut report = Report::default();
            sync::pull(
                state.userdata(),
                target,
                &account,
                &client,
                &book_id,
                &instance,
                &mut report,
            )?;
            export_annotated(&state, &report);
            Ok::<_, SyncError>(report)
        })
        .await?;
        if let Err(e) = pulled {
            tracing::info!(error = %e, "pulling an account's requests failed");
        }
    }
    Ok(())
}

/// The accounts on the hub whose request records moved past their sync cursor in
/// `pending` (the hub's answer), except those in `skip`.
fn due_accounts(
    conn: &rusqlite::Connection,
    pending: &[(crate::hub::AccountId, i64)],
    skip: &[ProfileId],
) -> Result<Vec<HubAccount>, super::AppError> {
    let mut due = Vec::new();
    for account in store::accounts(conn)?
        .into_iter()
        .filter(HubAccount::on_hub)
    {
        let ahead = account.account.as_ref().is_some_and(|id| {
            pending
                .iter()
                .any(|(p, seq)| p == id && *seq > account.last_seq)
        });
        if ahead && !skip.contains(&account.profile) {
            due.push(account);
        }
    }
    Ok(due)
}

/// How long the watcher waits before its next check of a hub without held calls
/// (older than 0.25): the worker's check at every tick does it all meanwhile, and the
/// hub may be updated.
const UNHELD_RECHECK: Duration = Duration::from_secs(600);

/// The watcher's pause before its next held check, after one that took `took` with
/// the hub holding up to `wait` seconds: none after the first check (no mark to pass
/// yet) or one whose mark moved; a check answered at once although nothing moved (the
/// hub's cap on held calls) waits a tick, so the watcher never spins.
fn pause_after(wait: u32, took: Duration, first: bool, moved: bool) -> Duration {
    if first || moved || took >= Duration::from_secs(u64::from(wait)) / 2 {
        Duration::ZERO
    } else {
        TICK
    }
}

/// The watcher: holds `GET /api/library/pending` on a hub that names `wait_max`
/// (0.25 and later) in a loop and wakes the worker (`wake`) as soon as an account's
/// request records moved past its sync cursor, so a member's request is served within
/// seconds instead of at the next tick. Serving stays in the worker (no two rounds at
/// once). Without a hub library, or with an older hub, it only looks again now and
/// then; an unreachable hub backs off like the worker.
async fn watch_requests(state: AppState, wake: std::sync::Arc<tokio::sync::Notify>) {
    let mut hub: Option<(crate::hub_client::HubUrl, u32)> = None;
    let mut seen: Option<String> = None;
    let mut failures = 0u32;
    loop {
        let userdata = state.userdata_handle();
        let library = match blocking(move || store::library(&userdata.lock())).await {
            Ok(Ok(Some(library))) => library,
            _ => {
                hub = None;
                tokio::time::sleep(TICK).await;
                continue;
            }
        };
        let client = state.hub_client(&library.hub_url);
        let wait = match &hub {
            Some((url, wait)) if *url == library.hub_url => *wait,
            _ => {
                seen = None;
                let info = {
                    let client = client.clone();
                    blocking(move || client.info()).await
                };
                match info {
                    Ok(Ok(info)) => {
                        failures = 0;
                        let wait = info.wait_max.unwrap_or(0).min(crate::hub::WAIT_MAX);
                        hub = Some((library.hub_url.clone(), wait));
                        wait
                    }
                    Ok(Err(e)) => {
                        failures += 1;
                        tokio::time::sleep(backoff(&e.into(), TICK, failures)).await;
                        continue;
                    }
                    Err(_) => {
                        tokio::time::sleep(TICK).await;
                        continue;
                    }
                }
            }
        };
        if wait == 0 {
            tokio::time::sleep(UNHELD_RECHECK).await;
            hub = None;
            continue;
        }
        let online = state.config().hub.show_online;
        let started = Instant::now();
        let checked = {
            let (seen, token) = (seen.clone(), library.token.clone());
            blocking(move || client.pending_held(&token, online, seen.as_deref(), wait)).await
        };
        match checked {
            Ok(Ok(answer)) => {
                failures = 0;
                let Some(mark) = answer.mark else {
                    // A hub that names `wait_max` but marks nothing: no holding.
                    tokio::time::sleep(UNHELD_RECHECK).await;
                    hub = None;
                    continue;
                };
                let first = seen.is_none();
                let moved = seen.as_deref() != Some(mark.as_str());
                seen = Some(mark);
                if moved {
                    let pending = answer.accounts;
                    let due = user(&state, move |conn| due_accounts(conn, &pending, &[])).await;
                    if due.is_ok_and(|due| !due.is_empty()) {
                        wake.notify_one();
                    }
                }
                let pause = pause_after(wait, started.elapsed(), first, moved);
                if !pause.is_zero() {
                    tokio::time::sleep(pause).await;
                }
            }
            Ok(Err(e)) => {
                failures += 1;
                // The hub may have changed (an update, another one at the address).
                hub = None;
                tracing::debug!(error = %e, "the held check of the hub failed");
                tokio::time::sleep(backoff(&e.into(), TICK, failures)).await;
            }
            Err(_) => tokio::time::sleep(TICK).await,
        }
    }
}

/// Aborts its task when dropped (the watcher goes with the worker).
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// When the catalog is built and uploaded next.
struct CatalogSchedule {
    /// Not before (after failures).
    next_try: Instant,
    failures: u32,
    /// The index's scan generation the last upload was built from.
    scan_generation: i64,
    /// Whether the hub's lack of catalogs was logged.
    told_unsupported: bool,
    /// Whether the hub keeps catalogs (and so answers the pending check), as the last
    /// upload found.
    supported: bool,
}

/// Rebuilds and uploads the catalog when something changed: the Showcase or a
/// publication (`AppState::mark_catalog_dirty`), a scan, or the full round (`due`).
async fn catalog_round(
    state: &AppState,
    library: &HubLibrary,
    due: bool,
    schedule: &mut CatalogSchedule,
) -> Result<(), super::AppError> {
    let generation = state
        .db(|conn| Ok(crate::storage::scans::scan_generation(conn)?))
        .await?;
    let dirty = state.take_catalog_dirty() | (generation != schedule.scan_generation) | due;
    if !dirty {
        return Ok(());
    }
    let now_instant = Instant::now();
    if now_instant < schedule.next_try {
        state.mark_catalog_dirty();
        return Ok(());
    }
    let result = {
        let (state, library) = (state.clone(), library.clone());
        blocking(move || upload_catalog(&state, &library)).await?
    };
    match result {
        Ok(outcome) => {
            schedule.failures = 0;
            schedule.scan_generation = generation;
            schedule.supported = outcome != catalog_sync::Outcome::Unsupported;
            if outcome == catalog_sync::Outcome::Unsupported && !schedule.told_unsupported {
                schedule.told_unsupported = true;
                tracing::info!("the hub keeps no catalogs (it is older than shelfgauge 0.22)");
            }
        }
        Err(e) => {
            schedule.failures += 1;
            schedule.next_try = now_instant + backoff(&e, TICK, schedule.failures);
            state.mark_catalog_dirty();
            tracing::info!(failures = schedule.failures, error = %e, "the catalog upload failed; retrying later");
        }
    }
    Ok(())
}

/// When a profile syncs next.
struct Schedule {
    next_full: Instant,
    failures: u32,
}

/// Starts the worker on the current runtime, with its watcher of the hub's held
/// check (stopped with the worker). The worker runs a round at every [`TICK`] and when
/// the watcher wakes it.
pub fn start(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let wake = std::sync::Arc::new(tokio::sync::Notify::new());
        let _watcher = AbortOnDrop(tokio::spawn(watch_requests(
            state.clone(),
            std::sync::Arc::clone(&wake),
        )));
        let mut schedules: HashMap<ProfileId, Schedule> = HashMap::new();
        let mut library_schedule = Schedule {
            next_full: Instant::now(),
            failures: 0,
        };
        let mut ahead_schedule = AheadSchedule::default();
        let mut catalog_schedule = CatalogSchedule {
            next_try: Instant::now(),
            failures: 0,
            scan_generation: -1,
            told_unsupported: false,
            supported: false,
        };
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            // A wake serves requests only; the thumbnails ahead wait for the tick, so
            // a burst of requests does not spend the token's rate on them.
            let ticked = tokio::select! {
                _ = tick.tick() => true,
                () = wake.notified() => {
                    tick.reset();
                    false
                }
            };
            if let Err(e) = round(
                &state,
                &mut library_schedule,
                &mut catalog_schedule,
                ticked.then_some(&mut ahead_schedule),
                &mut schedules,
            )
            .await
            {
                tracing::error!(error = ?e, "the hub sync failed");
            }
        }
    })
}

/// The wait after `failures` failed rounds.
fn backoff(error: &SyncError, interval: Duration, failures: u32) -> Duration {
    match error {
        // A token the hub no longer knows stays unknown: check rarely.
        SyncError::Client(e) if e.is_unauthorized() => MAX_BACKOFF,
        SyncError::Client(_) => {
            (interval * 2u32.saturating_pow(failures.saturating_sub(1))).min(MAX_BACKOFF)
        }
        SyncError::Storage(_) => interval,
    }
}

async fn round(
    state: &AppState,
    library_schedule: &mut Schedule,
    catalog_schedule: &mut CatalogSchedule,
    ahead_schedule: Option<&mut AheadSchedule>,
    schedules: &mut HashMap<ProfileId, Schedule>,
) -> Result<(), super::AppError> {
    let interval =
        Duration::from_secs(u64::from(state.config().hub.sync_interval_minutes.max(1)) * 60);
    let now_instant = Instant::now();
    let library_due = now_instant >= library_schedule.next_full;
    let userdata = state.userdata_handle();
    let library = blocking(move || store::library(&userdata.lock())).await??;
    if library_due {
        let result = {
            let state = state.clone();
            let library = library.clone();
            blocking(move || {
                pull_legacy_accounts(&state)?;
                match &library {
                    Some(library) => refresh_library(&state, library),
                    None => Ok(()),
                }
            })
            .await?
        };
        match result {
            Ok(()) => {
                library_schedule.failures = 0;
                library_schedule.next_full = now_instant + interval;
            }
            Err(e) => {
                library_schedule.failures += 1;
                library_schedule.next_full =
                    now_instant + backoff(&e, interval, library_schedule.failures);
                tracing::info!(failures = library_schedule.failures, error = %e, "the hub library round failed; retrying later");
            }
        }
    }
    let Some(library) = library else {
        schedules.clear();
        return Ok(());
    };
    catalog_round(state, &library, library_due, catalog_schedule).await?;
    if catalog_schedule.supported {
        let waiting: Vec<ProfileId> = schedules
            .iter()
            .filter(|(_, s)| s.failures > 0)
            .map(|(p, _)| *p)
            .collect();
        pull_requests(state, &library, &waiting).await?;
    }
    let userdata = state.userdata_handle();
    let accounts = blocking(move || {
        let conn = userdata.lock();
        let accounts = store::accounts(&conn)?;
        accounts
            .into_iter()
            .filter(HubAccount::on_hub)
            .map(|a| Ok((store::outbox_len(&conn, a.profile)?, a)))
            .collect::<Result<Vec<_>, crate::storage::StorageError>>()
    })
    .await??;
    schedules.retain(|p, _| accounts.iter().any(|(_, a)| a.profile == *p));
    for (queued, account) in accounts {
        let profile = account.profile;
        let now = Instant::now();
        let schedule = schedules.entry(profile).or_insert(Schedule {
            next_full: now,
            failures: 0,
        });
        let full = now >= schedule.next_full;
        if !full && (queued == 0 || schedule.failures > 0) {
            continue;
        }
        let result = if full {
            sync_account(state, library.clone(), account)
                .await?
                .map(|_| ())
        } else {
            let userdata = state.userdata_handle();
            let library = library.clone();
            blocking(move || {
                let Some(target) = Target::of(&library, &account) else {
                    return Ok(());
                };
                let client = HubClient::new(library.hub_url.clone());
                sync::push(&userdata, target, &account, &client).map(|_| ())
            })
            .await?
        };
        match result {
            Ok(()) => {
                if full {
                    schedule.next_full = now + interval;
                }
                schedule.failures = 0;
            }
            Err(e) => {
                schedule.failures += 1;
                schedule.next_full = now + backoff(&e, interval, schedule.failures);
                tracing::info!(
                    profile = profile.0,
                    failures = schedule.failures,
                    error = %e,
                    "the hub sync failed; retrying later"
                );
            }
        }
    }
    // Requests pulled by the check above or by a full sync, then the thumbnails
    // ahead of requests.
    serve_requests(state, &library).await?;
    if let Some(ahead) = ahead_schedule.filter(|_| catalog_schedule.supported) {
        thumbnails_ahead(state, &library, library_due, ahead).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_watcher_never_spins() {
        let (wait, quick, held) = (25, Duration::from_millis(30), Duration::from_secs(25));
        // The first check carries no mark: the next one goes at once.
        assert_eq!(pause_after(wait, quick, true, false), Duration::ZERO);
        // Something moved: at once.
        assert_eq!(pause_after(wait, quick, false, true), Duration::ZERO);
        // Held to the end: at once.
        assert_eq!(pause_after(wait, held, false, false), Duration::ZERO);
        // Answered at once with nothing new (over the hub's cap): a tick.
        assert_eq!(pause_after(wait, quick, false, false), TICK);
    }

    #[test]
    fn thumbnails_ahead_keep_to_the_lower_cap() {
        // No quota: the count only.
        assert_eq!(ahead_room(0, 0, 0), (AHEAD_MAX_COUNT, None));
        assert_eq!(ahead_room(AHEAD_MAX_COUNT, 0, 0).0, 0);
        assert_eq!(ahead_room(AHEAD_MAX_COUNT + 7, 0, 0).0, 0);
        // 5 % of a 1 GiB quota, less what is up already.
        let gib = 1 << 30;
        assert_eq!(
            ahead_room(10, 1000, gib),
            (AHEAD_MAX_COUNT - 10, Some(gib / 100 * 5 - 1000))
        );
        assert_eq!(ahead_room(10, gib, gib).1, Some(0));
    }
}
