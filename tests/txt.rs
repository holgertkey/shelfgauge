//! Plain text books: style detection, headings, verse, rules, metadata and the golden
//! outline of block numbering.

mod common;

use std::collections::BTreeMap;

use shelfgauge::config::{ReaderConfig, SplitMode};
use shelfgauge::format::model::{BlockKind, Inline, Section};
use shelfgauge::format::txt::{Style, TxtParser, style_of};
use shelfgauge::format::{BookParser, run_with_parser_stack};
use shelfgauge::render::split::split;

/// Prints what the parser makes of every `.txt` in the directory named by
/// `SHELFGAUGE_TXT_SAMPLES`: `cargo test --test txt -- --ignored --nocapture`.
#[test]
#[ignore = "needs real books in SHELFGAUGE_TXT_SAMPLES"]
fn report_on_real_books() {
    let Ok(dir) = std::env::var("SHELFGAUGE_TXT_SAMPLES") else {
        eprintln!("SHELFGAUGE_TXT_SAMPLES is not set");
        return;
    };
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "txt"))
        .collect();
    paths.sort();
    for path in paths {
        let data = std::fs::read(&path).unwrap();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        println!("\n=== {name}");
        run_with_parser_stack(|| report(&data, &name));
    }
}

fn report(data: &[u8], name: &str) {
    let start = std::time::Instant::now();
    match TxtParser.parse_metadata_named(data, name) {
        Ok(m) => println!(
            "title {:?}; authors {:?}",
            m.title,
            m.authors
                .iter()
                .map(shelfgauge::storage::authors::display_name)
                .collect::<Vec<_>>(),
        ),
        Err(e) => {
            println!("metadata: {e} (not a book: {})", e.is_not_a_book());
            return;
        }
    }
    let content = TxtParser.parse_content(data).unwrap();
    println!(
        "parsed in {:?}; style {:?}",
        start.elapsed(),
        style_of(data)
    );
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    let mut verse = 0;
    content.for_each_block(|b| {
        *kinds.entry(b.kind.name()).or_default() += 1;
        if let BlockKind::Paragraph(inlines) = &b.kind
            && inlines.contains(&Inline::LineBreak)
        {
            verse += 1;
        }
    });
    println!(
        "blocks {}: {kinds:?}; with line breaks {verse}",
        content.block_count()
    );
    let main = &content.bodies()[0].section;
    let first: Vec<String> = main
        .blocks
        .iter()
        .take(3)
        .map(|b| b.plain_text().chars().take(60).collect())
        .collect();
    println!(
        "before the first section: {} blocks {first:?}",
        main.blocks.len()
    );
    let mut toc = Vec::new();
    titles(main, 0, &mut toc);
    let shown: Vec<_> = toc.iter().take(15).collect();
    println!("toc ({}): {shown:?}", toc.len());
    let mut sample = Vec::new();
    content.for_each_block(|b| {
        if sample.len() < 4 && b.id.ordinal() > 40 && matches!(b.kind, BlockKind::Paragraph(_)) {
            sample.push(b.plain_text().chars().take(100).collect::<String>());
        }
    });
    println!("sample paragraphs: {sample:#?}");
    for mode in [SplitMode::Section, SplitMode::Size] {
        let parts = split(
            &content,
            &ReaderConfig {
                split: mode,
                ..ReaderConfig::default()
            },
        );
        println!("{mode:?}: {} parts", parts.len());
    }
}

fn titles(section: &Section, depth: usize, out: &mut Vec<String>) {
    for child in &section.children {
        out.push(format!(
            "{}{}",
            "  ".repeat(depth),
            child.title_text().unwrap_or_else(|| "<untitled>".into())
        ));
        titles(child, depth + 1, out);
    }
}

/// One line per block: `b<id> <kind> <text>`, section titles marked with `#`.
fn outline(data: &[u8]) -> Vec<String> {
    fn walk(section: &Section, depth: usize, out: &mut Vec<String>) {
        for block in &section.title {
            out.push(format!(
                "{}# b{} {}",
                "  ".repeat(depth),
                block.id.ordinal(),
                block.plain_text()
            ));
        }
        for block in &section.blocks {
            let text: String = match &block.kind {
                BlockKind::Paragraph(inlines) => inlines
                    .iter()
                    .map(|i| match i {
                        Inline::Text(t) => t.clone(),
                        _ => " / ".to_owned(),
                    })
                    .collect(),
                _ => String::new(),
            };
            let short: String = text.chars().take(30).collect();
            out.push(format!(
                "{}b{} {} {short}",
                "  ".repeat(depth),
                block.id.ordinal(),
                block.kind.name()
            ));
        }
        for child in &section.children {
            walk(child, depth + 1, out);
        }
    }
    let content = TxtParser.parse_content(data).unwrap();
    let mut out = Vec::new();
    walk(&content.bodies()[0].section, 0, &mut out);
    out
}

/// The golden outline pins the block numbering of plain text books: if it changes, the
/// block IDs of existing TXT books changed: bump `PARSER_VERSION`.
#[test]
fn golden_outline() {
    let text = common::txt_libru();
    let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(&text);
    let p = "Несколько лет тому назад в одн";
    let expected = [
        "b1 paragraph Александр Пушкин. Дубровский".to_owned(),
        "b2 rule ".to_owned(),
        "b3 paragraph Текст по изданию 1936 года".to_owned(),
        "b4 rule ".to_owned(),
        "  # b5 ЧАСТЬ ПЕРВАЯ".to_owned(),
        "    # b6 ГЛАВА I.".to_owned(),
        "    # b7 Троекуров".to_owned(),
        format!("    b8 paragraph {p}"),
        format!("    b9 paragraph {p}"),
        format!("    b10 paragraph {p}"),
        format!("    b11 paragraph {p}"),
        "    b12 paragraph Мороз и солнце; день чудесный!".to_owned(),
        format!("    b13 paragraph {p}"),
        format!("    b14 paragraph {p}"),
        "    b15 rule ".to_owned(),
        format!("    b16 paragraph {p}"),
        format!("    b17 paragraph {p}"),
        "    b18 paragraph - Да.".to_owned(),
        "    b19 paragraph - Нет.".to_owned(),
        "    # b20 ГЛАВА II.".to_owned(),
        format!("    b21 paragraph {p}"),
        format!("    b22 paragraph {p}"),
        format!("    b23 paragraph {p}"),
        format!("    b24 paragraph {p}"),
        format!("    b25 paragraph {p}"),
    ];
    assert_eq!(outline(&bytes), expected);
}

#[test]
fn verse_keeps_its_line_breaks_and_paragraphs_are_joined() {
    let content = TxtParser
        .parse_content(common::txt_libru().as_bytes())
        .unwrap();
    let blocks = content.blocks();
    let verse = blocks.iter().find(|b| b.id.ordinal() == 12).unwrap();
    assert_eq!(
        verse.kind,
        BlockKind::Paragraph(vec![
            Inline::Text("Мороз и солнце; день чудесный!".into()),
            Inline::LineBreak,
            Inline::Text("Еще ты дремлешь, друг прелестный -".into()),
            Inline::LineBreak,
            Inline::Text("Пора, красавица, проснись.".into()),
        ])
    );
    let paragraph = blocks.iter().find(|b| b.id.ordinal() == 8).unwrap();
    assert_eq!(
        paragraph.plain_text(),
        "Несколько лет тому назад в одном из своих поместий жил старинный русской барин, \
         Кирила Петрович Троекуров. Его богатство, знатный род и связи давали ему большой \
         вес в губерниях, где находилось его имение."
    );
}

#[test]
fn every_code_page_gives_the_same_book() {
    let text = common::txt_libru();
    let expected = outline(text.as_bytes());
    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice(text.as_bytes());
    let crlf = text.replace('\n', "\r\n");
    let mut inputs = vec![bom, crlf.into_bytes()];
    for encoding in [
        encoding_rs::WINDOWS_1251,
        encoding_rs::KOI8_R,
        encoding_rs::IBM866,
        encoding_rs::UTF_16LE,
    ] {
        let mut bytes = Vec::new();
        if encoding == encoding_rs::UTF_16LE {
            bytes.extend_from_slice(&[0xff, 0xfe]);
            for unit in text.encode_utf16() {
                bytes.extend_from_slice(&unit.to_le_bytes());
            }
        } else {
            let (encoded, _, unmappable) = encoding.encode(&text);
            assert!(!unmappable, "{}", encoding.name());
            bytes.extend_from_slice(&encoded);
        }
        inputs.push(bytes);
    }
    for bytes in inputs {
        assert_eq!(outline(&bytes), expected);
        assert_eq!(style_of(&bytes), Style::Indented);
    }
}

#[test]
fn gutenberg_style_book() {
    let data = common::txt_gutenberg();
    assert_eq!(style_of(data.as_bytes()), Style::Blocks);
    let outline = outline(data.as_bytes());
    let titles: Vec<&String> = outline.iter().filter(|l| l.contains('#')).collect();
    assert_eq!(
        titles,
        ["  # b5 CHAPTER I", "  # b12 CHAPTER II"],
        "{outline:#?}"
    );
    assert_eq!(
        &outline[..4],
        [
            "b1 paragraph PRIDE AND PREJUDICE / By Jane ",
            "b2 paragraph Contents",
            "b3 paragraph CHAPTER I",
            "b4 paragraph CHAPTER II"
        ],
        "the contents lines stay text"
    );
    assert!(outline.contains(&"  b9 rule ".to_owned()), "{outline:#?}");
}

#[test]
fn metadata_from_the_file_name() {
    let data = common::txt_libru();
    let metadata = TxtParser
        .parse_metadata_named(data.as_bytes(), "Пушкин - Дубровский.txt")
        .unwrap();
    assert_eq!(metadata.title, "Дубровский");
    assert_eq!(
        metadata
            .authors
            .iter()
            .map(shelfgauge::storage::authors::display_name)
            .collect::<Vec<_>>(),
        ["Пушкин"]
    );
    for name in ["readme.txt", "LICENSE.txt"] {
        let err = TxtParser
            .parse_metadata_named(data.as_bytes(), name)
            .unwrap_err();
        assert!(err.is_not_a_book(), "{name}");
    }
}

#[test]
fn a_text_without_line_breaks_is_cut_into_paragraphs() {
    let text = "Слово за словом шла длинная фраза без конца. ".repeat(10_000);
    let content = TxtParser.parse_content(text.as_bytes()).unwrap();
    assert!(content.block_count() >= 4, "{}", content.block_count());
    for mode in [SplitMode::Section, SplitMode::Size] {
        let parts = split(
            &content,
            &ReaderConfig {
                split: mode,
                ..ReaderConfig::default()
            },
        );
        assert!(parts.len() > 1, "{mode:?}");
    }
}
