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
    let client = HubClient::new(library.hub_url.clone());
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
        let client = HubClient::new(library.hub_url.clone());
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
    let client = HubClient::new(library.hub_url.clone());
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
/// forged) is published to the person's shelf, anything else refused; thumbnails of
/// books on the Showcase are made and uploaded. Answers how many were answered.
pub(crate) async fn serve_requests(
    state: &AppState,
    library: &HubLibrary,
) -> Result<usize, super::AppError> {
    use crate::hub_client::requests;
    let pending = user(state, |conn| Ok(requests::pending(conn)?)).await?;
    if pending.is_empty() {
        return Ok(0);
    }
    let (built, catalog_key) = state
        .user_db(|s, index| {
            let (marks, published, key) = {
                let conn = s.userdata().lock();
                (
                    crate::hub_client::showcase::marks(&conn)?,
                    catalog_sync::own_publications(&conn)?,
                    catalog_sync::catalog_key(&conn)?,
                )
            };
            // Book ids need no key; thumbnail ids are named only once a catalog was
            // built.
            let any = key
                .clone()
                .unwrap_or_else(|| crate::hub_client::catalog::CatalogKey::new([0; 32]));
            Ok((catalog_sync::build(index, &marks, &published, &any)?, key))
        })
        .await?;
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
            blocking(move || answer_request(&library, &account, &request, record)).await?
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

/// A book request: published if the book is on the Showcase now.
async fn serve_book(
    state: &AppState,
    library: &HubLibrary,
    account: &HubAccount,
    built: &catalog_sync::Built,
    asked: &crate::hub_client::requests::BookRequest,
) -> Result<Answer, super::AppError> {
    let book = asked
        .book
        .parse::<shelfgauge_core::catalog::BookId>()
        .ok()
        .and_then(|id| built.books.get(&id).copied());
    let Some(book) = book else {
        return Ok(Answer::Refused("closed"));
    };
    let code = super::routes::shelf::publish(state, library.clone(), account.clone(), book).await;
    Ok(match code {
        Ok("published") => {
            // The catalog entry names the publication.
            state.mark_catalog_dirty();
            Answer::Served
        }
        Ok("busy" | "unreachable") => Answer::Later,
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
    HubClient::new(library.hub_url.clone()).push(
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

/// Makes and uploads the thumbnails among `covers` (hex ids) of books on the Showcase
/// that the hub does not have yet; [`Answer::Later`] when the hub cannot be reached.
/// Runs blocking HTTP.
fn upload_thumbnails(
    state: &AppState,
    library: &HubLibrary,
    key: &crate::hub_client::catalog::CatalogKey,
    built: &catalog_sync::Built,
    covers: &[String],
) -> Answer {
    use crate::cache::covers;
    use crate::hub_client::requests::{MAX_COVERS, thumbnail};
    use shelfgauge_core::catalog::{CatalogPath, CoverId};
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
    let client = HubClient::new(library.hub_url.clone());
    let mut made = 0;
    for hex in covers.iter().take(MAX_COVERS) {
        let Ok(id) = hex.parse::<CoverId>() else {
            continue;
        };
        let Some(&book) = built.covers.get(&id).filter(|_| !uploaded.contains(&id)) else {
            continue;
        };
        let Ok(Some(details)) = crate::storage::books::book_by_id(&index, book) else {
            continue;
        };
        let Some(mime) = details.cover_mime.as_deref().and_then(covers::known_mime) else {
            continue;
        };
        let Ok(data) = std::fs::read(covers::path(&state.library().covers_dir(), book, mime))
        else {
            continue;
        };
        let Some(jpeg) = thumbnail(&data) else {
            tracing::debug!(book_id = book, "a cover does not make a thumbnail");
            continue;
        };
        let path = CatalogPath::Thumbnail(id);
        let Ok(blob) = crate::hub_client::catalog::seal(&library.library_id, key, path, b"", &jpeg)
        else {
            return Answer::Later;
        };
        if let Err(e) = client.put_catalog_blob(&library.token, path, &blob) {
            tracing::info!(error = %e, "a thumbnail could not be uploaded");
            return Answer::Later;
        }
        if let Err(e) = catalog_sync::cover_uploaded(&state.userdata().lock(), &id, now()) {
            tracing::error!(error = %e, "cannot note an uploaded thumbnail");
        }
        made += 1;
    }
    if made > 0 {
        tracing::info!(count = made, "thumbnails uploaded");
    }
    Answer::Served
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
        blocking(move || {
            HubClient::new(library.hub_url.clone()).pending_requests(&library.token, online)
        })
        .await?
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
    let due = user(state, move |conn| {
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
    })
    .await?;
    for account in due {
        let (state, library) = (state.clone(), library.clone());
        let pulled = blocking(move || {
            let Some(target) = Target::of(&library, &account) else {
                return Ok(Report::default());
            };
            let client = HubClient::new(library.hub_url.clone());
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

/// Starts the worker on the current runtime.
pub fn start(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut schedules: HashMap<ProfileId, Schedule> = HashMap::new();
        let mut library_schedule = Schedule {
            next_full: Instant::now(),
            failures: 0,
        };
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
            tick.tick().await;
            if let Err(e) = round(
                &state,
                &mut library_schedule,
                &mut catalog_schedule,
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
    // Requests pulled by the check above or by a full sync.
    serve_requests(state, &library).await?;
    Ok(())
}
