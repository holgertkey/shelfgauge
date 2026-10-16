//! Search within the book: the fragment route, the read page's `find` parameter and
//! the Search tab, on a scanned temporary library.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use shelfgauge::annotations::find::{MAX_HITS, Query, find};
use shelfgauge::annotations::text::BookText;
use shelfgauge::config::{Config, ReaderConfig, SplitMode};
use shelfgauge::container::ReadLimits;
use shelfgauge::format::txt::TxtParser;
use shelfgauge::format::{BookParser, Registry, run_with_parser_stack};
use shelfgauge::library::Library;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::{all_books, book_by_id};
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

const FULL: &[u8] = include_bytes!("fixtures/fb2/full.fb2");

fn fb2(title: &str, body: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
<author><last-name>Тестов</last-name></author><book-title>{title}</book-title><lang>en</lang>
</title-info></description><body>{body}</body></FictionBook>"#
    )
    .into_bytes()
}

/// Three chapters of four paragraphs each.
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
    fb2("Chapters", &body)
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
        lib.write("chapters.fb2", &chapters());
        lib.write("full.fb2", FULL);
        lib.write("book.epub", &common::epub3());
        lib.write("Пушкин - Дубровский.txt", common::txt_libru().as_bytes());
        lib.write(
            "markup.fb2",
            &fb2(
                "Markup",
                "<section><p>A &lt;b&gt;bold&lt;/b&gt; claim &amp; a \"quote\".</p></section>",
            ),
        );
        lib.write(
            "many.fb2",
            &fb2(
                "Many",
                &format!("<section><p>{}</p></section>", "echo ".repeat(1234)),
            ),
        );
        lib.write("gone.fb2", &chapters());
        lib.scan();
        std::fs::remove_file(lib.dir.path().join("gone.fb2")).unwrap();
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

    fn id_of(&self, path: &str) -> i64 {
        let conn = open_writer(&self.library.db_path()).unwrap();
        all_books(&conn)
            .unwrap()
            .into_iter()
            .map(|b| book_by_id(&conn, b.id).unwrap().unwrap())
            .find(|b| b.container_path == path)
            .map(|b| b.id)
            .unwrap_or_else(|| panic!("no book at {path}"))
    }

    fn server(&self, reader: ReaderConfig) -> Server {
        let mut config = Config {
            reader,
            ..Config::default()
        };
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
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn ok(&self, uri: &str) -> Reply {
        let reply = self.get(uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
        reply
    }
}

/// One part per chapter.
fn by_chapter() -> ReaderConfig {
    ReaderConfig {
        split: SplitMode::Section,
        section_depth: 1,
        ..ReaderConfig::default()
    }
}

/// The result links of a results list, unescaped.
fn hrefs(body: &str) -> Vec<String> {
    body.split("<a href=\"")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap().replace("&#38;", "&"))
        .filter(|href| href.contains("hit="))
        .collect()
}

/// The number of results listed.
fn count(body: &str) -> usize {
    body.matches(" data-hit=\"").count()
}

#[tokio::test]
async fn results_link_to_the_right_parts() {
    let lib = TestLibrary::new();
    let server = lib.server(by_chapter());
    let id = lib.id_of("chapters.fb2");
    let reply = server.ok(&format!("/book/{id}/find?q=Paragraph%202")).await;
    assert_eq!(
        reply.headers.get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert_eq!(count(&reply.body), 3);
    for n in 1..=3 {
        assert!(
            reply
                .body
                .contains(&format!("<h3 class=\"find-section\">Chapter {n}</h3>")),
            "{}",
            reply.body
        );
    }
    assert!(reply.body.contains("<mark>paragraph 2</mark>"));
    let links = hrefs(&reply.body);
    assert_eq!(links.len(), 3);
    let mut parts = Vec::new();
    for (i, link) in links.iter().enumerate() {
        assert!(link.contains("find=Paragraph%202"), "{link}");
        assert!(link.contains(&format!("hit={}", i + 1)), "{link}");
        let (url, block) = link.split_once('#').unwrap();
        let page = server.ok(url).await;
        // The block is in the part linked to, and the page lists the results with this
        // one marked.
        assert!(
            page.body.contains(&format!(" id=\"{block}\"")),
            "{block} not in {url}"
        );
        assert!(page.body.contains(&format!(
            "<li class=\"current\" aria-current=\"true\" data-hit=\"{}\"",
            i + 1
        )));
        assert_eq!(
            page.body.matches("aria-current=\"true\" data-hit").count(),
            1
        );
        parts.push(
            url.split("part=")
                .nth(1)
                .map(|p| p.split('&').next().unwrap().to_owned()),
        );
    }
    // One chapter per part, all different.
    parts.dedup();
    assert_eq!(parts.len(), 3, "{parts:?}");
}

#[tokio::test]
async fn every_format_is_searched_with_normalization() {
    let lib = TestLibrary::new();
    let server = lib.server(ReaderConfig::default());
    // FB2: a title, main text and a footnote (grouped as notes).
    let full = lib.id_of("full.fb2");
    let reply = server.ok(&format!("/book/{full}/find?q=STAIRS")).await;
    assert_eq!(count(&reply.body), 2, "{}", reply.body);
    assert!(reply.body.contains("<h3 class=\"find-section\">Notes</h3>"));
    assert!(
        reply
            .body
            .contains("A footnote about the <mark>stairs</mark>.")
    );
    // `ё` and `е` match; Cyrillic case folds.
    let reply = server
        .ok(&format!(
            "/book/{full}/find?q=%D1%82%D0%B5%D0%BC%D0%BD%D0%BE%D1%8E"
        )) // "темною"
        .await;
    assert!(reply.body.contains("<mark>тёмною</mark>"), "{}", reply.body);
    // EPUB.
    let epub = lib.id_of("book.epub");
    let reply = server
        .ok(&format!("/book/{epub}/find?q=endnote%20one"))
        .await;
    assert_eq!(count(&reply.body), 1, "{}", reply.body);
    assert!(reply.body.contains("<mark>Endnote one</mark>"));
    // TXT: whitespace runs of the hard-wrapped text count as one space.
    let txt = lib.id_of("Пушкин - Дубровский.txt");
    let reply = server
        .ok(&format!(
            "/book/{txt}/find?q={}",
            "кирила петрович".replace(' ', "%20")
        ))
        .await;
    assert!(count(&reply.body) > 1, "{}", reply.body);
    assert!(reply.body.contains("<mark>Кирила Петрович</mark>"));
}

#[tokio::test]
async fn short_long_and_empty_queries_are_not_searched() {
    let lib = TestLibrary::new();
    let server = lib.server(ReaderConfig::default());
    let id = lib.id_of("chapters.fb2");
    for (query, message) in [
        ("", "Type a word or a phrase"),
        ("%20%20", "Type a word or a phrase"),
        ("x", "Type at least 2 characters."),
        (&"x".repeat(101), "at most 100 characters"),
        (&"x".repeat(5000), "at most 100 characters"),
    ] {
        let reply = server.ok(&format!("/book/{id}/find?q={query}")).await;
        assert!(reply.body.contains(message), "{query:?}: {}", reply.body);
        assert!(!reply.body.contains("find-hits"));
    }
    let reply = server.ok(&format!("/book/{id}/find?q=zzz")).await;
    assert!(reply.body.contains("<p class=\"hint\">No matches.</p>"));
    assert_eq!(count(&reply.body), 0);
    // Invalid UTF-8 is decoded lossily: one replacement character.
    let reply = server.ok(&format!("/book/{id}/find?q=%FF")).await;
    assert!(reply.body.contains("at least 2"), "{}", reply.body);
}

#[tokio::test]
async fn at_most_500_results_are_listed_and_all_are_counted() {
    let lib = TestLibrary::new();
    let server = lib.server(ReaderConfig::default());
    let id = lib.id_of("many.fb2");
    let reply = server.ok(&format!("/book/{id}/find?q=echo")).await;
    assert_eq!(count(&reply.body), 500);
    assert!(
        reply.body.contains(
            "The first 500 of 1\u{202f}234 matches are listed; refine the query to find the rest."
        ),
        "{}",
        reply.body
    );
    let reply = server.ok(&format!("/book/{id}/find?q=echo%20echo")).await;
    assert_eq!(count(&reply.body), 500, "non-overlapping: 617 matches");
    assert!(reply.body.contains("The first 500 of 617 matches"));
    let chapters = lib.id_of("chapters.fb2");
    let reply = server.ok(&format!("/book/{chapters}/find?q=chapter")).await;
    assert!(!reply.body.contains("find-more"), "all listed: no note");
}

#[tokio::test]
async fn markup_in_the_text_and_the_query_is_escaped() {
    let lib = TestLibrary::new();
    let server = lib.server(ReaderConfig::default());
    let id = lib.id_of("markup.fb2");
    let query = "%3Cb%3Ebold%3C%2Fb%3E%20claim%20%26"; // "<b>bold</b> claim &"
    let reply = server.ok(&format!("/book/{id}/find?q={query}")).await;
    assert_eq!(count(&reply.body), 1, "{}", reply.body);
    assert!(
        reply
            .body
            .contains("A <mark>&#60;b&#62;bold&#60;/b&#62; claim &#38;</mark> a &#34;quote&#34;."),
        "{}",
        reply.body
    );
    assert!(!reply.body.contains("<b>"));
    // The read page: the field and the links carry the query escaped.
    let reply = server.ok(&format!("/book/{id}/read?find={query}")).await;
    assert!(
        reply
            .body
            .contains("value=\"&#60;b&#62;bold&#60;/b&#62; claim &#38;\"")
    );
    assert!(!reply.body.contains("<b>bold"));
    for link in hrefs(&reply.body) {
        // `/` needs no escaping in a query value.
        assert!(
            link.contains(&format!("find={}", query.replace("%2F", "/"))),
            "{link}"
        );
    }
}

#[tokio::test]
async fn unknown_and_removed_books() {
    let lib = TestLibrary::new();
    let server = lib.server(ReaderConfig::default());
    assert_eq!(
        server.get("/book/999/find?q=text").await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        server.get("/book/x/find?q=text").await.status,
        StatusCode::NOT_FOUND
    );
    let gone = lib.id_of("gone.fb2");
    let reply = server.get(&format!("/book/{gone}/find?q=text")).await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.body.contains("rescan"), "{}", reply.body);
}

#[tokio::test]
async fn the_read_page_lists_results_without_scripts() {
    let lib = TestLibrary::new();
    let server = lib.server(by_chapter());
    let id = lib.id_of("chapters.fb2");
    // Without `find` the Search tab and panel are hidden; the top bar's field searches
    // the book and keeps the part.
    let reply = server.ok(&format!("/book/{id}/read?part=2")).await;
    assert!(reply.body.contains(
        "class=\"find-tab\" title=\"Search results\" aria-label=\"Search results\" hidden><svg"
    ));
    assert!(
        reply
            .body
            .contains("id=\"tab-find\" data-tab=\"find\" aria-label=\"Search results\" hidden>")
    );
    let panel = reply.body.find("id=\"tab-find\"").unwrap();
    assert!(panel > reply.body.find("id=\"tab-marks\"").unwrap());
    assert!(reply.body.contains(&format!(
        "<form class=\"search find-form\" action=\"/book/{id}/read\" method=\"get\""
    )));
    assert!(
        reply
            .body
            .contains("<input type=\"hidden\" name=\"part\" value=\"2\">")
    );
    assert!(
        reply
            .body
            .contains("title=\"Search in this book (/ or s)\"")
    );
    assert!(
        !reply.body.contains("name=\"q\""),
        "no library search field in reading mode"
    );
    assert!(!reply.body.contains("find-hits"));
    // The form's answer: the results first, in the same part, under their count.
    let reply = server
        .ok(&format!("/book/{id}/read?part=2&find=paragraph+3"))
        .await;
    assert_eq!(count(&reply.body), 3);
    assert!(
        reply
            .body
            .contains("title=\"Search results: 3 matches\" aria-label=\"Search results: 3 matches\"><svg class=\"icon\" aria-hidden=\"true\"><use href=\"#i-search\"></use></svg><span class=\"tab-count\">3</span></a>")
    );
    assert!(
        reply
            .body
            .contains("id=\"tab-find\" data-tab=\"find\" aria-label=\"Search results\">")
    );
    let panel = reply.body.find("id=\"tab-find\"").unwrap();
    assert!(panel < reply.body.find("id=\"tab-toc\"").unwrap());
    assert!(reply.body.contains("value=\"paragraph 3\""));
    assert!(!reply.body.contains("Search the library"));
    assert!(
        reply.body.contains("Chapter 2, paragraph 1"),
        "still part 2"
    );
    // An empty query shows no results.
    let reply = server.ok(&format!("/book/{id}/read?find=+")).await;
    assert!(!reply.body.contains("find-hits"));
    assert!(
        reply
            .body
            .contains("aria-label=\"Search results\" hidden><svg")
    );
    // Library pages have no Search tab, and their field searches the library.
    for page in ["/".to_owned(), format!("/book/{id}")] {
        let reply = server.ok(&page).await;
        assert!(!reply.body.contains("tab-find"), "{page}");
        assert!(reply.body.contains("title=\"Search (/ or s)\""), "{page}");
    }
}

/// Times searches in every `.txt` in the directory named by `SHELFGAUGE_FIND_SAMPLES`:
/// `cargo test --release --test find -- --ignored --nocapture`.
#[test]
#[ignore = "needs real books in SHELFGAUGE_FIND_SAMPLES"]
fn timing_on_real_books() {
    let Ok(dir) = std::env::var("SHELFGAUGE_FIND_SAMPLES") else {
        eprintln!("SHELFGAUGE_FIND_SAMPLES is not set");
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
        run_with_parser_stack(|| {
            let content = TxtParser.parse_content(&data).unwrap();
            let start = std::time::Instant::now();
            let text = BookText::new(&content);
            let built = start.elapsed();
            println!(
                "\n=== {name}: {} bytes, {} leaves; BookText::new {built:?}",
                data.len(),
                text.leaves().len()
            );
            // The first search builds the normalized text.
            let start = std::time::Instant::now();
            find(&text, &Query::parse("zz").unwrap(), MAX_HITS);
            println!("  first search: {:?}", start.elapsed());
            for raw in [
                "the",
                "Napoleon",
                "zzzz",
                "князь",
                "a long phrase that is not there",
            ] {
                let query = Query::parse(raw).unwrap();
                let start = std::time::Instant::now();
                let hits = find(&text, &query, MAX_HITS);
                println!(
                    "  {raw:?}: {} hits ({} listed) in {:?}",
                    hits.total,
                    hits.hits.len(),
                    start.elapsed()
                );
            }
        });
    }
}
