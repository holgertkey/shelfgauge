//! Mutation tests (a small in-tree fuzzer): corrupted FB2 files and zip archives must
//! never panic, and parsed content must keep the split invariants.
//!
//! The inputs are the synthetic fixtures, mutated by a seeded generator, so runs are
//! reproducible. `SHELFGAUGE_FUZZ_ITERATIONS` sets the number of cases per test (default
//! 300) and `SHELFGAUGE_FUZZ_SEED` the seed, e.g. for a long local run:
//!
//! ```sh
//! SHELFGAUGE_FUZZ_ITERATIONS=100000 cargo test --release --test mutation
//! ```

use std::io::Write;

use shelfgauge::config::{ReaderConfig, SplitMode};
use shelfgauge::container::ReadLimits;
use shelfgauge::container::zip::{ArchiveReader, list_books};
use shelfgauge::format::fb2::Fb2Parser;
use shelfgauge::format::model::{BlockId, BookContent};
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
            assert!(!html.contains("javascript:"), "unsafe link");
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
