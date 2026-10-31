//! Sealing and opening a library's catalog blobs (`shelfgauge_core::catalog`, re-exported
//! here as [`format`](mod@format)): postcard, raw deflate, padding, AES-256-GCM under the
//! catalog key with the catalog's associated data.
//!
//! The catalog key is a secret of the library and its members: `Debug` hides it, and
//! neither it nor book ids or titles are logged.

use std::fmt;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use super::keys::{AccountKey, open_record, seal_record};
use crate::hub::AccountId;
use crate::publication::OpenError;
use crate::publication::format::{LIBRARY_SCOPE, NONCE_LEN, pad, unpad};
use crate::publication::seal::{aes_decrypt, aes_encrypt};
use crate::publication::{compress, inflate};
use format::{
    CATALOG_FORMAT_VERSION, CATALOG_KEY_KIND, CatalogHeader, CatalogPath, associated_data,
    parse_envelope,
};
pub use shelfgauge_core::catalog as format;

/// A catalog key: 32 random bytes encrypting one library's catalog blobs
/// (AES-256-GCM). `Debug` hides it.
#[derive(Clone, PartialEq, Eq)]
pub struct CatalogKey([u8; 32]);

impl CatalogKey {
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
}

impl fmt::Debug for CatalogKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CatalogKey(..)")
    }
}

/// Encrypts an already compressed and padded catalog blob with the given nonce and
/// wraps it with the plaintext `header` (empty but for the index). Only for fixed test
/// vectors: [`seal`] picks a fresh nonce.
pub fn encrypt_padded(
    library: &AccountId,
    key: &CatalogKey,
    path: CatalogPath,
    header: &[u8],
    padded: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Vec<u8> {
    let ad = associated_data(CATALOG_FORMAT_VERSION, library.as_bytes(), path, header);
    let ciphertext = aes_encrypt(key.as_bytes(), &ad, padded, nonce);
    crate::publication::format::Envelope {
        version: CATALOG_FORMAT_VERSION,
        header,
        nonce: &nonce,
        ciphertext: &ciphertext,
    }
    .to_bytes()
}

/// Seals the plaintext of the catalog blob at `path` of `library`: compress, pad,
/// encrypt under `key` with a fresh random nonce, with the plaintext `header`.
pub fn seal(
    library: &AccountId,
    key: &CatalogKey,
    path: CatalogPath,
    header: &[u8],
    plain: &[u8],
) -> Result<Vec<u8>, getrandom::Error> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce)?;
    Ok(encrypt_padded(
        library,
        key,
        path,
        header,
        &pad(&compress(plain)),
        nonce,
    ))
}

/// Opens the catalog blob at `path` of `library` with `key`: the plaintext, at most
/// `limit` bytes.
pub fn open(
    library: &AccountId,
    key: &CatalogKey,
    path: CatalogPath,
    blob: &[u8],
    limit: u64,
) -> Result<Vec<u8>, OpenError> {
    let envelope = parse_envelope(blob)?;
    let ad = associated_data(envelope.version, library.as_bytes(), path, envelope.header);
    let padded = aes_decrypt(key.as_bytes(), &ad, envelope.ciphertext, envelope.nonce)?;
    inflate(unpad(&padded)?, limit)
}

/// The index header of `library`'s catalog: its catalog key sealed under generation
/// `key_id` of the library key, as a record of kind [`CATALOG_KEY_KIND`] with the
/// library id (hex) as its id.
pub fn catalog_header(
    library: &AccountId,
    key_id: u32,
    library_key: &AccountKey,
    catalog_key: &CatalogKey,
) -> Result<Vec<u8>, getrandom::Error> {
    let sealed = seal_record(
        library_key,
        CATALOG_KEY_KIND,
        &library.to_string(),
        catalog_key.as_bytes(),
    )?;
    Ok(CatalogHeader {
        library: library.to_string(),
        key_scope: LIBRARY_SCOPE.to_owned(),
        key_owner: library.to_string(),
        key_id,
        catalog_key: URL_SAFE_NO_PAD.encode(sealed),
    }
    .encode())
}

/// The catalog key out of an index header, with the library key of generation
/// [`CatalogHeader::key_id`]; `None` if it does not open.
pub fn open_catalog_key(header: &CatalogHeader, library_key: &AccountKey) -> Option<CatalogKey> {
    let sealed = URL_SAFE_NO_PAD.decode(&header.catalog_key).ok()?;
    let plain = open_record(library_key, CATALOG_KEY_KIND, &header.library, &sealed).ok()?;
    Some(CatalogKey(plain.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub_client::keys::BOOK_KEY_KIND;
    use format::{Chunk, MAX_CHUNK_LEN};

    #[test]
    fn blobs_open_only_at_their_place() {
        let library = AccountId::new([1; 16]);
        let key = CatalogKey::new([2; 32]);
        let plain = Chunk::default().encode();
        let blob = seal(&library, &key, CatalogPath::Chunk(0), b"", &plain).unwrap();
        assert_eq!(
            open(&library, &key, CatalogPath::Chunk(0), &blob, MAX_CHUNK_LEN).unwrap(),
            plain
        );
        for (library, path) in [
            (library, CatalogPath::Chunk(1)),
            (library, CatalogPath::Index),
            (AccountId::new([3; 16]), CatalogPath::Chunk(0)),
        ] {
            assert!(matches!(
                open(&library, &key, path, &blob, MAX_CHUNK_LEN),
                Err(OpenError::Decrypt)
            ));
        }
        // Never a publication blob.
        assert!(crate::publication::format::Envelope::parse(&blob).is_err());
    }

    #[test]
    fn the_catalog_key_opens_from_the_header_and_never_as_a_book_key() {
        let library = AccountId::new([4; 16]);
        let library_key = AccountKey::new([5; 32]);
        let key = CatalogKey::new([6; 32]);
        let header =
            CatalogHeader::decode(&catalog_header(&library, 2, &library_key, &key).unwrap())
                .unwrap();
        assert_eq!(
            (header.key_scope.as_str(), header.key_id),
            (LIBRARY_SCOPE, 2)
        );
        assert_eq!(open_catalog_key(&header, &library_key), Some(key));
        assert_eq!(open_catalog_key(&header, &AccountKey::new([7; 32])), None);
        let sealed = URL_SAFE_NO_PAD.decode(&header.catalog_key).unwrap();
        assert!(open_record(&library_key, BOOK_KEY_KIND, &header.library, &sealed).is_err());
        assert_eq!(format!("{:?}", CatalogKey::new([1; 32])), "CatalogKey(..)");
    }
}
