//! A library's catalog on the device: the decrypted, inflated index and chunks are
//! checked and decoded here, then searched and listed with the core's code.

use serde::Serialize;
use shelfgauge_core::catalog::{self, BookId, Catalog, Chunk, Entry, Index};
use shelfgauge_core::format::genres::genre_label;
use shelfgauge_core::publication::FormatError;

/// Why a catalog blob was refused, as a message for the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogError(pub String);

impl From<FormatError> for CatalogError {
    fn from(e: FormatError) -> Self {
        Self(e.to_string())
    }
}

/// A catalog being kept on the device: the current index and the chunks decoded so
/// far. Every chunk held matches the current index: a new index drops the chunks whose
/// hash it changed, so a book is never shown from an old and a new chunk at once.
#[derive(Debug, Default)]
pub struct DeviceCatalog {
    index: Option<Index>,
    /// Per chunk number, the SHA-256 of the chunk held (`None`: none held).
    held: Vec<Option<[u8; 32]>>,
    catalog: Catalog,
}

/// The index as the page needs it.
#[derive(Serialize)]
struct IndexSummary {
    generation: u64,
    entries: u32,
    /// Per chunk (in order) its SHA-256 in hex, so the page fetches only chunks that
    /// changed.
    chunks: Vec<String>,
}

/// A genre of an entry, with its name.
#[derive(Serialize)]
struct Subject<'a> {
    code: &'a str,
    name: String,
}

/// An entry as JSON for the page.
#[derive(Serialize)]
struct EntryJson<'a> {
    id: String,
    title: &'a str,
    author: &'a [String],
    series: &'a [catalog::SeriesRef],
    subject: Vec<Subject<'a>>,
    language: Option<&'a str>,
    published: Option<i32>,
    format: Option<&'static str>,
    size: u64,
    description: Option<&'a str>,
    added: u64,
    publication: Option<String>,
    cover: Option<String>,
}

impl<'a> From<&'a Entry> for EntryJson<'a> {
    fn from(e: &'a Entry) -> Self {
        Self {
            id: e.id.to_string(),
            title: &e.title,
            author: &e.author,
            series: &e.series,
            subject: e
                .subject
                .iter()
                .map(|code| Subject {
                    code,
                    name: genre_label(code),
                })
                .collect(),
            language: e.language.as_deref(),
            published: e.published,
            format: catalog::format_name(e.format),
            size: e.size,
            description: e.description.as_deref(),
            added: e.added,
            publication: e.publication.map(hex),
            cover: e.cover.map(|c| c.to_string()),
        }
    }
}

/// One page of a book list.
#[derive(Serialize)]
struct Page<'a> {
    total: usize,
    entries: Vec<EntryJson<'a>>,
}

impl DeviceCatalog {
    /// Takes a new index (its plaintext); refused if its generation is below
    /// `lowest` (the highest the device has seen), so the hub cannot roll the catalog
    /// back. Chunks beyond the new count and chunks whose hash changed are dropped; the
    /// others stay.
    /// Returns the index as JSON `{generation, entries, chunks: [sha256 hex]}`.
    pub fn set_index(&mut self, plain: &[u8], lowest: u64) -> Result<String, CatalogError> {
        let index = Index::decode(plain)?;
        if index.generation < lowest {
            return Err(CatalogError(format!(
                "the hub sent an older catalog (generation {}, seen {lowest})",
                index.generation
            )));
        }
        let summary = IndexSummary {
            generation: index.generation,
            entries: index.entries,
            chunks: index.chunks.iter().map(|c| hex(c.sha256)).collect(),
        };
        let count = index.chunks.len();
        self.catalog.truncate(count as u32);
        self.held.resize(count, None);
        for (held, info) in self.held.iter_mut().zip(&index.chunks) {
            if held.is_some_and(|hash| hash != info.sha256) {
                *held = None;
                self.catalog.set_chunk(info.number, Vec::new());
            }
        }
        self.index = Some(index);
        Ok(serde_json::to_string(&summary).expect("JSON values always serialize"))
    }

    /// Adds the plaintext of chunk `number`, checked against the index.
    pub fn set_chunk(&mut self, number: u32, plain: &[u8]) -> Result<(), CatalogError> {
        let index = self
            .index
            .as_ref()
            .ok_or_else(|| CatalogError("no catalog index yet".into()))?;
        index.check_chunk(number, plain)?;
        let chunk = Chunk::decode(plain)?;
        self.catalog.set_chunk(number, chunk.entries);
        self.held[number as usize] = Some(index.chunks[number as usize].sha256);
        Ok(())
    }

    /// Adds chunk `number` that this device checked against an index when it got it
    /// and kept with its hash (`hash`, hex): taken without hashing it again (the check
    /// is against the hub, not this device's own storage), if the current index names
    /// that hash for it.
    pub fn set_kept_chunk(
        &mut self,
        number: u32,
        plain: &[u8],
        hash: &str,
    ) -> Result<(), CatalogError> {
        let index = self
            .index
            .as_ref()
            .ok_or_else(|| CatalogError("no catalog index yet".into()))?;
        let info = index
            .chunks
            .get(number as usize)
            .ok_or_else(|| CatalogError(format!("no chunk {number}")))?;
        if hex(info.sha256) != hash {
            return Err(CatalogError(format!(
                "chunk {number} kept here is not the index's"
            )));
        }
        let chunk = Chunk::decode(plain)?;
        self.catalog.set_chunk(number, chunk.entries);
        self.held[number as usize] = Some(info.sha256);
        Ok(())
    }

    /// Drops everything (another library, a new key).
    pub fn clear(&mut self) {
        self.index = None;
        self.held.clear();
        self.catalog.clear();
    }

    /// The number of books.
    pub fn len(&self) -> usize {
        self.catalog.len()
    }

    /// Whether there are no books.
    pub fn is_empty(&self) -> bool {
        self.catalog.is_empty()
    }

    /// The book with id `id` (hex) as JSON, or `None`.
    pub fn get(&self, id: &str) -> Option<String> {
        let id: BookId = id.parse().ok()?;
        self.catalog.get(&id).map(|e| json(&EntryJson::from(e)))
    }

    /// A page of a list: `list` is `search`, `all`, `recent`, `author`, `series` or
    /// `genre`, `arg` the query, the author's or series' key or the genre's code.
    /// JSON `{total, entries}`.
    pub fn list(
        &self,
        list: &str,
        arg: &str,
        offset: usize,
        limit: usize,
    ) -> Result<String, CatalogError> {
        let books = match list {
            "search" => self.catalog.search(arg),
            "all" => self.catalog.all(),
            "recent" => self.catalog.recent(usize::MAX),
            "author" => self.catalog.by_author(arg),
            "series" => self.catalog.by_series(arg),
            "genre" => self.catalog.by_genre(arg),
            _ => return Err(CatalogError(format!("no list {list:?}"))),
        };
        Ok(json(&Page {
            total: books.len(),
            entries: books
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(EntryJson::from)
                .collect(),
        }))
    }

    /// The names of `authors`, `series` or `genres` with their counts, as JSON
    /// `[{key, name, count}]` (genres: `[{code, name, count}]`).
    pub fn names(&self, which: &str) -> Result<String, CatalogError> {
        Ok(match which {
            "authors" => json(&self.catalog.authors()),
            "series" => json(&self.catalog.series()),
            "genres" => json(&self.catalog.genres()),
            _ => return Err(CatalogError(format!("no names {which:?}"))),
        })
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn json(value: &impl Serialize) -> String {
    serde_json::to_string(value).expect("JSON values always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use shelfgauge_core::catalog::{CoverId, SeriesRef};

    fn entry(n: u8, title: &str) -> Entry {
        Entry {
            id: BookId::new([n; 16]),
            title: title.into(),
            sort_as: title.to_lowercase(),
            author: vec!["Анна Ахматова".into()],
            series: vec![SeriesRef {
                name: "Стихи".into(),
                position: Some(n.to_string()),
            }],
            subject: vec!["poetry".into()],
            language: Some("ru".into()),
            published: Some(1912),
            format: 1,
            size: 100,
            description: Some("Сборник".into()),
            added: u64::from(n),
            publication: (n == 2).then_some([0xaa; 16]),
            cover: Some(CoverId::new([n; 16])),
        }
    }

    fn device() -> (DeviceCatalog, Vec<Vec<u8>>, Vec<u8>) {
        let chunks = vec![
            Chunk {
                entries: vec![entry(1, "Вечер")],
            }
            .encode(),
            Chunk {
                entries: vec![entry(2, "Чётки")],
            }
            .encode(),
        ];
        let index = Index::new(5, 2, &chunks).encode();
        (DeviceCatalog::default(), chunks, index)
    }

    #[test]
    fn chunks_are_checked_against_the_index() {
        let (mut device, chunks, index) = device();
        assert!(device.set_chunk(0, &chunks[0]).is_err(), "no index yet");
        let summary: serde_json::Value =
            serde_json::from_str(&device.set_index(&index, 0).unwrap()).unwrap();
        assert_eq!(summary["generation"], 5);
        assert_eq!(summary["chunks"].as_array().unwrap().len(), 2);
        assert!(
            device.set_chunk(0, &chunks[1]).is_err(),
            "chunk 1 as chunk 0"
        );
        device.set_chunk(0, &chunks[0]).unwrap();
        device.set_chunk(1, &chunks[1]).unwrap();
        assert_eq!(device.len(), 2);
    }

    #[test]
    fn a_new_index_drops_the_chunks_it_changed() {
        let (mut device, chunks, index) = device();
        device.set_index(&index, 0).unwrap();
        device.set_chunk(0, &chunks[0]).unwrap();
        device.set_chunk(1, &chunks[1]).unwrap();
        // Generation 6: chunk 0 changed, chunk 1 did not.
        let changed = Chunk {
            entries: vec![entry(1, "Вечер"), entry(3, "Подорожник")],
        }
        .encode();
        let next = Index::new(6, 3, &[changed.clone(), chunks[1].clone()]).encode();
        device.set_index(&next, 5).unwrap();
        assert_eq!(device.len(), 1, "only the unchanged chunk 1 is held");
        device.set_chunk(0, &changed).unwrap();
        assert_eq!(device.len(), 3);
        // Redistributed into four chunks: everything held before is dropped (chunk 0
        // and 1 changed, 2 and 3 are new), so no book shows twice meanwhile.
        let mut four = vec![Chunk::default(); 4];
        for e in [entry(1, "Вечер"), entry(2, "Чётки"), entry(3, "Подорожник")]
        {
            four[usize::from(e.id.as_bytes()[0]) % 4].entries.push(e);
        }
        let four: Vec<Vec<u8>> = four.iter().map(Chunk::encode).collect();
        device
            .set_index(&Index::new(7, 3, &four).encode(), 6)
            .unwrap();
        assert!(device.is_empty());
        for (n, plain) in four.iter().enumerate() {
            device.set_chunk(n as u32, plain).unwrap();
        }
        assert_eq!(device.len(), 3);
    }

    #[test]
    fn kept_chunks_are_taken_by_their_hash() {
        let (mut device, chunks, index) = device();
        device.set_index(&index, 0).unwrap();
        let hash = |plain: &[u8]| hex(shelfgauge_core::catalog::chunk_hash(plain));
        device
            .set_kept_chunk(0, &chunks[0], &hash(&chunks[0]))
            .unwrap();
        assert!(
            device
                .set_kept_chunk(1, &chunks[1], &hash(&chunks[0]))
                .is_err(),
            "a hash the index does not name for that chunk"
        );
        assert!(
            device
                .set_kept_chunk(5, &chunks[1], &hash(&chunks[1]))
                .is_err()
        );
        device
            .set_kept_chunk(1, &chunks[1], &hash(&chunks[1]))
            .unwrap();
        assert_eq!(device.len(), 2);
    }

    #[test]
    fn an_older_generation_is_refused() {
        let (mut device, _, index) = device();
        device.set_index(&index, 5).unwrap();
        let error = device.set_index(&index, 6).unwrap_err();
        assert!(error.0.contains("older catalog"), "{error:?}");
    }

    #[test]
    fn lists_and_entries_come_as_json() {
        let (mut device, chunks, index) = device();
        device.set_index(&index, 0).unwrap();
        for (n, chunk) in chunks.iter().enumerate() {
            device.set_chunk(n as u32, chunk).unwrap();
        }
        let page: serde_json::Value =
            serde_json::from_str(&device.list("search", "четки", 0, 10).unwrap()).unwrap();
        assert_eq!(page["total"], 1);
        let found = &page["entries"][0];
        assert_eq!(found["title"], "Чётки");
        assert_eq!(found["format"], "fb2");
        assert_eq!(found["subject"][0]["name"], "Poetry");
        assert_eq!(found["publication"], "aa".repeat(16));
        assert_eq!(found["cover"], "02".repeat(16));
        let page: serde_json::Value =
            serde_json::from_str(&device.list("series", "стихи", 1, 10).unwrap()).unwrap();
        assert_eq!(
            (page["total"].as_u64(), page["entries"][0]["title"].as_str()),
            (Some(2), Some("Чётки"))
        );
        let authors: serde_json::Value =
            serde_json::from_str(&device.names("authors").unwrap()).unwrap();
        assert_eq!(authors[0]["count"], 2);
        assert!(device.get(&"01".repeat(16)).unwrap().contains("Вечер"));
        assert!(device.get("nope").is_none());
        assert!(device.list("nope", "", 0, 1).is_err());
        // A shorter index drops the chunks beyond it.
        let one = Index::new(6, 1, &chunks[..1]).encode();
        device.set_index(&one, 5).unwrap();
        assert_eq!(device.len(), 1);
        device.clear();
        assert!(device.is_empty());
    }
}
