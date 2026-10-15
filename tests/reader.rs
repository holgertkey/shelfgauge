//! Reading-mode tests: parts, deep links, images, the content cache and error pages,
//! on a scanned temporary library.

mod common;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use shelfgauge::config::{Config, ReaderConfig, SplitMode};
use shelfgauge::container::ReadLimits;
use shelfgauge::format::fb2::Fb2Parser;
use shelfgauge::format::{BookParser, Registry, run_with_parser_stack};
use shelfgauge::library::Library;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::{all_books, book_by_id};
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

const FULL: &[u8] = include_bytes!("fixtures/fb2/full.fb2");

/// A 1×1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn fb2(title: &str, body: &str, binaries: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
<author><last-name>Тестов</last-name></author><book-title>{title}</book-title><lang>ru</lang>
</title-info></description><body>{body}</body>{binaries}</FictionBook>"#
    )
    .into_bytes()
}

/// Three chapters of a few paragraphs each.
fn chapters() -> Vec<u8> {
    let body: String = (1..=3)
        .map(|n| {
            format!(
                "<section><title><p>Chapter {n}</p></title>{}</section>",
                (1..=4)
                    .map(|i| format!("<p>Chapter {n}, paragraph {i}: {}</p>", "text ".repeat(20)))
                    .collect::<String>()
            )
        })
        .collect();
    fb2("Chapters", &body, "")
}

struct TestLibrary {
    dir: tempfile::TempDir,
    library: Library,
}

impl TestLibrary {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let lib = Self { dir, library };
        lib.write("full.fb2", FULL);
        lib.write("chapters.fb2", &chapters());
        let mut zip =
            zip::ZipWriter::new(std::fs::File::create(lib.dir.path().join("archive.zip")).unwrap());
        zip.start_file("inner/full.fb2", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(FULL).unwrap();
        zip.finish().unwrap();
        lib.write(
            "images.fb2",
            &fb2(
                "Images",
                r##"<section><p>Pictures</p>
                <image l:href="#a/b #1?.png" alt="odd id"/>
                <image l:href="#evil.svg"/>
                <p><a l:href="javascript:alert(1)">bad link</a> &lt;script&gt;alert(2)&lt;/script&gt;</p>
                </section>"##,
                &format!(
                    r#"<binary id="a/b #1?.png" content-type="image/png">{PNG}</binary>
                    <binary id="evil.svg" content-type="image/svg+xml">{}</binary>"#,
                    "PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciPjxzY3JpcHQ+YWxlcnQoMSk8L3NjcmlwdD48L3N2Zz4="
                ),
            ),
        );
        // Nesting close to the parser's limit (256 XML levels).
        let deep = format!(
            "{}<p>Deep text</p>{}",
            "<section>".repeat(240),
            "</section>".repeat(240)
        );
        lib.write("deep.fb2", &fb2("Deep", &deep, ""));
        lib.write(
            "broken.fb2",
            b"<FictionBook><body a=1></body></FictionBook>",
        );
        lib.write("gone.fb2", &chapters());
        lib.scan();
        std::fs::remove_file(lib.dir.path().join("gone.fb2")).unwrap();
        lib.scan();
        lib
    }

    fn write(&self, path: &str, data: &[u8]) {
        std::fs::write(self.dir.path().join(path), data).unwrap();
    }

    fn scan(&self) {
        let mut conn = open_writer(&self.library.db_path()).unwrap();
        let registry = Registry::with_builtin_formats();
        Scanner {
            library: &self.library,
            exclude: &[],
            registry: &registry,
            limits: ReadLimits::default(),
            threads: 2,
            progress: &ScanProgress::default(),
            cancel: &AtomicBool::new(false),
        }
        .run(&mut conn)
        .unwrap();
    }

    /// The id of the live book whose file (or archive) is `path`.
    fn id_of(&self, path: &str) -> i64 {
        let conn = open_writer(&self.library.db_path()).unwrap();
        all_books(&conn)
            .unwrap()
            .into_iter()
            .map(|b| book_by_id(&conn, b.id).unwrap().unwrap())
            .filter(|b| b.container_path == path)
            .map(|b| b.id)
            .next_back()
            .unwrap_or_else(|| panic!("no book at {path}"))
    }

    fn server(&self, base_path: &str, reader: ReaderConfig) -> Server {
        let mut config = Config::default();
        config.server.base_path = base_path.to_owned();
        config.reader = reader;
        config.validate().unwrap();
        let pool = open_read_pool(&self.library.db_path(), 2).unwrap();
        let scan = Arc::new(BackgroundScan::new(self.library.clone(), Vec::new()));
        let state = AppState::new(self.library.clone(), &config, pool, Arc::clone(&scan));
        Server {
            router: web::router(state),
            scan,
        }
    }
}

struct Server {
    router: Router,
    scan: Arc<BackgroundScan>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.scan.shutdown();
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
    bytes: Vec<u8>,
}

impl Reply {
    fn header(&self, name: header::HeaderName) -> &str {
        self.headers
            .get(&name)
            .unwrap_or_else(|| panic!("no {name} header"))
            .to_str()
            .unwrap()
    }

    /// The rendered text of the part.
    fn text(&self) -> &str {
        let start = self
            .body
            .find("<div class=\"text\">")
            .expect("a reading page");
        let end = self.body[start..].find("</article>").unwrap() + start;
        &self.body[start..end]
    }

    /// The block ordinals rendered in the part.
    fn block_ids(&self) -> Vec<u32> {
        self.text()
            .split(" id=\"b")
            .skip(1)
            .map(|rest| rest.split('"').next().unwrap().parse().unwrap())
            .collect()
    }
}

impl Server {
    async fn get(&self, uri: &str) -> Reply {
        let request = Request::get(uri)
            .header(header::HOST, "127.0.0.1:8742")
            .body(Body::empty())
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
            bytes,
        }
    }

    async fn get_owned(&self, uri: String) -> Reply {
        self.get(&uri).await
    }

    async fn ok(&self, uri: &str) -> Reply {
        let reply = self.get(uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
        reply
    }

    /// Every part of a book, following the "next" links from part 1.
    async fn parts(&self, read_url: &str) -> Vec<Reply> {
        let mut parts = vec![self.ok(read_url).await];
        while let Some(next) = next_href(&parts.last().unwrap().body) {
            assert!(parts.len() < 100, "runaway next links");
            parts.push(self.ok(&next).await);
        }
        parts
    }
}

fn next_href(body: &str) -> Option<String> {
    let at = body.find("rel=\"next\"")?;
    let start = body[..at].rfind("href=\"")? + 6;
    Some(body[start..].split('"').next()?.replace("&amp;", "&"))
}

/// The whole book (main body) on one page.
fn whole() -> ReaderConfig {
    ReaderConfig {
        split: SplitMode::None,
        ..ReaderConfig::default()
    }
}

fn modes() -> Vec<ReaderConfig> {
    vec![
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
            section_depth: 3,
            ..ReaderConfig::default()
        },
        ReaderConfig {
            split: SplitMode::Size,
            part_size: 60,
            ..ReaderConfig::default()
        },
    ]
}

fn full_block_count() -> u32 {
    run_with_parser_stack(|| Fb2Parser.parse_content(FULL).unwrap().block_count() as u32)
}

#[tokio::test]
async fn every_block_is_rendered_once_in_every_mode() {
    let lib = TestLibrary::new();
    let id = lib.id_of("full.fb2");
    let all: Vec<u32> = (1..=full_block_count()).collect();
    for mode in modes() {
        let server = lib.server("/", mode.clone());
        let parts = server.parts(&format!("/book/{id}/read")).await;
        let rendered: Vec<u32> = parts.iter().flat_map(Reply::block_ids).collect();
        assert_eq!(rendered, all, "{mode:?}");
        if mode.split == SplitMode::None {
            assert_eq!(parts.len(), 2, "main body and notes");
        }
    }
}

#[tokio::test]
async fn deep_links_resolve_to_the_right_part_in_every_mode() {
    let lib = TestLibrary::new();
    let id = lib.id_of("full.fb2");
    for mode in modes() {
        let server = lib.server("/", mode.clone());
        for n in 1..=full_block_count() {
            let reply = server.get(&format!("/book/{id}/read?at=b{n}")).await;
            assert_eq!(reply.status, StatusCode::SEE_OTHER, "{mode:?} b{n}");
            let location = reply.header(header::LOCATION).to_owned();
            let (url, fragment) = location.split_once('#').unwrap();
            assert_eq!(fragment, format!("b{n}"));
            assert!(url.starts_with(&format!("/book/{id}/read")), "{location}");
            let part = server.ok(url).await;
            assert!(part.block_ids().contains(&n), "{mode:?}: b{n} not in {url}");
        }
    }
}

#[tokio::test]
async fn part_numbers_and_block_ids_are_checked() {
    let lib = TestLibrary::new();
    let id = lib.id_of("chapters.fb2");
    let server = lib.server(
        "/",
        ReaderConfig {
            split: SplitMode::Section,
            ..ReaderConfig::default()
        },
    );
    let first = server.ok(&format!("/book/{id}/read")).await;
    assert!(first.body.contains("Part 1 of 3"));
    assert!(first.text().contains("Chapter 1, paragraph 1"));
    assert!(!first.body.contains("rel=\"prev\""));
    assert!(
        first
            .body
            .contains(&format!("href=\"/book/{id}/read?part=2\" rel=\"next\""))
    );
    let last = server.ok(&format!("/book/{id}/read?part=3")).await;
    assert!(last.text().contains("Chapter 3"));
    assert!(!last.body.contains("rel=\"next\""));
    for (part, expected) in [
        ("0", "Part 1 of 3"),
        ("x", "Part 1 of 3"),
        ("99", "Part 3 of 3"),
        ("18446744073709551615", "Part 3 of 3"),
        ("99999999999999999999999", "Part 1 of 3"),
    ] {
        let reply = server.ok(&format!("/book/{id}/read?part={part}")).await;
        assert!(reply.body.contains(expected), "{part}");
    }
    // `at` wins over `part`.
    let reply = server.get(&format!("/book/{id}/read?part=1&at=b11")).await;
    assert_eq!(
        reply.header(header::LOCATION),
        format!("/book/{id}/read?part=3#b11")
    );
    for at in ["b0", "b999", "x", "B1", "b01", ""] {
        let reply = server.get(&format!("/book/{id}/read?at={at}")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{at:?}");
    }
    for uri in ["/book/999/read", "/book/x/read", "/book/0/read"] {
        assert_eq!(server.get(uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn reading_page_has_toc_navigation_and_semantic_html() {
    let lib = TestLibrary::new();
    let id = lib.id_of("full.fb2");
    let server = lib.server(
        "/",
        ReaderConfig {
            split: SplitMode::Section,
            ..ReaderConfig::default()
        },
    );
    let parts = server.parts(&format!("/book/{id}/read")).await;
    let html: String = parts.iter().map(|p| p.text()).collect();
    for part in [
        "<blockquote id=\"b2\" class=\"epigraph\">",
        "class=\"poem\">",
        "class=\"verse\"",
        "<table id=",
        "<th class=\"align-right\">Ships</th>",
        "<strong>винтовой</strong>",
        "H<sub>2</sub>O",
        "rel=\"noopener noreferrer nofollow\"",
        "class=\"note-ref\"",
        "<div class=\"body notes\">",
    ] {
        assert!(html.contains(part), "{part} missing in {html}");
    }
    // The note link leads to the notes part.
    let note_href = html
        .split("class=\"note-ref\"")
        .next()
        .and_then(|before| before.rsplit("href=\"").next())
        .and_then(|s| s.split('"').next())
        .unwrap();
    assert!(
        note_href.starts_with(&format!("/book/{id}/read?part=")),
        "{note_href}"
    );
    // Source anchors never become HTML ids.
    for anchor in ["ch1", "n1", "quote1"] {
        assert!(!html.contains(&format!("id=\"{anchor}\"")), "{anchor}");
    }

    let first = &parts[0];
    assert!(
        first.body.contains("<nav class=\"toc\""),
        "TOC in the sidebar"
    );
    assert!(first.body.contains("Глава 1 Смотритель"));
    assert!(first.body.contains("Примечания"));
    assert!(
        !first.body.contains("class=\"modes\""),
        "no library sidebar"
    );
    assert!(
        first.body.contains(&format!("href=\"/book/{id}\"")),
        "link to the card"
    );
}

#[tokio::test]
async fn images_are_served_from_the_content() {
    let lib = TestLibrary::new();
    let id = lib.id_of("images.fb2");
    let server = lib.server("/", ReaderConfig::default());
    let page = server.ok(&format!("/book/{id}/read")).await;
    let src = page
        .text()
        .split("<img src=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap()
        .replace("&amp;", "&")
        .to_owned();
    assert!(
        src.starts_with(&format!("/book/{id}/image/a%2Fb%20%231%3F.png?v=")),
        "{src}"
    );
    let image = server.ok(&src).await;
    assert_eq!(image.header(header::CONTENT_TYPE), "image/png");
    assert_eq!(
        image.header(header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );
    assert!(image.bytes.starts_with(b"\x89PNG"));

    for uri in [
        format!("/book/{id}/image/evil.svg"),
        format!("/book/{id}/image/missing.png"),
        "/book/999/image/x.png".to_owned(),
    ] {
        let reply = server.get(&uri).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{uri}");
    }

    let text = page.text();
    assert!(!text.contains("javascript:"), "{text}");
    assert!(!text.contains("<script>"), "{text}");
    assert!(text.contains("&lt;script&gt;alert(2)&lt;/script&gt;"));
    assert!(
        text.contains("bad link"),
        "the text of a dropped link stays"
    );
}

#[tokio::test]
async fn books_in_archives_and_deep_books_are_read() {
    let lib = TestLibrary::new();
    let server = lib.server("/", whole());
    let zipped = lib.id_of("archive.zip");
    let reply = server.ok(&format!("/book/{zipped}/read")).await;
    assert!(reply.text().contains("Каждый вечер смотритель"));
    let deep = lib.id_of("deep.fb2");
    let reply = server.ok(&format!("/book/{deep}/read")).await;
    assert!(reply.text().contains("Deep text"));
}

#[tokio::test]
async fn content_is_cached_and_the_cache_survives_a_missing_file() {
    let lib = TestLibrary::new();
    let id = lib.id_of("chapters.fb2");
    let cache = lib.library.content_dir().join(format!("{id}.bin"));
    assert!(!cache.exists());
    lib.server("/", whole())
        .ok(&format!("/book/{id}/read"))
        .await;
    assert!(cache.is_file(), "the parsed content is cached");

    // A new server (no in-memory copy) reads the cache even though the file is gone.
    std::fs::remove_file(lib.dir.path().join("chapters.fb2")).unwrap();
    let reply = lib
        .server("/", whole())
        .ok(&format!("/book/{id}/read"))
        .await;
    assert!(reply.text().contains("Chapter 2, paragraph 3"));
}

#[tokio::test]
async fn changed_or_missing_files_ask_for_a_rescan() {
    let lib = TestLibrary::new();
    let server = lib.server("/", ReaderConfig::default());

    let changed = lib.id_of("chapters.fb2");
    lib.write(
        "chapters.fb2",
        &fb2("Other", "<section><p>New text</p></section>", ""),
    );
    let reply = server.get(&format!("/book/{changed}/read")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.body.contains("File not found"), "{}", reply.body);
    assert!(reply.body.contains("Rescan the library</button>"));
    assert!(
        !lib.library
            .content_dir()
            .join(format!("{changed}.bin"))
            .exists(),
        "nothing is cached for a changed file"
    );

    let missing = lib.id_of("full.fb2");
    std::fs::remove_file(lib.dir.path().join("full.fb2")).unwrap();
    let reply = server.get(&format!("/book/{missing}/read")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.body.contains("File not found"));

    let gone = lib.id_of("gone.fb2");
    let reply = server.get(&format!("/book/{gone}/read")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.body.contains("File not found"));
}

#[tokio::test]
async fn broken_books_show_why_they_cannot_be_read() {
    let lib = TestLibrary::new();
    let server = lib.server("/", ReaderConfig::default());
    let id = lib.id_of("broken.fb2");
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(
        !card.body.contains(">Read</a>"),
        "no Read button for an error book"
    );
    let reply = server.get(&format!("/book/{id}/read")).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(
        reply.body.contains("Cannot read the book"),
        "{}",
        reply.body
    );
    assert!(reply.body.contains("invalid XML"), "the reason is shown");
}

#[tokio::test]
async fn base_path_applies_to_reader_links() {
    let lib = TestLibrary::new();
    let id = lib.id_of("full.fb2");
    let server = lib.server(
        "/books",
        ReaderConfig {
            split: SplitMode::Section,
            ..ReaderConfig::default()
        },
    );
    let reply = server.ok(&format!("/books/book/{id}/read")).await;
    // The book title and epigraph are merged into chapter 1, which has the map.
    assert!(
        reply
            .text()
            .contains(&format!("src=\"/books/book/{id}/image/map.png?v="))
    );
    assert!(reply.body.contains(&format!(
        "href=\"/books/book/{id}/read?part=2\" rel=\"next\""
    )));
    let redirect = server.get(&format!("/books/book/{id}/read?at=b3")).await;
    assert!(
        redirect
            .header(header::LOCATION)
            .starts_with(&format!("/books/book/{id}/read"))
    );
    let card = server.ok(&format!("/books/book/{id}")).await;
    assert!(
        card.body
            .contains(&format!("href=\"/books/book/{id}/read\">Read</a>"))
    );
}

/// A book without chapters is cut by `reader.max_part_size` instead of becoming one huge
/// page; deep links still find every block.
#[tokio::test]
async fn long_parts_are_limited() {
    let lib = TestLibrary::new();
    let paragraphs: String = (1..=30)
        .map(|i| format!("<p>Paragraph {i}: {}</p>", "слово ".repeat(330)))
        .collect();
    lib.write("flat.fb2", &fb2("Flat", &paragraphs, ""));
    lib.scan();
    let id = lib.id_of("flat.fb2");
    let server = lib.server(
        "/",
        ReaderConfig {
            max_part_size: 20_000,
            ..ReaderConfig::default()
        },
    );
    let parts = server.parts(&format!("/book/{id}/read")).await;
    assert_eq!(
        parts.len(),
        3,
        "about 59 900 characters, at most 20 000 per part: three pieces, no short fourth"
    );
    assert!(parts[0].body.contains("Part 1 of 3"));
    assert!(
        parts[1].body.contains("Part 1 (2/3)"),
        "pieces are numbered"
    );
    let rendered: Vec<u32> = parts.iter().flat_map(Reply::block_ids).collect();
    assert_eq!(rendered, (1..=rendered.len() as u32).collect::<Vec<_>>());
    let last = *rendered.last().unwrap();
    let reply = server.get(&format!("/book/{id}/read?at=b{last}")).await;
    assert_eq!(
        reply.header(header::LOCATION),
        format!("/book/{id}/read?part=3#b{last}")
    );

    let unlimited = lib.server(
        "/",
        ReaderConfig {
            max_part_size: 0,
            ..ReaderConfig::default()
        },
    );
    assert_eq!(unlimited.parts(&format!("/book/{id}/read")).await.len(), 1);
}

/// The reader settings: name, allowed values (default first) and the `data-` attribute
/// that theme.js sets on `<html>`.
const READER_SETTINGS: [(&str, &[&str], &str); 4] = [
    ("font", &["sans", "serif"], "rfont"),
    ("leading", &["normal", "tight", "loose"], "rleading"),
    ("width", &["medium", "narrow", "wide", "full"], "rwidth"),
    ("align", &["left", "justify"], "ralign"),
];

const THEMES: [&str; 6] = ["", "light", "sepia", "dark", "navy", "black"];

#[tokio::test]
async fn reader_settings_panel_is_on_read_pages_only() {
    let lib = TestLibrary::new();
    let id = lib.id_of("full.fb2");
    let server = lib.server("/", whole());
    let read = server.ok(&format!("/book/{id}/read")).await;
    for part in [
        "class=\"icon-button focus-button\"",
        "aria-pressed=\"false\" hidden>",
        "popovertarget=\"reader-settings\"",
        "<div id=\"reader-settings\" class=\"reader-settings\" popover",
        "name=\"focus\"",
        "data-step=\"-1\"",
        "data-step=\"1\"",
        "class=\"settings-reset\"",
    ] {
        assert!(read.body.contains(part), "{part} missing");
    }
    for (name, values, _) in READER_SETTINGS {
        for value in values {
            let radio = format!("name=\"{name}\" value=\"{value}\"");
            assert!(read.body.contains(&radio), "{radio} missing");
        }
    }
    assert!(read.body.contains("name=\"status\" value=\"on\""));
    for theme in THEMES {
        let radio = format!("name=\"theme\" value=\"{theme}\"");
        assert!(read.body.contains(&radio), "{radio} missing");
    }
    // Neither the panel nor the reader assets on library pages.
    for uri in ["/".to_owned(), format!("/book/{id}")] {
        let page = server.ok(&uri).await;
        for part in ["reader-settings", "focus-button", "reader.css", "reader.js"] {
            assert!(!page.body.contains(part), "{part} on {uri}");
        }
        // The CSP stays the same: no inline styles or scripts.
        assert_eq!(
            page.header(header::CONTENT_SECURITY_POLICY),
            read.header(header::CONTENT_SECURITY_POLICY)
        );
    }
    let csp = read.header(header::CONTENT_SECURITY_POLICY);
    assert!(csp.starts_with("default-src 'self';"), "{csp}");
    assert!(!csp.contains("unsafe-inline"), "{csp}");
}

#[tokio::test]
async fn reader_settings_assets_agree_on_the_values() {
    let lib = TestLibrary::new();
    let server = lib.server("/", whole());
    let theme_js = server.ok("/assets/theme.js").await.body;
    let reader_css = server.ok("/assets/reader.css").await.body;
    // Windows checkouts may embed CRLF line endings.
    let style_css = server
        .ok("/assets/style.css")
        .await
        .body
        .replace("\r\n", "\n");
    let app_js = server.ok("/assets/app.js").await.body;

    // theme.js whitelists exactly the panel's values, the default first; reader.css has
    // a preset for every value but the default (which is the stylesheet's own value).
    for (name, values, attr) in READER_SETTINGS {
        let list = values
            .iter()
            .map(|v| format!("\"{v}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let whitelist = format!("{name}: [{list}],");
        assert!(theme_js.contains(&whitelist), "{whitelist} missing");
        assert!(theme_js.contains(&format!("{name}: \"{attr}\",")), "{attr}");
        assert!(
            !reader_css.contains(&format!("[data-{attr}=\"{}\"]", values[0])),
            "no preset for the default {name}"
        );
        for value in &values[1..] {
            let preset = format!(":root[data-{attr}=\"{value}\"] {{");
            assert!(reader_css.contains(&preset), "{preset} missing");
        }
    }
    // The running head and status line: on by default.
    assert!(theme_js.contains("status: [\"on\", \"off\"],"));
    assert!(theme_js.contains("status: \"rstatus\","));
    assert!(reader_css.contains(":root[data-rstatus=\"off\"] {"));
    // Sizes: 8 steps, 3 (18px, the old size) is the default.
    assert!(theme_js.contains("size: [\"3\", \"0\", \"1\", \"2\", \"4\", \"5\", \"6\", \"7\"],"));
    for step in ["0", "1", "2", "4", "5", "6", "7"] {
        assert!(reader_css.contains(&format!(":root[data-rsize=\"{step}\"] {{")));
    }
    // The base rules consume the properties, with the old values as defaults.
    for rule in [
        "max-width: var(--reader-width, 40rem);",
        "font-family: var(--reader-font, inherit);",
        "font-size: var(--reader-size, 1.125rem);",
        "line-height: var(--reader-leading, 1.65);",
        "text-align: var(--reader-align, start);",
    ] {
        assert!(style_css.contains(rule), "{rule} missing");
    }
    // Every theme of the panel exists (light is the plain `:root` palette).
    for theme in &THEMES[1..] {
        assert!(
            app_js.contains(&format!("\"{theme}\"")),
            "{theme} in THEMES"
        );
        if *theme != "light" {
            assert!(style_css.contains(&format!(":root[data-theme=\"{theme}\"] {{")));
        }
    }
    // Buttons that need JavaScript stay hidden without it.
    assert!(style_css.contains(".icon-button[hidden] {\n  display: none;"));
    // Focus mode is styled in reader.css only, so it never touches library pages.
    assert!(reader_css.contains(":root.reading-focus .sidebar"));
    assert!(!style_css.contains("reading-focus"));
}

/// The value of attribute `name` in the first tag that has it.
fn attribute(body: &str, name: &str) -> Option<u16> {
    let start = body.find(&format!(" {name}=\""))? + name.len() + 3;
    body[start..].split('"').next()?.parse().ok()
}

#[tokio::test]
async fn read_pages_carry_the_running_head_and_progress() {
    let lib = TestLibrary::new();
    let id = lib.id_of("full.fb2");
    let server = lib.server(
        "/",
        ReaderConfig {
            split: SplitMode::Section,
            ..ReaderConfig::default()
        },
    );
    let parts = server.parts(&format!("/book/{id}/read")).await;
    assert!(parts.len() > 2, "several parts");
    for part in &parts {
        assert!(
            part.body
                .contains("<div class=\"running-head\" aria-hidden=\"true\">")
        );
        assert!(part.body.contains("<span class=\"status-chapter\">"));
        assert!(
            part.body.contains("The Lighthouse Keeper · "),
            "authors · title in the running head"
        );
    }
    // Progress per part, in ten-thousandths: from 0 to 10 000 without gaps; the notes
    // part has none.
    let (main, notes): (Vec<_>, Vec<_>) = parts
        .iter()
        .partition(|p| attribute(&p.body, "data-progress-start").is_some());
    assert_eq!(notes.len(), 1, "only the notes part has no progress");
    assert!(
        notes[0]
            .body
            .contains("<span class=\"status-progress\"></span>")
    );
    let ranges: Vec<(u16, u16)> = main
        .iter()
        .map(|p| {
            (
                attribute(&p.body, "data-progress-start").unwrap(),
                attribute(&p.body, "data-progress-end").unwrap(),
            )
        })
        .collect();
    assert_eq!(ranges.first().unwrap().0, 0, "{ranges:?}");
    assert_eq!(ranges.last().unwrap().1, 10_000, "{ranges:?}");
    for pair in ranges.windows(2) {
        assert!(pair[0].0 <= pair[0].1, "{ranges:?}");
        assert_eq!(pair[0].1, pair[1].0, "contiguous: {ranges:?}");
    }
    // The server-rendered label is the progress at the start of the part.
    let second = main[1];
    let start = attribute(&second.body, "data-progress-start").unwrap();
    assert!(second.body.contains(&format!(
        "<span class=\"status-progress\">{} %</span>",
        start / 100
    )));
}

/// All values of attribute `name`, in document order.
fn attributes(body: &str, name: &str) -> Vec<String> {
    body.split(&format!(" {name}=\""))
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn the_track_has_chapter_ticks_and_progress_links_open_the_place() {
    let lib = TestLibrary::new();
    let id = lib.id_of("chapters.fb2");
    let server = lib.server("/", whole());
    let read = server.ok(&format!("/book/{id}/read")).await;
    assert!(read.body.contains(&format!(
        "<div class=\"track\" data-href=\"/book/{id}/read\" hidden>"
    )));
    // Three chapters of equal length: ticks at their starts, in order.
    let titles = attributes(&read.body, "data-title");
    assert_eq!(titles, ["Chapter 1", "Chapter 2", "Chapter 3"]);
    let at: Vec<u64> = attributes(&read.body, "data-at")
        .iter()
        .map(|a| a.parse().unwrap())
        .collect();
    assert_eq!(at.len(), 3);
    assert_eq!(at[0], 0);
    assert!(
        at[1] > 3000 && at[1] < 3400 && at[2] > 6500 && at[2] < 6800,
        "{at:?}"
    );

    // `?progress=` opens the main-text block at that share: a chapter's start opens its
    // title, the end the last paragraph. `from=track` tells the reader not to highlight
    // the block (the place is only rough).
    let location = |reply: Reply| {
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        reply.header(header::LOCATION).to_owned()
    };
    let open = |share: u64| server.get_owned(format!("/book/{id}/read?progress={share}"));
    assert_eq!(
        location(open(0).await),
        format!("/book/{id}/read?from=track#b1")
    );
    let chapter_two = location(open(at[1]).await);
    let block = chapter_two.rsplit('#').next().unwrap().to_owned();
    assert!(
        read.body
            .contains(&format!("<p id=\"{block}\" class=\"title-line\">Chapter 2")),
        "{block}"
    );
    let last = *read.block_ids().last().unwrap();
    assert_eq!(
        location(open(10_000).await),
        format!("/book/{id}/read?from=track#b{last}")
    );
    // Split into parts: the redirect names the part.
    let parts = lib.server(
        "/",
        ReaderConfig {
            split: SplitMode::Section,
            ..ReaderConfig::default()
        },
    );
    let reply = parts
        .get(&format!("/book/{id}/read?progress={}", at[2]))
        .await;
    let target = location(reply);
    assert!(
        target.starts_with(&format!("/book/{id}/read?part=3&from=track#b")),
        "{target}"
    );
    for bad in ["10001", "-1", "abc", "", "1.5"] {
        let reply = server.get(&format!("/book/{id}/read?progress={bad}")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{bad}");
    }
}

#[tokio::test]
async fn progress_links_never_open_the_notes() {
    let lib = TestLibrary::new();
    let id = lib.id_of("full.fb2");
    let server = lib.server(
        "/",
        ReaderConfig {
            split: SplitMode::Section,
            ..ReaderConfig::default()
        },
    );
    let parts = server.parts(&format!("/book/{id}/read")).await;
    let notes = parts
        .iter()
        .position(|p| attribute(&p.body, "data-progress-start").is_none())
        .unwrap();
    let reply = server.get(&format!("/book/{id}/read?progress=10000")).await;
    let location = reply.header(header::LOCATION);
    assert!(
        !location.contains(&format!("part={}&", notes + 1)),
        "the end of the main text, not the notes: {location}"
    );
}

#[tokio::test]
async fn read_pages_name_the_book_for_reader_views() {
    let lib = TestLibrary::new();
    // No author and a language that is not a tag.
    lib.write(
        "anonymous.fb2",
        br#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook><description><title-info><book-title>Anonymous</book-title><lang>Russian, English</lang>
</title-info></description><body><section><p>Text</p></section></body></FictionBook>"#,
    );
    lib.scan();
    let server = lib.server("/", whole());

    let page = server
        .ok(&format!("/book/{}/read", lib.id_of("full.fb2")))
        .await
        .body;
    // The book's language for the text, English for the UI around it.
    assert!(page.contains("<html lang=\"ru\""), "{page}");
    assert!(page.contains("<header class=\"topbar\" lang=\"en\">"));
    assert!(page.contains("id=\"reader-settings\""));
    assert!(page.contains("popover aria-label=\"Reading settings\" lang=\"en\">"));
    // Title and authors as metadata, so reader views do not guess them from the page.
    assert!(page.contains("<meta property=\"og:title\" content=\"Старый маяк\">"));
    assert!(page.contains(
        "<meta name=\"author\" content=\"Сидоров Иван Петрович, The Lighthouse Keeper\">"
    ));
    // An epigraph's author is no byline: no class in the text mentions `author`.
    let text = page.split("<div class=\"text\">").nth(1).unwrap();
    assert!(
        text.contains("class=\"attribution\">Unknown sailor</p>"),
        "{text}"
    );
    assert!(!text.contains("author"), "{text}");

    let anonymous = server
        .ok(&format!("/book/{}/read", lib.id_of("anonymous.fb2")))
        .await
        .body;
    assert!(
        anonymous.contains("<html lang=\"en\""),
        "invalid language dropped"
    );
    assert!(anonymous.contains("<meta property=\"og:title\" content=\"Anonymous\">"));
    assert!(
        !anonymous.contains("<meta name=\"author\""),
        "no empty author"
    );

    // Library pages stay English.
    let card = server
        .ok(&format!("/book/{}", lib.id_of("full.fb2")))
        .await
        .body;
    assert!(card.contains("<html lang=\"en\""));
    assert!(!card.contains("og:title"));
}

#[tokio::test]
async fn epub_books_are_read_and_protected_ones_say_why_not() {
    let lib = TestLibrary::new();
    lib.write("war.epub", &common::epub3());
    let mut locked = common::epub3_entries();
    locked.push(("META-INF/license.lcpl", b"{}".to_vec()));
    lib.write("locked.epub", &common::zip(&locked));
    lib.scan();
    let server = lib.server("/", whole());

    let id = lib.id_of("war.epub");
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(card.body.contains(">Read</a>"), "{}", card.body);
    assert!(card.body.contains("<dd>EPUB</dd>"), "the format is shown");
    let page = server.ok(&format!("/book/{id}/read")).await;
    let text = page.text();
    assert!(text.contains("Text with<br>a break"), "{text}");
    assert!(text.contains("<ul id=\"b6\"><li id=\"b7\">"), "{text}");
    assert!(!text.contains("<script"), "{text}");
    let src = text
        .split("<img src=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap()
        .replace("&amp;", "&");
    assert!(
        src.starts_with(&format!("/book/{id}/image/OEBPS%2Fimg%2Fpic.png?v=")),
        "{src}"
    );
    let image = server.ok(&src).await;
    assert_eq!(image.header(header::CONTENT_TYPE), "image/png");
    let parts = server.parts(&format!("/book/{id}/read")).await;
    assert!(parts.last().unwrap().text().contains("Endnote one."));

    let locked = lib.id_of("locked.epub");
    let card = server.ok(&format!("/book/{locked}")).await;
    assert!(card.body.contains("protected by DRM"), "{}", card.body);
    assert!(card.body.contains("Readium LCP"), "{}", card.body);
    assert!(!card.body.contains(">Read</a>"), "no Read button");
    let reply = server.get(&format!("/book/{locked}/read")).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.body.contains("Protected by DRM"), "{}", reply.body);
    assert!(reply.body.contains("Readium LCP"), "{}", reply.body);
    let reply = server
        .get(&format!("/book/{locked}/image/OEBPS%2Fimg%2Fpic.png"))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}
