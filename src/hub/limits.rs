//! Request rate limits of the hub, in memory (a restart clears them).
//!
//! - [`DeviceLimiter`]: a token bucket per device token, the main control: up to
//!   `requests_per_minute` requests at once, refilled at that rate.
//! - [`PublicLimiter`]: a fixed one-hour window per client address for the routes
//!   without a token (account, pair, recover). Only a non-loopback peer has an address
//!   of its own; requests from loopback (a reverse proxy on this machine) share one
//!   budget, since forwarded headers are never read.
//!
//! The clock is passed in, so tests need not wait.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The number of keys kept before stale entries are dropped.
const PRUNE_AT: usize = 4096;

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

/// A token bucket per device id.
#[derive(Debug)]
pub(crate) struct DeviceLimiter {
    per_minute: f64,
    buckets: Mutex<HashMap<i64, Bucket>>,
}

impl DeviceLimiter {
    /// Allows `per_minute` requests per device and minute.
    pub(crate) fn new(per_minute: u32) -> Self {
        Self {
            per_minute: f64::from(per_minute.max(1)),
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Takes one request of `device` at `now`: `Err` with the wait when its bucket is
    /// empty.
    pub(crate) fn take(&self, device: i64, now: Instant) -> Result<(), Duration> {
        let rate = self.per_minute / 60.0;
        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= PRUNE_AT {
            // A bucket that has been refilled completely is the same as none.
            let full = Duration::from_secs_f64(self.per_minute / rate);
            map.retain(|_, b| now.saturating_duration_since(b.updated) < full);
        }
        let bucket = map.entry(device).or_insert(Bucket {
            tokens: self.per_minute,
            updated: now,
        });
        let elapsed = now.saturating_duration_since(bucket.updated).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * rate).min(self.per_minute);
        bucket.updated = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            Err(Duration::from_secs_f64((1.0 - bucket.tokens) / rate))
        }
    }
}

/// The length of a [`PublicLimiter`] window.
pub(crate) const PUBLIC_WINDOW: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    requests: u32,
}

/// A fixed window per client address (`None`: the shared budget of loopback peers).
#[derive(Debug)]
pub(crate) struct PublicLimiter {
    per_window: u32,
    windows: Mutex<HashMap<Option<IpAddr>, Window>>,
}

impl PublicLimiter {
    /// Allows `per_hour` requests per address and hour.
    pub(crate) fn new(per_hour: u32) -> Self {
        Self {
            per_window: per_hour.max(1),
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Counts a request from `address` at `now`: `Err` with the wait when its window is
    /// used up (nothing is counted then).
    pub(crate) fn begin(&self, address: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= PRUNE_AT {
            map.retain(|_, w| now.saturating_duration_since(w.started) < PUBLIC_WINDOW);
        }
        let window = map.entry(address).or_insert(Window {
            started: now,
            requests: 0,
        });
        let age = now.saturating_duration_since(window.started);
        if age >= PUBLIC_WINDOW {
            *window = Window {
                started: now,
                requests: 0,
            };
        } else if window.requests >= self.per_window {
            return Err(PUBLIC_WINDOW - age);
        }
        window.requests += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_buckets_refill_over_time() {
        let limiter = DeviceLimiter::new(60);
        let start = Instant::now();
        for _ in 0..60 {
            limiter.take(1, start).unwrap();
        }
        let wait = limiter.take(1, start).unwrap_err();
        assert!(wait <= Duration::from_secs(1), "{wait:?}");
        assert!(limiter.take(2, start).is_ok(), "per device");
        assert!(limiter.take(1, start + Duration::from_secs(1)).is_ok());
        assert!(limiter.take(1, start + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn public_windows_are_per_address_and_reset() {
        let limiter = PublicLimiter::new(2);
        let start = Instant::now();
        let ip = Some("192.0.2.1".parse().unwrap());
        limiter.begin(ip, start).unwrap();
        limiter.begin(ip, start).unwrap();
        assert!(limiter.begin(ip, start).is_err());
        assert!(
            limiter.begin(None, start).is_ok(),
            "the proxy budget is separate"
        );
        assert!(limiter.begin(ip, start + PUBLIC_WINDOW).is_ok());
    }
}
