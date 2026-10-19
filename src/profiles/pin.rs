//! Profile PINs: 4–8 digits, stored as argon2id PHC strings, and the lockout after
//! wrong tries.
//!
//! A PIN keeps family members apart on a shared computer; it is not authentication
//! (anyone with access to the computer can read `userdata.sqlite`). Hashing uses the
//! `argon2` crate's defaults (Argon2id v19, 19 MiB, 2 passes) and takes tens of
//! milliseconds: call [`hash`] and [`verify`] on a blocking thread. PINs are never
//! logged.
//!
//! [`PinLimiter`] locks a profile for 30 s after 5 tries without the right PIN, doubling
//! with every further lockout up to 15 minutes; a correct PIN resets it. Tries are
//! counted before the PIN is checked, so parallel requests get no more guesses. It is
//! kept in memory: a restart clears it, which needs access to the computer anyway.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

use super::ProfileId;

/// Wrong tries in a row before a lockout.
pub const MAX_TRIES: u32 = 5;

/// The first lockout; every further one doubles it.
pub const FIRST_LOCKOUT: Duration = Duration::from_secs(30);

/// The longest lockout.
pub const MAX_LOCKOUT: Duration = Duration::from_secs(15 * 60);

/// Why a PIN is not valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PinError {
    /// Not 4–8 ASCII digits.
    #[error("a PIN has 4 to 8 digits")]
    Format,
    /// Hashing failed (the system random number generator).
    #[error("the PIN could not be stored")]
    Hash,
}

/// Checks that `pin` is 4–8 ASCII digits.
///
/// # Examples
///
/// ```
/// use shelfgauge::profiles::pin::check_format;
///
/// assert!(check_format("0042").is_ok());
/// assert!(check_format("123").is_err());
/// assert!(check_format("12345678 ").is_err());
/// assert!(check_format("١٢٣٤").is_err());
/// ```
pub fn check_format(pin: &str) -> Result<(), PinError> {
    if (4..=8).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit()) {
        Ok(())
    } else {
        Err(PinError::Format)
    }
}

/// The argon2id PHC string of a PIN (after [`check_format`]).
pub fn hash(pin: &str) -> Result<String, PinError> {
    check_format(pin)?;
    Argon2::default()
        .hash_password(pin.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|_| PinError::Hash)
}

/// Whether `pin` matches the stored PHC string (a broken string never matches).
pub fn verify(pin: &str, stored: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(pin.as_bytes(), &parsed)
        .is_ok()
}

/// What a wrong PIN leads to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// The tries left before the profile is locked.
    TriesLeft(u32),
    /// The profile is locked for this long.
    Locked(Duration),
}

#[derive(Debug, Default)]
struct Entry {
    /// Tries since the last lockout or success.
    failures: u32,
    /// Lockouts since the last success.
    lockouts: u32,
    locked_until: Option<Instant>,
}

/// Counts PIN tries per profile and locks profiles out; the clock is passed in, so
/// tests need not wait.
///
/// A try is counted *before* the PIN is checked ([`begin`](Self::begin)), under the
/// limiter's lock, so parallel requests cannot get more than [`MAX_TRIES`] guesses
/// past it; a correct PIN forgets the count ([`success`](Self::success)). PIN hashing
/// is serialized with [`hashing`](Self::hashing): every argon2 run takes 19 MiB.
#[derive(Debug, Default)]
pub struct PinLimiter {
    tries: Mutex<HashMap<ProfileId, Entry>>,
    hashing: Mutex<()>,
}

impl PinLimiter {
    /// A limiter with no tries.
    pub fn new() -> Self {
        Self::default()
    }

    /// How long `profile` is still locked at `now`; `None` if a PIN may be tried.
    pub fn locked(&self, profile: ProfileId, now: Instant) -> Option<Duration> {
        let map = self.tries.lock().unwrap_or_else(|e| e.into_inner());
        map.get(&profile).and_then(|e| left(e, now))
    }

    /// Counts a try of `profile` at `now`, before its PIN is checked: `Err` with the
    /// wait while the profile is locked (nothing is counted then), else what a wrong
    /// PIN leads to. The [`MAX_TRIES`]th try locks the profile at once; a correct PIN
    /// lifts the lock again with [`success`](Self::success).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::{Duration, Instant};
    /// use shelfgauge::profiles::ProfileId;
    /// use shelfgauge::profiles::pin::{Failure, PinLimiter};
    ///
    /// let limiter = PinLimiter::new();
    /// let (p, now) = (ProfileId(2), Instant::now());
    /// for left in (1..5).rev() {
    ///     assert_eq!(limiter.begin(p, now), Ok(Failure::TriesLeft(left)));
    /// }
    /// assert_eq!(limiter.begin(p, now), Ok(Failure::Locked(Duration::from_secs(30))));
    /// assert_eq!(limiter.begin(p, now), Err(Duration::from_secs(30)));
    /// ```
    pub fn begin(&self, profile: ProfileId, now: Instant) -> Result<Failure, Duration> {
        let mut map = self.tries.lock().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(profile).or_default();
        if let Some(wait) = left(entry, now) {
            return Err(wait);
        }
        entry.failures += 1;
        if entry.failures < MAX_TRIES {
            return Ok(Failure::TriesLeft(MAX_TRIES - entry.failures));
        }
        let lockout = FIRST_LOCKOUT
            .saturating_mul(2u32.saturating_pow(entry.lockouts))
            .min(MAX_LOCKOUT);
        entry.failures = 0;
        entry.lockouts = entry.lockouts.saturating_add(1);
        entry.locked_until = Some(now + lockout);
        Ok(Failure::Locked(lockout))
    }

    /// Forgets the tries and lockouts of `profile` (a correct PIN, or its PIN was
    /// removed).
    pub fn success(&self, profile: ProfileId) {
        self.tries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&profile);
    }

    /// Held while a PIN is hashed or verified: one argon2 run at a time.
    pub fn hashing(&self) -> MutexGuard<'_, ()> {
        self.hashing.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The time `entry` is still locked at `now`.
fn left(entry: &Entry, now: Instant) -> Option<Duration> {
    entry
        .locked_until
        .and_then(|until| until.checked_duration_since(now))
        .filter(|left| !left.is_zero())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_argon2id_phc_strings() {
        let stored = hash("2468").unwrap();
        assert!(stored.starts_with("$argon2id$v=19$"), "{stored}");
        assert!(verify("2468", &stored));
        assert!(!verify("2469", &stored));
        assert!(!verify("2468", "not a hash"));
        assert_ne!(hash("2468").unwrap(), stored, "salted");
        assert_eq!(hash("12a4"), Err(PinError::Format));
    }

    #[test]
    fn lockouts_double_up_to_the_limit_and_success_resets() {
        let limiter = PinLimiter::new();
        let p = ProfileId(3);
        let start = Instant::now();
        let mut now = start;
        let mut lockouts = Vec::new();
        for _ in 0..7 {
            for _ in 1..MAX_TRIES {
                assert!(matches!(limiter.begin(p, now), Ok(Failure::TriesLeft(_))));
            }
            let Ok(Failure::Locked(wait)) = limiter.begin(p, now) else {
                panic!("not locked");
            };
            assert_eq!(limiter.locked(p, now), Some(wait));
            assert_eq!(limiter.begin(p, now), Err(wait), "locked: not counted");
            assert_eq!(limiter.locked(ProfileId(4), now), None, "per profile");
            lockouts.push(wait.as_secs());
            now += wait;
            assert_eq!(limiter.locked(p, now), None, "over");
        }
        assert_eq!(lockouts, [30, 60, 120, 240, 480, 900, 900]);

        limiter.success(p);
        assert_eq!(limiter.begin(p, now), Ok(Failure::TriesLeft(MAX_TRIES - 1)));
        // The fifth try locks at once, but a right PIN in it lifts the lock.
        for _ in 0..3 {
            limiter.begin(p, now).unwrap();
        }
        assert!(matches!(limiter.begin(p, now), Ok(Failure::Locked(_))));
        limiter.success(p);
        assert_eq!(limiter.locked(p, now), None);
    }
}
