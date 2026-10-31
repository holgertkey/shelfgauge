//! The library's catalog on its hub (Phase 31): the books of the Showcase
//! ([`super::showcase`]) as the sealed blobs of `shelfgauge_core::catalog`, built from
//! the index and uploaded by the library's token.
//!
//! [`build`] turns the Showcase into entries (readable books only, one per content,
//! the lowest book id of duplicates); [`sync_catalog`] uploads what changed: the
//! chunks whose plaintext changed (each chunk's hash stored once its upload
//! succeeded, so an interrupted run resumes), then the index under a new generation,
//! then deletes the blobs the index no longer names (worked out from the hub's
//! listing, so crashes and key changes are cleaned up too). A new library key
//! generation makes a new catalog key and uploads everything again. An empty Showcase
//! still has an index (no entries, one empty chunk), so a member's reader tells
//! "nothing on the Showcase" from "never uploaded".
//!
//! The generation is `max(last + 1, now in milliseconds)`: it grows even if this state
//! is lost, since the members' readers refuse a lower one (an upload takes far longer
//! than a millisecond, so the counter never runs ahead of the clock). Never logged: the catalog key, titles,
//! book and cover ids.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use super::catalog::{CatalogKey, catalog_header, seal};
use super::showcase;
use super::store::{self, HubLibrary};
use super::sync::SyncError;
use super::{ClientError, HubClient};
use crate::publication::format::PublicationId;
use crate::storage::StorageError;
use crate::storage::books::catalog_rows;
use shelfgauge_core::catalog::{
    BookId, CATALOG_FORMAT_VERSION, CatalogPath, Chunk, CoverId, Entry, Index, SeriesRef,
    chunk_count, chunk_of, cover_id, cut_description, format_code,
};

/// The catalog as built from the Showcase.
#[derive(Debug, Clone, Default)]
pub struct Built {
    /// The entries, by book id.
    pub entries: Vec<Entry>,
    /// The library's book (its lowest id) of each entry.
    pub books: HashMap<BookId, i64>,
    /// The book of each thumbnail id.
    pub covers: HashMap<CoverId, i64>,
}

/// The publications this library made that are on the hub, by content key (of any
/// profile: every member's device opens them).
pub fn own_publications(
    conn: &Connection,
) -> Result<HashMap<Vec<u8>, PublicationId>, StorageError> {
    let mut stmt = conn.prepare(
        "SELECT book_key, publication_id FROM hub_shelf
          WHERE own = 1 AND kind = 'upload' AND state = 'published'
                AND publication_id IS NOT NULL
          ORDER BY profile_id",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut out = HashMap::new();
    for row in rows {
        let (key, id) = row?;
        if let Ok(id) = <[u8; 16]>::try_from(id) {
            out.entry(key).or_insert(PublicationId::new(id));
        }
    }
    Ok(out)
}

/// The catalog of the books on the Showcase now (`marks`, the folders open), with the
/// publications already on the hub; thumbnail ids under `key`.
pub fn build(
    index: &Connection,
    marks: &[crate::storage::folders::FolderPath],
    published: &HashMap<Vec<u8>, PublicationId>,
    key: &CatalogKey,
) -> Result<Built, StorageError> {
    let ids: Vec<i64> = showcase::showcase_books(index, marks)?
        .into_iter()
        .map(|b| b.id)
        .collect();
    let mut built = Built::default();
    // By book id: of duplicates the lowest id stays.
    for row in catalog_rows(index, &ids)? {
        let Some(content_key) = row.content_key else {
            continue;
        };
        let id = BookId::from_content_key(&content_key);
        if built.books.contains_key(&id) {
            continue;
        }
        built.books.insert(id, row.id);
        let cover = row.has_cover.then(|| cover_id(key.as_bytes(), &id));
        if let Some(cover) = cover {
            built.covers.insert(cover, row.id);
        }
        built.entries.push(Entry {
            id,
            sort_as: row.sort_title,
            author: row.authors,
            series: row
                .series
                .into_iter()
                .map(|s| SeriesRef {
                    name: s.name,
                    position: s.number,
                })
                .collect(),
            subject: row.genres,
            language: row.language,
            published: None,
            format: format_code(&row.format),
            size: row.size,
            description: cut_description(&row.annotation),
            added: row.added_at.max(0) as u64,
            publication: published.get(&content_key).map(|p| *p.as_bytes()),
            cover,
            title: row.title,
        });
    }
    built.entries.sort_by_key(|a| a.id);
    Ok(built)
}

/// The catalog's state on this library.
#[derive(Debug, Clone)]
struct State {
    key: CatalogKey,
    key_id: u32,
    generation: u64,
    chunk_count: u32,
    index_hash: Option<Vec<u8>>,
}

fn load(conn: &Connection) -> Result<Option<State>, StorageError> {
    Ok(conn
        .query_row(
            "SELECT catalog_key, key_id, generation, chunk_count, index_hash FROM hub_catalog
              WHERE id = 1",
            [],
            |row| {
                let key: Vec<u8> = row.get(0)?;
                Ok((
                    key,
                    row.get::<_, u32>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, u32>(3)?,
                    row.get::<_, Option<Vec<u8>>>(4)?,
                ))
            },
        )
        .optional()?
        .and_then(|(key, key_id, generation, chunk_count, index_hash)| {
            Some(State {
                key: CatalogKey::new(key.try_into().ok()?),
                key_id,
                generation: generation.max(0) as u64,
                chunk_count,
                index_hash,
            })
        }))
}

/// Starts over with a new catalog key sealed under library key generation `key_id`:
/// every chunk and thumbnail is uploaded again; the generation is kept.
fn reset(conn: &Connection, key: &CatalogKey, key_id: u32) -> Result<(), StorageError> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO hub_catalog (id, catalog_key, key_id) VALUES (1, ?1, ?2)
         ON CONFLICT (id) DO UPDATE SET catalog_key = excluded.catalog_key,
           key_id = excluded.key_id, chunk_count = 0, index_hash = NULL",
        params![key.as_bytes().as_slice(), key_id],
    )?;
    tx.execute_batch("DELETE FROM hub_catalog_chunks; DELETE FROM hub_catalog_covers;")?;
    tx.commit()?;
    Ok(())
}

fn chunk_hashes(conn: &Connection) -> Result<HashMap<u32, Vec<u8>>, StorageError> {
    let mut stmt = conn.prepare("SELECT number, sha256 FROM hub_catalog_chunks")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The current catalog key, once a catalog was built.
pub fn catalog_key(conn: &Connection) -> Result<Option<CatalogKey>, StorageError> {
    Ok(load(conn)?.map(|state| state.key))
}

/// Notes that thumbnail `id` is on the hub.
pub fn cover_uploaded(conn: &Connection, id: &CoverId, now: i64) -> Result<(), StorageError> {
    conn.execute(
        "INSERT INTO hub_catalog_covers (cover_id, uploaded_at) VALUES (?1, ?2)
         ON CONFLICT (cover_id) DO NOTHING",
        params![id.as_bytes().as_slice(), now],
    )?;
    Ok(())
}

/// The thumbnails on the hub, as far as this library knows.
pub fn uploaded_covers(conn: &Connection) -> Result<HashSet<CoverId>, StorageError> {
    let mut stmt = conn.prepare("SELECT cover_id FROM hub_catalog_covers")?;
    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut out = HashSet::new();
    for row in rows {
        if let Ok(id) = <[u8; 16]>::try_from(row?) {
            out.insert(CoverId::new(id));
        }
    }
    Ok(out)
}

/// What [`sync_catalog`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The hub keeps no catalogs (older than 0.22, or another catalog format).
    Unsupported,
    /// The library has no key yet (nothing to seal under).
    NoKey,
    /// Nothing changed.
    Unchanged,
    /// Uploaded: the number of chunks sent and the index's generation.
    Uploaded {
        /// Chunks uploaded.
        chunks: usize,
        /// The new generation.
        generation: u64,
    },
}

/// The time in milliseconds since the Unix epoch.
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// What an index lists: key generation, entry count, chunk hashes.
fn index_identity(key_id: u32, entries: usize, hashes: &[[u8; 32]]) -> Vec<u8> {
    let mut hash = Sha256::new();
    hash.update(key_id.to_be_bytes());
    hash.update((entries as u64).to_be_bytes());
    for h in hashes {
        hash.update(h);
    }
    hash.finalize().to_vec()
}

/// Builds the catalog from the Showcase and uploads what changed (see the module
/// documentation); `index` is a connection to the library's index. Runs blocking HTTP;
/// the user-data lock is not held during hub calls.
pub fn sync_catalog(
    userdata: &crate::annotations::store::UserData,
    index: &Connection,
    library: &HubLibrary,
    client: &HubClient,
) -> Result<Outcome, SyncError> {
    let info = client.info()?;
    if !info
        .catalog_formats
        .is_some_and(|range| range.contains(CATALOG_FORMAT_VERSION))
    {
        return Ok(Outcome::Unsupported);
    }
    let (library_key_id, library_key, state, marks, published) = {
        let conn = userdata.lock();
        let Some((key_id, key)) = store::current_library_key(&conn)? else {
            return Ok(Outcome::NoKey);
        };
        let mut state = load(&conn)?;
        if state.as_ref().is_none_or(|s| s.key_id != key_id) {
            let fresh = CatalogKey::generate().map_err(ClientError::from)?;
            reset(&conn, &fresh, key_id)?;
            state = load(&conn)?;
        }
        (
            key_id,
            key,
            state.expect("written above"),
            showcase::marks(&conn)?,
            own_publications(&conn)?,
        )
    };
    let built = build(index, &marks, &published, &state.key)?;
    let previous = (state.chunk_count > 0).then_some(state.chunk_count);
    let count = chunk_count(built.entries.len() as u64, previous);
    let mut chunks = vec![Chunk::default(); count as usize];
    for entry in &built.entries {
        chunks[chunk_of(state.key.as_bytes(), &entry.id, count) as usize]
            .entries
            .push(entry.clone());
    }
    let plains: Vec<Vec<u8>> = chunks.iter().map(Chunk::encode).collect();
    let hashes: Vec<[u8; 32]> = plains
        .iter()
        .map(|p| shelfgauge_core::catalog::chunk_hash(p))
        .collect();
    let stored = chunk_hashes(&userdata.lock())?;
    let mut sent = 0;
    for (n, (plain, hash)) in plains.iter().zip(&hashes).enumerate() {
        let n = n as u32;
        if stored.get(&n).is_some_and(|h| h.as_slice() == hash) {
            continue;
        }
        let path = CatalogPath::Chunk(n);
        let blob =
            seal(&library.library_id, &state.key, path, b"", plain).map_err(ClientError::from)?;
        client.put_catalog_blob(&library.token, path, &blob)?;
        userdata.lock().execute(
            "INSERT INTO hub_catalog_chunks (number, sha256) VALUES (?1, ?2)
             ON CONFLICT (number) DO UPDATE SET sha256 = excluded.sha256",
            params![n, hash.as_slice()],
        )?;
        sent += 1;
    }
    let identity = index_identity(library_key_id, built.entries.len(), &hashes);
    let outcome = if state.index_hash.as_deref() == Some(identity.as_slice()) {
        Outcome::Unchanged
    } else {
        let generation = (state.generation + 1).max(now_millis());
        // Kept before the upload: a failed upload never reuses a generation.
        userdata.lock().execute(
            "UPDATE hub_catalog SET generation = ?1, chunk_count = ?2 WHERE id = 1",
            params![generation as i64, count],
        )?;
        let plain = Index::new(generation, built.entries.len() as u32, &plains).encode();
        let header = catalog_header(
            &library.library_id,
            library_key_id,
            &library_key,
            &state.key,
        )
        .map_err(ClientError::from)?;
        let blob = seal(
            &library.library_id,
            &state.key,
            CatalogPath::Index,
            &header,
            &plain,
        )
        .map_err(ClientError::from)?;
        client.put_catalog_blob(&library.token, CatalogPath::Index, &blob)?;
        userdata.lock().execute(
            "UPDATE hub_catalog SET index_hash = ?1 WHERE id = 1",
            [identity.as_slice()],
        )?;
        Outcome::Uploaded {
            chunks: sent,
            generation,
        }
    };
    clean_up(userdata, library, client, count, &built)?;
    if let Outcome::Uploaded { chunks, .. } = outcome {
        tracing::info!(
            books = built.entries.len(),
            chunks,
            "catalog uploaded to the hub"
        );
    }
    Ok(outcome)
}

/// Deletes the hub's catalog blobs the index no longer names: chunks at or above
/// `count`, thumbnails of books no longer on the Showcase (or of an old catalog key).
fn clean_up(
    userdata: &crate::annotations::store::UserData,
    library: &HubLibrary,
    client: &HubClient,
    count: u32,
    built: &Built,
) -> Result<(), SyncError> {
    for path in client.catalog_paths(&library.token)? {
        let stale = match path {
            CatalogPath::Index => false,
            CatalogPath::Chunk(n) => n >= count,
            CatalogPath::Thumbnail(id) => !built.covers.contains_key(&id),
        };
        if !stale {
            continue;
        }
        client.delete_catalog_blob(&library.token, path)?;
        let conn = userdata.lock();
        match path {
            CatalogPath::Chunk(n) => {
                conn.execute("DELETE FROM hub_catalog_chunks WHERE number = ?1", [n])?;
            }
            CatalogPath::Thumbnail(id) => {
                conn.execute(
                    "DELETE FROM hub_catalog_covers WHERE cover_id = ?1",
                    [id.as_bytes().as_slice()],
                )?;
            }
            CatalogPath::Index => {}
        }
    }
    // Chunk hashes beyond the count that the hub never had.
    userdata
        .lock()
        .execute("DELETE FROM hub_catalog_chunks WHERE number >= ?1", [count])?;
    Ok(())
}
