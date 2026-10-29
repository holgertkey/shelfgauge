//! The keys of the hub and what they protect: sync records, device names and record
//! ids under an account key; book keys under a library key; invite payloads under a
//! one-time key; the account key under a password's wrapping key.
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
use sha2::Sha256;

use crate::hub::AccountId;
use crate::publication::BookKey;
use crate::publication::format::{KeyHeader, LIBRARY_SCOPE, NONCE_LEN, PublicationId};

/// The version of the record layout (and of its associated data).
pub const RECORD_VERSION: u16 = 1;

/// The kind under which device names are sealed (not a record kind of the hub).
pub const DEVICE_NAME_KIND: &str = "device";

/// The record kinds the hub accepts.
pub const RECORD_KINDS: &[&str] = &[
    "shelf_item",
    "annotation",
    "position",
    LIBRARY_KEY_KIND,
    PROFILE_KIND,
];

/// A 32-byte record key: an account key (the account's records), and in the same
/// shape a library key, an invite's one-time key or a password's wrapping key. Known
/// only to the clients; `Debug` hides it.
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

/// The kind of the records that carry a library key generation into an account (only
/// the library writes them).
pub const LIBRARY_KEY_KIND: &str = "library_key";

/// The kind of the record a member's device writes with their display name and colour
/// (only the member's devices write it; the library reads it).
pub const PROFILE_KIND: &str = "profile";

/// The record id of the `profile` record.
pub const PROFILE_ID: &str = "profile";

/// The kind under which a book key is sealed in a manifest header (not a record kind
/// of the hub; the id is the publication id in hex).
pub const BOOK_KEY_KIND: &str = "book_key";

/// The kind under which an invite's payload is sealed under its one-time key (empty
/// id).
pub const INVITE_KIND: &str = "invite";

/// The kind under which the account key is sealed under a password's wrapping key
/// (the bundle; empty id).
pub const BUNDLE_KIND: &str = "bundle";

/// The record id of library key generation `key_id`: `k<key_id>`.
pub fn library_key_id(key_id: u32) -> String {
    format!("k{key_id}")
}

/// The plaintext of a `library_key` record: JSON `{key_id, key, created_at}` (the key
/// as base64url).
pub fn library_key_plain(key_id: u32, key: &AccountKey, created_at: i64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "key_id": key_id,
        "key": key.to_text(),
        "created_at": created_at,
    }))
    .expect("JSON values always serialize")
}

/// The manifest header of publication `id` of library `library`: its book key sealed
/// under generation `key_id` of the library key (see
/// [`KeyHeader`]).
pub fn key_header(
    library: &AccountId,
    key_id: u32,
    library_key: &AccountKey,
    id: &PublicationId,
    book_key: &BookKey,
) -> Result<Vec<u8>, getrandom::Error> {
    let sealed = seal_record(
        library_key,
        BOOK_KEY_KIND,
        &id.to_string(),
        book_key.as_bytes(),
    )?;
    Ok(KeyHeader {
        library: library.to_string(),
        key_scope: LIBRARY_SCOPE.to_owned(),
        key_owner: library.to_string(),
        key_id,
        book_key: URL_SAFE_NO_PAD.encode(sealed),
    }
    .encode())
}

/// What an invite link's payload tells the person's device, sealed under the link's
/// one-time key: the library, the name the admin gave them, their account and its key,
/// and the current library key generation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InvitePayload {
    /// The library's public id, hex.
    pub library_id: String,
    /// The library's name.
    pub library_name: String,
    /// The name the admin gave the profile.
    pub profile_name: String,
    /// The account key, base64url.
    pub account_key: String,
    /// The current library key generation.
    pub library_key: InviteLibraryKey,
}

/// The library key in an [`InvitePayload`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InviteLibraryKey {
    /// The generation.
    pub key_id: u32,
    /// The key, base64url.
    pub key: String,
}

/// Seals an invite payload under the one-time key.
pub fn seal_invite(
    one_time: &AccountKey,
    payload: &InvitePayload,
) -> Result<Vec<u8>, getrandom::Error> {
    let plain = serde_json::to_vec(payload).expect("an invite payload always serializes");
    seal_record(one_time, INVITE_KIND, "", &plain)
}

/// Opens an invite payload; `None` when it does not open or is not one.
pub fn open_invite(one_time: &AccountKey, sealed: &[u8]) -> Option<InvitePayload> {
    let plain = open_record(one_time, INVITE_KIND, "", sealed).ok()?;
    serde_json::from_slice(&plain).ok()
}

/// Seals the account key under a password's wrapping key (the bundle the hub keeps).
pub fn seal_bundle(
    wrapping: &AccountKey,
    account: &AccountKey,
) -> Result<Vec<u8>, getrandom::Error> {
    seal_record(wrapping, BUNDLE_KIND, "", account.as_bytes())
}

/// Opens a bundle with the wrapping key; `None` when it does not open.
pub fn open_bundle(wrapping: &AccountKey, sealed: &[u8]) -> Option<AccountKey> {
    let plain = open_record(wrapping, BUNDLE_KIND, "", sealed).ok()?;
    Some(AccountKey::new(plain.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_book_key_comes_out_of_the_header_with_the_library_key_alone() {
        let library = AccountId::new([0x0f; 16]);
        let library_key = AccountKey::new([0x24; 32]);
        let id = PublicationId::new([7; 16]);
        let book_key = BookKey::new([9; 32]);
        let header = key_header(&library, 3, &library_key, &id, &book_key).unwrap();
        let parsed = KeyHeader::decode(&header).unwrap();
        assert_eq!(parsed.library, library.to_string());
        assert_eq!(
            (parsed.key_scope.as_str(), parsed.key_id),
            (LIBRARY_SCOPE, 3)
        );
        let sealed = URL_SAFE_NO_PAD.decode(&parsed.book_key).unwrap();
        let opened = open_record(&library_key, BOOK_KEY_KIND, &id.to_string(), &sealed).unwrap();
        assert_eq!(opened, book_key.as_bytes());
        // Bound to the publication.
        let other = PublicationId::new([8; 16]).to_string();
        assert!(open_record(&library_key, BOOK_KEY_KIND, &other, &sealed).is_err());
    }

    #[test]
    fn invites_and_bundles_open_only_with_their_keys() {
        let one_time = AccountKey::new([1; 32]);
        let payload = InvitePayload {
            library_id: "0f".repeat(16),
            library_name: "Home".into(),
            profile_name: "Grandma".into(),
            account_key: AccountKey::new([2; 32]).to_text(),
            library_key: InviteLibraryKey {
                key_id: 1,
                key: AccountKey::new([3; 32]).to_text(),
            },
        };
        let sealed = seal_invite(&one_time, &payload).unwrap();
        assert_eq!(open_invite(&one_time, &sealed), Some(payload));
        assert_eq!(open_invite(&AccountKey::new([9; 32]), &sealed), None);
        let wrapping = AccountKey::new([4; 32]);
        let account = AccountKey::new([5; 32]);
        let bundle = seal_bundle(&wrapping, &account).unwrap();
        assert_eq!(open_bundle(&wrapping, &bundle), Some(account));
        assert_eq!(open_bundle(&one_time, &bundle), None);
        let plain = library_key_plain(2, &AccountKey::new([3; 32]), 100);
        let json: serde_json::Value = serde_json::from_slice(&plain).unwrap();
        assert_eq!(json["key_id"], 2);
        assert_eq!(library_key_id(2), "k2");
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
