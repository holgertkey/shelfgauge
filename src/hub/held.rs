//! Held calls (long polling): `GET /api/library/pending?wait=N` and
//! `GET /api/sync?wait=N` answer as soon as something changed for their library or
//! account, or after at most [`WAIT_MAX`] seconds.
//!
//! Everything here is in memory and carries no data: a wake is a hint, and the
//! answer is read from the database afterwards as without waiting. A library's
//! waiters are woken by a device pushing a request record (`book_request`,
//! `cover_request`), an account's by any push to it. Each token holds at most
//! [`MAX_HELD_PER_TOKEN`] calls and the hub at most [`MAX_HELD`]; a call over a cap,
//! or after the hub began to shut down, is answered at once (no error, so the client
//! falls back to its interval). Shutting down wakes every held call.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::watch;

/// The longest hold in seconds: under the 60 s of nginx's default
/// `proxy_read_timeout` (named as `wait_max` in `GET /api/info`).
pub const WAIT_MAX: u32 = 25;

/// The most calls one token holds at once.
pub const MAX_HELD_PER_TOKEN: usize = 2;

/// The most calls the hub holds at once.
pub const MAX_HELD: usize = 4096;

/// What a held call waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Topic {
    /// A request record pushed by a device of the library (internal id).
    Requests(i64),
    /// Any push to the account (internal id).
    Account(i64),
}

/// The held calls of the hub.
pub(super) struct Held {
    inner: Mutex<Inner>,
    closing: watch::Sender<bool>,
    max_per_token: usize,
    max: usize,
}

#[derive(Default)]
struct Inner {
    /// One sender per topic someone waits for; removed with its last waiter.
    topics: HashMap<Topic, watch::Sender<()>>,
    /// The held calls per token (its rate-limit bucket).
    per_token: HashMap<i64, usize>,
    total: usize,
}

impl Default for Held {
    fn default() -> Self {
        Self::with_caps(MAX_HELD_PER_TOKEN, MAX_HELD)
    }
}

impl Held {
    /// Held calls with these caps.
    pub(super) fn with_caps(max_per_token: usize, max: usize) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            closing: watch::Sender::new(false),
            max_per_token,
            max,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A slot for the token `bucket` to wait on `topic`; `None` over a cap or while
    /// shutting down (the call is then answered at once). Taken before the database
    /// is read, so a change committed after that read wakes it.
    pub(super) fn hold(self: &Arc<Self>, bucket: i64, topic: Topic) -> Option<Slot> {
        if *self.closing.borrow() {
            return None;
        }
        let mut inner = self.lock();
        let count = inner.per_token.get(&bucket).copied().unwrap_or(0);
        if count >= self.max_per_token || inner.total >= self.max {
            tracing::debug!("a held call over the cap is answered at once");
            return None;
        }
        *inner.per_token.entry(bucket).or_insert(0) += 1;
        inner.total += 1;
        let changed = inner
            .topics
            .entry(topic)
            .or_insert_with(|| watch::Sender::new(()))
            .subscribe();
        Some(Slot {
            held: Arc::clone(self),
            bucket,
            topic,
            changed: Some(changed),
            closing: self.closing.subscribe(),
        })
    }

    /// Wakes the calls waiting on `topic` (if any).
    pub(super) fn signal(&self, topic: Topic) {
        if let Some(sender) = self.lock().topics.get(&topic) {
            sender.send_replace(());
        }
    }

    /// Wakes every held call and holds no more (the hub shuts down).
    pub(super) fn close(&self) {
        self.closing.send_replace(true);
    }

    /// The calls held now, for tests.
    #[cfg(test)]
    fn total(&self) -> usize {
        self.lock().total
    }
}

/// A held call's place: released when dropped (also when the client goes away and
/// the handler is cancelled).
pub(super) struct Slot {
    held: Arc<Held>,
    bucket: i64,
    topic: Topic,
    changed: Option<watch::Receiver<()>>,
    closing: watch::Receiver<bool>,
}

impl Slot {
    /// Waits until the topic is signalled, the hub shuts down or `limit` passes.
    pub(super) async fn wait(&mut self, limit: Duration) {
        let Some(changed) = self.changed.as_mut() else {
            return;
        };
        let closing = self.closing.wait_for(|closing| *closing);
        tokio::select! {
            _ = changed.changed() => {}
            _ = closing => {}
            () = tokio::time::sleep(limit) => {}
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        // The receiver goes first, so the count below no longer includes it.
        self.changed = None;
        let mut inner = self.held.lock();
        if let Some(count) = inner.per_token.get_mut(&self.bucket) {
            *count -= 1;
            if *count == 0 {
                inner.per_token.remove(&self.bucket);
            }
        }
        inner.total -= 1;
        if inner
            .topics
            .get(&self.topic)
            .is_some_and(|sender| sender.receiver_count() == 0)
        {
            inner.topics.remove(&self.topic);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn caps_are_kept_and_slots_released() {
        let held = Arc::new(Held::with_caps(2, 3));
        let a = held.hold(1, Topic::Account(1)).unwrap();
        let b = held.hold(1, Topic::Account(1)).unwrap();
        assert!(held.hold(1, Topic::Account(2)).is_none(), "per token");
        let c = held.hold(2, Topic::Requests(1)).unwrap();
        assert!(held.hold(3, Topic::Account(3)).is_none(), "on the hub");
        assert_eq!(held.total(), 3);
        drop((a, b, c));
        assert_eq!(held.total(), 0);
        assert!(held.lock().topics.is_empty());
        assert!(held.lock().per_token.is_empty());
        assert!(held.hold(1, Topic::Account(1)).is_some());
    }

    #[tokio::test]
    async fn a_signal_wakes_only_its_topic() {
        let held = Arc::new(Held::default());
        let mut account = held.hold(1, Topic::Account(1)).unwrap();
        let mut other = held.hold(2, Topic::Account(2)).unwrap();
        // Signalled before waiting: still seen (the slot was taken first).
        held.signal(Topic::Account(1));
        let limit = Duration::from_secs(5);
        tokio::time::timeout(Duration::from_secs(1), account.wait(limit))
            .await
            .expect("woken");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), other.wait(limit))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn closing_wakes_every_slot_and_holds_no_more() {
        let held = Arc::new(Held::default());
        let mut slot = held.hold(1, Topic::Requests(1)).unwrap();
        held.close();
        tokio::time::timeout(Duration::from_secs(1), slot.wait(Duration::from_secs(5)))
            .await
            .expect("woken");
        assert!(held.hold(1, Topic::Requests(1)).is_none());
    }
}
