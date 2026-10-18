//! Library statistics end to end: `/stats`, the book lists behind its figures, the
//! duplicates and the library page's notice, on scanned temporary libraries.

mod common;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use shelfgauge::annotations::export::Exporter;
use shelfgauge::annotations::store::UserData;
use shelfgauge::config::Config;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::library::Library;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::{BookStatus, all_books, book_by_id};
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

const ORIGIN: &str = "http://127.0.0.1:8742";

/// An FB2 book; `author` is `(first, middle, last)`, `None` for none.
fn fb2(title: &str, author: Option<(&str, &str, &str)>, text: &str, genre: &str) -> Vec<u8> {
    let escape = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };
    let author = author.map_or(String::new(), |(first, middle, last)| {
        format!(
            "<author><first-name>{first}</first-name><middle-name>{middle}</middle-name>\
             <last-name>{last}</last-name></author>"
        )
    });
    let paragraphs: String = (1..=6)
        .map(|n| format!("<p>{} {n}</p>", escape(text)))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
<genre>{genre}</genre>{author}<book-title>{}</book-title><lang>ru</lang></title-info></description>
<body><section><title><p>Chapter</p></title>{paragraphs}</section></body></FictionBook>"#,
        escape(title)
    )
    .into_bytes()
}

const TOLSTOY: Option<(&str, &str, &str)> = Some(("Лев", "Николаевич", "Толстой"));
const CHEKHOV: Option<(&str, &str, &str)> = Some(("Антон", "Павлович", "Чехов"));

struct TestLibrary {
    dir: tempfile::TempDir,
    library: Library,
    userdata: Arc<UserData>,
}

impl TestLibrary {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let userdata = Arc::new(UserData::open(&library.userdata_path()).unwrap());
        Self {
            dir,
            library,
            userdata,
        }
    }

    fn write(&self, path: &str, data: &[u8]) {
        let full = self.dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, data).unwrap();
    }

    fn zip(&self, path: &str, entries: &[(&str, Vec<u8>)]) {
        let full = self.dir.path().join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut writer = zip::ZipWriter::new(std::fs::File::create(full).unwrap());
        for (name, data) in entries {
            writer
                .start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
    }

    fn scan(&self) {
        let mut conn = open_writer(&self.library.db_path()).unwrap();
        Scanner {
            library: &self.library,
            exclude: &[],
            registry: &Registry::with_builtin_formats(),
            reread: false,
            limits: ReadLimits::default(),
            threads: 2,
            progress: &ScanProgress::default(),
            cancel: &AtomicBool::new(false),
        }
        .run(&mut conn)
        .unwrap();
    }

    /// The id of the live book at `path` (`archive/entry` for a zip entry).
    fn id_of(&self, path: &str) -> i64 {
        let conn = open_writer(&self.library.db_path()).unwrap();
        all_books(&conn)
            .unwrap()
            .into_iter()
            .filter(|b| b.status != BookStatus::Removed)
            .map(|b| book_by_id(&conn, b.id).unwrap().unwrap())
            .find(|b| match &b.entry {
                Some(entry) => format!("{}/{}", b.container_path, entry.path) == path,
                None => b.container_path == path,
            })
            .unwrap_or_else(|| panic!("no live book at {path}"))
            .id
    }

    fn server(&self) -> Server {
        let config = Config::default();
        let pool = open_read_pool(&self.library.db_path(), 2).unwrap();
        let scan = Arc::new(BackgroundScan::new(self.library.clone(), Vec::new()));
        let exporter = Exporter::new(self.library.annotations_dir(), "http://127.0.0.1:8742/");
        let state = AppState::new(self.library.clone(), &config, pool, Arc::clone(&scan))
            .with_annotations(Arc::clone(&self.userdata), exporter);
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

impl Server {
    async fn send(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn get(&self, uri: &str) -> (StatusCode, String) {
        self.send(
            Request::get(uri)
                .header(header::HOST, "127.0.0.1:8742")
                .body(Body::empty())
                .unwrap(),
        )
        .await
    }

    /// The main area of a page that must exist.
    async fn main(&self, uri: &str) -> String {
        let (status, body) = self.get(uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        body[body.find("<main").expect("a page")..].to_owned()
    }

    async fn post_json(&self, uri: &str, body: serde_json::Value) -> StatusCode {
        let request = Request::post(uri)
            .header(header::HOST, "127.0.0.1:8742")
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        self.send(request).await.0
    }

    /// The id (`b12`) of the first block whose text starts with `text`.
    async fn block_of(&self, id: i64, text: &str) -> String {
        let body = self.main(&format!("/book/{id}/read")).await;
        let at = body
            .find(&format!("\">{text}"))
            .unwrap_or_else(|| panic!("{text:?} not found"));
        let start = body[..at].rfind("id=\"").unwrap() + 4;
        body[start..at].to_owned()
    }
}

/// A library with every kind of book the statistics tell apart:
///
/// - `war.fb2` and the same bytes in `pack.zip` (identical copies across kinds);
/// - `copies/a.zip` and `copies/b.zip` with the same entry (identical copies in two
///   archives);
/// - `war.epub`: the same title and author as `war.fb2`, another content (probable);
/// - `other-war.fb2`: the same title by another author (not a duplicate);
/// - `anon-1.fb2` and `anon-2.fb2`: the same title without an author (not matched);
/// - `book.txt` (without an author), `markup.fb2` (a title with markup), `broken.fb2`
///   and `locked.epub`.
fn mixed_library() -> TestLibrary {
    let lib = TestLibrary::new();
    let war = fb2("Война и мир", TOLSTOY, "Text of the war", "prose_classic");
    lib.write("war.fb2", &war);
    lib.zip(
        "pack.zip",
        &[
            ("war.fb2", war),
            (
                "steppe.fb2",
                fb2("Степь", CHEKHOV, "Steppe", "prose_classic"),
            ),
        ],
    );
    let same = fb2("Каштанка", CHEKHOV, "A dog", "child_prose");
    lib.zip("copies/a.zip", &[("same.fb2", same.clone())]);
    lib.zip("copies/b.zip", &[("same.fb2", same)]);
    lib.write("war.epub", &common::epub3());
    lib.write(
        "other-war.fb2",
        &fb2("Война и мир", Some(("Иван", "", "Иванов")), "Other", "sf"),
    );
    lib.write("anon-1.fb2", &fb2("Без автора", None, "One", "sf"));
    lib.write("anon-2.fb2", &fb2("Без автора", None, "Two", "sf"));
    lib.write("book.txt", common::txt_libru().as_bytes());
    lib.write(
        "markup.fb2",
        &fb2("<b>Bold</b> & co", CHEKHOV, "Markup", "sf"),
    );
    lib.write("broken.fb2", b"<broken");
    lib.write("locked.epub", &common::epub3_drm());
    lib.scan();
    lib
}

#[tokio::test]
async fn statistics_of_a_mixed_library() {
    let lib = mixed_library();
    let server = lib.server();
    let main = server.main("/stats").await;

    // 13 books that are not removed, 11 readable (not the broken and the DRM one).
    assert!(
        main.contains(
            "<span class=\"stat-value\">13</span> <span class=\"stat-label\">books</span>"
        ),
        "{main}"
    );
    assert!(main.contains("11 readable"));
    assert!(main.contains("in zip archives"));
    // Formats: 9 FB2, 1 EPUB, 1 TXT, linked to their lists.
    assert!(
        main.contains("<a href=\"/books?format=fb2\">FB2</a>"),
        "{main}"
    );
    let fb2_bar = &main[main.find("/books?format=fb2").unwrap()..];
    assert!(
        fb2_bar[..fb2_bar.find("</li>").unwrap()].contains("<span class=\"bar-count\">9</span>")
    );
    assert!(main.contains("<a href=\"/books?format=epub\">EPUB</a>"));
    // Genres by name; the bar width is a class.
    assert!(main.contains("<a href=\"/books?genre=prose_classic\">Classic prose</a>"));
    assert!(main.contains("<span class=\"bar\" aria-hidden=\"true\"><span class=\"w"));
    assert!(!main.contains("style="));
    // Top authors link to their pages.
    assert!(main.contains("Чехов Антон Павлович"));
    // Index health.
    for part in [
        "<a href=\"/errors\">1 book could not be read</a>",
        "<a href=\"/errors#protected\">1 book is protected by DRM</a>",
        "<a href=\"/duplicates\">2 books have identical copies: 4 files, ",
        "<a href=\"/duplicates?kind=probable\">1 book looks like the same book in several files</a>",
        "<a href=\"/books?missing=author\">3 books without an author</a>",
        "The last scan, ",
    ] {
        assert!(main.contains(part), "{part:?} missing in {main}");
    }
    // Titles are escaped.
    assert!(!main.contains("<b>Bold"));

    // The lists behind the figures.
    let list = server.main("/books?format=fb2").await;
    assert!(list.contains("<h1>Books in FB2</h1>"), "{list}");
    assert!(list.contains("9 books"));
    assert_eq!(list.matches("<li><a href=\"/book/").count(), 9, "{list}");
    assert!(list.contains("&#60;b&#62;Bold&#60;/b&#62; &#38; co"));
    assert!(!list.contains("<b>Bold"));
    let list = server.main("/books?genre=prose_classic").await;
    assert!(list.contains("<h1>Genre: Classic prose</h1>") && list.contains("3 books"));
    // The two books without an author and the text book.
    let list = server.main("/books?missing=author").await;
    assert_eq!(list.matches("Без автора").count(), 2, "{list}");
    assert!(list.contains("3 books"));
    let list = server.main("/books?language=ru").await;
    assert!(list.contains("Books in the language “ru”"));
    let list = server.main("/books?genre=nothing%20here").await;
    assert!(
        list.contains("0 books") && list.contains("No books."),
        "{list}"
    );
    for uri in ["/books", "/books?missing=nonsense", "/books?format=%20"] {
        assert_eq!(server.get(uri).await.0, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn duplicates_are_listed_never_merged() {
    let lib = mixed_library();
    let server = lib.server();

    let identical = server.main("/duplicates").await;
    assert!(
        identical.contains("2 books have identical copies"),
        "{identical}"
    );
    assert!(identical.contains("class=\"active\" aria-current=\"page\">Identical copies"));
    assert_eq!(
        identical.matches("class=\"books duplicate-group\"").count(),
        2
    );
    for path in [
        ">war.fb2</span>",
        ">pack.zip/war.fb2</span>",
        ">copies/a.zip/same.fb2</span>",
        ">copies/b.zip/same.fb2</span>",
    ] {
        assert!(identical.contains(path), "{path} missing in {identical}");
    }
    // Loose and zipped copies of war.fb2 are one group.
    let war_group = identical
        .split("duplicate-group")
        .find(|g| g.contains(">war.fb2</span>"))
        .unwrap();
    assert!(war_group.contains(">pack.zip/war.fb2</span>"));
    assert!(!identical.contains("war.epub") && !identical.contains("anon-"));

    let probable = server.main("/duplicates?kind=probable").await;
    assert!(
        probable.contains("1 book looks like the same book"),
        "{probable}"
    );
    assert_eq!(
        probable.matches("class=\"books duplicate-group\"").count(),
        1
    );
    assert!(probable.contains(">war.epub</span>") && probable.contains(">war.fb2</span>"));
    assert!(probable.contains("· EPUB ·") && probable.contains("· FB2 ·"));
    // Another author, or no author at all: not the same book.
    assert!(!probable.contains("other-war") && !probable.contains("anon-"));

    // Every copy is still in the library.
    for path in ["war.fb2", "pack.zip/war.fb2", "copies/b.zip/same.fb2"] {
        lib.id_of(path);
    }
    // The library page's notice.
    let home = server.main("/").await;
    assert!(
        home.contains("<a href=\"/duplicates\">2 books have identical copies</a>"),
        "{home}"
    );
    assert!(home.contains("<a class=\"stats-link\" href=\"/stats\">Library statistics ›</a>"));
}

#[tokio::test]
async fn reading_and_quotes_of_books_in_the_library() {
    let lib = mixed_library();
    let server = lib.server();
    assert!(!server.main("/stats").await.contains("Reading and quotes"));

    let war = lib.id_of("war.fb2");
    let steppe = lib.id_of("pack.zip/steppe.fb2");
    let anon = lib.id_of("anon-1.fb2");
    for (id, text) in [
        (war, "Text of the war 2"),
        (steppe, "Steppe 2"),
        (anon, "One 2"),
    ] {
        let block = server.block_of(id, text).await;
        let status = server
            .post_json(
                &format!("/book/{id}/position"),
                serde_json::json!({ "block": block }),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }
    let block = server.block_of(war, "Text of the war 3").await;
    let status = server
        .post_json(
            &format!("/book/{war}/annotations"),
            serde_json::json!({
                "start_block": block, "start_offset": 0, "end_block": block, "end_offset": 8,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let status = server
        .post_json(
            &format!("/book/{steppe}/annotations"),
            serde_json::json!({ "block": block }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    let request = Request::post(format!("/book/{steppe}/finished"))
        .header(header::HOST, "127.0.0.1:8742")
        .header(header::ORIGIN, ORIGIN)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::empty())
        .unwrap();
    assert_eq!(server.send(request).await.0, StatusCode::SEE_OTHER);
    // A book no longer in the library is left out.
    std::fs::remove_file(lib.dir.path().join("anon-1.fb2")).unwrap();
    lib.scan();

    let main = server.main("/stats").await;
    let section = &main[main.find("Reading and quotes").expect("the section")..];
    for part in [
        "<li>1 book in progress</li>",
        "<li>1 book finished</li>",
        "<li>1 bookmark</li>",
        "<li>1 quote in 1 book</li>",
        ">Text of </a>",
        &format!("href=\"/book/{war}/read?at={block}&#38;from=mark\""),
        &format!("<a href=\"/book/{war}\">Война и мир</a>"),
    ] {
        assert!(section.contains(part), "{part:?} missing in {section}");
    }
}

#[tokio::test]
async fn statistics_of_an_empty_library_and_after_a_scan() {
    let lib = TestLibrary::new();
    lib.scan();
    let server = lib.server();
    let main = server.main("/stats").await;
    assert!(main.contains("<h1>Library statistics</h1>"));
    assert!(
        main.contains(
            "<span class=\"stat-value\">0</span> <span class=\"stat-label\">books</span>"
        )
    );
    for absent in [
        "class=\"chart\"",
        "Reading and quotes",
        "/duplicates",
        "/errors",
    ] {
        assert!(!main.contains(absent), "{absent} in {main}");
    }
    assert!(main.contains("The last scan, "));
    let duplicates = server.main("/duplicates").await;
    assert!(duplicates.contains("No book has identical copies."));

    // The cached statistics are dropped when a scan is recorded.
    lib.write("one.fb2", &fb2("Один", CHEKHOV, "One", "sf"));
    lib.scan();
    let main = server.main("/stats").await;
    assert!(
        main.contains("<span class=\"stat-value\">1</span> <span class=\"stat-label\">book</span>"),
        "{main}"
    );
}

#[tokio::test]
async fn paths_with_markup_are_escaped() {
    let lib = TestLibrary::new();
    let book = fb2("Копия", CHEKHOV, "Copy", "sf");
    // `<` and `>` are not allowed in Windows file names, so the markup lives in zip
    // entry names, which take any character.
    lib.zip(
        "a&b.zip",
        &[
            ("<c>/one.fb2", book.clone()),
            ("<c>/two.fb2", book),
            ("<c>/broken.fb2", b"<broken".to_vec()),
        ],
    );
    lib.scan();
    let server = lib.server();
    for uri in ["/duplicates", "/errors"] {
        let main = server.main(uri).await;
        assert!(main.contains("a&#38;b.zip/&#60;c&#62;/"), "{uri}: {main}");
        assert!(!main.contains("<c>"), "{uri}: {main}");
    }
}
