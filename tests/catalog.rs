//! The catalog format: interop vectors for the hub reader (sealed index, chunks and a
//! thumbnail from fixed keys and nonces) and the sealing round trip.

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use shelfgauge::hub::AccountId;
use shelfgauge::hub_client::catalog::format::{
    BookId, CATALOG_FORMAT_VERSION, CATALOG_KEY_KIND, CatalogHeader, CatalogPath, Chunk, CoverId,
    Entry, Index, MAX_CHUNK_LEN, MAX_INDEX_LEN, SeriesRef, associated_data, chunk_of, cover_id,
    cut_description, format_code, parse_envelope,
};
use shelfgauge::hub_client::catalog::{CatalogKey, encrypt_padded, open, open_catalog_key, seal};
use shelfgauge::hub_client::keys::{AccountKey, encrypt_record};
use shelfgauge::publication::compress;
use shelfgauge::publication::format::{LIBRARY_SCOPE, pad};

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalog")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

fn writing() -> bool {
    std::env::var_os("SHELFGAUGE_WRITE_VECTORS").is_some()
}

/// Compares `actual` with the committed vector file `name`; writes it instead with
/// `SHELFGAUGE_WRITE_VECTORS=1`.
fn check_vector(name: &str, actual: &serde_json::Value) {
    let path = vectors_dir().join(name);
    if writing() {
        std::fs::create_dir_all(vectors_dir()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(actual).unwrap() + "\n").unwrap();
    }
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        actual, &expected,
        "{name} changed: the catalog format is not compatible any more"
    );
}

fn read_vector(name: &str) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(vectors_dir().join(name)).unwrap()).unwrap()
}

const LIBRARY: [u8; 16] = [0x0f; 16];
const CATALOG_KEY: [u8; 32] = [0x42; 32];
const LIBRARY_KEY: [u8; 32] = [0x24; 32];

fn entries(key: &[u8; 32]) -> Vec<Entry> {
    let id = |content_key: &[u8]| BookId::from_content_key(content_key);
    let first = id(&[0x11; 16]);
    let second = {
        // A zip entry's content key: size and CRC32, little-endian.
        let mut k = 4096u64.to_le_bytes().to_vec();
        k.extend_from_slice(&0xdead_beefu32.to_le_bytes());
        id(&k)
    };
    vec![
        Entry {
            id: first,
            title: "Пикник на обочине".into(),
            sort_as: "пикник на обочине".into(),
            author: vec!["Аркадий Стругацкий".into(), "Борис Стругацкий".into()],
            series: vec![SeriesRef {
                name: "Мир Полудня".into(),
                position: Some("7".into()),
            }],
            subject: vec!["sf_social".into(), "Free text subject".into()],
            language: Some("ru".into()),
            published: Some(1972),
            format: format_code("fb2"),
            size: 312_345,
            description: cut_description(&"Зона — место посещения. ".repeat(20)),
            added: 1_761_900_000,
            publication: Some([0xab; 16]),
            cover: Some(cover_id(key, &first)),
        },
        Entry {
            id: second,
            title: "Les Misérables".into(),
            sort_as: "misérables".into(),
            author: vec!["Victor Hugo".into()],
            series: vec![],
            subject: vec![],
            language: None,
            published: None,
            format: format_code("epub"),
            size: 4096,
            description: None,
            added: 1_761_800_000,
            publication: None,
            cover: None,
        },
    ]
}

#[test]
fn derivations_are_stable() {
    // Book ids, chunk numbers and cover ids: the device recomputes none of them, but a
    // second implementation must.
    let key = CATALOG_KEY;
    let ids: Vec<serde_json::Value> = [
        &[0x11u8; 16][..],
        b"\x00\x10\x00\x00\x00\x00\x00\x00\xef\xbe\xad\xde",
    ]
    .iter()
    .map(|content_key| {
        let id = BookId::from_content_key(content_key);
        serde_json::json!({
            "content_key": hex(content_key),
            "book_id": id.to_string(),
            "chunk_of_8": chunk_of(&key, &id, 8),
            "cover_id": cover_id(&key, &id).to_string(),
        })
    })
    .collect();
    check_vector(
        "derivations.json",
        &serde_json::json!({
            "description": "book id = SHA-256('sg-catalog book' || 0 || content key)[..16]; chunk = HMAC-SHA256(catalog key, 'chunk' || 0 || book id)[..8] as u64 BE mod n; cover id = HMAC-SHA256(catalog key, 'cover' || 0 || book id)[..16]",
            "catalog_key": hex(&key),
            "books": ids,
        }),
    );
}

#[test]
fn encryption_vector_is_stable() {
    let library = AccountId::new(LIBRARY);
    let key = CatalogKey::new(std::array::from_fn(|i| i as u8));
    let path = CatalogPath::Chunk(3);
    let nonce: [u8; 12] = std::array::from_fn(|i| 0xa0 + i as u8);
    let padded = pad("shelfgauge catalog vector: каталог".as_bytes());
    let blob = encrypt_padded(&library, &key, path, b"", &padded, nonce);
    check_vector(
        "encryption.json",
        &serde_json::json!({
            "description": "AES-256-GCM of a padded catalog blob: blob = version u16 BE || header length u16 BE || header || nonce || ciphertext || tag; associated data = 'sg-catalog' || 0 || version u16 BE || library id || path (UTF-8) || 0 || header",
            "key": hex(key.as_bytes()),
            "library_id": library.to_string(),
            "path": path.to_string(),
            "format_version": CATALOG_FORMAT_VERSION,
            "nonce": hex(&nonce),
            "associated_data": hex(&associated_data(CATALOG_FORMAT_VERSION, &LIBRARY, path, b"")),
            "padded_plaintext": hex(&padded),
            "blob": hex(&blob),
        }),
    );
}

#[test]
fn a_whole_sealed_catalog_vector_opens() {
    // Two chunks, the index with its header and a thumbnail, sealed with fixed nonces.
    // The device side checks that it gets the catalog key from the header with the
    // library key alone, and the entries from the chunks.
    let library = AccountId::new(LIBRARY);
    let key = CatalogKey::new(CATALOG_KEY);
    let library_key = AccountKey::new(LIBRARY_KEY);
    let mut chunks = [Chunk::default(), Chunk::default()];
    for entry in entries(&CATALOG_KEY) {
        chunks[chunk_of(&CATALOG_KEY, &entry.id, 2) as usize]
            .entries
            .push(entry);
    }
    let plains: Vec<Vec<u8>> = chunks.iter().map(Chunk::encode).collect();
    let index = Index::new(3, 2, &plains);
    if writing() {
        let sealed_key = encrypt_record(
            &library_key,
            CATALOG_KEY_KIND,
            &library.to_string(),
            key.as_bytes(),
            [0x5c; 12],
        );
        let header = CatalogHeader {
            library: library.to_string(),
            key_scope: LIBRARY_SCOPE.into(),
            key_owner: library.to_string(),
            key_id: 1,
            catalog_key: URL_SAFE_NO_PAD.encode(&sealed_key),
        }
        .encode();
        let thumbnail = b"RIFF\x0c\x00\x00\x00WEBPVP8 not really an image";
        let cover = entries(&CATALOG_KEY)[0].cover.unwrap();
        let mut blobs = serde_json::Map::new();
        let mut put = |n: u8, path: CatalogPath, header: &[u8], plain: &[u8]| {
            let nonce: [u8; 12] = std::array::from_fn(|i| n * 16 + i as u8);
            let blob = encrypt_padded(&library, &key, path, header, &pad(&compress(plain)), nonce);
            blobs.insert(path.to_string(), hex(&blob).into());
        };
        put(0, CatalogPath::Index, &header, &index.encode());
        for (n, plain) in plains.iter().enumerate() {
            put(n as u8 + 1, CatalogPath::Chunk(n as u32), b"", plain);
        }
        put(9, CatalogPath::Thumbnail(cover), b"", thumbnail);
        let titles: Vec<serde_json::Value> = chunks
            .iter()
            .map(|c| c.entries.iter().map(|e| e.title.clone()).collect())
            .collect();
        check_vector(
            "catalog.json",
            &serde_json::json!({
                "description": "a sealed catalog of two chunks: the index's header holds the catalog key sealed as a catalog_key record (id: the library id) under the library key; chunk_titles lists the titles per chunk",
                "library_id": library.to_string(),
                "library_key": hex(library_key.as_bytes()),
                "catalog_key": hex(key.as_bytes()),
                "generation": 3,
                "chunk_titles": titles,
                "thumbnail": {
                    "path": CatalogPath::Thumbnail(cover).to_string(),
                    "plaintext": hex(thumbnail),
                },
                "blobs": blobs,
            }),
        );
    }
    let vector = read_vector("catalog.json");
    let blob = |path: &str| unhex(vector["blobs"][path].as_str().unwrap());
    let index_blob = blob("index");
    let header = CatalogHeader::decode(parse_envelope(&index_blob).unwrap().header).unwrap();
    assert_eq!(
        (header.key_scope.as_str(), header.key_id),
        (LIBRARY_SCOPE, 1)
    );
    let opened_key = open_catalog_key(&header, &library_key).unwrap();
    assert_eq!(opened_key, key);
    let decoded = Index::decode(
        &open(
            &library,
            &key,
            CatalogPath::Index,
            &index_blob,
            MAX_INDEX_LEN,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(decoded, index);
    for (n, chunk) in chunks.iter().enumerate() {
        let path = CatalogPath::Chunk(n as u32);
        let plain = open(
            &library,
            &key,
            path,
            &blob(&path.to_string()),
            MAX_CHUNK_LEN,
        )
        .unwrap();
        decoded.check_chunk(n as u32, &plain).unwrap();
        assert_eq!(&Chunk::decode(&plain).unwrap(), chunk);
    }
    let thumbnail = vector["thumbnail"]["path"].as_str().unwrap();
    let path: CatalogPath = thumbnail.parse().unwrap();
    assert!(matches!(path, CatalogPath::Thumbnail(CoverId { .. })));
    assert_eq!(
        hex(&open(&library, &key, path, &blob(thumbnail), 1 << 20).unwrap()),
        vector["thumbnail"]["plaintext"].as_str().unwrap()
    );
}

#[test]
fn sealed_blobs_round_trip_with_fresh_nonces() {
    let library = AccountId::new([9; 16]);
    let key = CatalogKey::generate().unwrap();
    let plain = Chunk {
        entries: entries(key.as_bytes()),
    }
    .encode();
    let a = seal(&library, &key, CatalogPath::Chunk(0), b"", &plain).unwrap();
    let b = seal(&library, &key, CatalogPath::Chunk(0), b"", &plain).unwrap();
    assert_ne!(a, b);
    for blob in [a, b] {
        assert_eq!(
            open(&library, &key, CatalogPath::Chunk(0), &blob, MAX_CHUNK_LEN).unwrap(),
            plain
        );
    }
}

/// Sizes and times of the catalog of a large library (decision 6 of Phase 31):
/// building it from the Showcase (the whole library open), sealing it, the device
/// loading and searching it (the reader's WASM code, run natively here), and the
/// root folder page's search for missing marks. The library's index is copied first
/// (opening it may migrate it). Run with
/// `SHELFGAUGE_CATALOG_SAMPLES=<library folder> cargo test --release --test catalog --
/// --ignored --nocapture`.
#[test]
#[ignore = "timing; set SHELFGAUGE_CATALOG_SAMPLES to a scanned library folder"]
fn the_catalog_of_a_large_library() {
    use std::collections::HashMap;
    use std::time::Instant;

    use shelfgauge::annotations::store::UserData;
    use shelfgauge::hub_client::catalog_sync::build;
    use shelfgauge::hub_client::showcase;
    use shelfgauge::storage::folders::FolderPath;
    use shelfgauge_core::catalog::{chunk_count, chunk_of};
    use shelfgauge_wasm::catalog::DeviceCatalog;

    let Some(folder) = std::env::var_os("SHELFGAUGE_CATALOG_SAMPLES") else {
        eprintln!("SHELFGAUGE_CATALOG_SAMPLES is not set");
        return;
    };
    let source = PathBuf::from(folder).join(".shelfgauge/library.db");
    let dir = tempfile::tempdir().unwrap();
    std::fs::copy(&source, dir.path().join("library.db")).unwrap();
    let index = shelfgauge::storage::open_writer(&dir.path().join("library.db")).unwrap();
    let userdata = UserData::open(&dir.path().join("userdata.sqlite")).unwrap();
    let ms = |t: Instant| t.elapsed().as_secs_f64() * 1000.0;

    showcase::set_mark(&userdata.lock(), &FolderPath::Dir(String::new()), true, 0).unwrap();
    let marks = showcase::marks(&userdata.lock()).unwrap();
    let key = CatalogKey::new([7; 32]);
    let t = Instant::now();
    let built = build(&index, &marks, &HashMap::new(), &key).unwrap();
    let build_ms = ms(t);
    let books = built.entries.len();
    println!("books on the Showcase: {books} (built in {build_ms:.0} ms)");

    let library = AccountId::new([1; 16]);
    let measure = |entries: &[Entry], label: &str| {
        let count = chunk_count(entries.len() as u64, None);
        let mut chunks = vec![Chunk::default(); count as usize];
        for e in entries {
            chunks[chunk_of(key.as_bytes(), &e.id, count) as usize]
                .entries
                .push(e.clone());
        }
        let t = Instant::now();
        let plains: Vec<Vec<u8>> = chunks.iter().map(Chunk::encode).collect();
        let encode_ms = ms(t);
        let t = Instant::now();
        let sealed: usize = plains
            .iter()
            .enumerate()
            .map(|(n, p)| {
                seal(&library, &key, CatalogPath::Chunk(n as u32), b"", p)
                    .unwrap()
                    .len()
            })
            .sum();
        let seal_ms = ms(t);
        let index = Index::new(1, entries.len() as u32, &plains).encode();
        let plain: usize = plains.iter().map(Vec::len).sum();
        println!(
            "{label}: {count} chunks, plaintext {:.1} MB, sealed {:.1} MB ({:.0} bytes a book), index {} bytes; encoded in {encode_ms:.0} ms, sealed in {seal_ms:.0} ms",
            plain as f64 / 1e6,
            sealed as f64 / 1e6,
            sealed as f64 / entries.len().max(1) as f64,
            index.len()
        );
        (plains, index)
    };
    let (plains, index_plain) = measure(&built.entries, "as scanned");
    // The largest annotation the catalog keeps, on every book (the synthetic library
    // has none): different words for each book, from a vocabulary of 4096 words, so
    // deflate cannot fold them away as it would one text repeated.
    let words: Vec<String> = (0..4096u32)
        .map(|n| {
            let h = n.wrapping_mul(2_654_435_761);
            (0..4 + (h % 6))
                .map(|i| char::from(b'a' + ((h >> (i * 3)) % 26) as u8))
                .collect()
        })
        .collect();
    let annotated: Vec<Entry> = built
        .entries
        .iter()
        .enumerate()
        .map(|(n, e)| {
            let mut x = n as u64 * 0x9e37_79b9_7f4a_7c15 + 1;
            let mut text = String::new();
            while text.chars().count() < 320 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                text.push_str(&words[(x % 4096) as usize]);
                text.push(' ');
            }
            Entry {
                description: cut_description(&text),
                ..e.clone()
            }
        })
        .collect();
    measure(&annotated, "with a 300-character annotation each");

    // For the browser measurement (`.debug/TESTS/catalog_bench.mjs`): the plaintexts
    // with annotations, as files.
    if let Some(out) = std::env::var_os("SHELFGAUGE_CATALOG_OUT") {
        let (annotated_plains, annotated_index) = measure(&annotated, "written for the browser");
        let out = PathBuf::from(out);
        std::fs::create_dir_all(out.join("c")).unwrap();
        std::fs::write(out.join("index"), &annotated_index).unwrap();
        for (n, plain) in annotated_plains.iter().enumerate() {
            std::fs::write(out.join("c").join(n.to_string()), plain).unwrap();
        }
        println!("written to {}", out.display());
    }

    // Where loading goes: hashing (the check against the index), decoding, building
    // the searchable books.
    for (label, plains) in [
        ("as scanned", &plains),
        ("annotated", &annotated_plains_for_split(&annotated, &key)),
    ] {
        let t = Instant::now();
        for p in plains.iter() {
            std::hint::black_box(shelfgauge_core::catalog::chunk_hash(p));
        }
        let hash_ms = ms(t);
        let t = Instant::now();
        let decoded: Vec<Chunk> = plains.iter().map(|p| Chunk::decode(p).unwrap()).collect();
        let decode_ms = ms(t);
        let t = Instant::now();
        let mut cat = shelfgauge_core::catalog::Catalog::new();
        for (n, c) in decoded.into_iter().enumerate() {
            cat.set_chunk(n as u32, c.entries);
        }
        println!(
            "device ({label}): hashing {hash_ms:.0} ms, decoding {decode_ms:.0} ms, building {:.0} ms",
            ms(t)
        );
    }

    // The device: loading (decoding and folding) and searching.
    let mut device = DeviceCatalog::default();
    let t = Instant::now();
    device.set_index(&index_plain, 0).unwrap();
    for (n, plain) in plains.iter().enumerate() {
        device.set_chunk(n as u32, plain).unwrap();
    }
    println!("device: loaded {} books in {:.0} ms", device.len(), ms(t));
    for (list, arg) in [
        // One letter (prefix of almost everything), a word of many titles, two words,
        // an author, a series, nothing.
        ("search", "с"),
        ("search", "камень"),
        ("search", "камень чайка"),
        ("search", "кузнецов160"),
        ("search", "серия 40"),
        ("search", "zz"),
        ("recent", ""),
        ("all", ""),
    ] {
        let t = Instant::now();
        let page: serde_json::Value =
            serde_json::from_str(&device.list(list, arg, 0, 50).unwrap()).unwrap();
        println!(
            "device: {list} {arg:?}: {} found in {:.1} ms",
            page["total"],
            ms(t)
        );
    }
    for which in ["authors", "series", "genres"] {
        let t = Instant::now();
        let names: serde_json::Value = serde_json::from_str(&device.names(which).unwrap()).unwrap();
        println!(
            "device: {which}: {} in {:.1} ms",
            names.as_array().unwrap().len(),
            ms(t)
        );
    }

    // The root folder page: missing marks (two that exist, one that is gone).
    let first = showcase::showcase_books(&index, &marks).unwrap()[0]
        .folder
        .clone();
    let checked = vec![
        FolderPath::Dir(String::new()),
        first,
        FolderPath::Dir("gone/away".into()),
    ];
    let t = Instant::now();
    let missing = showcase::missing_marks(&index, &checked).unwrap();
    println!(
        "root folder page: missing marks {} of {} found in {:.0} ms",
        missing.len(),
        checked.len(),
        ms(t)
    );
}

/// The chunk plaintexts of `entries` under `key`.
fn annotated_plains_for_split(entries: &[Entry], key: &CatalogKey) -> Vec<Vec<u8>> {
    use shelfgauge_core::catalog::{chunk_count, chunk_of};
    let count = chunk_count(entries.len() as u64, None);
    let mut chunks = vec![Chunk::default(); count as usize];
    for e in entries {
        chunks[chunk_of(key.as_bytes(), &e.id, count) as usize]
            .entries
            .push(e.clone());
    }
    chunks.iter().map(Chunk::encode).collect()
}
