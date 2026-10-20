//! Profile passwords: the login from other devices.
//!
//! A password has 10–128 characters (Unicode code points) and no other rules. It is
//! NFC-normalized before it is hashed or checked, so the same text typed on a phone
//! and on a computer matches even when one sends composed and the other decomposed
//! characters. Hashing uses argon2id with the parameters of the PINs (see
//! [`pin`](super::pin)) and takes tens of milliseconds: call [`hash`] and [`verify`] on
//! a blocking thread. Passwords are never logged.
//!
//! Wrong passwords lock the profile out like wrong PINs (a
//! [`PinLimiter`](super::pin::PinLimiter) of its own); [`AddressLimiter`] adds a
//! budget per client address, so one client cannot try a few passwords on every
//! profile in turn.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use unicode_normalization::UnicodeNormalization;

/// The shortest password, in characters.
pub const MIN_CHARS: usize = 10;

/// The longest password, in characters.
pub const MAX_CHARS: usize = 128;

/// Failed logins from one address within [`ADDRESS_WINDOW`] before it is refused.
pub const ADDRESS_TRIES: u32 = 20;

/// The window of [`ADDRESS_TRIES`].
pub const ADDRESS_WINDOW: Duration = Duration::from_secs(15 * 60);

/// Why a password is not valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    /// Shorter than [`MIN_CHARS`].
    #[error("a password has at least 10 characters")]
    TooShort,
    /// Longer than [`MAX_CHARS`].
    #[error("a password has at most 128 characters")]
    TooLong,
    /// Hashing failed (the system random number generator).
    #[error("the password could not be stored")]
    Hash,
}

/// The password as it is hashed: NFC-normalized.
fn normalized(password: &str) -> String {
    password.nfc().collect()
}

/// Checks the length of `password` (after normalization).
///
/// # Examples
///
/// ```
/// use shelfgauge::profiles::password::{PasswordError, check_format};
///
/// assert!(check_format("correct horse").is_ok());
/// assert!(check_format("ёлка ёлка!").is_ok());
/// assert_eq!(check_format("short"), Err(PasswordError::TooShort));
/// assert_eq!(check_format(&"x".repeat(129)), Err(PasswordError::TooLong));
/// ```
pub fn check_format(password: &str) -> Result<(), PasswordError> {
    let chars = normalized(password).chars().count();
    if chars < MIN_CHARS {
        Err(PasswordError::TooShort)
    } else if chars > MAX_CHARS {
        Err(PasswordError::TooLong)
    } else {
        Ok(())
    }
}

/// The argon2id PHC string of a password (after [`check_format`]).
pub fn hash(password: &str) -> Result<String, PasswordError> {
    check_format(password)?;
    Argon2::default()
        .hash_password(normalized(password).as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|_| PasswordError::Hash)
}

/// Whether `password` matches the stored PHC string (a broken string never matches,
/// nor does a password of a length that could not have been set).
pub fn verify(password: &str, stored: &str) -> bool {
    if check_format(password).is_err() {
        return false;
    }
    let Ok(parsed) = PasswordHash::new(stored) else {
        return false;
    };
    Argon2::default()
        .verify_password(normalized(password).as_bytes(), &parsed)
        .is_ok()
}

#[derive(Debug)]
struct Window {
    started: Instant,
    tries: u32,
}

/// Counts failed logins per client address: [`ADDRESS_TRIES`] within
/// [`ADDRESS_WINDOW`], then every login from that address is refused until the window
/// ends. The clock is passed in, so tests need not wait.
///
/// Not for a loopback client: behind a reverse proxy on this computer all clients
/// share its address, and one of them could lock out everyone. (A proxy on another
/// machine has the same problem; such a setup relies on the proxy's own rate limit.)
///
/// Like [`PinLimiter`](super::pin::PinLimiter), a try is counted before the password
/// is checked, and a right one is given back with [`success`](Self::success).
#[derive(Debug, Default)]
pub struct AddressLimiter {
    windows: Mutex<HashMap<IpAddr, Window>>,
}

/// The number of addresses kept before expired windows are dropped.
const PRUNE_AT: usize = 1024;

impl AddressLimiter {
    /// A limiter with no tries.
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts a try from `address` at `now`: `Err` with the wait while the address has
    /// used its budget (nothing is counted then).
    ///
    /// # Examples
    ///
    /// ```
    /// use std::time::Instant;
    /// use shelfgauge::profiles::password::{ADDRESS_TRIES, AddressLimiter};
    ///
    /// let limiter = AddressLimiter::new();
    /// let (ip, now) = ("100.64.0.7".parse().unwrap(), Instant::now());
    /// for _ in 0..ADDRESS_TRIES {
    ///     assert!(limiter.begin(ip, now).is_ok());
    /// }
    /// assert!(limiter.begin(ip, now).is_err());
    /// ```
    pub fn begin(&self, address: IpAddr, now: Instant) -> Result<(), Duration> {
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if map.len() >= PRUNE_AT {
            map.retain(|_, w| now.saturating_duration_since(w.started) < ADDRESS_WINDOW);
        }
        let window = map.entry(address).or_insert(Window {
            started: now,
            tries: 0,
        });
        let age = now.saturating_duration_since(window.started);
        if age >= ADDRESS_WINDOW {
            *window = Window {
                started: now,
                tries: 0,
            };
        } else if window.tries >= ADDRESS_TRIES {
            return Err(ADDRESS_WINDOW - age);
        }
        window.tries += 1;
        Ok(())
    }

    /// Gives back the try of a right password from `address`.
    pub fn success(&self, address: IpAddr) {
        let mut map = self.windows.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(window) = map.get_mut(&address) {
            window.tries = window.tries.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_are_argon2id_and_normalized() {
        let stored = hash("correct horse").unwrap();
        assert!(stored.starts_with("$argon2id$v=19$"), "{stored}");
        assert!(verify("correct horse", &stored));
        assert!(!verify("correct horsE", &stored));
        assert!(!verify("correct horse", "not a hash"));
        assert_ne!(hash("correct horse").unwrap(), stored, "salted");

        // "й" composed (U+0439) and decomposed (U+0438 U+0306) are one password.
        let composed = "мой пароль \u{0439}";
        let decomposed = "мой пароль \u{0438}\u{0306}";
        let stored = hash(decomposed).unwrap();
        assert!(verify(composed, &stored));
        assert!(verify(decomposed, &stored));
    }

    #[test]
    fn lengths_are_counted_in_characters() {
        assert_eq!(check_format(&"я".repeat(9)), Err(PasswordError::TooShort));
        assert!(check_format(&"я".repeat(10)).is_ok());
        assert!(check_format(&"я".repeat(128)).is_ok());
        assert_eq!(check_format(&"я".repeat(129)), Err(PasswordError::TooLong));
        // Spaces count and are kept.
        assert!(check_format("          ").is_ok());
        assert_eq!(hash("short"), Err(PasswordError::TooShort));
        let stored = hash("0123456789").unwrap();
        assert!(!verify(&"0".repeat(200), &stored));
    }

    #[test]
    fn address_budget_resets_after_the_window() {
        let limiter = AddressLimiter::new();
        let ip: IpAddr = "192.168.1.20".parse().unwrap();
        let other: IpAddr = "192.168.1.21".parse().unwrap();
        let start = Instant::now();
        for _ in 0..ADDRESS_TRIES {
            limiter.begin(ip, start).unwrap();
        }
        let later = start + Duration::from_secs(60);
        assert_eq!(
            limiter.begin(ip, later),
            Err(ADDRESS_WINDOW - Duration::from_secs(60))
        );
        assert!(limiter.begin(other, later).is_ok(), "per address");
        // A right password gives its try back.
        limiter.success(ip);
        assert!(limiter.begin(ip, later).is_ok());
        assert!(limiter.begin(ip, later).is_err());
        assert!(limiter.begin(ip, start + ADDRESS_WINDOW).is_ok());
    }
}
