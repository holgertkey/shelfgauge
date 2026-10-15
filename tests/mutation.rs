//! Mutation tests (a small in-tree fuzzer): corrupted FB2 files (plain and zipped as
//! `.fbz`), EPUB books, plain text books and zip archives must never panic, and parsed content must keep the split invariants.
//!
//! The inputs are the synthetic fixtures, mutated by a seeded generator, so runs are
//! reproducible. `SHELFGAUGE_FUZZ_ITERATIONS` sets the number of cases per test (default
//! 300) and `SHELFGAUGE_FUZZ_SEED` the seed, e.g. for a long local run:
//!
//! ```sh
//! SHELFGAUGE_FUZZ_ITERATIONS=100000 cargo test --release --test mutation
//! ```

use std::io::Write;

mod common;

use shelfgauge::config::{ReaderConfig, SplitMode};
use shelfgauge::container::ReadLimits;
use shelfgauge::container::zip::{ArchiveReader, list_books};
use shelfgauge::format::epub::EpubParser;
use shelfgauge::format::fb2::Fb2Parser;
use shelfgauge::format::model::{BlockId, BookContent};
use shelfgauge::format::txt::TxtParser;
use shelfgauge::format::{BookParser, run_with_parser_stack};
use shelfgauge::render::html::{ReaderLinks, render_part, render_toc};
use shelfgauge::render::split::{part_of, split};

const SEEDS: &[&[u8]] = &[
    include_bytes!("fixtures/fb2/full.fb2"),
    include_bytes!("fixtures/fb2/minimal.fb2"),
    include_bytes!("fixtures/fb2/unbalanced.fb2"),
];

/// Fragments inserted at random places.
const FRAGMENTS: &[&str] = &[
    "</section>",
    "<section>",
    "</p>",
    "<p>",
    "<poem><stanza><v>",
    "</body>",
    "<body name=\"notes\">",
    "<![CDATA[",
    "]]>",
    "<!--",
    "-->",
    "&amp;",
    "&#xFFFF;",
    "&unknown;",
    "<image l:href=\"#missing\"/>",
    "<a l:href=\"#n1\" type=\"note\">",
    "<a l:href=\"javascript:alert(1)\">",
    "<binary id=\"x\" content-type=\"image/png\">!!notbase64!!</binary>",
    "<table><tr><td colspan=\"0\" rowspan=\"999999\">",
    "<title><p>",
    "<?xml version=\"1.0\" encoding=\"windows-1251\"?>",
    "\u{0}",
    "\u{feff}",
    // EPUB / XHTML.
    "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.1//EN\" \"x.dtd\">",
    "&nbsp;",
    "&#0;",
    "<br>",
    "<ul><li>",
    "</li></ol>",
    "<pre>\n",
    "<hr/>",
    "<h1>",
    "</h2>",
    "<aside epub:type=\"footnote\" id=\"fn1\">",
    "<section epub:type=\"endnotes\"><ol><li id=\"n\">",
    "<a href=\"ch2.xhtml#target\" epub:type=\"noteref\">",
    "<a href=\"../../../etc/passwd\">",
    "<img src=\"../img/pic.png\"/>",
    "<svg><image xlink:href=\"../img/cover.png\"/></svg>",
    "<itemref idref=\"ch1\"/>",
    "<itemref idref=\"nav\" linear=\"no\"/>",
    "<li><span>x</span><ol>",
    "<navPoint><content src=\"#\"/>",
    "epub:type=\"",
];

/// xorshift64*: small, deterministic, good enough to pick mutations.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn env_number(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn iterations() -> u64 {
    env_number("SHELFGAUGE_FUZZ_ITERATIONS", 300)
}

fn rng() -> Rng {
    Rng(env_number("SHELFGAUGE_FUZZ_SEED", 0x5eed_5eed_5eed_5eed) | 1)
}

/// Applies 1–4 random mutations to `data`.
fn mutate(rng: &mut Rng, data: &[u8]) -> Vec<u8> {
    let mut data = data.to_vec();
    for _ in 0..=rng.below(4) {
        let at = rng.below(data.len() + 1);
        match rng.below(7) {
            0 if !data.is_empty() => {
                let i = rng.below(data.len());
                data[i] ^= 1 << rng.below(8);
            }
            1 => data.truncate(at),
            2 => {
                let fragment = FRAGMENTS[rng.below(FRAGMENTS.len())];
                data.splice(at..at, fragment.bytes());
            }
            3 if !data.is_empty() => {
                let start = rng.below(data.len());
                let end = (start + rng.below(400)).min(data.len());
                let copy = data[start..end].to_vec();
                data.splice(at..at, copy);
            }
            4 => {
                let depth = 250 + rng.below(100);
                data.splice(at..at, "<section>".repeat(depth).into_bytes());
            }
            5 if !data.is_empty() => {
                let start = rng.below(data.len());
                let end = (start + rng.below(200)).min(data.len());
                data.drain(start..end);
            }
            _ => {
                let byte = rng.next() as u8;
                data.insert(at.min(data.len()), byte);
            }
        }
    }
    data
}

struct Links;

impl ReaderLinks for Links {
    fn block(&self, part: usize, id: BlockId) -> String {
        format!("/read?part={part}#{id}")
    }

    fn image(&self, image_id: &str) -> String {
        format!("/image/{image_id}")
    }
}

/// Splits in every mode and renders every part, checking the split invariants.
fn exercise(content: &BookContent) {
    let count = content.block_count() as u32;
    for config in [
        ReaderConfig {
            split: SplitMode::None,
            ..ReaderConfig::default()
        },
        ReaderConfig {
            split: SplitMode::Section,
            section_depth: 1,
            ..ReaderConfig::default()
        },
        ReaderConfig {
            split: SplitMode::Section,
            section_depth: 1000,
            ..ReaderConfig::default()
        },
        ReaderConfig {
            split: SplitMode::Size,
            part_size: 1,
            ..ReaderConfig::default()
        },
        ReaderConfig {
            split: SplitMode::None,
            max_part_size: 50,
            ..ReaderConfig::default()
        },
    ] {
        let parts = split(content, &config);
        if count == 0 {
            assert!(parts.is_empty());
            continue;
        }
        assert_eq!(parts[0].start.ordinal(), 1);
        assert_eq!(parts.last().unwrap().end.ordinal(), count);
        for pair in parts.windows(2) {
            assert_eq!(pair[0].end.ordinal() + 1, pair[1].start.ordinal());
        }
        content.for_each_block(|block| {
            let i = part_of(&parts, block.id).expect("every block is in a part");
            assert!(parts[i].contains(block.last_id()), "a container is cut");
        });
        let mut rendered = 0;
        for i in 0..parts.len() {
            let html = render_part(content, &parts, i, &Links);
            rendered += html.matches(" id=\"b").count();
            assert!(!html.contains("<script"), "unescaped markup");
            assert!(!html.contains("href=\"javascript:"), "unsafe link");
        }
        assert_eq!(rendered, count as usize, "every block rendered once");
        render_toc(content, &parts, 0, &Links);
    }
}

#[test]
fn mutated_fb2_never_panics() {
    let mut rng = rng();
    let (mut parsed, mut failed) = (0, 0);
    for n in 0..iterations() {
        let seed = SEEDS[n as usize % SEEDS.len()];
        let data = mutate(&mut rng, seed);
        let ok = run_with_parser_stack(|| {
            let _ = Fb2Parser.parse_metadata(&data);
            match Fb2Parser.parse_content(&data) {
                Ok(content) => {
                    exercise(&content);
                    true
                }
                Err(_) => false,
            }
        });
        if ok {
            parsed += 1;
        } else {
            failed += 1;
        }
    }
    // The mutations must not be so destructive that nothing is ever parsed.
    assert!(parsed > 0 && failed > 0, "parsed {parsed}, failed {failed}");
}

#[test]
fn mutated_epub_never_panics() {
    let books = [common::epub3_entries(), common::epub2_entries()];
    let mut rng = rng();
    let (mut parsed, mut failed) = (0, 0);
    for n in 0..iterations() {
        let mut entries = books[n as usize % books.len()].clone();
        // Mutate one or two documents (or the zip itself, below).
        for _ in 0..=rng.below(2) {
            let i = rng.below(entries.len());
            if entries[i].0 != "mimetype" && !entries[i].0.ends_with(".png") {
                entries[i].1 = mutate(&mut rng, &entries[i].1);
            }
        }
        let mut data = common::zip(&entries);
        if rng.below(8) == 0 && !data.is_empty() {
            let i = rng.below(data.len());
            data[i] ^= 1 << rng.below(8);
        }
        let ok = run_with_parser_stack(|| {
            let _ = EpubParser.parse_metadata(&data);
            match EpubParser.parse_content(&data) {
                Ok(content) => {
                    exercise(&content);
                    true
                }
                Err(_) => false,
            }
        });
        if ok {
            parsed += 1;
        } else {
            failed += 1;
        }
    }
    assert!(parsed > 0 && failed > 0, "parsed {parsed}, failed {failed}");
}

#[test]
fn mutated_fbz_never_panics() {
    let mut rng = rng();
    let (mut parsed, mut failed) = (0, 0);
    for n in 0..iterations() {
        let seed = SEEDS[n as usize % SEEDS.len()];
        let mut entries = vec![("book.fb2", mutate(&mut rng, seed))];
        if rng.below(3) == 0 {
            entries.insert(0, ("cover.jpg", common::PNG.to_vec()));
        }
        let mut data = common::zip(&entries);
        if rng.below(4) == 0 && !data.is_empty() {
            data = mutate(&mut rng, &data);
        }
        let ok = run_with_parser_stack(|| {
            let _ = Fb2Parser.parse_metadata(&data);
            match Fb2Parser.parse_content(&data) {
                Ok(content) => {
                    exercise(&content);
                    true
                }
                Err(_) => false,
            }
        });
        if ok {
            parsed += 1;
        } else {
            failed += 1;
        }
    }
    assert!(parsed > 0 && failed > 0, "parsed {parsed}, failed {failed}");
}

/// Text-specific changes: line ends, controls, headings, rules, verse, other code pages
/// and a line without breaks.
const TEXT_FRAGMENTS: &[&[u8]] = &[
    b"\r",
    b"\r\n",
    b"\n\n",
    b"\x0c",
    b"\t",
    b"\0",
    b"\xef\xbb\xbf",
    b"\xff\xfe",
    b"\n\nCHAPTER IV\n\n",
    b"\n\n\xd0\x93\xd0\xbb\xd0\xb0\xd0\xb2\xd0\xb0 1\n",
    b"\n\n\xc3\xeb\xe0\xe2\xe0 2\n\n",
    b"\n* * *\n",
    b"\n-----\n",
    b"\nXIV\n\n",
    b"\n          short line\n          another one\n",
    b"     ",
    b"-\n",
];

#[test]
fn mutated_txt_never_panics() {
    let libru = common::txt_libru();
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(&libru);
    let (koi8, _, _) = encoding_rs::KOI8_R.encode(&libru);
    let seeds: Vec<Vec<u8>> = vec![
        libru.clone().into_bytes(),
        cp1251.into_owned(),
        koi8.into_owned(),
        common::txt_gutenberg().into_bytes(),
        "Один очень длинный абзац без переводов строк. "
            .repeat(100)
            .into_bytes(),
    ];
    let mut rng = rng();
    for n in 0..iterations() {
        let mut data = mutate(&mut rng, &seeds[n as usize % seeds.len()]);
        for _ in 0..rng.below(4) {
            let at = rng.below(data.len() + 1);
            let fragment = TEXT_FRAGMENTS[rng.below(TEXT_FRAGMENTS.len())];
            data.splice(at..at, fragment.iter().copied());
        }
        if rng.below(50) == 0 {
            let at = rng.below(data.len() + 1);
            data.splice(at..at, "x".repeat(150_000).into_bytes());
        }
        if rng.below(20) == 0 {
            data = (0..rng.below(5000)).map(|_| rng.next() as u8).collect();
        }
        run_with_parser_stack(|| {
            let _ = TxtParser.parse_metadata(&data);
            let _ = TxtParser.parse_metadata_named(&data, "Автор - Книга.txt");
            let content = TxtParser
                .parse_content(&data)
                .expect("plain text always parses");
            exercise(&content);
        });
    }
}

fn zip_bytes() -> Vec<u8> {
    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buffer);
        for (i, seed) in SEEDS.iter().enumerate() {
            let method = if i % 2 == 0 {
                zip::CompressionMethod::Deflated
            } else {
                zip::CompressionMethod::Stored
            };
            zip.start_file(
                format!("dir/book{i}.fb2"),
                zip::write::SimpleFileOptions::default().compression_method(method),
            )
            .unwrap();
            zip.write_all(seed).unwrap();
        }
        zip.finish().unwrap();
    }
    buffer.into_inner()
}

#[test]
fn corrupted_zip_archives_never_panic() {
    let original = zip_bytes();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("archive.zip");
    let limits = ReadLimits::default();
    let mut rng = rng();
    for _ in 0..iterations() {
        let mut data = original.clone();
        for _ in 0..=rng.below(3) {
            if data.is_empty() {
                break;
            }
            match rng.below(3) {
                0 => {
                    let i = rng.below(data.len());
                    data[i] ^= 1 << rng.below(8);
                }
                1 => data.truncate(rng.below(data.len() + 1)),
                _ => {
                    let i = rng.below(data.len());
                    let end = (i + 8).min(data.len());
                    for byte in &mut data[i..end] {
                        *byte = rng.next() as u8;
                    }
                }
            }
        }
        std::fs::write(&path, &data).unwrap();
        let Ok(entries) = list_books(&path, &["fb2"]) else {
            continue;
        };
        let Ok(mut reader) = ArchiveReader::open(&path) else {
            continue;
        };
        for entry in entries {
            if let Ok(bytes) = reader.read_entry(&entry.entry, &limits) {
                let _ = run_with_parser_stack(|| Fb2Parser.parse_content(&bytes));
            }
        }
    }
}
