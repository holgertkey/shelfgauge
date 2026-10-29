//! Request rate limits of the hub, in memory (a restart clears them).
//!
//! - [`DeviceLimiter`]: a token bucket per device token, the main control: up to
//!   `requests_per_minute` requests at once, refilled at that rate.
//! - [`WindowLimiter`]: a fixed window per client address: one hour for the routes
//!   without a token (sign-in, invites, pairing, new libraries), one day for the
//!   libraries created with open registration. Keyed by [`client_key`]: the peer, or
//!   for a peer in `[server] trusted_proxies` the client it names in
//!   `X-Forwarded-For`. Requests from an untrusted loopback peer (a reverse proxy on
//!   this machine that is not listed) share one budget. An IPv6 client is counted by
//!   its /64, the block one subscriber usually gets.
//!
//! - [`LoginLimiter`]: failed password checks per address (or account) and per client
//!   address, answered with a growing delay, never a lock, on the hub's clock.
//!
//! Client addresses are never logged. The clock is passed in, so tests need not wait.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
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

/// An entry of `[server] trusted_proxies`: one address or a CIDR range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct IpRange {
    network: IpAddr,
    prefix: u8,
}

impl IpRange {
    /// Parses `address` or `address/prefix` (`127.0.0.1`, `10.0.0.0/8`, `::1`,
    /// `fd00::/8`).
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let mut address: IpAddr = address.parse().ok()?;
        let prefix = match prefix {
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max_prefix(address))?,
            None => {
                address = address.to_canonical();
                max_prefix(address)
            }
        };
        Some(Self {
            network: mask(address, prefix),
            prefix,
        })
    }

    /// Whether `ip` (canonical) is in the range.
    fn contains(&self, ip: IpAddr) -> bool {
        ip.is_ipv4() == self.network.is_ipv4() && mask(ip, self.prefix) == self.network
    }
}

fn max_prefix(ip: IpAddr) -> u8 {
    if ip.is_ipv4() { 32 } else { 128 }
}

/// `ip` with all but its first `prefix` bits cleared.
fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = u32::from(v4) & u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V4(Ipv4Addr::from(bits))
        }
        IpAddr::V6(v6) => {
            let bits = u128::from(v6) & u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
            IpAddr::V6(Ipv6Addr::from(bits))
        }
    }
}

/// The key of a client's public budget: `None` for the shared budget.
///
/// A peer in `trusted` (a reverse proxy) is replaced by the rightmost address of the
/// `X-Forwarded-For` values (in header order) that is not itself trusted; values
/// that cannot be read (`forwarded` is `None`, or an entry is no address) or that
/// name only trusted addresses give the shared budget, never the proxy's own. An
/// untrusted loopback peer shares the budget too (an unlisted proxy on this machine).
/// IPv6 clients are keyed by their /64.
pub(crate) fn client_key(
    peer: Option<IpAddr>,
    forwarded: Option<&[&str]>,
    trusted: &[IpRange],
) -> Option<IpAddr> {
    let peer = peer?.to_canonical();
    let is_trusted = |ip: IpAddr| trusted.iter().any(|range| range.contains(ip));
    let client = if is_trusted(peer) {
        let entries: Vec<&str> = forwarded?.iter().flat_map(|v| v.split(',')).collect();
        let mut client = None;
        for entry in entries.into_iter().rev() {
            let ip = forwarded_address(entry)?;
            if !is_trusted(ip) {
                client = Some(ip);
                break;
            }
        }
        client?
    } else if peer.is_loopback() {
        return None;
    } else {
        peer
    };
    Some(match client {
        IpAddr::V6(_) => mask(client, 64),
        IpAddr::V4(_) => client,
    })
}

/// One `X-Forwarded-For` entry: an address, maybe with a port or in brackets.
fn forwarded_address(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    let ip = entry
        .parse::<IpAddr>()
        .or_else(|_| entry.parse::<SocketAddr>().map(|a| a.ip()))
        .or_else(|_| {
            entry
                .strip_prefix('[')
                .and_then(|e| e.strip_suffix(']'))
                .unwrap_or("")
                .parse::<IpAddr>()
        })
        .ok()?;
    Some(ip.to_canonical())
}

/// An hour, the window of the public budget.
pub(crate) const HOUR: Duration = Duration::from_secs(3600);

/// A day, the window of the new-account limit.
pub(crate) const DAY: Duration = Duration::from_secs(86_400);

#[derive(Debug, Clone, Copy)]
struct Window {
    started: Instant,
    requests: u32,
}

/// A fixed window per client address (`None`: the shared budget of loopback peers).
#[derive(Debug)]
pub(crate) struct WindowLimiter {
    /// 0 = no limit.
    per_window: u32,
    length: Duration,
    windows: Mutex<HashMap<Option<IpAddr>, Window>>,
}

impl WindowLimiter {
    /// Allows `per_window` events per address in each window of `length` (0 = no
    /// limit).
    pub(crate) fn new(per_window: u32, length: Duration) -> Self {
        Self {
            per_window,
            length,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Counts an event of `address` at `now`: `Err` with the wait when its window is
    /// used up (nothing is counted then).
    pub(crate) fn begin(&self, address: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        self.with_window(address, now, true)
    }

    /// Like [`begin`](Self::begin) without counting: for events counted only when
    /// they succeed ([`count`](Self::count)).
    pub(crate) fn check(&self, address: Option<IpAddr>, now: Instant) -> Result<(), Duration> {
        self.with_window(address, now, false)
    }

    /// Counts an event of `address` in the window a [`check`](Self::check) opened.
    pub(crate) fn count(&self, address: Option<IpAddr>) {
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(window) = map.get_mut(&address) {
            window.requests = window.requests.saturating_add(1);
        }
    }

    fn with_window(
        &self,
        address: Option<IpAddr>,
        now: Instant,
        count: bool,
    ) -> Result<(), Duration> {
        if self.per_window == 0 {
            return Ok(());
        }
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= PRUNE_AT {
            map.retain(|_, w| now.saturating_duration_since(w.started) < self.length);
        }
        let window = map.entry(address).or_insert(Window {
            started: now,
            requests: 0,
        });
        let age = now.saturating_duration_since(window.started);
        if age >= self.length {
            *window = Window {
                started: now,
                requests: 0,
            };
        } else if window.requests >= self.per_window {
            return Err(self.length - age);
        }
        if count {
            window.requests += 1;
        }
        Ok(())
    }
}

/// What failed password checks are counted by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum LoginKey {
    /// The SHA-256 of an address `name@handle` (normalized) tried at sign-in.
    Address([u8; 32]),
    /// The internal id of an account whose current password a device confirms.
    Account(i64),
    /// A client address ([`client_key`]).
    Client(IpAddr),
}

impl LoginKey {
    /// Failures without a delay.
    fn free(self) -> u32 {
        match self {
            Self::Address(_) | Self::Account(_) => 3,
            // A family signs in from one home address.
            Self::Client(_) => 10,
        }
    }
}

/// The longest delay after failed password checks, in seconds: knowing someone's
/// address slows their sign-in down at worst this much, never locks it.
pub(crate) const MAX_LOGIN_DELAY: i64 = 900;

/// Failures are forgotten after this long without one (seconds).
const LOGIN_MEMORY: i64 = 86_400;

#[derive(Debug, Clone, Copy)]
struct Strikes {
    failures: u32,
    until: i64,
    last: i64,
}

/// Failed password checks per [`LoginKey`]: after a few free ones each failure makes
/// the key wait twice as long as the last, up to [`MAX_LOGIN_DELAY`]. Checked before
/// any hashing; a success clears its address or account. Times are Unix seconds.
#[derive(Debug, Default)]
pub(crate) struct LoginLimiter {
    strikes: Mutex<HashMap<LoginKey, Strikes>>,
}

impl LoginLimiter {
    /// `Err` with the wait when any of `keys` must still wait at `now`.
    pub(crate) fn check(&self, keys: &[LoginKey], now: i64) -> Result<(), Duration> {
        let map = self.strikes.lock().unwrap_or_else(|e| e.into_inner());
        let wait = (keys.iter())
            .filter_map(|key| map.get(key))
            .map(|strikes| strikes.until - now)
            .max()
            .unwrap_or(0);
        if wait > 0 {
            Err(Duration::from_secs(wait as u64))
        } else {
            Ok(())
        }
    }

    /// Counts a failure of each of `keys` at `now`.
    pub(crate) fn fail(&self, keys: &[LoginKey], now: i64) {
        let mut map = self.strikes.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= PRUNE_AT {
            map.retain(|_, s| now - s.last < LOGIN_MEMORY);
        }
        for key in keys {
            let strikes = map.entry(*key).or_insert(Strikes {
                failures: 0,
                until: now,
                last: now,
            });
            if now - strikes.last >= LOGIN_MEMORY {
                strikes.failures = 0;
            }
            strikes.failures += 1;
            strikes.last = now;
            let over = strikes.failures.saturating_sub(key.free());
            if over > 0 {
                let delay = 1i64
                    .checked_shl(over - 1)
                    .unwrap_or(MAX_LOGIN_DELAY)
                    .min(MAX_LOGIN_DELAY);
                strikes.until = now + delay;
            }
        }
    }

    /// Clears the failures of `key` (a correct password for it).
    pub(crate) fn succeed(&self, key: LoginKey) {
        let mut map = self.strikes.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&key);
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
        let limiter = WindowLimiter::new(2, HOUR);
        let start = Instant::now();
        let ip = Some("192.0.2.1".parse().unwrap());
        limiter.begin(ip, start).unwrap();
        limiter.begin(ip, start).unwrap();
        assert!(limiter.begin(ip, start).is_err());
        assert!(
            limiter.begin(None, start).is_ok(),
            "the proxy budget is separate"
        );
        assert!(limiter.begin(ip, start + HOUR).is_ok());
    }

    #[test]
    fn checked_windows_count_only_successes() {
        let limiter = WindowLimiter::new(1, DAY);
        let start = Instant::now();
        let ip = Some("192.0.2.1".parse().unwrap());
        limiter.check(ip, start).unwrap();
        limiter.check(ip, start).unwrap();
        limiter.count(ip);
        let wait = limiter.check(ip, start + HOUR).unwrap_err();
        assert_eq!(wait, DAY - HOUR);
        assert!(limiter.check(ip, start + DAY).is_ok());
        let unlimited = WindowLimiter::new(0, DAY);
        for _ in 0..10 {
            unlimited.begin(ip, start).unwrap();
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn ranges(list: &[&str]) -> Vec<IpRange> {
        list.iter().map(|r| IpRange::parse(r).unwrap()).collect()
    }

    #[test]
    fn ranges_parse_and_match() {
        let lan = IpRange::parse("10.1.0.0/16").unwrap();
        assert!(lan.contains(ip("10.1.200.3")));
        assert!(!lan.contains(ip("10.2.0.1")));
        assert!(!lan.contains(ip("::1")));
        assert!(
            IpRange::parse("0.0.0.0/0")
                .unwrap()
                .contains(ip("192.0.2.1"))
        );
        let v6 = IpRange::parse("fd00::/8").unwrap();
        assert!(v6.contains(ip("fd12::1")));
        assert!(
            IpRange::parse("::ffff:127.0.0.1")
                .unwrap()
                .contains(ip("127.0.0.1"))
        );
        for bad in [
            "",
            "10.0.0.0/33",
            "::/129",
            "10.0.0.0/",
            "host",
            "10.0.0.1/8/8",
        ] {
            assert_eq!(IpRange::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn untrusted_peers_are_their_own_clients() {
        let trusted = ranges(&["127.0.0.1"]);
        let spoofed = Some(&["198.51.100.7"][..]);
        assert_eq!(
            client_key(Some(ip("192.0.2.1")), spoofed, &trusted),
            Some(ip("192.0.2.1")),
            "forwarded values of an untrusted peer are ignored"
        );
        assert_eq!(client_key(Some(ip("127.0.0.1")), spoofed, &[]), None);
        assert_eq!(client_key(Some(ip("::1")), None, &trusted), None);
        assert_eq!(client_key(None, spoofed, &trusted), None);
        assert_eq!(
            client_key(Some(ip("::ffff:192.0.2.1")), None, &[]),
            Some(ip("192.0.2.1"))
        );
    }

    #[test]
    fn a_trusted_proxy_names_the_rightmost_untrusted_client() {
        let trusted = ranges(&["127.0.0.1", "10.0.0.0/8"]);
        let local = Some(ip("127.0.0.1"));
        let key = |values: &[&str]| client_key(local, Some(values), &trusted);
        assert_eq!(key(&["192.0.2.1"]), Some(ip("192.0.2.1")));
        // nginx appends the peer to what the client sent: the client's own value is
        // left of it and not believed.
        assert_eq!(
            key(&["203.0.113.9, 192.0.2.1, 10.0.0.2"]),
            Some(ip("192.0.2.1"))
        );
        assert_eq!(key(&["203.0.113.9", "192.0.2.1"]), Some(ip("192.0.2.1")));
        assert_eq!(key(&[" 192.0.2.1:4711 "]), Some(ip("192.0.2.1")));
        assert_eq!(key(&["[2001:db8::1]:443"]), Some(ip("2001:db8::")));
        assert_eq!(key(&["[2001:db8::1]"]), Some(ip("2001:db8::")));
        // Nothing usable: the shared budget, never the proxy's own address.
        assert_eq!(key(&[]), None);
        assert_eq!(key(&["10.0.0.3"]), None);
        assert_eq!(key(&["192.0.2.1, unknown"]), None);
        assert_eq!(key(&[""]), None);
        assert_eq!(client_key(local, None, &trusted), None);
    }

    #[test]
    fn ipv6_clients_are_keyed_by_their_slash_64() {
        let a = client_key(Some(ip("2001:db8:1:2::1")), None, &[]);
        let b = client_key(Some(ip("2001:db8:1:2:ffff::9")), None, &[]);
        let c = client_key(Some(ip("2001:db8:1:3::1")), None, &[]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn failed_logins_slow_down_and_never_lock() {
        let limiter = LoginLimiter::default();
        let address = LoginKey::Address([1; 32]);
        let other = LoginKey::Address([2; 32]);
        let mut now = 1000;
        for _ in 0..3 {
            limiter.check(&[address], now).unwrap();
            limiter.fail(&[address], now);
        }
        limiter.check(&[address], now).unwrap();
        let mut waits = Vec::new();
        for _ in 0..13 {
            limiter.fail(&[address], now);
            let wait = limiter.check(&[address], now).unwrap_err().as_secs() as i64;
            waits.push(wait);
            now += wait;
            // Once the wait is over the key may try again.
            limiter.check(&[address], now).unwrap();
        }
        assert_eq!(&waits[..5], [1, 2, 4, 8, 16]);
        assert_eq!(*waits.last().unwrap(), MAX_LOGIN_DELAY);
        limiter.check(&[other], 1000).unwrap();
        // A success clears it; a day without failures forgets them.
        limiter.fail(&[address], now);
        limiter.succeed(address);
        limiter.check(&[address], now).unwrap();
        for _ in 0..4 {
            limiter.fail(&[other], now);
        }
        assert!(limiter.check(&[address, other], now).is_err());
        limiter.fail(&[other], now + LOGIN_MEMORY + 1);
        limiter.check(&[other], now + LOGIN_MEMORY + 1).unwrap();
    }
}
