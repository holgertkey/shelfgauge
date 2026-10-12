//! FB2 parser tests against the synthetic fixtures in `tests/fixtures/fb2/`.

use shelfgauge::format::fb2::Fb2Parser;
use shelfgauge::format::model::{BlockKind, BodyKind, BookContent, BookMetadata};
use shelfgauge::format::{BookParser, ParseError, Registry, run_with_parser_stack};

const MINIMAL: &[u8] = include_bytes!("fixtures/fb2/minimal.fb2");
const FULL: &[u8] = include_bytes!("fixtures/fb2/full.fb2");

fn metadata(data: &[u8]) -> Result<BookMetadata, ParseError> {
    let data = data.to_vec();
    run_with_parser_stack(move || Fb2Parser.parse_metadata(&data))
}

fn content(data: &[u8]) -> Result<BookContent, ParseError> {
    let data = data.to_vec();
    run_with_parser_stack(move || Fb2Parser.parse_content(&data))
}

fn kinds(content: &BookContent) -> Vec<&'static str> {
    content.blocks().iter().map(|b| b.kind.name()).collect()
}

#[test]
fn minimal_book() {
    let m = metadata(MINIMAL).unwrap();
    assert_eq!(m.title, "Minimal Book");
    assert_eq!(m.authors[0].first_name.as_deref(), Some("Jane"));
    assert_eq!(m.genres, ["prose"]);
    assert_eq!(m.language.as_deref(), Some("en"));
    assert!(m.annotation.is_empty() && m.cover.is_none() && m.series.is_empty());

    let c = content(MINIMAL).unwrap();
    assert_eq!(c.bodies().len(), 1);
    assert_eq!(kinds(&c), ["paragraph"]);
    assert_eq!(c.blocks()[0].plain_text(), "The only paragraph.");
}

#[test]
fn full_book_metadata() {
    let m = metadata(FULL).unwrap();
    assert_eq!(m.title, "Старый маяк");
    assert_eq!(m.genres, ["sf_fantasy", "adventure"]);
    assert_eq!(
        m.authors.len(),
        2,
        "translators and document authors are not authors"
    );
    assert_eq!(m.authors[0].middle_name.as_deref(), Some("Петрович"));
    assert_eq!(
        m.authors[1].nickname.as_deref(),
        Some("The Lighthouse Keeper")
    );
    let series: Vec<(&str, Option<&str>)> = m
        .series
        .iter()
        .map(|s| (s.name.as_str(), s.index.as_deref()))
        .collect();
    assert_eq!(
        series,
        [("Sea Stories", Some("2")), ("Lighthouses", Some("1.5"))]
    );
    assert_eq!(
        m.annotation.plain_text(),
        "A short synthetic book about an old lighthouse.\nSecond annotation paragraph."
    );
    assert_eq!(m.language.as_deref(), Some("ru"));
    let cover = m.cover.unwrap();
    assert_eq!(cover.mime, "image/png");
    assert!(cover.data.starts_with(b"\x89PNG"));
}

#[test]
fn full_book_content() {
    let c = content(FULL).unwrap();
    let bodies = c.bodies();
    assert_eq!(bodies.len(), 2);
    assert_eq!(
        (bodies[0].kind, bodies[1].kind),
        (BodyKind::Main, BodyKind::Notes)
    );

    let main = &bodies[0].section;
    assert_eq!(main.title_text().as_deref(), Some("Старый маяк"));
    let titles: Vec<Option<String>> = main.children.iter().map(|s| s.title_text()).collect();
    assert_eq!(
        titles,
        [
            Some("Глава 1 Смотритель".to_owned()),
            Some("Глава 2".to_owned())
        ]
    );
    let ch2 = &main.children[1];
    assert_eq!(ch2.children.len(), 2);
    assert_eq!(ch2.children[1].anchor.as_deref(), Some("ch2-2"));

    let note = &bodies[1].section.children[0];
    assert_eq!(note.anchor.as_deref(), Some("n1"));

    let ch1_kinds: Vec<&str> = main.children[0]
        .blocks
        .iter()
        .map(|b| b.kind.name())
        .collect();
    assert_eq!(
        ch1_kinds,
        [
            "epigraph",
            "image",
            "annotation",
            "paragraph",
            "paragraph",
            "paragraph",
            "subtitle",
            "empty_line",
            "poem",
            "cite",
            "table"
        ]
    );

    let images: Vec<&str> = c.images().keys().map(String::as_str).collect();
    assert_eq!(images, ["cover.png", "map.png"]);
    assert_eq!(c.images()["map.png"].mime, "image/png");

    // Every block is numbered consecutively.
    let ordinals: Vec<u32> = c.blocks().iter().map(|b| b.id.ordinal()).collect();
    assert_eq!(ordinals, (1..=ordinals.len() as u32).collect::<Vec<_>>());
    let poem_lines = c
        .blocks()
        .iter()
        .filter(|b| matches!(b.kind, BlockKind::Verse(_)))
        .count();
    assert_eq!(poem_lines, 5);
}

#[test]
fn same_book_in_legacy_encodings_parses_identically() {
    let text = std::str::from_utf8(FULL).unwrap();
    let expected_metadata = metadata(FULL).unwrap();
    let expected_content = content(FULL).unwrap();
    for (encoding, label) in [
        (encoding_rs::WINDOWS_1251, "windows-1251"),
        (encoding_rs::KOI8_R, "koi8-r"),
    ] {
        let relabeled = text.replacen("encoding=\"UTF-8\"", &format!("encoding=\"{label}\""), 1);
        let (bytes, _, unmappable) = encoding.encode(&relabeled);
        assert!(!unmappable);
        assert_eq!(metadata(&bytes).unwrap(), expected_metadata, "{label}");
        assert_eq!(content(&bytes).unwrap(), expected_content, "{label}");
    }
}

#[test]
fn registry_finds_the_fb2_parser() {
    let registry = Registry::with_builtin_formats();
    let parser = registry
        .for_path(std::path::Path::new("Books/Маяк.FB2"))
        .unwrap();
    assert_eq!(parser.format(), "fb2");
}

#[test]
fn huge_base64_image() {
    let image: Vec<u8> = (0..6_000_000u32).map(|i| (i % 251) as u8).collect();
    let mut encoded = String::new();
    use base64::Engine as _;
    for chunk in base64::engine::general_purpose::STANDARD
        .encode(&image)
        .as_bytes()
        .chunks(76)
    {
        encoded.push_str(std::str::from_utf8(chunk).unwrap());
        encoded.push('\n');
    }
    let xml = format!(
        r##"<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
        <book-title>Big</book-title><coverpage><image l:href="#big"/></coverpage></title-info></description>
        <body><section><p>x</p></section></body>
        <binary id="big" content-type="image/jpeg">{encoded}</binary></FictionBook>"##
    );
    let m = metadata(xml.as_bytes()).unwrap();
    let cover = m.cover.unwrap();
    assert_eq!(cover.data, image);
    assert_eq!(
        cover.mime, "image/jpeg",
        "unknown magic falls back to content-type"
    );
    assert_eq!(content(xml.as_bytes()).unwrap().images()["big"].data, image);
}

/// A small deterministic xorshift generator, so the test needs no extra dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Mutated and random input must never panic: parsing either succeeds or returns an
/// error.
#[test]
fn mutated_and_random_input_never_panics() {
    run_with_parser_stack(|| {
        let mut rng = Rng(0x5eed_1234_abcd_ef01);
        let interesting: &[&[u8]] = &[
            b"<",
            b">",
            b"</",
            b"/>",
            b"&",
            b"\"",
            b"<section>",
            b"\xff",
            b"\0",
        ];
        let mut parsed = 0;
        for round in 0..1500 {
            let mut data = FULL.to_vec();
            match round % 4 {
                0 => {
                    let at = rng.below(data.len());
                    data.truncate(at);
                }
                1 => {
                    for _ in 0..1 + rng.below(8) {
                        let at = rng.below(data.len());
                        data[at] = rng.next() as u8;
                    }
                }
                2 => {
                    for _ in 0..1 + rng.below(4) {
                        let at = rng.below(data.len());
                        let piece = interesting[rng.below(interesting.len())];
                        data.splice(at..at, piece.iter().copied());
                    }
                }
                _ => {
                    data = (0..rng.below(4096)).map(|_| rng.next() as u8).collect();
                }
            }
            if Fb2Parser.parse_metadata(&data).is_ok() {
                parsed += 1;
            }
            let _ = Fb2Parser.parse_content(&data);
        }
        assert!(parsed > 0, "some mutations should still be valid FB2");
    });
}

/// Regression: a stray `</section>` (found 2026-10-11 in a real library) made the book
/// unreadable ("expected 'body' tag, not 'section'"); it is now repaired.
#[test]
fn stray_close_tag_is_repaired() {
    const UNBALANCED: &[u8] = include_bytes!("fixtures/fb2/unbalanced.fb2");
    assert_eq!(metadata(UNBALANCED).unwrap().title, "Лишний тег");
    let content = content(UNBALANCED).unwrap();
    let chapters: Vec<String> = content.bodies()[0]
        .section
        .children
        .iter()
        .filter_map(|s| s.title_text())
        .collect();
    assert_eq!(chapters, ["Глава 1", "Глава 2", "Глава 3"]);
}
