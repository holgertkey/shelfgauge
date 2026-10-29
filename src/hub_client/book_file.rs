//! Book files (`.sgbook`, concept §11 "Book files"): a publication in one file that
//! travels by any means and opens in the hub's reader app on every device signed in
//! to the library.
//!
//! The container is [`shelfgauge_core::publication::container`]: `mimetype`, `sg.json`,
//! then the sealed blobs at their hub paths. The book key is in the manifest's header
//! ([`KeyHeader`]), sealed under the library key, as in a publication on the hub.
//! `sg.json` (container version 2) holds no book data:
//!
//! - `format` (the container version), `mime`, `format_version` (of the blobs);
//! - `publication` (hex), `library` (hex; to find the device's account of that library,
//!   or to say "not signed in to this library" before anything is decrypted),
//!   `key_scope` and `key_id` (which key sealed the book key);
//! - `account` (hex) and `snapshot`: the exporting person's `shelf_item`,
//!   `annotation` and `position` records of the book, sealed under their account key
//!   exactly as they are pushed (`{kind, id, data}`, data base64url), so that person's
//!   devices show the book and their marks before the first sync; other members'
//!   devices skip it.
//!
//! [`open`] is the reference for the reader app's import: the checks run in the same
//! order, and nothing is decrypted before the library and key generation match. A
//! file of container version 1 (0.19–0.20) is refused as made by an older shelfgauge.

use std::io::{Cursor, Write};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use zip::CompressionMethod;
use zip::write::{SimpleFileOptions, ZipWriter};

use super::keys::{self, AccountKey};
use super::records;
use crate::hub::AccountId;
use crate::profiles::ProfileId;
use crate::publication::BookKey;
use crate::publication::format::container::{
    self, CONTAINER_VERSION, ContainerError, MIME, Name, OLD_CONTAINER_VERSION,
};
use crate::publication::format::{
    BlobPath, Envelope, KeyHeader, LIBRARY_SCOPE, PublicationId, SUPPORTED_VERSIONS,
};
use crate::storage::StorageError;

/// A sealed record in a file's snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRecord {
    /// `shelf_item`, `annotation` or `position`.
    pub kind: String,
    /// The record id.
    pub id: String,
    /// The sealed record, base64url.
    pub data: String,
}

/// The `sg.json` entry of a book file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SgJson {
    /// The container version ([`CONTAINER_VERSION`]).
    pub format: u16,
    /// The media type ([`MIME`]).
    pub mime: String,
    /// The publication format version of the blobs.
    pub format_version: u16,
    /// The publication id, hex.
    pub publication: String,
    /// The public id of the publishing library, hex.
    pub library: String,
    /// Whose key sealed the book key in the manifest's header: [`LIBRARY_SCOPE`].
    pub key_scope: String,
    /// The generation of that key.
    pub key_id: u32,
    /// The public id of the account whose records are in `snapshot`, hex.
    pub account: String,
    /// That account's records of the book at export time.
    #[serde(default)]
    pub snapshot: Vec<SnapshotRecord>,
}

/// The `sg.json` of publication `id` of `library`, sealed under generation `key_id`
/// of its key, with `account`'s snapshot.
pub fn sg_json(
    library: &AccountId,
    key_id: u32,
    account: &AccountId,
    id: &PublicationId,
    format_version: u16,
    snapshot: Vec<SnapshotRecord>,
) -> SgJson {
    SgJson {
        format: CONTAINER_VERSION,
        mime: MIME.to_owned(),
        format_version,
        publication: id.to_string(),
        library: library.to_string(),
        key_scope: LIBRARY_SCOPE.to_owned(),
        key_id,
        account: account.to_string(),
        snapshot,
    }
}

/// The book file: `mimetype`, `sg.json` and the sealed blobs (the manifest first, as
/// [`Publication::seal`](crate::publication::Publication::seal) gives them).
pub fn write(sg: &SgJson, blobs: &[(BlobPath, Vec<u8>)]) -> Vec<u8> {
    let json = serde_json::to_vec(sg).expect("sg.json always serializes");
    let size: usize = blobs.iter().map(|(_, b)| b.len() + 80).sum::<usize>() + json.len();
    let mut zip = ZipWriter::new(Cursor::new(Vec::with_capacity(size + 1024)));
    // Stored, no extra fields, a fixed time and system: the layout of the container
    // module, and the same bytes on every platform.
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default())
        .system(zip::System::Unix)
        .unix_permissions(0o644);
    let entries = [
        ("mimetype".to_owned(), MIME.as_bytes()),
        (container::SG_JSON.to_owned(), json.as_slice()),
    ]
    .into_iter()
    .chain(blobs.iter().map(|(p, b)| (p.to_string(), b.as_slice())));
    for (name, data) in entries {
        zip.start_file(name, options)
            .and_then(|()| zip.write_all(data).map_err(Into::into))
            .expect("writing into memory does not fail");
    }
    zip.finish()
        .expect("writing into memory does not fail")
        .into_inner()
}

/// The snapshot of the book with `content_key`: its shelf item, annotations (also
/// deletions by the person) and position, sealed as they are pushed. Records larger
/// than the hub takes are left out, as in a push.
pub fn snapshot(
    conn: &Connection,
    profile: ProfileId,
    key: &AccountKey,
    content_key: &[u8],
) -> Result<Vec<SnapshotRecord>, SnapshotError> {
    let mut plain = Vec::new();
    if let Some(item) = records::shelf_item(conn, profile, content_key)? {
        plain.push(("shelf_item", keys::shelf_id(key, content_key), item));
    }
    let mut stmt = conn.prepare(
        "SELECT id FROM annotations
          WHERE profile_id = ?1 AND book_key = ?2 AND deleted_reason IS NOT 'book_removed'
          ORDER BY id",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![profile.0, content_key], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for id in ids {
        if let Some(record) = records::annotation(conn, profile, &id)? {
            plain.push(("annotation", id, record.plain));
        }
    }
    if let Some(position) = records::position(conn, profile, content_key)? {
        plain.push(("position", keys::position_id(key, content_key), position));
    }
    let mut out = Vec::with_capacity(plain.len());
    for (kind, id, plain) in plain {
        let data = keys::seal_record(key, kind, &id, &plain)?;
        if data.len() <= super::sync::MAX_RECORD {
            out.push(SnapshotRecord {
                kind: kind.to_owned(),
                id,
                data: URL_SAFE_NO_PAD.encode(data),
            });
        }
    }
    Ok(out)
}

/// A snapshot could not be made.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The user-data database failed.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// No randomness for a nonce.
    #[error(transparent)]
    Random(#[from] getrandom::Error),
}

impl From<rusqlite::Error> for SnapshotError {
    fn from(e: rusqlite::Error) -> Self {
        Self::Storage(e.into())
    }
}

/// Why a book file does not open here (the reader app shows the same cases).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FileError {
    /// Not a book file, or a damaged one.
    #[error(transparent)]
    Container(#[from] ContainerError),
    /// `sg.json` is not what this version writes.
    #[error("the book file is damaged ({0})")]
    SgJson(String),
    /// A container or blob format this version does not read.
    #[error("this book file needs a newer version of shelfgauge")]
    Unsupported,
    /// Made by shelfgauge 0.19–0.20, whose keys no longer exist.
    #[error("this book file was made by an older shelfgauge; get it again from your library")]
    Old,
    /// From another library: nothing was decrypted.
    #[error("this book file is from a library this device is not signed in to")]
    OtherLibrary,
    /// Sealed with a generation of the library key this client does not have yet.
    #[error("this book file needs a newer library key (key {0}): sync first")]
    UnknownKey(u32),
    /// The book key does not open: the file was changed.
    #[error("the book file is damaged (its key does not open)")]
    Decrypt,
}

/// A book file opened with the library key.
#[derive(Debug)]
pub struct Opened<'a> {
    /// Its `sg.json`.
    pub sg: SgJson,
    /// The publication id.
    pub id: PublicationId,
    /// The book key.
    pub book_key: BookKey,
    /// The sealed blobs by path.
    pub blobs: Vec<(BlobPath, &'a [u8])>,
}

/// Opens a book file for library `library` with the generations of its key: the
/// container, then `sg.json` (versions, library, key scope and generation), then the
/// manifest's header, and only then the book key. The blobs stay sealed.
pub fn open<'a>(
    file: &'a [u8],
    library: &AccountId,
    library_keys: &[(u32, AccountKey)],
) -> Result<Opened<'a>, FileError> {
    let entries = container::read(file)?;
    #[derive(Deserialize)]
    struct Format {
        format: u16,
    }
    let format: Format =
        serde_json::from_slice(entries[1].1).map_err(|e| FileError::SgJson(e.to_string()))?;
    if format.format == OLD_CONTAINER_VERSION {
        return Err(FileError::Old);
    }
    if format.format != CONTAINER_VERSION {
        return Err(FileError::Unsupported);
    }
    let sg: SgJson =
        serde_json::from_slice(entries[1].1).map_err(|e| FileError::SgJson(e.to_string()))?;
    if !SUPPORTED_VERSIONS.contains(sg.format_version) || sg.key_scope != LIBRARY_SCOPE {
        return Err(FileError::Unsupported);
    }
    if sg.mime != MIME {
        return Err(FileError::SgJson("another media type".to_owned()));
    }
    let owner: AccountId = sg
        .library
        .parse()
        .map_err(|_| FileError::SgJson("a bad library id".to_owned()))?;
    let id: PublicationId = sg
        .publication
        .parse()
        .map_err(|_| FileError::SgJson("a bad publication id".to_owned()))?;
    if owner != *library {
        return Err(FileError::OtherLibrary);
    }
    let Some((_, library_key)) = library_keys.iter().find(|(key_id, _)| *key_id == sg.key_id)
    else {
        return Err(FileError::UnknownKey(sg.key_id));
    };
    let blobs = entries[2..]
        .iter()
        .filter_map(|(name, data)| match name {
            Name::Blob(path) => Some((*path, *data)),
            _ => None,
        })
        .collect::<Vec<_>>();
    let Some((BlobPath::Manifest, manifest)) = blobs.first() else {
        return Err(FileError::SgJson("no manifest".to_owned()));
    };
    let envelope = Envelope::parse(manifest).map_err(|_| FileError::Unsupported)?;
    let header = KeyHeader::decode(envelope.header)
        .map_err(|_| FileError::SgJson("a bad manifest header".to_owned()))?;
    if header.library != sg.library
        || header.key_scope != sg.key_scope
        || header.key_id != sg.key_id
    {
        return Err(FileError::SgJson(
            "the manifest names another key".to_owned(),
        ));
    }
    let sealed = URL_SAFE_NO_PAD
        .decode(&header.book_key)
        .map_err(|_| FileError::SgJson("a bad book key".to_owned()))?;
    let raw = keys::open_record(library_key, keys::BOOK_KEY_KIND, &sg.publication, &sealed)
        .map_err(|_| FileError::Decrypt)?;
    let book_key = BookKey::new(raw.try_into().map_err(|_| FileError::Decrypt)?);
    Ok(Opened {
        sg,
        id,
        book_key,
        blobs,
    })
}

/// The name of the book file: `Authors - Title.sgbook`, without characters file
/// systems refuse or that would leave the name (path separators, controls).
///
/// # Examples
///
/// ```
/// use shelfgauge::hub_client::book_file::file_name;
///
/// assert_eq!(file_name("A/B: c?", "Ann, Bob"), "Ann, Bob - A_B_ c_.sgbook");
/// assert_eq!(file_name("  ", ""), "book.sgbook");
/// assert_eq!(file_name("...", ""), "book.sgbook");
/// ```
pub fn file_name(title: &str, authors: &str) -> String {
    let clean = |s: &str| -> String {
        s.chars()
            .map(|c| {
                if c.is_control() || "/\\:*?\"<>|".contains(c) {
                    '_'
                } else {
                    c
                }
            })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    let (title, authors) = (clean(title), clean(authors));
    let mut stem = match (authors.is_empty(), title.is_empty()) {
        (_, true) => "book".to_owned(),
        (true, false) => title,
        (false, false) => format!("{authors} - {title}"),
    };
    // Most file systems allow 255 bytes per name.
    while stem.len() > 200 {
        stem.pop();
    }
    let stem = match stem.trim_end_matches(['.', ' ']) {
        "" => "book",
        stem => stem,
    };
    format!("{stem}.{}", container::EXTENSION)
}
