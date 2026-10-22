//! The publication format: building, sealing and opening publications, and the key
//! property that a device decoding a publication renders exactly what the server
//! renders.

mod common;

use std::collections::BTreeMap;
use std::path::PathBuf;

use shelfgauge::annotations::text::{BookText, rendered_text};
use shelfgauge::config::{ReaderConfig, SplitMode};
use shelfgauge::format::epub::EpubParser;
use shelfgauge::format::fb2::Fb2Parser;
use shelfgauge::format::html::HtmlParser;
use shelfgauge::format::model::{BlockId, BookContent, Image};
use shelfgauge::format::txt::TxtParser;
use shelfgauge::format::{BookParser, run_with_parser_stack};
use shelfgauge::publication::format::{
    BlobPath, Envelope, FormatError, HEADER_LEN, Manifest, PartBlob, PublicationId, TAG_LEN,
    assemble, bucket, pad,
};
use shelfgauge::publication::{
    BookInfo, BookKey, OpenError, PublishError, build, compress, encrypt_padded, inflate, open,
};
use shelfgauge::render::html::{ReaderLinks, render_part, render_toc, render_toc_entries};
use shelfgauge::render::outline::part_progress;
use shelfgauge::render::split::split;

/// The plaintext limit used when opening blobs in these tests.
const LIMIT: u64 = 64 << 20;

struct Links;

impl ReaderLinks for Links {
    fn block(&self, part: usize, id: BlockId) -> String {
        format!("/book/1/read?part={}#{id}", part + 1)
    }

    fn image(&self, id: &str) -> String {
        format!("/book/1/image/{id}")
    }
}

/// Every synthetic book and fixture, parsed (`html_saved_page` is not a book).
fn books() -> Vec<(&'static str, BookContent, Option<Image>)> {
    run_with_parser_stack(|| {
        let fixture = |name: &str| {
            std::fs::read(
                PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                    .join("tests/fixtures/fb2")
                    .join(name),
            )
            .unwrap()
        };
        let parse = |parser: &dyn BookParser, data: &[u8]| {
            let cover = parser.parse_metadata(data).unwrap().cover;
            (parser.parse_content(data).unwrap(), cover)
        };
        let mut books = Vec::new();
        for name in ["full.fb2", "minimal.fb2", "unbalanced.fb2"] {
            let (content, cover) = parse(&Fb2Parser, &fixture(name));
            books.push((name, content, cover));
        }
        for (name, data) in [("epub3", common::epub3()), ("epub2", common::epub2())] {
            let (content, cover) = parse(&EpubParser, &data);
            books.push((name, content, cover));
        }
        for (name, text) in [
            ("txt_libru", common::txt_libru()),
            ("txt_gutenberg", common::txt_gutenberg()),
        ] {
            let (content, cover) = parse(&TxtParser, text.as_bytes());
            books.push((name, content, cover));
        }
        for (name, html) in [
            ("html_tag_soup", common::html_tag_soup()),
            ("html_libru", common::html_libru()),
            ("html_gutenberg", common::html_gutenberg()),
        ] {
            let (content, cover) = parse(&HtmlParser, html.as_bytes());
            books.push((name, content, cover));
        }
        books
    })
}

fn configs() -> Vec<ReaderConfig> {
    let config = |split, section_depth, part_size, max_part_size| ReaderConfig {
        split,
        section_depth,
        part_size,
        max_part_size,
        ..ReaderConfig::default()
    };
    vec![
        ReaderConfig::default(),
        config(SplitMode::None, 1, 50_000, 0),
        config(SplitMode::Section, 2, 50_000, 0),
        config(SplitMode::Section, 1, 50_000, 200),
        config(SplitMode::Size, 1, 300, 0),
        config(SplitMode::Size, 1, 1, 0),
    ]
}

fn info() -> BookInfo {
    BookInfo {
        title: "Book".into(),
        authors: vec!["A. Author".into()],
        series: vec!["Series #2".into()],
        language: Some("en_GB".into()),
        drm: None,
        content_key: vec![7; 16],
    }
}

fn id() -> PublicationId {
    PublicationId::new([0x5a; 16])
}

/// Seals `publication` and opens every blob again, as a device would.
fn round_trip(
    content: &BookContent,
    cover: Option<&Image>,
    config: &ReaderConfig,
) -> (Manifest, Vec<PartBlob>, Vec<Vec<u8>>) {
    let publication = build(&info(), content, cover, config).unwrap();
    let key = BookKey::generate().unwrap();
    let sealed = publication.seal(&id(), &key).unwrap();
    let opened: BTreeMap<String, Vec<u8>> = sealed
        .iter()
        .map(|(path, blob)| {
            (
                path.to_string(),
                open(&id(), &key, *path, blob, LIMIT).unwrap(),
            )
        })
        .collect();
    let manifest = Manifest::decode(&opened["manifest"]).unwrap();
    let parts: Vec<PartBlob> = (0..manifest.parts.len())
        .map(|n| PartBlob::decode(&opened[&format!("p/{n}")]).unwrap())
        .collect();
    let images = (0..manifest.images.len())
        .map(|n| opened[&format!("i/{n}")].clone())
        .collect();
    assert_eq!(sealed.len(), 1 + parts.len() + manifest.images.len());
    (manifest, parts, images)
}

#[test]
fn decoded_publications_render_exactly_as_the_server() {
    // Links between parts (text → notes and back) must have been tried.
    let mut cross_part_links = 0;
    for (name, content, cover) in books() {
        let full = BookText::new(&content);
        for config in configs() {
            let (manifest, blobs, _) = round_trip(&content, cover.as_ref(), &config);
            let parts = split(&content, &config);
            let what = format!("{name}, {config:?}");
            assert_eq!(manifest.split(), parts, "{what}");
            assert_eq!(
                manifest.block_count as usize,
                content.block_count(),
                "{what}"
            );
            for (index, blob) in blobs.iter().enumerate() {
                cross_part_links += blob
                    .anchors
                    .iter()
                    .filter(|(_, id)| !parts[index].contains(*id))
                    .count();
                assert_eq!(
                    blob.render(&manifest.split(), index, &Links),
                    render_part(&content, &parts, index, &Links),
                    "{what}, part {index}"
                );
                assert_eq!(
                    render_toc_entries(&manifest.toc, &manifest.split(), index, &Links),
                    render_toc(&content, &parts, index, &Links),
                    "{what}, contents at part {index}"
                );
                assert_eq!(
                    manifest.parts[index].progress,
                    part_progress(&full, &parts, index),
                    "{what}, progress of part {index}"
                );
                // The text that annotation offsets count in, block by block.
                for body in &blob.bodies {
                    let mut blocks = Vec::new();
                    collect(&body.body.section, &mut blocks);
                    for (id, text) in blocks {
                        assert_eq!(full.text(id), Some(text.as_str()), "{what}, {id}");
                    }
                }
            }
        }
    }
    assert!(cross_part_links > 10, "{cross_part_links}");
}

/// Every block of `section` with its rendered text.
fn collect(section: &shelfgauge::format::model::Section, out: &mut Vec<(BlockId, String)>) {
    for block in section.title.iter().chain(&section.blocks) {
        block.walk(|b| out.push((b.id, rendered_text(b))));
    }
    for child in &section.children {
        collect(child, out);
    }
}

#[test]
fn all_parts_of_a_publication_give_the_book_text_again() {
    for (name, content, cover) in books() {
        let full = BookText::new(&content);
        for config in configs() {
            let (manifest, blobs, images) = round_trip(&content, cover.as_ref(), &config);
            let images: BTreeMap<String, Image> = manifest
                .images
                .iter()
                .zip(images)
                .filter(|(info, _)| !info.id.is_empty())
                .map(|(info, data)| {
                    let image = Image {
                        mime: info.mime.clone(),
                        data,
                    };
                    (info.id.clone(), image)
                })
                .collect();
            let book = assemble(blobs, images, manifest.block_count).unwrap();
            assert_eq!(book.blocks(), content.blocks(), "{name}, {config:?}");
            let text = BookText::new(&book);
            assert_eq!(text.flat(), full.flat(), "{name}");
            assert_eq!(text.leaves(), full.leaves(), "{name}");
            for (id, image) in book.images() {
                assert_eq!(
                    &content.images()[id].data,
                    &image.data,
                    "{name}, image {id}"
                );
            }
        }
    }
}

#[test]
fn the_manifest_carries_the_book_and_its_images() {
    let books = books();
    let (_, content, cover) = books.iter().find(|(n, ..)| *n == "full.fb2").unwrap();
    let (manifest, _, images) = round_trip(content, cover.as_ref(), &ReaderConfig::default());
    assert_eq!(manifest.title, "Book");
    assert_eq!(manifest.authors, ["A. Author"]);
    assert_eq!(manifest.series, ["Series #2"]);
    assert_eq!(manifest.language.as_deref(), Some("en-GB"));
    assert_eq!(manifest.content_key, vec![7; 16]);
    assert!(!manifest.toc.is_empty());
    let cover = cover.as_ref().expect("the full fixture has a cover");
    let index = manifest.cover.expect("cover published") as usize;
    assert_eq!(images[index], cover.data);
    // Every content image has an image blob.
    for (id, image) in content.images() {
        let n = manifest.image_index(id).expect("published image") as usize;
        assert_eq!(images[n], image.data);
        assert_eq!(manifest.images[n].mime, image.mime);
    }
}

#[test]
fn only_raster_images_are_published() {
    let books = books();
    let (_, content, _) = books.iter().find(|(n, ..)| *n == "full.fb2").unwrap();
    let mut images = content.images().clone();
    images.insert(
        "evil".into(),
        Image {
            mime: "image/svg+xml".into(),
            data: b"<svg onload='alert(1)'/>".to_vec(),
        },
    );
    let content = BookContent::from_numbered(content.bodies().to_vec(), images).unwrap();
    let svg_cover = Image {
        mime: "image/svg+xml".into(),
        data: b"<svg/>".to_vec(),
    };
    let publication = build(
        &info(),
        &content,
        Some(&svg_cover),
        &ReaderConfig::default(),
    )
    .unwrap();
    assert_eq!(publication.manifest.image_index("evil"), None);
    assert_eq!(publication.manifest.cover, None);
    assert!(
        publication
            .manifest
            .images
            .iter()
            .all(|i| i.mime != "image/svg+xml")
    );
}

#[test]
fn a_cover_that_is_also_a_content_image_is_stored_once() {
    let content = BookContent::new(Vec::new(), {
        let mut images = BTreeMap::new();
        images.insert(
            "cover.png".into(),
            Image {
                mime: "image/png".into(),
                data: common::PNG.to_vec(),
            },
        );
        images
    });
    let cover = content.images()["cover.png"].clone();
    let publication = build(&info(), &content, Some(&cover), &ReaderConfig::default()).unwrap();
    assert_eq!(publication.images.len(), 1);
    assert_eq!(publication.manifest.cover, Some(0));
    assert!(
        publication.manifest.parts.is_empty(),
        "a book without text has no parts"
    );
}

#[test]
fn drm_protected_books_are_refused() {
    let content = run_with_parser_stack(|| {
        let data = common::epub3_drm();
        let metadata = EpubParser.parse_metadata(&data).unwrap();
        assert!(metadata.drm.is_some());
        let info = BookInfo {
            drm: metadata.drm,
            ..info()
        };
        (info, BookContent::default())
    });
    let (info, content) = content;
    let error = build(&info, &content, None, &ReaderConfig::default()).unwrap_err();
    assert!(matches!(error, PublishError::Protected(_)));
    assert!(error.to_string().contains("cannot be published"), "{error}");
}

#[test]
fn blobs_open_only_with_their_key_publication_and_path() {
    let books = books();
    let (_, content, cover) = &books[0];
    let publication = build(&info(), content, cover.as_ref(), &ReaderConfig::default()).unwrap();
    let key = BookKey::generate().unwrap();
    let sealed = publication.seal(&id(), &key).unwrap();
    let (path, blob) = &sealed[1];
    assert_eq!(*path, BlobPath::Part(0));
    assert!(open(&id(), &key, *path, blob, LIMIT).is_ok());

    let other_key = BookKey::generate().unwrap();
    assert!(matches!(
        open(&id(), &other_key, *path, blob, LIMIT),
        Err(OpenError::Decrypt)
    ));
    let other_id = PublicationId::new([1; 16]);
    assert!(matches!(
        open(&other_id, &key, *path, blob, LIMIT),
        Err(OpenError::Decrypt)
    ));
    // A blob moved to another position does not open there.
    assert!(matches!(
        open(&id(), &key, BlobPath::Manifest, blob, LIMIT),
        Err(OpenError::Decrypt)
    ));
    let mut altered = blob.clone();
    *altered.last_mut().unwrap() ^= 1;
    assert!(matches!(
        open(&id(), &key, *path, &altered, LIMIT),
        Err(OpenError::Decrypt)
    ));
    // A rewritten version is refused, or fails the tag (it is part of the associated data).
    let mut future = blob.clone();
    future[..2].copy_from_slice(&7u16.to_be_bytes());
    let error = open(&id(), &key, *path, &future, LIMIT).unwrap_err();
    assert!(
        matches!(
            error,
            OpenError::Format(FormatError::UnsupportedVersion { found: 7, .. })
        ),
        "{error}"
    );
    assert!(error.to_string().contains("not supported"), "{error}");
    // Truncated blobs are errors, never panics.
    for len in [
        0,
        1,
        2,
        HEADER_LEN,
        HEADER_LEN + TAG_LEN - 1,
        HEADER_LEN + TAG_LEN,
        blob.len() - 1,
    ] {
        assert!(
            open(&id(), &key, *path, &blob[..len], LIMIT).is_err(),
            "{len}"
        );
    }
}

#[test]
fn each_seal_uses_a_fresh_nonce() {
    let books = books();
    let (_, content, cover) = &books[0];
    let publication = build(&info(), content, cover.as_ref(), &ReaderConfig::default()).unwrap();
    let key = BookKey::generate().unwrap();
    let a = publication.seal(&id(), &key).unwrap();
    let b = publication.seal(&id(), &key).unwrap();
    for ((_, a), (_, b)) in a.iter().zip(&b) {
        assert_ne!(
            Envelope::parse(a).unwrap().nonce,
            Envelope::parse(b).unwrap().nonce
        );
    }
}

#[test]
fn inflating_stops_at_the_limit() {
    let zeros = compress(&vec![0u8; 1 << 20]);
    assert!(zeros.len() < 10_000);
    assert_eq!(inflate(&zeros, 1 << 20).unwrap().len(), 1 << 20);
    assert!(matches!(
        inflate(&zeros, 1000),
        Err(OpenError::TooLarge(1000))
    ));
    assert!(matches!(
        inflate(&[0xff; 32], LIMIT),
        Err(OpenError::Inflate(_))
    ));
}

#[test]
fn sealed_blobs_are_padded_to_a_bucket() {
    let books = books();
    for (name, content, cover) in &books {
        let config = ReaderConfig {
            split: SplitMode::Size,
            part_size: 2000,
            ..ReaderConfig::default()
        };
        let publication = build(&info(), content, cover.as_ref(), &config).unwrap();
        let key = BookKey::generate().unwrap();
        for ((path, blob), (_, plain)) in publication
            .seal(&id(), &key)
            .unwrap()
            .iter()
            .zip(publication.blobs())
        {
            let padded = blob.len() - HEADER_LEN - TAG_LEN;
            let compressed = compress(&plain).len();
            assert_eq!(padded, bucket(4 + compressed), "{name} {path}");
        }
    }
}

// Interop vectors: fixed key, nonce and padded plaintext → blob, so the browser side
// (WebCrypto, `DecompressionStream`) can be checked against what Rust writes. The
// encryption vector starts from the padded plaintext, after compression, so a
// compressor upgrade cannot change it; the inflate vector is separate.

fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/publication")
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

/// Compares `actual` with the committed vector file `name`; writes it instead with
/// `SHELFGAUGE_WRITE_VECTORS=1`.
fn check_vector(name: &str, actual: &serde_json::Value) {
    let path = vectors_dir().join(name);
    if std::env::var_os("SHELFGAUGE_WRITE_VECTORS").is_some() {
        std::fs::create_dir_all(vectors_dir()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(actual).unwrap() + "\n").unwrap();
    }
    let expected: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        actual, &expected,
        "{name} changed: the format is not compatible any more"
    );
}

#[test]
fn encryption_vector_is_stable() {
    let key = BookKey::new(std::array::from_fn(|i| i as u8));
    let id: PublicationId = "000102030405060708090a0b0c0d0e0f".parse().unwrap();
    let path = BlobPath::Part(3);
    let nonce: [u8; 12] = std::array::from_fn(|i| 0xa0 + i as u8);
    let padded = pad(b"shelfgauge publication vector: \xd0\xba\xd0\xbd\xd0\xb8\xd0\xb3\xd0\xb0");
    let blob = encrypt_padded(&id, &key, path, &padded, nonce);
    check_vector(
        "encryption.json",
        &serde_json::json!({
            "description": "AES-256-GCM of a padded blob: blob = version u16 BE || nonce || ciphertext || tag; associated data = version u16 BE || publication id || path (UTF-8)",
            "key": hex(key.as_bytes()),
            "publication_id": id.to_string(),
            "path": path.to_string(),
            "format_version": 1,
            "nonce": hex(&nonce),
            "associated_data": hex(&shelfgauge::publication::format::associated_data(1, &id, path)),
            "padded_plaintext": hex(&padded),
            "blob": hex(&blob),
        }),
    );
    // And it opens (the padded plaintext is not deflate data, so open stops there).
    let vector: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(vectors_dir().join("encryption.json")).unwrap(),
    )
    .unwrap();
    let blob = unhex(vector["blob"].as_str().unwrap());
    assert!(matches!(
        open(&id, &key, path, &blob, LIMIT),
        Err(OpenError::Inflate(_))
    ));
}

#[test]
fn inflate_vector_is_stable() {
    // A deflate stream written once by this crate; a newer compressor may write other
    // bytes, but every inflater must read these.
    let plain = "Глава 1. It was a dark and stormy night; the rain fell in torrents. ".repeat(4);
    let path = vectors_dir().join("inflate.json");
    if std::env::var_os("SHELFGAUGE_WRITE_VECTORS").is_some() {
        check_vector(
            "inflate.json",
            &serde_json::json!({
                "description": "raw deflate (RFC 1951, no zlib header): DecompressionStream('deflate-raw')",
                "deflate_raw": hex(&compress(plain.as_bytes())),
                "plaintext_utf8": plain,
            }),
        );
    }
    let vector: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(vector["plaintext_utf8"], plain.as_str());
    let data = unhex(vector["deflate_raw"].as_str().unwrap());
    assert_eq!(inflate(&data, LIMIT).unwrap(), plain.as_bytes());
}

#[test]
fn a_whole_sealed_publication_vector_opens() {
    // A complete small publication sealed with fixed nonces: the device side checks
    // that it decodes manifest and parts from these exact bytes. Written from the
    // current parser's output; after a model change (a new format version) regenerate
    // it with `SHELFGAUGE_WRITE_VECTORS=1`.
    let content = run_with_parser_stack(|| {
        HtmlParser
            .parse_content(common::html_tag_soup().as_bytes())
            .unwrap()
    });
    let key = BookKey::new([0x42; 32]);
    let id: PublicationId = "ffeeddccbbaa99887766554433221100".parse().unwrap();
    let publication = build(&info(), &content, None, &ReaderConfig::default()).unwrap();
    let path = vectors_dir().join("publication.json");
    if std::env::var_os("SHELFGAUGE_WRITE_VECTORS").is_some() {
        let blobs: serde_json::Map<String, serde_json::Value> = publication
            .blobs()
            .into_iter()
            .enumerate()
            .map(|(n, (path, plain))| {
                let nonce: [u8; 12] = std::array::from_fn(|i| (n * 16 + i) as u8);
                let blob = encrypt_padded(&id, &key, path, &pad(&compress(&plain)), nonce);
                (path.to_string(), hex(&blob).into())
            })
            .collect();
        check_vector(
            "publication.json",
            &serde_json::json!({
                "description": "a sealed publication of the html_tag_soup test page",
                "key": hex(key.as_bytes()),
                "publication_id": id.to_string(),
                "blobs": blobs,
            }),
        );
    }
    let vector: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let blob = |path: &str| unhex(vector["blobs"][path].as_str().unwrap());
    let manifest =
        Manifest::decode(&open(&id, &key, BlobPath::Manifest, &blob("manifest"), LIMIT).unwrap())
            .unwrap();
    assert_eq!(manifest.title, "Book");
    let parts = (0..manifest.parts.len()).map(|n| {
        let path = BlobPath::Part(n as u32);
        PartBlob::decode(&open(&id, &key, path, &blob(&path.to_string()), LIMIT).unwrap()).unwrap()
    });
    let book = assemble(parts, BTreeMap::new(), manifest.block_count).unwrap();
    assert!(!BookText::new(&book).flat().is_empty());
}

/// Times building, sealing and opening the publications of the books (any registered
/// format) in the directory named by `SHELFGAUGE_PUBLICATION_SAMPLES`, with the
/// default split, and reports blob sizes and the padding overhead:
/// `cargo test --release --test publication -- --ignored --nocapture`.
#[test]
#[ignore = "needs real books in SHELFGAUGE_PUBLICATION_SAMPLES"]
fn timing_on_real_books() {
    use std::time::Instant;

    let Ok(dir) = std::env::var("SHELFGAUGE_PUBLICATION_SAMPLES") else {
        eprintln!("SHELFGAUGE_PUBLICATION_SAMPLES is not set");
        return;
    };
    let registry = shelfgauge::format::Registry::with_builtin_formats();
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    paths.sort();
    let config = ReaderConfig::default();
    println!(
        "| book | file | parts | images | build | seal | open + decode | render all | assemble + BookText | plaintext | deflate | sealed | padding |"
    );
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for path in paths {
        let Some(parser) = registry.for_path(&path) else {
            continue;
        };
        let data = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        run_with_parser_stack(|| {
            let (Ok(metadata), Ok(content)) =
                (parser.parse_metadata(&data), parser.parse_content(&data))
            else {
                return;
            };
            let ms = |start: Instant| format!("{:.1} ms", start.elapsed().as_secs_f64() * 1e3);

            let start = Instant::now();
            let publication = build(&info(), &content, metadata.cover.as_ref(), &config).unwrap();
            let built = ms(start);

            let key = BookKey::generate().unwrap();
            let start = Instant::now();
            let sealed = publication.seal(&id(), &key).unwrap();
            let seal_time = ms(start);

            let start = Instant::now();
            let manifest = Manifest::decode(
                &open(&id(), &key, BlobPath::Manifest, &sealed[0].1, LIMIT).unwrap(),
            )
            .unwrap();
            let blobs: Vec<PartBlob> = sealed[1..=manifest.parts.len()]
                .iter()
                .map(|(path, blob)| {
                    PartBlob::decode(&open(&id(), &key, *path, blob, LIMIT).unwrap()).unwrap()
                })
                .collect();
            for (path, blob) in &sealed[1 + manifest.parts.len()..] {
                open(&id(), &key, *path, blob, LIMIT).unwrap();
            }
            let opened = ms(start);

            let parts = manifest.split();
            let start = Instant::now();
            let html: usize = blobs
                .iter()
                .enumerate()
                .map(|(i, blob)| blob.render(&parts, i, &Links).len())
                .sum();
            let rendered = ms(start);
            assert!(html > 0);

            let start = Instant::now();
            let book = assemble(blobs, BTreeMap::new(), manifest.block_count).unwrap();
            let text = BookText::new(&book);
            let assembled = ms(start);
            assert!(!text.flat().is_empty());

            let plain: usize = publication.blobs().iter().map(|(_, b)| b.len()).sum();
            let compressed: usize = publication
                .blobs()
                .iter()
                .map(|(_, b)| compress(b).len())
                .sum();
            let padded: usize = sealed
                .iter()
                .map(|(_, b)| b.len() - HEADER_LEN - TAG_LEN)
                .sum();
            let total: usize = sealed.iter().map(|(_, b)| b.len()).sum();
            let kb = |n: usize| format!("{:.0} KB", n as f64 / 1024.0);
            println!(
                "| {name} | {} | {} | {} | {built} | {seal_time} | {opened} | {rendered} | {assembled} | {} | {} | {} | {:.1} % |",
                kb(data.len()),
                manifest.parts.len(),
                manifest.images.len(),
                kb(plain),
                kb(compressed),
                kb(total),
                (padded - compressed) as f64 * 100.0 / compressed as f64,
            );
            let largest = sealed.iter().map(|(_, b)| b.len()).max().unwrap();
            let smallest = sealed.iter().map(|(_, b)| b.len()).min().unwrap();
            println!(
                "|  | blobs {}: {}–{} |",
                sealed.len(),
                kb(smallest),
                kb(largest)
            );
        });
    }
}

#[test]
fn records_vector_is_stable() {
    // Sync records and device names under the account key (Phase 25), and the HMAC
    // record ids: what the reader app's WebCrypto code must reproduce. The plaintexts
    // are examples; the crypto does not depend on their shape.
    use shelfgauge::hub_client::keys::{
        self, AccountKey, encrypt_record, open_record, position_id, record_associated_data,
        shelf_id,
    };

    let key = AccountKey::new(std::array::from_fn(|i| 0x80 + i as u8));
    let content_key: Vec<u8> = (0..16).map(|i| 0x10 + i).collect();
    let examples = [
        (
            "annotation",
            "0f8fad5b-d9cb-469f-a165-70867728950e".to_owned(),
            r#"{"text":"Глава","note":null}"#,
        ),
        (
            "position",
            position_id(&key, &content_key),
            r#"{"block":12}"#,
        ),
        (
            "shelf_item",
            shelf_id(&key, &content_key),
            r#"{"title":"Книга"}"#,
        ),
        (keys::DEVICE_NAME_KIND, String::new(), "Home library"),
    ];
    let records: Vec<serde_json::Value> = examples
        .iter()
        .enumerate()
        .map(|(n, (kind, id, plain))| {
            let nonce: [u8; 12] = std::array::from_fn(|i| (0x30 + n * 16 + i) as u8);
            let sealed = encrypt_record(&key, kind, id, plain.as_bytes(), nonce);
            assert_eq!(
                open_record(&key, kind, id, &sealed).unwrap(),
                plain.as_bytes()
            );
            serde_json::json!({
                "kind": kind,
                "id": id,
                "nonce": hex(&nonce),
                "associated_data": hex(&record_associated_data(1, kind, id)),
                "plaintext_utf8": plain,
                "record": hex(&sealed),
            })
        })
        .collect();
    check_vector(
        "records.json",
        &serde_json::json!({
            "description": "AES-256-GCM records under the account key: record = version u16 BE || nonce || ciphertext || tag; associated data = 'sg-record' || version u16 BE || kind (UTF-8) || 0x00 || record id (UTF-8); device names use kind 'device' and an empty id. Record ids: position = base64url (no padding) of HMAC-SHA256(account key, 'position' || content key), shelf_item = the same with 'shelf'",
            "account_key": hex(key.as_bytes()),
            "record_version": 1,
            "content_key": hex(&content_key),
            "position_id": position_id(&key, &content_key),
            "shelf_id": shelf_id(&key, &content_key),
            "records": records,
        }),
    );
}

#[test]
fn record_plaintexts_are_stable() {
    // The JSON the reader app parses (Phase 26): fixed rows through the record
    // builders. A renamed or added field is a change of the sync protocol.
    use shelfgauge::annotations::store::UserData;
    use shelfgauge::hub::{AccountId, Secret};
    use shelfgauge::hub_client::keys::AccountKey;
    use shelfgauge::hub_client::store::{self, NewHubAccount, ShelfEntry, ShelfState};
    use shelfgauge::hub_client::{HubUrl, records};
    use shelfgauge::profiles::ProfileId;

    let userdata = UserData::open_in_memory().unwrap();
    let conn = userdata.lock();
    let owner = ProfileId::OWNER;
    let url = HubUrl::parse("https://hub.example.org").unwrap();
    let key = AccountKey::new([1; 32]);
    let token = Secret::from_text("token");
    let account = NewHubAccount {
        hub_url: &url,
        account: AccountId::new([2; 16]),
        key: &key,
        device_id: 1,
        token: &token,
        recovery_code: None,
    };
    store::save_account(&conn, owner, &account, 0).unwrap();
    let content_key: Vec<u8> = (0..16).collect();
    let entry = ShelfEntry {
        content_key: content_key.clone(),
        publication: Some("00112233445566778899aabbccddeeff".parse().unwrap()),
        book_key: Some(BookKey::new([3; 32])),
        own: true,
        state: ShelfState::Uploading,
        instance: "Home library".into(),
        title: "Война и мир".into(),
        authors: "Лев Толстой".into(),
        published_at: Some(1_800_000_000),
        updated_at: 1_800_000_001,
    };
    store::begin_publish(&conn, owner, &entry).unwrap();
    store::set_state(&conn, owner, &content_key, ShelfState::Published, None).unwrap();
    conn.execute(
        "INSERT INTO annotations (id, profile_id, book_key, book_id, start_block, start_offset,
             end_block, end_offset, text, prefix, suffix, section_title, book_title,
             book_authors, note, created_at, updated_at)
         VALUES ('0f8fad5b-d9cb-469f-a165-70867728950e', 1, ?1, 7, 12, 3, 12, 9, 'quoted',
             'before ', ' after', 'Том 1', 'Война и мир', 'Лев Толстой', 'a note',
             1_800_000_002, 1_800_000_003)",
        [&content_key],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO reading_positions (profile_id, book_key, book_id, block, offset, suffix,
             progress, book_title, book_authors, updated_at)
         VALUES (1, ?1, 7, 40, 5, 'Глава', 379, 'Война и мир', 'Лев Толстой', 1_800_000_004)",
        [&content_key],
    )
    .unwrap();

    let text = |bytes: Vec<u8>| String::from_utf8(bytes).unwrap();
    let shelf_item = text(
        records::shelf_item(&conn, owner, &content_key)
            .unwrap()
            .unwrap(),
    );
    let annotation = text(
        records::annotation(&conn, owner, "0f8fad5b-d9cb-469f-a165-70867728950e")
            .unwrap()
            .unwrap()
            .plain,
    );
    let position = text(
        records::position(&conn, owner, &content_key)
            .unwrap()
            .unwrap(),
    );
    check_vector(
        "record_plaintexts.json",
        &serde_json::json!({
            "description": "the JSON plaintexts of sync records (UTF-8, keys sorted); binary values base64url without padding, times Unix seconds, progress per mille",
            "shelf_item": shelf_item,
            "annotation": annotation,
            "position": position,
        }),
    );
}
