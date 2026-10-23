//! EPUB parsing: synthetic books built in the tests, and a report over real books.

mod common;

use std::collections::BTreeMap;

use shelfgauge::config::{ReaderConfig, SplitMode};
use shelfgauge::format::epub::EpubParser;
use shelfgauge::format::model::BlockId;
use shelfgauge::format::model::{BlockKind, BodyKind, BookContent, Inline, LinkTarget, Section};
use shelfgauge::format::{BookParser, run_with_parser_stack};
use shelfgauge::render::html::{ReaderLinks, render_part, render_toc};
use shelfgauge::render::split::split;

/// Prints what the parser makes of every `.epub` in the directory named by
/// `SHELFGAUGE_EPUB_SAMPLES`: `cargo test --test epub -- --ignored --nocapture`.
#[test]
#[ignore = "needs real books in SHELFGAUGE_EPUB_SAMPLES"]
fn report_on_real_books() {
    let Ok(dir) = std::env::var("SHELFGAUGE_EPUB_SAMPLES") else {
        eprintln!("SHELFGAUGE_EPUB_SAMPLES is not set");
        return;
    };
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "epub"))
        .collect();
    paths.sort();
    for path in paths {
        let data = std::fs::read(&path).unwrap();
        println!("\n=== {}", path.display());
        run_with_parser_stack(|| report(&data));
    }
}

fn report(data: &[u8]) {
    let start = std::time::Instant::now();
    match EpubParser.parse_metadata(data) {
        Ok(m) => println!(
            "title {:?}; authors {:?}; series {:?}; genres {}; lang {:?}; annotation {} chars; cover {:?}; drm {:?}",
            m.title,
            m.authors
                .iter()
                .map(shelfgauge::storage::authors::display_name)
                .collect::<Vec<_>>(),
            m.series
                .iter()
                .map(|s| (s.name.clone(), s.index.clone()))
                .collect::<Vec<_>>(),
            m.genres.len(),
            m.language,
            m.annotation.plain_text().chars().count(),
            m.cover.as_ref().map(|c| (c.mime.clone(), c.data.len())),
            m.drm,
        ),
        Err(e) => println!("metadata error: {e}"),
    }
    let content = match EpubParser.parse_content(data) {
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
    println!("images {}", content.images().len());
    for body in content.bodies() {
        let sections = count_sections(&body.section);
        println!(
            "body {:?}: title {:?}, {} sections",
            body.kind,
            body.section.title_text(),
            sections
        );
        if body.kind == BodyKind::Main {
            let mut toc = Vec::new();
            titles(&body.section, 0, &mut toc);
            let shown: Vec<_> = toc.iter().take(12).collect();
            println!("toc ({}): {shown:?}", toc.len());
        }
    }
    let (mut links, mut notes) = (0, 0);
    content.for_each_block(|b| {
        if let BlockKind::Paragraph(inlines) = &b.kind {
            count_links(inlines, &mut links, &mut notes);
        }
    });
    println!("internal links {links}, note links {notes}");
    for mode in [SplitMode::Section, SplitMode::Size, SplitMode::None] {
        let parts = split(
            &content,
            &ReaderConfig {
                split: mode,
                ..ReaderConfig::default()
            },
        );
        let first: Vec<_> = parts.iter().take(6).map(|p| p.title.clone()).collect();
        println!("{mode:?}: {} parts {first:?}", parts.len());
    }
}

fn count_sections(section: &Section) -> usize {
    section.children.iter().map(|c| 1 + count_sections(c)).sum()
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

fn count_links(inlines: &[Inline], links: &mut usize, notes: &mut usize) {
    for inline in inlines {
        match inline {
            Inline::Link {
                target, children, ..
            } => {
                if matches!(target, LinkTarget::Anchor(_)) {
                    *links += 1;
                }
                if matches!(inline, Inline::Link { note: true, .. }) {
                    *notes += 1;
                }
                count_links(children, links, notes);
            }
            Inline::Emphasis(c)
            | Inline::Strong(c)
            | Inline::Strikethrough(c)
            | Inline::Subscript(c)
            | Inline::Superscript(c)
            | Inline::Code(c) => count_links(c, links, notes),
            _ => {}
        }
    }
}

/// One line per block: id, kind, depth in the section tree, anchor, text.
fn dump(content: &BookContent) -> Vec<String> {
    fn section(s: &Section, depth: usize, out: &mut Vec<String>) {
        out.push(format!(
            "{}section title={:?} nav={:?}",
            "  ".repeat(depth),
            s.title_text(),
            s.nav_title
        ));
        for b in s.title.iter().chain(&s.blocks) {
            b.walk(|b| {
                out.push(format!(
                    "{}{} {} {:?}{}",
                    "  ".repeat(depth + 1),
                    b.id,
                    b.kind.name(),
                    b.plain_text(),
                    b.anchor
                        .as_deref()
                        .map(|a| format!(" @{a}"))
                        .unwrap_or_default()
                ))
            });
        }
        for c in &s.children {
            section(c, depth + 1, out);
        }
    }
    let mut out = Vec::new();
    for body in content.bodies() {
        out.push(format!("body {:?}", body.kind));
        section(&body.section, 0, &mut out);
    }
    out
}

fn parse(data: &[u8]) -> BookContent {
    let data = data.to_vec();
    run_with_parser_stack(move || EpubParser.parse_content(&data)).unwrap()
}

/// The dump without anchors: the numbering contract only.
fn outline(content: &BookContent) -> Vec<String> {
    dump(content)
        .into_iter()
        .map(|line| match line.find(" @") {
            Some(i) => line[..i].to_owned(),
            None => line,
        })
        .collect()
}

/// Golden test of the EPUB mapping and numbering. If this fails after a change, the
/// block IDs of existing EPUB books changed: bump `PARSER_VERSION`.
#[test]
fn epub3_numbering_follows_the_documented_rule() {
    let expected = [
        "body Main",
        "section title=None nav=None",
        "  section title=Some(\"Title page\") nav=Some(\"Title page\")",
        "    b1 paragraph \"War\\u{a0}and Peace\"",
        "  section title=Some(\"Part One\") nav=Some(\"Part One\")",
        "    section title=Some(\"Chapter 1 The Beginning\") nav=None",
        "      b2 paragraph \"Chapter 1\"",
        "      b3 paragraph \"The Beginning\"",
        "      b4 image \"\"",
        "      b5 paragraph \"Text with\\na break and a footnote1.\"",
        "      b6 list \"One\\nNested\\nTwo\"",
        "      b7 list_item \"One\\nNested\"",
        "      b8 paragraph \"One\"",
        "      b9 list \"Nested\"",
        "      b10 list_item \"Nested\"",
        "      b11 paragraph \"Nested\"",
        "      b12 list_item \"Two\"",
        "      b13 paragraph \"Two\"",
        "      b14 preformatted \"  code\\n    indented\"",
        "      b15 rule \"\"",
        "      b16 paragraph \"Loose inline text\"",
        "      section title=Some(\"Chapter 1.1\") nav=None",
        "        b17 paragraph \"Chapter 1.1\"",
        "        b18 paragraph \"See chapter 2.\"",
        "    section title=Some(\"Chapter 2\") nav=None",
        "      b19 paragraph \"Chapter 2\"",
        "      b20 paragraph \"Here it is: a site, a lost link, an endnote2.\"",
        "      b21 table \"Head\\nx\\ny\"",
        "      b22 image \"\"",
        "      b23 list \"Third\"",
        "      b24 list_item \"Third\"",
        "      b25 paragraph \"Third\"",
        "body Notes",
        "section title=Some(\"Endnotes\") nav=None",
        "  b26 paragraph \"Endnotes\"",
        "  section title=None nav=None",
        "    b27 paragraph \"Footnote one.\"",
        "  section title=Some(\"1\") nav=None",
        "    b28 paragraph \"1\"",
        "    b29 paragraph \"Endnote one. ↩\"",
        "  section title=Some(\"Appendix\") nav=None",
        "    b30 paragraph \"Appendix\"",
        "    b31 paragraph \"Extra text.\"",
    ];
    assert_eq!(outline(&parse(&common::epub3())), expected);
}

#[test]
fn epub3_metadata() {
    let m = EpubParser.parse_metadata(&common::epub3()).unwrap();
    assert_eq!(m.title, "Война и мир");
    let authors: Vec<String> = m
        .authors
        .iter()
        .map(shelfgauge::storage::authors::display_name)
        .collect();
    assert_eq!(
        authors,
        ["Толстой Лев Николаевич"],
        "illustrators are not authors"
    );
    let series: Vec<(&str, Option<&str>)> = m
        .series
        .iter()
        .map(|s| (s.name.as_str(), s.index.as_deref()))
        .collect();
    assert_eq!(series, [("Эпопея", Some("2"))], "a `set` is not a series");
    assert_eq!(m.genres, ["Historical fiction", "War stories"]);
    assert_eq!(
        m.annotation.plain_text(),
        "An epic novel.\nSecond\u{a0}paragraph."
    );
    assert!(matches!(
        &m.annotation.blocks()[0].kind,
        BlockKind::Paragraph(inlines) if inlines.iter().any(|i| matches!(i, Inline::Emphasis(_)))
    ));
    assert_eq!(m.language.as_deref(), Some("ru"));
    assert_eq!(m.cover.map(|c| c.mime).as_deref(), Some("image/png"));
    assert_eq!(m.drm, None);
}

struct Links;

impl ReaderLinks for Links {
    fn block(&self, part: usize, id: BlockId) -> String {
        format!("?part={}#{id}", part + 1)
    }
    fn image(&self, id: &str) -> String {
        format!("/image/{id}")
    }
}

fn render_whole(content: &BookContent) -> String {
    let parts = split(
        content,
        &ReaderConfig {
            split: SplitMode::None,
            ..ReaderConfig::default()
        },
    );
    (0..parts.len())
        .map(|i| render_part(content, &parts, i, &Links))
        .collect()
}

#[test]
fn epub3_links_notes_and_images_render() {
    let content = parse(&common::epub3());
    let html = render_whole(&content);
    // Footnote and endnote references lead into the notes body.
    assert!(
        html.contains(r##"<a href="?part=2#b27" class="note-ref">1</a>"##),
        "{html}"
    );
    assert!(
        html.contains(r##"an endnote<a href="?part=2#b28" class="note-ref">2</a>."##),
        "{html}"
    );
    // Links between documents, also into the middle of a paragraph and back.
    assert!(html.contains(r##"<a href="#b20">chapter 2</a>"##), "{html}");
    assert!(html.contains(r##"<a href="?part=1#b20">↩</a>"##), "{html}");
    assert!(html.contains(r#"href="https://example.org/" rel="noopener noreferrer nofollow""#));
    assert!(
        html.contains(", a lost link, "),
        "a link that leads nowhere is text"
    );
    assert!(html.contains("Text with<br>a break"), "{html}");
    assert!(
        html.contains("<pre id=\"b14\">\n  code\n    indented</pre>"),
        "{html}"
    );
    assert!(html.contains(r#"<ol id="b23" start="3">"#), "{html}");
    // Raster images only; scripts and SVG never reach the page.
    assert_eq!(
        content.images().keys().collect::<Vec<_>>(),
        ["OEBPS/img/pic.png"]
    );
    assert!(html.contains(r#"<img src="/image/OEBPS/img/pic.png" alt="A picture""#));
    assert!(!html.contains("<script") && !html.contains("alert(1)"));
    assert!(!html.contains("drawing.svg"));
    assert!(
        !html.contains("id=\"OEBPS"),
        "source ids never become HTML ids"
    );
}

#[test]
fn epub3_toc_and_parts() {
    let content = parse(&common::epub3());
    let parts = split(
        &content,
        &ReaderConfig {
            split: SplitMode::Section,
            section_depth: 2,
            ..ReaderConfig::default()
        },
    );
    let titles: Vec<&str> = parts.iter().map(|p| p.title.as_str()).collect();
    assert_eq!(titles, ["Title page", "Part One", "Chapter 2", "Endnotes"]);
    let toc = render_toc(&content, &parts, 0, &Links);
    for title in [
        "Title page",
        "Part One",
        "Chapter 1 The Beginning",
        "Chapter 1.1",
        "Endnotes",
    ] {
        assert!(toc.contains(&format!(">{title}</a>")), "{title}: {toc}");
    }
}

#[test]
fn epub2_with_ncx_and_entities() {
    let data = common::epub2();
    let m = EpubParser.parse_metadata(&data).unwrap();
    assert_eq!(m.title, "Pride and Prejudice");
    let authors: Vec<String> = m
        .authors
        .iter()
        .map(shelfgauge::storage::authors::display_name)
        .collect();
    assert_eq!(authors, ["Austen Jane"], "editors are not authors");
    assert_eq!(m.series[0].name, "Novels");
    assert_eq!(m.series[0].index.as_deref(), Some("2"));
    assert_eq!(m.annotation.plain_text(), "Plain text description.");
    assert_eq!(m.language.as_deref(), Some("en"));
    assert!(m.cover.is_some(), "meta name=cover");
    let expected = [
        "body Main",
        "section title=None nav=None",
        "  section title=Some(\"Chapter I.\") nav=None",
        "    b1 paragraph \"Chapter I.\"",
        "    b2 paragraph \"It is a truth—universally acknowledged… café & more &unknown; Q&A\"",
        "  section title=Some(\"CHAPTER II.\") nav=None",
        "    b3 paragraph \"CHAPTER II.\"",
        "    b4 paragraph \"Mr. Bennet was among the earliest.\"",
    ];
    assert_eq!(outline(&parse(&data)), expected);
}

#[test]
fn same_author_from_epub3_refines_and_epub2_attributes() {
    let key = |data: &[u8]| {
        let m = EpubParser.parse_metadata(data).unwrap();
        shelfgauge::storage::authors::sort_key(&m.authors[0])
    };
    let epub3 = common::zip(&common::with_entry(
        common::epub3_entries(),
        "OEBPS/content.opf",
        Some(
            r##"<package xmlns="http://www.idpf.org/2007/opf" version="3.0"><metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
<dc:title>P</dc:title><dc:creator id="c">Jane Austen</dc:creator>
<meta refines="#c" property="file-as">Austen, Jane</meta></metadata>
<manifest><item id="t" href="text/title.xhtml" media-type="application/xhtml+xml"/></manifest>
<spine><itemref idref="t"/></spine></package>"##
                .as_bytes()
                .to_vec(),
        ),
    ));
    assert_eq!(key(&epub3), key(&common::epub2()));
}

#[test]
fn without_a_table_of_contents_headings_or_documents_make_sections() {
    // No navigation document: the headings.
    let entries = common::with_entry(common::epub3_entries(), "OEBPS/nav.xhtml", None);
    let content = parse(&common::zip(&entries));
    let mut toc = Vec::new();
    titles(&content.bodies()[0].section, 0, &mut toc);
    assert_eq!(
        toc,
        ["Chapter 1 The Beginning", "  Chapter 1.1", "Chapter 2"]
    );
    // Neither a table of contents nor headings: one untitled section per document.
    let opf = r#"<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
<metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>T</dc:title></metadata>
<manifest><item id="a" href="a.xhtml" media-type="application/xhtml+xml"/>
<item id="b" href="b.xhtml" media-type="application/xhtml+xml"/></manifest>
<spine><itemref idref="a"/><itemref idref="b"/></spine></package>"#;
    let data = common::zip(&[
        (
            "META-INF/container.xml",
            common::CONTAINER.as_bytes().to_vec(),
        ),
        ("OEBPS/content.opf", opf.as_bytes().to_vec()),
        ("OEBPS/a.xhtml", common::xhtml("<p>One.</p>")),
        ("OEBPS/b.xhtml", common::xhtml("<p>Two.</p>")),
    ]);
    let content = parse(&data);
    let root = &content.bodies()[0].section;
    assert_eq!(root.children.len(), 2);
    assert!(root.children.iter().all(|c| c.title_text().is_none()));
}

fn encryption(entries: &[(&str, &str)]) -> Vec<u8> {
    let data: String = entries
        .iter()
        .map(|(algorithm, uri)| {
            format!(
                r#"<enc:EncryptedData><enc:EncryptionMethod Algorithm="{algorithm}"/>
<enc:CipherData><enc:CipherReference URI="{uri}"/></enc:CipherData></enc:EncryptedData>"#
            )
        })
        .collect();
    format!(
        r#"<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container" xmlns:enc="http://www.w3.org/2001/04/xmlenc#">{data}</encryption>"#
    )
    .into_bytes()
}

#[test]
fn drm_protected_books_keep_metadata_and_refuse_content() {
    let aes = "http://www.w3.org/2001/04/xmlenc#aes128-cbc";
    let mut entries = common::with_entry(
        common::epub3_entries(),
        "META-INF/encryption.xml",
        Some(encryption(&[
            (aes, "OEBPS/text/ch1.xhtml"),
            (aes, "OEBPS/img/cover.png"),
        ])),
    );
    entries.push(("META-INF/rights.xml", b"<rights/>".to_vec()));
    let data = common::zip(&entries);
    let m = EpubParser.parse_metadata(&data).unwrap();
    assert_eq!(m.title, "Война и мир");
    assert_eq!(m.drm.as_deref(), Some("Adobe ADEPT"));
    assert!(m.cover.is_none(), "an encrypted cover is not used");
    let err = EpubParser.parse_content(&data).unwrap_err();
    assert!(err.message.contains("DRM"), "{err}");

    // Font obfuscation only: an ordinary book.
    let entries = common::with_entry(
        common::epub3_entries(),
        "META-INF/encryption.xml",
        Some(encryption(&[(
            "http://www.idpf.org/2008/embedding",
            "OEBPS/fonts/a.otf",
        )])),
    );
    let data = common::zip(&entries);
    assert_eq!(EpubParser.parse_metadata(&data).unwrap().drm, None);
    assert!(EpubParser.parse_content(&data).is_ok());

    // An LCP license makes even an unreadable package a protected book.
    let data = common::zip(&[
        ("META-INF/license.lcpl", b"{}".to_vec()),
        ("META-INF/container.xml", b"<container/>".to_vec()),
    ]);
    let m = EpubParser.parse_metadata(&data).unwrap();
    assert_eq!(m.drm.as_deref(), Some("Readium LCP"));
}

#[test]
fn broken_books_are_errors_not_panics() {
    let error = |data: &[u8]| {
        let data = data.to_vec();
        run_with_parser_stack(move || {
            let metadata = EpubParser.parse_metadata(&data).err();
            let content = EpubParser.parse_content(&data).err();
            (metadata.map(|e| e.message), content.map(|e| e.message))
        })
    };
    let (m, c) = error(b"<FictionBook/>");
    assert!(m.unwrap().contains("not an EPUB") && c.unwrap().contains("not an EPUB"));

    let entries = common::with_entry(common::epub3_entries(), "META-INF/container.xml", None);
    let (m, _) = error(&common::zip(&entries));
    assert!(m.unwrap().contains("container.xml"));

    let entries = common::with_entry(common::epub3_entries(), "OEBPS/content.opf", None);
    let (m, c) = error(&common::zip(&entries));
    assert!(m.unwrap().contains("OEBPS/content.opf"));
    assert!(c.unwrap().contains("OEBPS/content.opf"));

    let entries = common::with_entry(
        common::epub3_entries(),
        "OEBPS/text/ch1.xhtml",
        Some(b"<html><body><p a=1>broken</p></body></html>".to_vec()),
    );
    let (m, c) = error(&common::zip(&entries));
    assert!(m.is_none(), "metadata does not read the spine");
    let c = c.unwrap();
    assert!(
        c.contains("OEBPS/text/ch1.xhtml") && c.contains("invalid XML"),
        "{c}"
    );

    // A missing spine document is skipped; no readable document at all is an error.
    let entries = common::with_entry(common::epub3_entries(), "OEBPS/text/ch2.xhtml", None);
    assert!(error(&common::zip(&entries)).1.is_none());
    let mut entries = common::epub3_entries();
    entries.retain(|(name, _)| !name.starts_with("OEBPS/text/"));
    let (_, c) = error(&common::zip(&entries));
    assert!(c.unwrap().contains("no readable spine documents"));
}

#[test]
fn registry_knows_epub() {
    let registry = shelfgauge::format::Registry::with_builtin_formats();
    let parser = registry
        .for_path(std::path::Path::new("books/Book.EPUB"))
        .unwrap();
    assert_eq!(parser.format(), "epub");
}

/// The deepest accepted nesting of each kind: name, open, close, repetitions
/// (`<ul><li>` nests two levels per repetition).
const DEEPEST: &[(&str, &str, &str, usize)] = &[
    ("div", "<div>", "</div>", 240),
    ("span", "<span>", "</span>", 240),
    ("blockquote", "<blockquote>", "</blockquote>", 240),
    ("list", "<ul><li>", "</li></ul>", 120),
    ("em", "<em>", "</em>", 240),
];

#[test]
fn deepest_accepted_nesting_fits_the_parser_stack() {
    for &(_, open, close, n) in DEEPEST {
        let body = format!("<p>{}deepest{}</p>", open.repeat(n), close.repeat(n));
        let entries = common::with_entry(
            common::epub3_entries(),
            "OEBPS/text/ch2.xhtml",
            Some(common::xhtml(&body)),
        );
        let data = common::zip(&entries);
        let content = run_with_parser_stack(move || EpubParser.parse_content(&data))
            .unwrap_or_else(|e| panic!("{open}: {e}"));
        let mut found = false;
        content.for_each_block(|b| found |= b.plain_text().contains("deepest"));
        assert!(found, "{open}");
    }
    let body = format!("{}x{}", "<div>".repeat(100_000), "</div>".repeat(100_000));
    let entries = common::with_entry(
        common::epub3_entries(),
        "OEBPS/text/ch2.xhtml",
        Some(common::xhtml(&body)),
    );
    let data = common::zip(&entries);
    let err = run_with_parser_stack(move || EpubParser.parse_content(&data)).unwrap_err();
    assert!(err.message.contains("nested more than"), "{err}");
}

/// Writes the synthetic books to the directory named by `SHELFGAUGE_EPUB_OUT`, for
/// browser checks: `cargo test --test epub write_samples -- --ignored`.
#[test]
#[ignore = "writes files to SHELFGAUGE_EPUB_OUT"]
fn write_samples() {
    let Ok(dir) = std::env::var("SHELFGAUGE_EPUB_OUT") else {
        return;
    };
    let dir = std::path::Path::new(&dir);
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("synthetic-epub3.epub"), common::epub3()).unwrap();
    std::fs::write(dir.join("synthetic-epub2.epub"), common::epub2()).unwrap();
    // The deepest nesting the parser accepts (the hub reader renders them in WASM,
    // on the browser's stack).
    for (name, open, close, n) in DEEPEST {
        let body = format!("<p>{}deepest{}</p>", open.repeat(*n), close.repeat(*n));
        let entries = common::with_entry(
            common::epub3_entries(),
            "OEBPS/text/ch2.xhtml",
            Some(common::xhtml(&body)),
        );
        std::fs::write(dir.join(format!("deep-{name}.epub")), common::zip(&entries)).unwrap();
    }
}
