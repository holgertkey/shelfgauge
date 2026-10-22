//! The account key and what it protects: sync records, device names, record ids and
//! the recovery code.
//!
//! Records are AES-256-GCM sealed under the account key as
//! `[record version u16 BE][nonce 12][ciphertext + tag]`; the associated data binds
//! the version, the record's kind and its id, so the hub cannot move a ciphertext to
//! another record. The reader app (Phase 26) opens them with WebCrypto; the interop
//! vectors are in `tests/fixtures/publication/records.json`.

use std::fmt;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

use crate::hub::{AccountId, Secret};
use crate::publication::format::NONCE_LEN;

/// The version of the record layout (and of its associated data).
pub const RECORD_VERSION: u16 = 1;

/// The kind under which device names are sealed (not a record kind of the hub).
pub const DEVICE_NAME_KIND: &str = "device";

/// The record kinds the hub accepts.
pub const RECORD_KINDS: &[&str] = &["shelf_item", "annotation", "position"];

/// The account key: 32 random bytes that encrypt the account's records. Known only to
/// the account's clients; `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountKey([u8; 32]);

impl AccountKey {
    /// A fresh random key.
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut key = [0u8; 32];
        getrandom::fill(&mut key)?;
        Ok(Self(key))
    }

    /// The key with these bytes.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// The key's bytes.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// The key as unpadded base64url, as in the pairing link's fragment.
    pub fn to_text(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.0)
    }

    fn cipher(&self) -> Aes256Gcm {
        Aes256Gcm::new(&Key::<Aes256Gcm>::from(self.0))
    }

    fn mac(&self, label: &[u8], data: &[u8]) -> String {
        let mut mac = <Hmac<Sha256> as hmac::KeyInit>::new_from_slice(&self.0)
            .expect("HMAC takes keys of any length");
        mac.update(label);
        mac.update(data);
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    }
}

impl fmt::Debug for AccountKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AccountKey(..)")
    }
}

/// The id of a book's `position` record: base64url of
/// HMAC-SHA256(account key, `"position"` ‖ content key). Deterministic per book and
/// account, and it does not reveal the content key.
pub fn position_id(key: &AccountKey, content_key: &[u8]) -> String {
    key.mac(b"position", content_key)
}

/// The id of a book's `shelf_item` record: base64url of
/// HMAC-SHA256(account key, `"shelf"` ‖ content key), so every instance of the account
/// updates one shelf item per book.
pub fn shelf_id(key: &AccountKey, content_key: &[u8]) -> String {
    key.mac(b"shelf", content_key)
}

/// The associated data of a record: `"sg-record"`, the version (u16 BE), the kind, a
/// zero byte and the record id.
pub fn record_associated_data(version: u16, kind: &str, id: &str) -> Vec<u8> {
    let mut ad = Vec::with_capacity(9 + 2 + kind.len() + 1 + id.len());
    ad.extend_from_slice(b"sg-record");
    ad.extend_from_slice(&version.to_be_bytes());
    ad.extend_from_slice(kind.as_bytes());
    ad.push(0);
    ad.extend_from_slice(id.as_bytes());
    ad
}

/// Why a record could not be opened.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecordError {
    /// Shorter than its header and tag.
    #[error("the record is truncated")]
    Truncated,
    /// A record version this build does not know.
    #[error("unsupported record version {0}")]
    UnsupportedVersion(u16),
    /// Another key, kind or id, or altered data.
    #[error("the record does not decrypt with this key at this place")]
    Decrypt,
}

/// Encrypts a record with the given nonce. Only for fixed test vectors: [`seal_record`]
/// picks a fresh nonce, and a nonce must never be used twice with one key.
pub fn encrypt_record(
    key: &AccountKey,
    kind: &str,
    id: &str,
    plain: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Vec<u8> {
    let ad = record_associated_data(RECORD_VERSION, kind, id);
    let ciphertext = key
        .cipher()
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plain,
                aad: &ad,
            },
        )
        .expect("AES-GCM encrypts any record below 64 GiB");
    let mut out = Vec::with_capacity(2 + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(&RECORD_VERSION.to_be_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    out
}

/// Seals the plaintext of record `id` of `kind` under the account key, with a fresh
/// random nonce.
pub fn seal_record(
    key: &AccountKey,
    kind: &str,
    id: &str,
    plain: &[u8],
) -> Result<Vec<u8>, getrandom::Error> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce)?;
    Ok(encrypt_record(key, kind, id, plain, nonce))
}

/// Opens record `id` of `kind`.
///
/// # Examples
///
/// ```
/// use shelfgauge::hub_client::keys::{AccountKey, open_record, seal_record};
///
/// let key = AccountKey::new([7; 32]);
/// let sealed = seal_record(&key, "annotation", "a1", b"{}").unwrap();
/// assert_eq!(open_record(&key, "annotation", "a1", &sealed).unwrap(), b"{}");
/// assert!(open_record(&key, "annotation", "a2", &sealed).is_err());
/// ```
pub fn open_record(
    key: &AccountKey,
    kind: &str,
    id: &str,
    data: &[u8],
) -> Result<Vec<u8>, RecordError> {
    if data.len() < 2 + NONCE_LEN + 16 {
        return Err(RecordError::Truncated);
    }
    let version = u16::from_be_bytes([data[0], data[1]]);
    if version != RECORD_VERSION {
        return Err(RecordError::UnsupportedVersion(version));
    }
    let nonce: [u8; NONCE_LEN] = data[2..2 + NONCE_LEN].try_into().expect("checked length");
    let ad = record_associated_data(version, kind, id);
    key.cipher()
        .decrypt(
            &Nonce::from(nonce),
            Payload {
                msg: &data[2 + NONCE_LEN..],
                aad: &ad,
            },
        )
        .map_err(|_| RecordError::Decrypt)
}

/// Seals a device name (kind [`DEVICE_NAME_KIND`], empty id).
pub fn seal_device_name(key: &AccountKey, name: &str) -> Result<Vec<u8>, getrandom::Error> {
    seal_record(key, DEVICE_NAME_KIND, "", name.as_bytes())
}

/// Opens a device name; `None` when it does not open or is not text.
pub fn open_device_name(key: &AccountKey, data: &[u8]) -> Option<String> {
    let plain = open_record(key, DEVICE_NAME_KIND, "", data).ok()?;
    String::from_utf8(plain).ok()
}

/// The Crockford base32 alphabet (no I, L, O, U).
const BASE32: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The prefix of a recovery code; its digit is the code's version.
const RECOVERY_PREFIX: &str = "SG1";

/// The bytes inside a recovery code: account id, account key, recovery secret.
const RECOVERY_LEN: usize = 16 + 32 + 32;

/// The checksum bytes after them (the first bytes of their SHA-256).
const CHECKSUM_LEN: usize = 2;

/// A recovery code: what a new instance needs to join the account (its id, the account
/// key and the recovery secret, whose SHA-256 the hub keeps). Written as `SG1-` and
/// Crockford base32 in groups of four; `Debug` hides it, and there is no `Display`.
///
/// # Examples
///
/// ```
/// use shelfgauge::hub::{AccountId, Secret};
/// use shelfgauge::hub_client::keys::{AccountKey, RecoveryCode};
///
/// let code = RecoveryCode::new(AccountId::new([1; 16]), AccountKey::new([2; 32]), [3; 32]);
/// let text = code.to_text();
/// assert!(text.starts_with("SG1-"));
/// let parsed = RecoveryCode::parse(&text.to_lowercase().replace('-', " ")).unwrap();
/// assert_eq!(parsed.secret().as_str(), code.secret().as_str());
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct RecoveryCode {
    account: AccountId,
    key: AccountKey,
    secret: [u8; 32],
}

impl fmt::Debug for RecoveryCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RecoveryCode(..)")
    }
}

/// The text of the recovery secret with these bytes, as the hub expects it.
pub fn recovery_secret(bytes: &[u8; 32]) -> Secret {
    Secret::from_text(&URL_SAFE_NO_PAD.encode(bytes))
}

/// Why a recovery code was not accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RecoveryCodeError {
    /// Not a recovery code of this version.
    #[error("this is not a shelfgauge recovery code (it starts with SG1)")]
    Format,
    /// The checksum does not match: a typing error.
    #[error("the recovery code has a typing error")]
    Checksum,
}

impl RecoveryCode {
    /// A code from its parts.
    pub fn new(account: AccountId, key: AccountKey, secret: [u8; 32]) -> Self {
        Self {
            account,
            key,
            secret,
        }
    }

    /// The account's public id.
    pub fn account(&self) -> AccountId {
        self.account
    }

    /// The account key.
    pub fn key(&self) -> &AccountKey {
        &self.key
    }

    /// The recovery secret as the hub expects it (its text; the hub keeps its SHA-256).
    pub fn secret(&self) -> Secret {
        recovery_secret(&self.secret)
    }

    /// The code's text: `SG1-` and groups of four characters.
    pub fn to_text(&self) -> String {
        let mut bytes = Vec::with_capacity(RECOVERY_LEN + CHECKSUM_LEN);
        bytes.extend_from_slice(self.account.as_bytes());
        bytes.extend_from_slice(self.key.as_bytes());
        bytes.extend_from_slice(&self.secret);
        let sum = Sha256::digest(&bytes);
        bytes.extend_from_slice(&sum[..CHECKSUM_LEN]);
        let digits = base32_encode(&bytes);
        let mut text = RECOVERY_PREFIX.to_owned();
        for group in digits.as_bytes().chunks(4) {
            text.push('-');
            text.push_str(std::str::from_utf8(group).expect("ASCII digits"));
        }
        text
    }

    /// Parses a code; case, dashes and spaces are ignored, and `O`, `I`, `L` read as
    /// `0`, `1`, `1`.
    pub fn parse(text: &str) -> Result<Self, RecoveryCodeError> {
        let cleaned: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .map(|c| c.to_ascii_uppercase())
            .collect();
        let digits = cleaned
            .strip_prefix(RECOVERY_PREFIX)
            .ok_or(RecoveryCodeError::Format)?;
        let bytes = base32_decode(digits).ok_or(RecoveryCodeError::Format)?;
        if bytes.len() != RECOVERY_LEN + CHECKSUM_LEN {
            return Err(RecoveryCodeError::Format);
        }
        let (payload, sum) = bytes.split_at(RECOVERY_LEN);
        if Sha256::digest(payload)[..CHECKSUM_LEN] != *sum {
            return Err(RecoveryCodeError::Checksum);
        }
        Ok(Self {
            account: AccountId::new(payload[..16].try_into().expect("16 bytes")),
            key: AccountKey::new(payload[16..48].try_into().expect("32 bytes")),
            secret: payload[48..].try_into().expect("32 bytes"),
        })
    }
}

/// Crockford base32 of `bytes`, the last digit padded with zero bits.
fn base32_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 8 / 5 + 1);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for &b in bytes {
        buffer = (buffer << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(BASE32[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(BASE32[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// The bytes of Crockford base32 `digits` (uppercase); `None` for other characters or
/// non-zero padding bits.
fn base32_decode(digits: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(digits.len() * 5 / 8);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for c in digits.bytes() {
        let c = match c {
            b'O' => b'0',
            b'I' | b'L' => b'1',
            c => c,
        };
        let value = BASE32.iter().position(|&d| d == c)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    (buffer == 0).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> RecoveryCode {
        RecoveryCode::new(
            AccountId::new([0xa5; 16]),
            AccountKey::new([0x5a; 32]),
            [0x0f; 32],
        )
    }

    #[test]
    fn recovery_codes_round_trip_and_tolerate_typing() {
        let text = code().to_text();
        // 82 bytes are 132 digits in 33 groups.
        assert_eq!(text.len(), 3 + 33 * 5);
        assert_eq!(RecoveryCode::parse(&text).unwrap(), code());
        let sloppy = text.to_lowercase().replace('-', "  ").replace('0', "o");
        assert_eq!(RecoveryCode::parse(&sloppy).unwrap(), code());
    }

    #[test]
    fn recovery_codes_catch_typing_errors() {
        let text = code().to_text();
        // The second digit of the eleventh group (the payload, not the checksum).
        let i = 3 + 5 * 10 + 2;
        let wrong = if &text[i..=i] == "A" { "B" } else { "A" };
        let typo = format!("{}{wrong}{}", &text[..i], &text[i + 1..]);
        assert_eq!(
            RecoveryCode::parse(&typo).unwrap_err(),
            RecoveryCodeError::Checksum
        );
        assert_eq!(
            RecoveryCode::parse(&text[..text.len() - 5]).unwrap_err(),
            RecoveryCodeError::Format
        );
        assert_eq!(
            RecoveryCode::parse("SG2-0000").unwrap_err(),
            RecoveryCodeError::Format
        );
        assert_eq!(
            RecoveryCode::parse(&text.replace("SG1-", "SG1-U")).unwrap_err(),
            RecoveryCodeError::Format
        );
    }

    #[test]
    fn recovery_secret_is_the_text_the_hub_hashes() {
        let secret = code().secret();
        assert_eq!(secret.as_str(), URL_SAFE_NO_PAD.encode([0x0f; 32]));
        assert_eq!(format!("{:?}", code()), "RecoveryCode(..)");
    }

    #[test]
    fn records_are_bound_to_kind_and_id() {
        let key = AccountKey::new([1; 32]);
        let sealed = seal_record(&key, "position", "p", b"data").unwrap();
        assert_eq!(&sealed[..2], &[0, 1]);
        assert_eq!(
            open_record(&key, "position", "p", &sealed).unwrap(),
            b"data"
        );
        for (kind, id) in [("annotation", "p"), ("position", "q"), ("positio", "np")] {
            assert_eq!(
                open_record(&key, kind, id, &sealed),
                Err(RecordError::Decrypt)
            );
        }
        let other = AccountKey::new([2; 32]);
        assert_eq!(
            open_record(&other, "position", "p", &sealed),
            Err(RecordError::Decrypt)
        );
        let mut future = sealed.clone();
        future[1] = 2;
        assert_eq!(
            open_record(&key, "position", "p", &future),
            Err(RecordError::UnsupportedVersion(2))
        );
        assert_eq!(
            open_record(&key, "position", "p", &sealed[..20]),
            Err(RecordError::Truncated)
        );
    }

    #[test]
    fn record_ids_are_deterministic_and_fit_the_hub() {
        let key = AccountKey::new([3; 32]);
        let a = position_id(&key, b"content");
        assert_eq!(a, position_id(&key, b"content"));
        assert_ne!(a, shelf_id(&key, b"content"));
        assert_ne!(a, position_id(&AccountKey::new([4; 32]), b"content"));
        assert_eq!(a.len(), 43);
        assert!(
            a.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        );
    }

    #[test]
    fn device_names_round_trip() {
        let key = AccountKey::new([5; 32]);
        let sealed = seal_device_name(&key, "Library").unwrap();
        assert_eq!(open_device_name(&key, &sealed).as_deref(), Some("Library"));
        assert_eq!(open_device_name(&AccountKey::new([6; 32]), &sealed), None);
        assert_eq!(format!("{key:?}"), "AccountKey(..)");
    }
}
