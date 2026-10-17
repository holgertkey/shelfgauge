//! Library-mode UI tests: pages, sidebar fragments, search, covers and pagination on a
//! scanned temporary library.

mod common;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use shelfgauge::config::Config;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::library::Library;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::{BookStatus, all_books, book_by_id};
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

/// An SVG with a script: declared as `image/svg+xml`, which must never be served.
const SVG: &str = "PHN2ZyB4bWxucz0iaHR0cDovL3d3dy53My5vcmcvMjAwMC9zdmciPjxzY3JpcHQ+YWxlcnQoMSk8L3NjcmlwdD48L3N2Zz4=";

/// A 1×1 PNG.
const PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

struct Book<'a> {
    title: &'a str,
    author: (&'a str, &'a str),
    series: Option<(&'a str, &'a str)>,
    /// `(content-type, base64 data)`.
    cover: Option<(&'a str, &'a str)>,
    annotation: &'a str,
}

impl<'a> Book<'a> {
    fn new(title: &'a str, last: &'a str, first: &'a str) -> Self {
        Self {
            title,
            author: (last, first),
            series: None,
            cover: None,
            annotation: "",
        }
    }

    fn fb2(&self) -> Vec<u8> {
        let escape = |s: &str| {
            s.replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
        };
        let series = self.series.map_or(String::new(), |(name, n)| {
            format!(r#"<sequence name="{}" number="{n}"/>"#, escape(name))
        });
        let (coverpage, binary) = match self.cover {
            Some((mime, data)) => (
                r##"<coverpage><image l:href="#c"/></coverpage>"##.to_owned(),
                format!(r#"<binary id="c" content-type="{mime}">{data}</binary>"#),
            ),
            None => (String::new(), String::new()),
        };
        let annotation = if self.annotation.is_empty() {
            String::new()
        } else {
            format!(
                "<annotation><p>{}</p></annotation>",
                escape(self.annotation)
            )
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
<genre>prose</genre><author><first-name>{first}</first-name><last-name>{last}</last-name></author>
<book-title>{title}</book-title>{annotation}{coverpage}<lang>ru</lang>{series}</title-info></description>
<body><section><p>{title}</p></section></body>{binary}
</FictionBook>"#,
            first = escape(self.author.1),
            last = escape(self.author.0),
            title = escape(self.title),
        )
        .into_bytes()
    }
}

struct TestLibrary {
    dir: tempfile::TempDir,
    library: Library,
}

impl TestLibrary {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        Self { dir, library }
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
        let registry = Registry::with_builtin_formats();
        let progress = ScanProgress::default();
        let cancel = AtomicBool::new(false);
        Scanner {
            library: &self.library,
            exclude: &[],
            registry: &registry,
            limits: ReadLimits::default(),
            threads: 2,
            progress: &progress,
            cancel: &cancel,
        }
        .run(&mut conn)
        .unwrap();
    }

    fn id_of(&self, title: &str) -> i64 {
        let conn = open_writer(&self.library.db_path()).unwrap();
        all_books(&conn)
            .unwrap()
            .into_iter()
            .map(|b| book_by_id(&conn, b.id).unwrap().unwrap())
            .find(|b| b.title == title)
            .unwrap_or_else(|| panic!("no book {title:?}"))
            .id
    }

    fn status_of(&self, title: &str) -> BookStatus {
        let conn = open_writer(&self.library.db_path()).unwrap();
        book_by_id(&conn, self.id_of(title))
            .unwrap()
            .unwrap()
            .status
    }

    fn server(&self, base_path: &str, page_size: u32) -> Server {
        let mut config = Config::default();
        config.server.base_path = base_path.to_owned();
        config.library.page_size = page_size;
        config.validate().unwrap();
        open_writer(&self.library.db_path()).unwrap();
        let pool = open_read_pool(&self.library.db_path(), 2).unwrap();
        let scan = Arc::new(BackgroundScan::new(self.library.clone(), Vec::new()));
        let state = AppState::new(self.library.clone(), &config, pool, Arc::clone(&scan));
        Server {
            router: web::router(state),
            scan,
        }
    }
}

/// The standard test library:
///
/// - `Толстой Лев/`: "Война и мир" (series "Эпопея" #1, PNG cover, annotation) and
///   "Анна Каренина" (no series);
/// - `a+b & c#1 %/Капитанская дочка.fb2` by Пушкин (a folder name that needs encoding);
/// - `zips/коллекция.zip` with `inner dir/Палата.fb2` and `Степь.fb2` by Чехов;
/// - `broken.fb2` (not XML), `svg-cover.fb2` (an SVG cover with a script),
///   `<script>.fb2` (a title with markup) and `gone.fb2` (removed after the first
///   scan).
fn library() -> TestLibrary {
    let lib = TestLibrary::new();
    lib.write(
        "Толстой Лев/Война и мир.fb2",
        &Book {
            series: Some(("Эпопея", "1")),
            cover: Some(("image/png", PNG)),
            annotation: "Первая строка аннотации",
            ..Book::new("Война и мир", "Толстой", "Лев")
        }
        .fb2(),
    );
    lib.write(
        "Толстой Лев/Анна Каренина.fb2",
        &Book::new("Анна Каренина", "Толстой", "Лев").fb2(),
    );
    lib.write(
        "a+b & c#1 %/Капитанская дочка.fb2",
        &Book::new("Капитанская дочка", "Пушкин", "Александр").fb2(),
    );
    lib.zip(
        "zips/коллекция.zip",
        &[
            (
                "inner dir/Палата.fb2",
                Book::new("Палата № 6", "Чехов", "Антон").fb2(),
            ),
            ("Степь.fb2", Book::new("Степь", "Чехов", "Антон").fb2()),
        ],
    );
    lib.write("broken.fb2", b"not xml at all");
    lib.write(
        "svg-cover.fb2",
        &Book {
            cover: Some(("image/svg+xml", SVG)),
            ..Book::new("Обложка", "Гоголь", "Николай")
        }
        .fb2(),
    );
    lib.write(
        "script.fb2",
        &Book::new("<script>alert(1)</script>", "Гоголь", "Николай").fb2(),
    );
    lib.write(
        "gone.fb2",
        &Book::new("Пропавшая", "Гоголь", "Николай").fb2(),
    );
    lib.scan();
    std::fs::remove_file(lib.dir.path().join("gone.fb2")).unwrap();
    lib.scan();
    lib
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

    fn has(&self, text: &str) -> bool {
        self.body.contains(text)
    }

    #[track_caller]
    fn assert_has(&self, text: &str) {
        assert!(self.has(text), "{text:?} missing in:\n{}", self.body);
    }

    /// The main area only.
    fn main(&self) -> &str {
        let start = self.body.find("<main").expect("a page with <main>");
        &self.body[start..]
    }

    /// The sidebar tree only.
    fn sidebar(&self) -> &str {
        let start = self.body.find("class=\"tree-root\"").expect("a sidebar");
        let end = self.body.find("<main").unwrap();
        &self.body[start..end]
    }
}

impl Server {
    async fn send(&self, request: Request<Body>) -> Reply {
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

    async fn get(&self, uri: &str) -> Reply {
        self.get_with(uri, &[]).await
    }

    async fn get_with(&self, uri: &str, headers: &[(&str, &str)]) -> Reply {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::HOST, "127.0.0.1:8742");
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        self.send(builder.body(Body::empty()).unwrap()).await
    }

    async fn ok(&self, uri: &str) -> Reply {
        let reply = self.get(uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
        reply
    }
}

const FOLDERS: (&str, &str) = ("cookie", "shelfgauge-sidebar=folders");

#[tokio::test]
async fn home_shows_counts_and_the_sidebar() {
    let lib = library();
    let server = lib.server("/", 100);
    let reply = server.ok("/").await;
    // 8 live books (one removed; the error book counts), authors: Толстой, Пушкин,
    // Чехов, Гоголь.
    reply.assert_has("8 books · 4 authors · ");
    assert!(
        reply.sidebar().contains("Толстой Лев"),
        "authors mode by default"
    );
    assert!(
        reply
            .body
            .contains("data-mode=\"authors\" class=\"active\"")
    );

    let reply = server.get_with("/", &[FOLDERS]).await;
    assert!(
        reply
            .body
            .contains("data-mode=\"folders\" class=\"active\"")
    );
    assert!(
        reply
            .sidebar()
            .contains("data-src=\"/sidebar/folder?path=zips\""),
        "{}",
        reply.sidebar()
    );
    assert!(!reply.sidebar().contains("коллекция.zip"), "collapsed");
}

#[tokio::test]
async fn home_shows_new_books_the_summary_and_what_needs_attention() {
    let lib = library();
    let server = lib.server("/", 100);
    let reply = server.ok("/").await;
    let main = reply.main();
    // Recently added: the 7 readable books (not the broken or the removed one).
    let recent = &main[main.find("Recently added").expect("the section")..];
    let recent = &recent[..recent.find("</section>").unwrap()];
    assert_eq!(recent.matches("<li>").count(), 7, "{recent}");
    assert!(!recent.contains("Пропавшая") && !recent.contains("broken"));
    // A served cover, and a placeholder for the SVG one (never served).
    let war = lib.id_of("Война и мир");
    assert!(
        recent.contains(&format!("src=\"/cover/{war}?v=")),
        "{recent}"
    );
    let svg = lib.id_of("Обложка");
    assert!(!recent.contains(&format!("/cover/{svg}")), "{recent}");
    assert!(
        recent.contains("<span class=\"thumb-placeholder\" aria-hidden=\"true\">Обложка</span>")
    );
    assert!(recent.contains(&format!("href=\"/book/{svg}\"")));
    // Titles are escaped.
    assert!(!main.contains("<script>alert"));
    assert!(recent.contains("&#60;script&#62;alert(1)&#60;/script&#62;"));
    // The summary line and the notice of the broken book.
    assert!(
        main.contains("<p class=\"summary\">8 books · 4 authors · "),
        "{main}"
    );
    assert!(
        main.contains("<a href=\"/errors\">1 book could not be read</a> ›"),
        "{main}"
    );
    assert!(!main.contains("protected by DRM"));
    // The keys are collapsed; the old paragraph is gone.
    assert!(main.contains("<details class=\"keys\">"));
    assert!(main.contains("Shift</kbd>+<kbd>N</kbd>"));
    assert!(!main.contains("Browse the library"));
    assert!(!main.contains("No books yet"));
}

#[tokio::test]
async fn home_of_an_empty_library_welcomes() {
    let lib = TestLibrary::new();
    lib.scan();
    let reply = lib.server("/", 100).ok("/").await;
    let main = reply.main();
    for part in [
        "No books yet.",
        "FB2",
        "EPUB",
        "plain text",
        "<form method=\"post\" action=\"/scan\">",
        "<details class=\"keys\">",
    ] {
        assert!(main.contains(part), "{part:?} missing in {main}");
    }
    for part in ["Recently added", "class=\"summary\"", "class=\"attention\""] {
        assert!(!main.contains(part), "{part}");
    }
}

#[tokio::test]
async fn home_notice_appears_only_with_unreadable_books() {
    let lib = TestLibrary::new();
    lib.write("good.fb2", &Book::new("Хорошая", "Гоголь", "Николай").fb2());
    lib.scan();
    let server = lib.server("/", 100);
    let main = server.ok("/").await.main().to_owned();
    assert!(!main.contains("class=\"attention\""), "{main}");
    assert!(main.contains("1 book · 1 author · "), "{main}");

    lib.write("locked.epub", &common::epub3_drm());
    lib.write("a.fb2", b"<broken");
    lib.write("b.fb2", b"<broken");
    lib.scan();
    let main = server.ok("/").await.main().to_owned();
    assert!(
        main.contains(
            "<a href=\"/errors\">2 books could not be read</a> · \
             <a href=\"/errors#protected\">1 book is protected by DRM</a> ›"
        ),
        "{main}"
    );
}

#[tokio::test]
async fn errors_page_lists_unreadable_books_by_relative_path() {
    let lib = library();
    lib.zip("packs/p.zip", &[("x/bad.fb2", b"<nope".to_vec())]);
    lib.write("drm/locked.epub", &common::epub3_drm());
    lib.scan();
    let server = lib.server("/", 100);
    let reply = server.ok("/errors").await;
    let main = reply.main();
    let root = lib.dir.path().display().to_string();
    assert!(!reply.body.contains(&root), "no absolute paths: {main}");
    let broken = lib.id_of("broken");
    assert!(
        main.contains(&format!(
            "<a class=\"path\" href=\"/book/{broken}\">broken.fb2</a> <span class=\"reason\">"
        )),
        "{main}"
    );
    assert!(main.contains(">packs/p.zip/x/bad.fb2</a>"), "{main}");
    let drm = &main[main.find("id=\"protected\"").expect("the DRM section")..];
    assert!(drm.contains(">drm/locked.epub</a> <span class=\"reason\">Adobe ADEPT</span>"));
    assert!(!drm.contains("broken.fb2"));
    assert!(main.contains("action=\"/scan\""));
    assert!(main.find("bad.fb2").unwrap() < main.find("id=\"protected\"").unwrap());

    // A library without unreadable books.
    let lib = TestLibrary::new();
    lib.write("good.fb2", &Book::new("Хорошая", "Гоголь", "Николай").fb2());
    lib.scan();
    let main = lib.server("/", 100).ok("/errors").await.main().to_owned();
    assert!(
        main.contains("Every book in the library could be read."),
        "{main}"
    );
    assert!(!main.contains("id=\"protected\""));
}

#[tokio::test]
async fn author_index_pages_and_filters() {
    let lib = library();
    let server = lib.server("/", 2);
    let first = server.ok("/authors").await;
    let main = first.main();
    assert!(main.contains("Гоголь Николай") && main.contains("Пушкин Александр"));
    assert!(!main.contains("Толстой"), "page 1 holds 2 of 4 authors");
    assert!(main.contains("Page 1 of 2"));
    assert!(main.contains("href=\"/authors?page=2\""));

    let second = server.ok("/authors?page=2").await;
    assert!(second.main().contains("Толстой Лев") && second.main().contains("Чехов Антон"));
    assert!(second.main().contains("href=\"/authors\" rel=\"prev\""));
    assert!(!second.main().contains("rel=\"next\""));

    // Out-of-range and invalid pages are clamped.
    assert!(
        server
            .ok("/authors?page=999")
            .await
            .main()
            .contains("Page 2 of 2")
    );
    for page in ["0", "-1", "x", "99999999999999999999999"] {
        let reply = server.ok(&format!("/authors?page={page}")).await;
        assert!(reply.main().contains("Page 1 of 2"), "{page}");
    }

    let filtered = server.ok("/authors?q=%D1%82%D0%BE%D0%BB").await; // "тол"
    assert!(filtered.main().contains("Толстой Лев"));
    assert!(!filtered.main().contains("Чехов"));
    assert!(!filtered.main().contains("class=\"pager\""));
    let none = server.ok("/authors?q=zzz").await;
    none.assert_has("No authors starting with “zzz”.");
}

#[tokio::test]
async fn author_page_groups_books_by_series() {
    let lib = library();
    let server = lib.server("/", 100);
    let tolstoy = server.ok("/authors?q=%D1%82%D0%BE%D0%BB").await;
    let href = tolstoy
        .main()
        .split("href=\"")
        .find_map(|s| s.split('"').next().filter(|u| u.starts_with("/author/")))
        .unwrap()
        .to_owned();
    let reply = server.ok(&href).await;
    let main = reply.main();
    assert!(main.contains("<h1>Толстой Лев</h1>"));
    assert!(main.contains("2 books"));
    let series = main.find("<li class=\"heading\">Эпопея</li>").unwrap();
    let other = main.find("<li class=\"heading\">Other books</li>").unwrap();
    let war = main.find("Война и мир").unwrap();
    let anna = main.find("Анна Каренина").unwrap();
    assert!(series < war && war < other && other < anna, "{main}");
    assert!(main.contains("Эпопея #1"));
    assert!(
        reply.sidebar().contains("class=\"node author open\""),
        "revealed"
    );

    for uri in ["/author/999", "/author/abc", "/author/0", "/author/-1"] {
        assert_eq!(server.get(uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn folder_pages_with_encoded_names_and_archives() {
    let lib = library();
    let server = lib.server("/", 100);
    let root = server.ok("/folder").await;
    let main = root.main();
    for name in ["a+b &#38; c#1 %", "zips", "Толстой Лев"] {
        assert!(main.contains(name), "{name} in {main}");
    }
    assert!(main.contains("href=\"/folder?path=a%2Bb%20%26%20c%231%20%25\""));

    let special = server.ok("/folder?path=a%2Bb%20%26%20c%231%20%25").await;
    assert!(special.main().contains("Капитанская дочка"));
    assert!(special.main().contains("<h1>a+b &#38; c#1 %</h1>"));

    let zips = server.ok("/folder?path=zips").await;
    let zip_href = "/folder?zip=zips/%D0%BA%D0%BE%D0%BB%D0%BB%D0%B5%D0%BA%D1%86%D0%B8%D1%8F.zip";
    assert!(zips.main().contains(zip_href), "{}", zips.main());
    let archive = server.ok(zip_href).await;
    assert!(archive.main().contains("Степь"));
    assert!(archive.main().contains("inner dir"));
    assert!(
        !archive.main().contains("Палата"),
        "inside the inner directory"
    );
    let inner = server.ok(&format!("{zip_href}&in=inner%20dir")).await;
    assert!(inner.main().contains("Палата № 6"));
    // Breadcrumbs: Library / zips / коллекция.zip / inner dir.
    let crumbs = inner.main();
    assert!(crumbs.contains("<a href=\"/folder\">Library</a>"));
    assert!(crumbs.contains("<a href=\"/folder?path=zips\">zips</a>"));
    assert!(crumbs.contains("<span aria-current=\"page\">inner dir</span>"));

    for uri in [
        "/folder?path=nope",
        "/folder?zip=nope.zip",
        "/folder?zip=zips/%D0%BA.zip&in=x",
    ] {
        assert_eq!(server.get(uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
    // A malformed query string is treated as empty (the root).
    assert_eq!(
        server.get("/folder?page=%FF%FE").await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn folder_books_are_paged() {
    let lib = library();
    let server = lib.server("/", 2);
    // The root has 3 books: broken.fb2 (an error book), svg-cover.fb2 and script.fb2.
    let first = server.ok("/folder").await;
    assert!(first.main().contains("Page 1 of 2"), "{}", first.main());
    let second = server.ok("/folder?page=2").await;
    assert!(second.main().contains("Page 2 of 2"));
    assert!(
        second.main().contains("class=\"folders\""),
        "folders on every page"
    );
}

#[tokio::test]
async fn book_card_shows_metadata_and_cover() {
    let lib = library();
    let server = lib.server("/", 100);
    let id = lib.id_of("Война и мир");
    let reply = server.ok(&format!("/book/{id}")).await;
    let main = reply.main();
    assert!(main.contains("<h1>Война и мир</h1>"));
    assert!(main.contains("Толстой Лев</a>"));
    assert!(main.contains("<dt>Series</dt><dd>Эпопея #1</dd>"));
    assert!(main.contains("<dt>Genres</dt><dd>prose</dd>"));
    assert!(main.contains("Первая строка аннотации"));
    assert!(main.contains("Толстой Лев/Война и мир.fb2"));
    assert!(!main.contains("class=\"notice"), "{main}");
    assert!(
        main.contains(&format!(
            "<a class=\"button primary\" href=\"/book/{id}/read\">Read</a>"
        )),
        "{main}"
    );

    let cover = main
        .split("src=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap()
        .to_owned();
    assert!(cover.starts_with(&format!("/cover/{id}?v=")), "{cover}");
    let image = server.ok(&cover).await;
    assert_eq!(image.header(header::CONTENT_TYPE), "image/png");
    assert_eq!(
        image.header(header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );
    assert!(image.bytes.starts_with(b"\x89PNG"));
    let etag = image.header(header::ETAG).to_owned();
    let cached = server
        .get_with(&format!("/cover/{id}"), &[("if-none-match", &etag)])
        .await;
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);
    assert_eq!(
        server
            .get(&format!("/cover/{id}"))
            .await
            .header(header::CACHE_CONTROL),
        "no-cache"
    );
}

#[tokio::test]
async fn covers_of_unknown_type_or_missing_are_not_served() {
    let lib = library();
    let server = lib.server("/", 100);
    let svg_cover = lib.id_of("Обложка");
    let card = server.ok(&format!("/book/{svg_cover}")).await;
    assert!(card.main().contains("cover placeholder"), "{}", card.main());
    let no_cover = lib.id_of("Анна Каренина");
    for id in [svg_cover, no_cover, 999] {
        let reply = server.get(&format!("/cover/{id}")).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{id}");
        assert!(reply.header(header::CONTENT_TYPE).starts_with("text/html"));
    }
}

#[tokio::test]
async fn book_card_statuses() {
    let lib = library();
    let server = lib.server("/", 100);

    let broken = lib.id_of("broken");
    assert_eq!(lib.status_of("broken"), BookStatus::Error);
    let reply = server.ok(&format!("/book/{broken}")).await;
    assert!(reply.main().contains("notice error"), "{}", reply.main());
    assert!(reply.main().contains("could not be read"));

    let gone = lib.id_of("Пропавшая");
    let reply = server.ok(&format!("/book/{gone}")).await;
    assert!(reply.main().contains("notice removed"));
    assert!(!reply.main().contains("notice missing"));

    // A file deleted since the last scan.
    let anna = lib.id_of("Анна Каренина");
    std::fs::remove_file(lib.dir.path().join("Толстой Лев/Анна Каренина.fb2")).unwrap();
    let reply = server.ok(&format!("/book/{anna}")).await;
    assert!(reply.main().contains("notice missing"));
    assert!(reply.main().contains("Rescan the library</button>"));

    for uri in ["/book/999", "/book/abc", "/book/"] {
        assert_eq!(server.get(uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn text_from_books_is_escaped() {
    let lib = library();
    let server = lib.server("/", 100);
    let id = lib.id_of("<script>alert(1)</script>");
    for uri in [format!("/book/{id}"), "/folder".to_owned()] {
        let reply = server.ok(&uri).await;
        assert!(!reply.has("<script>alert"), "{uri}");
        reply.assert_has("&#60;script&#62;alert(1)&#60;/script&#62;");
    }
    let reply = server.ok("/search?q=%3Cscript%3E").await;
    assert!(!reply.has("<script>alert"));
    assert!(!reply.has("value=\"<script>"));
}

#[tokio::test]
async fn search_results_and_paging() {
    let lib = library();
    let server = lib.server("/", 1);
    let reply = server.ok("/search?q=%D1%82%D0%BE%D0%BB%D1%81%D1%82").await; // "толст"
    reply.assert_has("value=\"толст\"");
    let main = reply.main();
    assert!(
        main.contains("Война и мир") ^ main.contains("Анна Каренина"),
        "one per page"
    );
    assert!(main.contains("Page 1") && main.contains("rel=\"next\""));
    let second = server
        .ok("/search?q=%D1%82%D0%BE%D0%BB%D1%81%D1%82&page=2")
        .await;
    assert!(second.main().contains("rel=\"prev\""));
    assert!(!second.main().contains("rel=\"next\""), "{}", second.main());
    let past = server
        .ok("/search?q=%D1%82%D0%BE%D0%BB%D1%81%D1%82&page=9")
        .await;
    past.assert_has("No more results.");

    // Regression: the largest page numbers used to wrap the SQL OFFSET to -1 (showing
    // the first results) and overflow the next-page number.
    for page in ["18446744073709551615", "9223372036854775808"] {
        let reply = server
            .ok(&format!(
                "/search?q=%D1%82%D0%BE%D0%BB%D1%81%D1%82&page={page}"
            ))
            .await;
        reply.assert_has("No more results.");
        assert!(!reply.main().contains("rel=\"next\""), "{page}");
    }

    server
        .ok("/search")
        .await
        .assert_has("Type words from a title");
    server
        .ok("/search?q=%20%20")
        .await
        .assert_has("Type words from a title");
    server
        .ok("/search?q=--%20%22")
        .await
        .assert_has("Nothing found.");
    server
        .ok("/search?q=zzzz")
        .await
        .assert_has("Nothing found.");
}

#[tokio::test]
async fn sidebar_fragments() {
    let lib = library();
    let server = lib.server("/", 2);
    let authors = server.ok("/sidebar/authors").await;
    assert!(
        authors.body.starts_with("<div class=\"listing\">"),
        "{}",
        authors.body
    );
    assert!(!authors.has("<html"), "a fragment, not a page");
    authors.assert_has("data-src=\"/sidebar/authors?page=2\"");
    authors.assert_has("href=\"/authors?page=2\"");
    let author_src = authors
        .body
        .split("data-src=\"")
        .find_map(|s| {
            s.split('"')
                .next()
                .filter(|u| u.starts_with("/sidebar/author/"))
        })
        .unwrap()
        .to_owned();
    let books = server.ok(&author_src).await;
    assert!(books.has("class=\"book\""), "{}", books.body);

    let filtered = server.ok("/sidebar/authors?q=%D1%87").await; // "ч"
    filtered.assert_has("Чехов Антон");
    assert!(!filtered.has("Толстой"));
    server
        .ok("/sidebar/authors?q=zzz")
        .await
        .assert_has("No authors.");

    let root = server.ok("/sidebar/folder").await;
    root.assert_has("data-src=\"/sidebar/folder?path=zips\"");
    let zip = server
        .ok("/sidebar/folder?zip=zips/%D0%BA%D0%BE%D0%BB%D0%BB%D0%B5%D0%BA%D1%86%D0%B8%D1%8F.zip")
        .await;
    zip.assert_has("class=\"node folder\"");
    zip.assert_has("Степь");
    server
        .ok("/sidebar/folder?path=nope")
        .await
        .assert_has("No books.");

    for uri in ["/sidebar/author/999", "/sidebar/author/x"] {
        assert_eq!(server.get(uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn sidebar_reveals_the_current_book() {
    let lib = library();
    let server = lib.server("/", 2);
    // Чехов is the 4th author: on page 2 of the author list.
    let id = lib.id_of("Палата № 6");
    let reply = server.ok(&format!("/book/{id}")).await;
    let sidebar = reply.sidebar();
    assert!(sidebar.contains("Page 2 of 2"), "{sidebar}");
    assert!(sidebar.contains("class=\"node author open\""));
    assert!(sidebar.contains(&format!("href=\"/book/{id}\" class=\"active\"")));

    let reply = server.get_with(&format!("/book/{id}"), &[FOLDERS]).await;
    let sidebar = reply.sidebar();
    assert_eq!(sidebar.matches("open\" data-src").count(), 3, "{sidebar}");
    assert!(sidebar.contains("class=\"node folder open\" data-src=\"/sidebar/folder?path=zips\""));
    assert!(sidebar.contains("class=\"node archive open\""));
    assert!(sidebar.contains(&format!("href=\"/book/{id}\" class=\"active\"")));
}

#[tokio::test]
async fn scan_status_and_json_rescan() {
    let lib = library();
    let server = lib.server("/", 100);
    let status = server.ok("/scan/status").await;
    assert_eq!(status.header(header::CONTENT_TYPE), "application/json");
    assert_eq!(status.header(header::CACHE_CONTROL), "no-store");
    let json: serde_json::Value = serde_json::from_str(&status.body).unwrap();
    assert_eq!(json["running"], false);

    let request = Request::post("/scan")
        .header(header::HOST, "127.0.0.1:8742")
        .header(header::ORIGIN, "http://127.0.0.1:8742")
        .header(header::ACCEPT, "application/json")
        .body(Body::empty())
        .unwrap();
    let reply = server.send(request).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED);
    let json: serde_json::Value = serde_json::from_str(&reply.body).unwrap();
    assert!(json["message"].is_string());

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while server.scan.is_running() {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let json: serde_json::Value =
        serde_json::from_str(&server.ok("/scan/status").await.body).unwrap();
    assert!(
        json["message"]
            .as_str()
            .unwrap()
            .starts_with("Scan finished"),
        "{json}"
    );
}

#[tokio::test]
async fn base_path_applies_to_pages_fragments_and_data_urls() {
    let lib = library();
    let server = lib.server("/books", 2);
    let reply = server.ok("/books/").await;
    for part in [
        "data-src=\"/books/scan/status\"",
        "data-cookie-path=\"/books/\"",
        "data-src=\"/books/sidebar/authors\"",
        "data-src=\"/books/sidebar/author/",
        "href=\"/books/author/",
        "action=\"/books/search\"",
        "href=\"/books/folder\" data-mode=\"folders\"",
    ] {
        reply.assert_has(part);
    }
    let fragment = server.ok("/books/sidebar/authors").await;
    fragment.assert_has("data-src=\"/books/sidebar/authors?page=2\"");
    let id = lib.id_of("Война и мир");
    let card = server.ok(&format!("/books/book/{id}")).await;
    card.assert_has(&format!("src=\"/books/cover/{id}?v="));
    assert_eq!(
        server.get("/sidebar/authors").await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn archives_mixing_formats_and_text_books() {
    let lib = TestLibrary::new();
    let long =
        "Строка текста без глав, которая повторяется очень много раз подряд. ".repeat(12_000);
    lib.zip(
        "mixed.zip",
        &[
            ("a.fb2", Book::new("Из FB2", "Чехов", "Антон").fb2()),
            ("b.epub", common::epub3()),
            ("Пушкин - Дубровский.txt", common::txt_libru().into_bytes()),
            ("long.txt", long.into_bytes()),
            ("readme.txt", common::txt_libru().into_bytes()),
        ],
    );
    lib.scan();
    let server = lib.server("/", 100);
    let archive = server.ok("/folder?zip=mixed.zip").await;
    let main = archive.main();
    for title in ["Из FB2", "Дубровский", "long"] {
        assert!(main.contains(title), "{title} in {main}");
    }
    assert!(!main.contains("readme"), "not a book: {main}");
    assert_eq!(main.matches("<li><a href=\"/book/").count(), 4, "{main}");

    let id = lib.id_of("Дубровский");
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(
        card.main().contains("<dt>Format</dt><dd>TXT</dd>"),
        "{}",
        card.main()
    );
    assert!(card.main().contains("Пушкин"));
    let read = server.ok(&format!("/book/{id}/read")).await;
    assert!(read.body.contains("ГЛАВА I."), "{}", read.body);

    // A long text without headings is read in pieces.
    let id = lib.id_of("long");
    let read = server.ok(&format!("/book/{id}/read")).await;
    assert!(read.body.contains("Строка текста без глав"));
    assert!(read.body.contains("(1/"), "titled pieces: {}", read.main());
}

/// Files that are not books leave a container row without a book; folders holding only
/// such files are not shown.
#[tokio::test]
async fn folders_with_only_non_books_are_not_listed() {
    let lib = TestLibrary::new();
    lib.write("docs/readme.txt", common::txt_libru().as_bytes());
    lib.write("mixed/readme.txt", common::txt_libru().as_bytes());
    lib.write("mixed/book.fb2", &Book::new("Книга", "Автор", "А").fb2());
    lib.scan();
    let server = lib.server("/", 100);
    let root = server.ok("/folder").await;
    assert!(root.main().contains("mixed"), "{}", root.main());
    assert!(!root.main().contains("docs"), "{}", root.main());
    let sidebar = server.ok("/sidebar/folder").await;
    assert!(!sidebar.body.contains("docs"), "{}", sidebar.body);
    let mixed = server.ok("/folder?path=mixed").await;
    assert_eq!(mixed.main().matches("<li><a href=\"/book/").count(), 1);
    assert!(!mixed.main().contains("readme"));
}
