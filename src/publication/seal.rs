//! Sealing and opening blobs: raw deflate, padding, AES-256-GCM.

use std::io::{Read, Write};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;

use super::BookKey;
use super::format::{
    BlobPath, Envelope, FORMAT_VERSION, FormatError, NONCE_LEN, PublicationId, associated_data,
    pad, unpad,
};

/// Why a blob could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// The framing or the version is wrong.
    #[error(transparent)]
    Format(#[from] FormatError),
    /// Decryption failed: another key, another publication or path, or altered data.
    #[error("the blob does not decrypt with this key at this place")]
    Decrypt,
    /// The decompressed content is larger than allowed.
    #[error("the blob's content is larger than {0} bytes")]
    TooLarge(u64),
    /// The compressed content is broken.
    #[error("the blob's content does not decompress: {0}")]
    Inflate(std::io::Error),
}

fn cipher(key: &BookKey) -> Aes256Gcm {
    Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key.as_bytes()))
}

/// Raw deflate (RFC 1951, no zlib header: `DecompressionStream("deflate-raw")`).
pub fn compress(data: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(data)
        .expect("writing to a Vec cannot fail");
    encoder.finish().expect("writing to a Vec cannot fail")
}

/// Inflates raw deflate data of at most `limit` bytes.
pub fn inflate(data: &[u8], limit: u64) -> Result<Vec<u8>, OpenError> {
    let mut out = Vec::new();
    DeflateDecoder::new(data)
        .take(limit.saturating_add(1))
        .read_to_end(&mut out)
        .map_err(OpenError::Inflate)?;
    if out.len() as u64 > limit {
        return Err(OpenError::TooLarge(limit));
    }
    Ok(out)
}

/// Encrypts an already compressed and padded blob with the given nonce and wraps it
/// in its envelope with the plaintext `header` (empty but for a manifest). Only for
/// fixed test vectors: [`seal`] picks a fresh nonce, and a nonce must never be used
/// twice with one key.
pub fn encrypt_padded(
    id: &PublicationId,
    key: &BookKey,
    path: BlobPath,
    header: &[u8],
    padded: &[u8],
    nonce: [u8; NONCE_LEN],
) -> Vec<u8> {
    let ad = associated_data(FORMAT_VERSION, id, path, header);
    let ciphertext = cipher(key)
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: padded,
                aad: &ad,
            },
        )
        .expect("AES-GCM encrypts any blob below 64 GiB");
    Envelope {
        version: FORMAT_VERSION,
        header,
        nonce: &nonce,
        ciphertext: &ciphertext,
    }
    .to_bytes()
}

/// Seals the plaintext of the blob at `path` of publication `id`: compress, pad,
/// encrypt under `key` with a fresh random nonce, with the plaintext `header`.
pub fn seal(
    id: &PublicationId,
    key: &BookKey,
    path: BlobPath,
    header: &[u8],
    plain: &[u8],
) -> Result<Vec<u8>, getrandom::Error> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce)?;
    Ok(encrypt_padded(
        id,
        key,
        path,
        header,
        &pad(&compress(plain)),
        nonce,
    ))
}

/// Opens the blob at `path` of publication `id` with `key`: the plaintext, at most
/// `limit` bytes.
pub fn open(
    id: &PublicationId,
    key: &BookKey,
    path: BlobPath,
    blob: &[u8],
    limit: u64,
) -> Result<Vec<u8>, OpenError> {
    let envelope = Envelope::parse(blob)?;
    let ad = associated_data(envelope.version, id, path, envelope.header);
    let padded = cipher(key)
        .decrypt(
            &Nonce::from(*envelope.nonce),
            Payload {
                msg: envelope.ciphertext,
                aad: &ad,
            },
        )
        .map_err(|_| OpenError::Decrypt)?;
    inflate(unpad(&padded)?, limit)
}
