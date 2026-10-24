//! The hub sync worker: one Tokio task for all profiles with a hub account.
//!
//! Every [`TICK`] it pushes the profiles' queued changes (so a change reaches the hub
//! within about half a minute; a reading position changed on every page turn goes out
//! once per tick), and every `[hub] sync_interval_minutes` it runs a full sync
//! ([`sync::sync_once`]): push, finish removals, pull and merge, expiry. An unreachable
//! hub backs off (doubling, at most an hour). Pulled annotations rewrite the Markdown
//! export of their books. It logs counts and profile ids only.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::{AppState, blocking};
use crate::annotations::attach::export_key;
use crate::annotations::now;
use crate::hub_client::store::{self, HubAccount};
use crate::hub_client::sync::{self, Report, SyncError};
use crate::hub_client::{ClientError, HubClient};
use crate::profiles::ProfileId;
use crate::storage::books::live_books_by_key;

/// How often queued changes are pushed.
pub const TICK: Duration = Duration::from_secs(30);

/// The longest wait after failures.
const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// Runs one full sync of `account` and exports the books whose annotations changed.
pub(crate) async fn sync_account(
    state: &AppState,
    account: HubAccount,
) -> Result<Result<Report, SyncError>, super::AppError> {
    let state = state.clone();
    blocking(move || {
        let client = HubClient::new(account.hub_url.clone());
        let pool = state.pool().clone();
        let book_id = |key: &[u8]| {
            let conn = pool.get().ok()?;
            live_books_by_key(&conn, key).ok()?.first().copied()
        };
        let instance = super::routes::hub::instance_name(&state);
        let report = sync::sync_once(
            state.userdata(),
            &account,
            &client,
            &book_id,
            &instance,
            now(),
        );
        if let Ok(report) = &report
            && !report.annotated.is_empty()
            && let Ok(index) = state.pool().get()
        {
            for key in &report.annotated {
                let user = state.userdata().lock();
                if let Err(e) = export_key(&index, &user, state.exporter(), key) {
                    tracing::error!(error = %e, "cannot update the annotation export");
                }
            }
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
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = round(&state, &mut schedules).await {
                tracing::error!(error = ?e, "the hub sync failed");
            }
        }
    })
}

async fn round(
    state: &AppState,
    schedules: &mut HashMap<ProfileId, Schedule>,
) -> Result<(), super::AppError> {
    let userdata = state.userdata_handle();
    let accounts = blocking(move || {
        let conn = userdata.lock();
        let accounts = store::accounts(&conn)?;
        accounts
            .into_iter()
            .map(|a| Ok((store::outbox_len(&conn, a.profile)?, a)))
            .collect::<Result<Vec<_>, crate::storage::StorageError>>()
    })
    .await??;
    schedules.retain(|p, _| accounts.iter().any(|(_, a)| a.profile == *p));
    let interval =
        Duration::from_secs(u64::from(state.config().hub.sync_interval_minutes.max(1)) * 60);
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
            sync_account(state, account).await?.map(|_| ())
        } else {
            let userdata = state.userdata_handle();
            blocking(move || {
                let client = HubClient::new(account.hub_url.clone());
                sync::push(&userdata, &account, &client).map(|_| ())
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
                let wait = match &e {
                    // A revoked token stays revoked: check rarely.
                    SyncError::Client(e) if e.is_unauthorized() => MAX_BACKOFF,
                    SyncError::Client(ClientError::Unreachable(_)) | SyncError::Client(_) => {
                        (interval * 2u32.saturating_pow(schedule.failures - 1)).min(MAX_BACKOFF)
                    }
                    SyncError::Storage(_) => interval,
                };
                schedule.next_full = now + wait;
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
