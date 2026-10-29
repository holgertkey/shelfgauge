//! The hub sync worker: one Tokio task for the library and all its accounts.
//!
//! Every [`TICK`] it pushes the profiles' queued changes (so a change reaches the hub
//! within about half a minute; a reading position changed on every page turn goes out
//! once per tick), and every `[hub] sync_interval_minutes` it runs a full round: the
//! last pull of any account of 0.18–0.20 ([`sync::pull_legacy`]), the hub's account
//! list (redeemed invites, devices, accounts gone) and the queued removals
//! (`refresh_library`), then a full sync of every account ([`sync::sync_once`]):
//! library keys, push, finish removals, pull and merge, expiry. Everything goes with
//! the library's token. An unreachable hub backs off (doubling, at most an hour).
//! Pulled annotations rewrite the Markdown export of their books. It logs counts and
//! profile ids only.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{AppState, blocking};
use crate::annotations::attach::export_key;
use crate::annotations::now;
use crate::hub_client::HubClient;
use crate::hub_client::store::{self, HubAccount, HubLibrary, Removal};
use crate::hub_client::sync::{self, Report, SyncError, Target};
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
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = round(&state, &mut library_schedule, &mut schedules).await {
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
    Ok(())
}
