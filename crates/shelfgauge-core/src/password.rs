//! The keys derived from a hub password.
//!
//! A person's hub password never leaves their browser (or, for the owner's optional
//! password, the library's computer). It gives two keys:
//!
//! 1. the password is NFKC-normalized and stretched with Argon2id (one lane, the
//!    account's random [`SALT_LEN`]-byte salt, at least [`MIN_MEMORY_KIB`] KiB and
//!    [`MIN_ITERATIONS`] iterations) into 32 bytes;
//! 2. HKDF-SHA256 (RFC 5869, no salt) expands those into the **login key**
//!    (info [`LOGIN_INFO`]), whose SHA-256 the hub keeps to check a sign-in, and the
//!    **wrapping key** (info [`WRAPPING_INFO`]), which seals the account's key bundle.
//!
//! The hub sees only the login key, which tells nothing about the wrapping key.
//! Weaker parameters than the minimum are refused here, so a hostile hub cannot lower
//! them to make guessing easier. The interop vectors are in
//! `tests/fixtures/publication/password.json` of the `shelfgauge` repository.

use std::fmt;

use argon2::{Algorithm, Argon2, Params, Version};
use hmac::{KeyInit, Mac};
use sha2::Sha256;
use unicode_normalization::UnicodeNormalization;

/// The length of a password salt in bytes.
pub const SALT_LEN: usize = 16;

/// The smallest Argon2id memory cost in KiB (19 MiB, OWASP's minimum).
pub const MIN_MEMORY_KIB: u32 = 19 * 1024;

/// The smallest Argon2id iteration count.
pub const MIN_ITERATIONS: u32 = 2;

/// The HKDF info of the login key.
pub const LOGIN_INFO: &[u8] = b"shelfgauge login key";

/// The HKDF info of the wrapping key.
pub const WRAPPING_INFO: &[u8] = b"shelfgauge wrapping key";

/// The fewest characters of a new hub password.
pub const MIN_PASSWORD_CHARS: usize = 10;

/// The two keys of a password. `Debug` hides them.
#[derive(Clone, PartialEq, Eq)]
pub struct PasswordKeys {
    /// The key whose SHA-256 the hub compares at sign-in.
    pub login_key: [u8; 32],
    /// The key that seals the account's key bundle.
    pub wrapping_key: [u8; 32],
}

impl fmt::Debug for PasswordKeys {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PasswordKeys(..)")
    }
}

/// Why keys could not be derived.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PasswordError {
    /// The parameters are below [`MIN_MEMORY_KIB`] or [`MIN_ITERATIONS`].
    #[error("the password parameters are weaker than this shelfgauge allows")]
    WeakParameters,
    /// The salt is not [`SALT_LEN`] bytes.
    #[error("the password salt has the wrong length")]
    BadSalt,
    /// Argon2 refused the input (e.g. memory too large to allocate).
    #[error("the password cannot be derived: {0}")]
    Argon2(String),
}

/// Derives the login and wrapping keys of `password` with the account's `salt` and
/// Argon2id parameters.
///
/// # Examples
///
/// ```
/// use shelfgauge_core::password::{MIN_ITERATIONS, MIN_MEMORY_KIB, derive};
///
/// let keys = derive("correct horse", &[7; 16], MIN_MEMORY_KIB, MIN_ITERATIONS).unwrap();
/// assert_ne!(keys.login_key, keys.wrapping_key);
/// assert!(derive("correct horse", &[7; 16], 1024, MIN_ITERATIONS).is_err());
/// ```
pub fn derive(
    password: &str,
    salt: &[u8],
    memory_kib: u32,
    iterations: u32,
) -> Result<PasswordKeys, PasswordError> {
    if memory_kib < MIN_MEMORY_KIB || iterations < MIN_ITERATIONS {
        return Err(PasswordError::WeakParameters);
    }
    if salt.len() != SALT_LEN {
        return Err(PasswordError::BadSalt);
    }
    let params = Params::new(memory_kib, iterations, 1, Some(32))
        .map_err(|e| PasswordError::Argon2(e.to_string()))?;
    let normalized: String = password.nfkc().collect();
    let mut stretched = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(normalized.as_bytes(), salt, &mut stretched)
        .map_err(|e| PasswordError::Argon2(e.to_string()))?;
    // HKDF-Extract with no salt (a string of zeros), then one block of Expand per key.
    let prk = hmac_sha256(&[0; 32], &[&stretched]);
    Ok(PasswordKeys {
        login_key: hmac_sha256(&prk, &[LOGIN_INFO, &[1]]),
        wrapping_key: hmac_sha256(&prk, &[WRAPPING_INFO, &[1]]),
    })
}

/// HMAC-SHA256 of the concatenated `parts` under `key`.
fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac =
        <hmac::Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC takes any key length");
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hkdf_matches_rfc_5869_with_a_zero_salt() {
        // RFC 5869, test case 3: IKM 22 × 0x0b, no salt, no info, L = 42; the first
        // block of T is the first 32 bytes of the OKM.
        let prk = hmac_sha256(&[0; 32], &[&[0x0b; 22]]);
        let okm = hmac_sha256(&prk, &[&[], &[1]]);
        let expected = "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d";
        let hex: String = okm.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected);
    }

    #[test]
    fn the_same_password_in_another_normal_form_gives_the_same_keys() {
        let composed = derive("Café ﬁle", &[1; 16], MIN_MEMORY_KIB, MIN_ITERATIONS).unwrap();
        let decomposed =
            derive("Cafe\u{301} file", &[1; 16], MIN_MEMORY_KIB, MIN_ITERATIONS).unwrap();
        assert_eq!(composed, decomposed);
        let other_salt = derive("Café ﬁle", &[2; 16], MIN_MEMORY_KIB, MIN_ITERATIONS).unwrap();
        assert_ne!(composed.login_key, other_salt.login_key);
        assert_eq!(format!("{composed:?}"), "PasswordKeys(..)");
    }

    #[test]
    fn weak_parameters_and_bad_salts_are_refused() {
        assert_eq!(
            derive("x", &[0; 16], MIN_MEMORY_KIB - 1, MIN_ITERATIONS),
            Err(PasswordError::WeakParameters)
        );
        assert_eq!(
            derive("x", &[0; 16], MIN_MEMORY_KIB, 1),
            Err(PasswordError::WeakParameters)
        );
        assert_eq!(
            derive("x", &[0; 15], MIN_MEMORY_KIB, MIN_ITERATIONS),
            Err(PasswordError::BadSalt)
        );
    }
}
