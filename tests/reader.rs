//! Reading-mode tests: parts, deep links, images, the content cache and error pages,
//! on a scanned temporary library.

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
