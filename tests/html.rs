//! HTML books: encodings, tag soup, lib.ru `<pre>` books, Gutenberg-style chapters,
//! pages that are not books, metadata and the golden outline.

mod common;

use std::collections::BTreeMap;

use shelfgauge::config::{ReaderConfig, SplitMode};
use shelfgauge::format::html::HtmlParser;
use shelfgauge::format::model::{BlockKind, Section};
use shelfgauge::format::{BookParser, run_with_parser_stack};
use shelfgauge::render::split::split;

/// Prints what the parser makes of every `.html`/`.htm` in the directory named by
/// `SHELFGAUGE_HTML_SAMPLES`: `cargo test --test html -- --ignored --nocapture`.
#[test]
#[ignore = "needs real pages in SHELFGAUGE_HTML_SAMPLES"]
fn report_on_real_books() {
    let Ok(dir) = std::env::var("SHELFGAUGE_HTML_SAMPLES") else {
        eprintln!("SHELFGAUGE_HTML_SAMPLES is not set");
        return;
    };
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "html" || e == "htm"))
        .collect();
    paths.sort();
    for path in paths {
        let data = std::fs::read(&path).unwrap();
        println!("\n=== {}", path.file_name().unwrap().to_string_lossy());
        run_with_parser_stack(|| report(&data));
    }
}

fn report(data: &[u8]) {
    let start = std::time::Instant::now();
    match HtmlParser.parse_metadata(data) {
        Ok(m) => println!(
            "title {:?}; authors {:?}; lang {:?}; annotation {} chars",
            m.title,
            m.authors
                .iter()
                .map(shelfgauge::storage::authors::display_name)
                .collect::<Vec<_>>(),
            m.language,
            m.annotation.plain_text().chars().count(),
        ),
        Err(e) => {
            println!("metadata: {e} (not a book: {})", e.is_not_a_book());
            if e.is_not_a_book() {
                return;
            }
        }
    }
    let content = match HtmlParser.parse_content(data) {
        Ok(content) => content,
        Err(e) => {
            println!("content error: {e}");
            return;
        }
    };
    println!("parsed in {:?}", start.elapsed());
    let mut kinds: BTreeMap<&str, usize> = BTreeMap::new();
    content.for_each_block(|b| *kinds.entry(b.kind.name()).or_default() += 1);
    println!("blocks {}: {kinds:?}", content.block_count());
    for body in content.bodies() {
        println!(
            "body {:?}: {} sections",
            body.kind,
            body.section.children.len()
        );
    }
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
        if sample.len() < 3 && b.id.ordinal() > 60 && matches!(b.kind, BlockKind::Paragraph(_)) {
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
            let text: String = block.plain_text().chars().take(20).collect();
            out.push(format!(
                "{}b{} {} {text}",
                "  ".repeat(depth),
                block.id.ordinal(),
                block.kind.name()
            ));
        }
        for child in &section.children {
            walk(child, depth + 1, out);
        }
    }
    let content = run_with_parser_stack(|| HtmlParser.parse_content(data)).unwrap();
    let mut out = Vec::new();
    for body in content.bodies() {
        walk(&body.section, 0, &mut out);
    }
    out
}

/// Section titles only.
fn section_titles(data: &[u8]) -> Vec<String> {
    outline(data)
        .into_iter()
        .filter(|l| l.trim_start().starts_with('#'))
        .map(|l| {
            let (indent, rest) = l.split_at(l.len() - l.trim_start().len());
            let title = rest.splitn(3, ' ').nth(2).unwrap_or_default();
            format!("{indent}{title}")
        })
        .collect()
}

/// The golden outline pins the block numbering of HTML books: if it changes, the block
/// IDs of existing HTML books changed: bump `PARSER_VERSION`.
#[test]
fn golden_outline() {
    let expected = [
        "b1 subtitle WAR AND PEACE",
        "  # b2 BOOK ONE: 1805",
        "    # b3 CHAPTER I",
        "    b4 paragraph “Well, Prince, so Ge",
        "    b5 paragraph “Well, Prince, so Ge",
        "    b6 paragraph “Well, Prince, so Ge",
        "    # b7 CHAPTER II",
        "    b8 paragraph “Well, Prince, so Ge",
        "    b9 paragraph “Well, Prince, so Ge",
        "    b10 paragraph “Well, Prince, so Ge",
        "  # b11 BOOK TWO: 1805",
        "    # b12 CHAPTER I",
        "    b13 paragraph “Well, Prince, so Ge",
        "    b14 paragraph “Well, Prince, so Ge",
        "    b15 paragraph “Well, Prince, so Ge",
    ];
    assert_eq!(outline(common::html_gutenberg().as_bytes()), expected);
}

#[test]
fn gutenberg_metadata() {
    let metadata = HtmlParser
        .parse_metadata(common::html_gutenberg().as_bytes())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(metadata.title, "War and Peace");
    assert_eq!(
        metadata
            .authors
            .iter()
            .map(shelfgauge::storage::authors::display_name)
            .collect::<Vec<_>>(),
        ["Tolstoy Leo"]
    );
    assert_eq!(metadata.language.as_deref(), Some("en"));
    assert!(metadata.cover.is_none());
}

#[test]
fn tag_soup_book() {
    let html = common::html_tag_soup();
    let metadata = HtmlParser.parse_metadata(html.as_bytes()).unwrap();
    assert_eq!(metadata.title, "Маяк");
    assert_eq!(metadata.authors.len(), 1);
    assert_eq!(metadata.annotation.plain_text(), "Повесть о маяке");
    assert_eq!(section_titles(html.as_bytes()), ["  Глава 1", "  Глава 2"]);
    let content = HtmlParser.parse_content(html.as_bytes()).unwrap();
    let mut links = Vec::new();
    content.for_each_block(|b| {
        if let BlockKind::Paragraph(inlines) = &b.kind {
            for inline in inlines {
                if let shelfgauge::format::model::Inline::Link { target, .. } = inline {
                    links.push(format!("{target:?}"));
                }
            }
        }
    });
    assert_eq!(links.len(), 2, "{links:?}");
    assert!(links[0].starts_with("Anchor("), "{links:?}");
    assert!(links[1].contains("http://example.org/"), "{links:?}");
    let text: Vec<String> = content.blocks().iter().map(|b| b.plain_text()).collect();
    assert!(
        text.iter().any(|t| t.contains("другой файл")),
        "relative links stay text"
    );
    assert!(text.iter().any(|t| t.contains("Вторая строка")));
}

#[test]
fn encodings() {
    let html = common::html_tag_soup();
    let with_meta = html.replace("<HEAD>", "<HEAD><META CHARSET=windows-1251>");
    let (cp1251, _, _) = encoding_rs::WINDOWS_1251.encode(&with_meta);
    let (koi8, _, _) = encoding_rs::KOI8_R.encode(&html);
    let lying = html.replace(
        "<HEAD>",
        "<HEAD><meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1251\">",
    );
    let mut bom = vec![0xef, 0xbb, 0xbf];
    bom.extend_from_slice(html.as_bytes());
    for (name, bytes) in [
        ("cp1251 with meta", cp1251.into_owned()),
        ("koi8 without meta", koi8.into_owned()),
        ("utf-8 with a wrong meta", lying.into_bytes()),
        ("bom", bom),
    ] {
        let metadata = HtmlParser.parse_metadata(&bytes).unwrap();
        assert_eq!(metadata.title, "Маяк", "{name}");
        assert_eq!(section_titles(&bytes), ["  Глава 1", "  Глава 2"], "{name}");
    }
}

#[test]
fn libru_pre_book_reads_like_the_text_file() {
    let html = common::html_libru();
    let metadata = HtmlParser
        .parse_metadata(html.as_bytes())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(metadata.title, "Дубровский");
    let titles = section_titles(html.as_bytes());
    assert_eq!(
        titles,
        [
            "  ЧАСТЬ ПЕРВАЯ",
            "    ГЛАВА I.",
            "    Троекуров",
            "    ГЛАВА II."
        ],
        "the same outline as the text file"
    );
    let outline = outline(html.as_bytes());
    assert!(
        !outline.iter().any(|l| l.contains("Содержание")),
        "the form is left out"
    );
    assert!(
        outline
            .iter()
            .any(|l| l.contains("paragraph Мороз и солнце")),
        "{outline:#?}"
    );
    assert!(outline.iter().any(|l| l.ends_with("rule ")), "{outline:#?}");
}

#[test]
fn pages_that_are_not_books() {
    for (name, html) in [
        ("saved page", common::html_saved_page()),
        ("short", "<html><body><p>Hello</p></body></html>".to_owned()),
        ("empty", String::new()),
    ] {
        let err = HtmlParser.parse_metadata(html.as_bytes()).unwrap_err();
        assert!(err.is_not_a_book(), "{name}: {err}");
    }
}

#[test]
fn deep_nesting_is_an_error_in_time() {
    let start = std::time::Instant::now();
    let html = format!("<p>{}x", "<div>".repeat(10_000));
    let err = HtmlParser.parse_content(html.as_bytes()).unwrap_err();
    assert!(err.message.contains("nested more than"), "{err}");
    assert!(!err.is_not_a_book());
    assert!(start.elapsed().as_secs() < 10, "{:?}", start.elapsed());
}

#[test]
fn adversarial_tag_soup_is_fast() {
    // Foster parenting around a table, misnested formatting and links.
    let cases = [
        format!("<table>{}</table>", "text<b>".repeat(20_000)),
        "<a href=#x>a<p>b</a>c".repeat(5_000),
        "<b><i>x</b></i>".repeat(20_000),
        "<p><font><font><font>x".repeat(5_000),
    ];
    for html in cases {
        let start = std::time::Instant::now();
        let _ = run_with_parser_stack(|| HtmlParser.parse_content(html.as_bytes()));
        assert!(start.elapsed().as_secs() < 20, "{:?}", start.elapsed());
    }
}

#[test]
fn xhtml_files_with_self_closing_head_elements() {
    let body = common::html_gutenberg();
    let body = &body[body.find("<body>").unwrap()..];
    let xhtml = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<html xmlns=\"http://www.w3.org/1999/xhtml\">\
         <head><title/><script src=\"a.js\"/><style type=\"text/css\"/></head>{body}"
    );
    let metadata = HtmlParser
        .parse_metadata(xhtml.as_bytes())
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(metadata.title, "");
    assert!(
        section_titles(xhtml.as_bytes())
            .iter()
            .any(|t| t.contains("CHAPTER II"))
    );
    // The XML declaration names the encoding when there is no `<meta charset>`.
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"windows-1251\"?><html><head><title>Иван Петров. Маяк</title></head>{body}"
    );
    let (bytes, _, _) = encoding_rs::WINDOWS_1251.encode(&xml);
    assert_eq!(HtmlParser.parse_metadata(&bytes).unwrap().title, "Маяк");
}
