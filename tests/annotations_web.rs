//! Annotations end to end: the write routes, the lists in the reader and on the book
//! card, the reading position, the Markdown export, re-attaching after a book changes
//! and the cleanup after a scan, on a scanned temporary library.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use shelfgauge::annotations::attach::{self, cleanup_after_scan, export_all};
use shelfgauge::annotations::export::{Exporter, MARKER};
use shelfgauge::annotations::store::{self, UserData};
use shelfgauge::config::{Config, SplitMode};
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::library::Library;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::{all_books, book_by_id};
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

const ORIGIN: &str = "http://127.0.0.1:8742";

/// A paragraph that is unique in the test books.
fn para(chapter: usize, n: usize) -> String {
    format!(
        "Paragraph {chapter}.{n}: the {} fox number {n} of chapter {chapter} jumps over the lazy dog.",
        ["quick", "brown", "sly", "red", "old", "grey"][n % 6]
    )
}

/// An FB2 book with `chapters` chapters of `paragraphs` paragraphs (see [`para`]),
/// optionally with a notes body and with paragraphs replaced by `edits`.
fn book(
    title: &str,
    author: &str,
    chapters: usize,
    notes: bool,
    edits: &[(usize, usize, &str)],
) -> Vec<u8> {
    let mut body = String::new();
    for c in 1..=chapters {
        body.push_str(&format!("<section><title><p>Chapter {c}</p></title>"));
        for n in 1..=4 {
            let text = edits
                .iter()
                .find(|(ec, en, _)| *ec == c && *en == n)
                .map_or_else(|| para(c, n), |(_, _, t)| (*t).to_owned());
            body.push_str(&format!("<p>{text}</p>"));
        }
        body.push_str("</section>");
    }
    let notes = if notes {
        r#"<body name="notes"><section id="n1"><title><p>1</p></title><p>A footnote text.</p></section></body>"#
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook xmlns:l="http://www.w3.org/1999/xlink"><description><title-info>
<author><first-name>Anna</first-name><last-name>{author}</last-name></author><book-title>{title}</book-title>
</title-info></description><body>{body}</body>{notes}</FictionBook>"#
    )
    .into_bytes()
}

struct TestLibrary {
    dir: tempfile::TempDir,
    library: Library,
    userdata: Arc<UserData>,
    exporter: Exporter,
}

impl TestLibrary {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        let userdata = Arc::new(UserData::open(&library.userdata_path()).unwrap());
        let exporter = Exporter::new(library.annotations_dir(), "http://127.0.0.1:8742/");
        Self {
            dir,
            library,
            userdata,
            exporter,
        }
    }

    fn path(&self, path: &str) -> PathBuf {
        self.dir.path().join(path)
    }

    fn write(&self, path: &str, data: &[u8]) {
        std::fs::write(self.path(path), data).unwrap();
    }

    fn write_zip(&self, path: &str, entry: &str, data: &[u8]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(self.path(path)).unwrap());
        zip.start_file(entry, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(data).unwrap();
        zip.finish().unwrap();
    }

    /// A complete scan followed by the annotation cleanup (as `shelfgauge scan` does).
    fn scan(&self) {
        self.scan_only();
        let index = open_writer(&self.library.db_path()).unwrap();
        cleanup_after_scan(
            &index,
            &self.userdata.lock(),
            &self.exporter,
            30,
            shelfgauge::annotations::now(),
        )
        .unwrap();
    }

    fn scan_only(&self) {
        let mut conn = open_writer(&self.library.db_path()).unwrap();
        let registry = Registry::with_builtin_formats();
        let summary = Scanner {
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
        assert!(!summary.cancelled);
    }

    /// The id of the live book at `path` (a file or an archive).
    fn id_of(&self, path: &str) -> i64 {
        let conn = open_writer(&self.library.db_path()).unwrap();
        all_books(&conn)
            .unwrap()
            .into_iter()
            .filter(|b| b.status != shelfgauge::storage::books::BookStatus::Removed)
            .map(|b| book_by_id(&conn, b.id).unwrap().unwrap())
            .find(|b| b.container_path == path)
            .unwrap_or_else(|| panic!("no live book at {path}"))
            .id
    }

    fn key_of(&self, id: i64) -> Vec<u8> {
        let conn = open_writer(&self.library.db_path()).unwrap();
        let book = book_by_id(&conn, id).unwrap().unwrap();
        attach::snapshot(&book).unwrap().key
    }

    fn server(&self, config: impl FnOnce(&mut Config)) -> Server {
        let mut c = Config::default();
        config(&mut c);
        c.validate().unwrap();
        let pool = open_read_pool(&self.library.db_path(), 2).unwrap();
        let scan = Arc::new(BackgroundScan::new(self.library.clone(), Vec::new()));
        let state = AppState::new(self.library.clone(), &c, pool, Arc::clone(&scan))
            .with_annotations(Arc::clone(&self.userdata), self.exporter.clone());
        Server {
            router: web::router(state),
            scan,
        }
    }

    fn export_files(&self) -> Vec<String> {
        let mut names: Vec<String> = match std::fs::read_dir(self.library.annotations_dir()) {
            Ok(entries) => entries
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    fn export_text(&self) -> String {
        let files = self.export_files();
        assert_eq!(files.len(), 1, "{files:?}");
        std::fs::read_to_string(self.library.annotations_dir().join(&files[0])).unwrap()
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

#[derive(Debug)]
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("{e}: {}", self.body))
    }

    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }
}

impl Server {
    async fn send(&self, request: Request<Body>) -> Reply {
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

    async fn get(&self, uri: &str) -> Reply {
        let request = Request::get(uri)
            .header(header::HOST, "127.0.0.1:8742")
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    async fn ok(&self, uri: &str) -> String {
        let reply = self.get(uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
        reply.body
    }

    async fn post(
        &self,
        uri: &str,
        content_type: &str,
        body: String,
        origin: Option<&str>,
    ) -> Reply {
        let mut request = Request::post(uri)
            .header(header::HOST, "127.0.0.1:8742")
            .header(header::CONTENT_TYPE, content_type);
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        self.send(request.body(Body::from(body)).unwrap()).await
    }

    async fn json(&self, uri: &str, body: serde_json::Value) -> Reply {
        self.post(uri, "application/json", body.to_string(), Some(ORIGIN))
            .await
    }

    async fn form(&self, uri: &str, fields: &[(&str, &str)]) -> Reply {
        let body = fields
            .iter()
            .map(|(k, v)| format!("{k}={}", utf8_percent_encode(v, NON_ALPHANUMERIC)))
            .collect::<Vec<_>>()
            .join("&");
        self.post(uri, "application/x-www-form-urlencoded", body, Some(ORIGIN))
            .await
    }

    /// Creates a mark of `len` code points from `offset` in block `block`.
    async fn mark(&self, id: i64, block: &str, offset: u32, len: u32, note: &str) -> String {
        let reply = self
            .json(
                &format!("/book/{id}/annotations"),
                serde_json::json!({
                    "start_block": block, "start_offset": offset,
                    "end_block": block, "end_offset": offset + len, "note": note,
                }),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.json()["id"].as_str().unwrap().to_owned()
    }

    async fn bookmark(&self, id: i64, block: &str) -> String {
        let reply = self
            .json(
                &format!("/book/{id}/annotations"),
                serde_json::json!({ "block": block }),
            )
            .await;
        assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
        reply.json()["id"].as_str().unwrap().to_owned()
    }

    /// The block id (`b12`) of the block whose text starts with `text`, from the whole
    /// book rendered on one page.
    async fn block_of(&self, id: i64, text: &str) -> String {
        let mut part = 1;
        loop {
            let body = self.ok(&format!("/book/{id}/read?part={part}")).await;
            if let Some(at) = body.find(&format!("\">{text}")) {
                let start = body[..at].rfind("id=\"").unwrap() + 4;
                return body[start..at].to_owned();
            }
            assert!(body.contains("rel=\"next\""), "{text:?} not found");
            part += 1;
        }
    }
}

#[tokio::test]
async fn marks_bookmarks_notes_and_export() {
    let lib = TestLibrary::new();
    lib.write("book.fb2", &book("The Book", "Writer", 3, false, &[]));
    lib.scan();
    let id = lib.id_of("book.fb2");
    let server = lib.server(|_| {});
    let p12 = server.block_of(id, &para(1, 2)).await;
    let p21 = server.block_of(id, &para(2, 1)).await;

    // "Paragraph 1.2: the sly fox" → "the sly fox" (offset 15, 11 code points).
    let mark = server.mark(id, &p12, 15, 11, "<b>bold</b> & note").await;
    let bookmark = server.bookmark(id, &p21).await;

    // The reader's lists (server-rendered, so they work without JS).
    let read = server.ok(&format!("/book/{id}/read")).await;
    assert!(read.contains("id=\"tab-bookmarks\"") && read.contains("id=\"tab-marks\""));
    assert!(
        read.contains("class=\"add-bookmark\"")
            && read.contains("Bookmark this place (or press <kbd>b</kbd>)")
    );
    assert!(read.contains(&format!("data-id=\"{mark}\"")), "{read}");
    assert!(read.contains("data-text=\"the sly fox\""), "{read}");
    assert!(
        read.contains("&#60;b&#62;bold&#60;/b&#62; &#38; note"),
        "note escaped: {read}"
    );
    assert!(!read.contains("<b>bold</b>"));
    assert!(read.contains(&format!("href=\"/book/{id}/read?at={p12}&#38;from=mark\"")));
    assert!(read.contains(&format!("href=\"/book/{id}/read?at={p21}&#38;from=mark\"")));
    assert!(
        read.contains("Chapter 2 — Paragraph 2.1: the brown fox"),
        "bookmark label"
    );
    assert!(read.contains("data-max-text=\"10000\" data-max-note=\"10000\""));
    assert!(read.contains("assets/reader.js?v=") && read.contains("assets/reader.css?v="));
    let fragment = server.ok(&format!("/book/{id}/annotations")).await;
    assert!(
        fragment.starts_with("<div class=\"annotation-lists\">"),
        "{fragment}"
    );

    // The book card.
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(
        card.contains("Bookmarks (1)") && card.contains("Quotes (1)"),
        "{card}"
    );
    assert!(
        card.contains("title=\"Open in the book\">the sly fox</a>"),
        "{card}"
    );
    assert!(
        card.contains("<p class=\"note\">&#60;b&#62;bold"),
        "the note is shown as text"
    );
    assert!(
        card.contains("<details class=\"note-edit\">"),
        "the form is folded away"
    );
    assert!(
        card.contains(">&#60;b&#62;bold&#60;/b&#62; &#38; note</textarea>"),
        "{card}"
    );
    assert!(!card.contains("<b>bold"));
    assert!(card.contains(&format!("action=\"/annotation/{mark}/note\"")));
    assert!(card.contains(&format!("href=\"/book/{id}/read?at={p12}&#38;from=mark\"")));

    // The export: bookmarks first, the quote with its note and a `from=mark` link.
    let md = lib.export_text();
    assert!(md.starts_with(MARKER));
    assert!(
        md.contains("# The Book") && md.contains("Writer Anna"),
        "{md}"
    );
    assert!(md.find("## Bookmarks").unwrap() < md.find("## Quotes").unwrap());
    assert!(
        md.contains("### Chapter 1\n\n> the sly fox\n\n<b>bold</b> & note\n"),
        "{md}"
    );
    assert!(md.contains(&format!(
        "http://127.0.0.1:8742/book/{id}/read?at={p12}&from=mark"
    )));
    assert!(lib.export_files()[0].starts_with("Writer Anna — The Book ("));

    // Edit the note with a form: back to the given page.
    let back = format!("/book/{id}#annotations");
    let reply = server
        .form(
            &format!("/annotation/{mark}/note"),
            &[("note", "Better note"), ("back", &back)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), back);
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(card.contains(">Better note</textarea>"));
    assert!(lib.export_text().contains("Better note"));
    // A foreign `back` is ignored.
    let reply = server
        .form(
            &format!("/annotation/{mark}/note"),
            &[("note", "x"), ("back", "https://evil.example/")],
        )
        .await;
    assert_eq!(reply.location(), format!("/book/{id}#annotations"));
    // JSON note edit.
    let reply = server
        .json(
            &format!("/annotation/{mark}/note"),
            serde_json::json!({"note": "JSON note"}),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json()["label"], "JSON note");

    // Delete: hidden everywhere, the export follows.
    let reply = server
        .json(&format!("/annotation/{mark}/delete"), serde_json::json!({}))
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.body);
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(card.contains("Quotes (0)") && card.contains("Bookmarks (1)"));
    assert!(!lib.export_text().contains("## Quotes"));
    let reply = server
        .form(
            &format!("/annotation/{bookmark}/delete"),
            &[("back", &back)],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(lib.export_files().is_empty(), "no file without annotations");
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(card.contains("No bookmarks yet") && card.contains("No quotes yet"));
    // Gone: 404 (JSON for the reader).
    let reply = server
        .json(&format!("/annotation/{mark}/delete"), serde_json::json!({}))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert!(reply.json()["error"].is_string());
    let reply = server
        .json("/annotation/not-a-uuid/delete", serde_json::json!({}))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_requests_are_rejected() {
    let lib = TestLibrary::new();
    lib.write("book.fb2", &book("The Book", "Writer", 3, false, &[]));
    lib.scan();
    let id = lib.id_of("book.fb2");
    let server = lib.server(|c| {
        c.annotations.max_text_length = 20;
        c.annotations.max_note_length = 5;
    });
    let p11 = server.block_of(id, &para(1, 1)).await;
    let p12 = server.block_of(id, &para(1, 2)).await;
    let p21 = server.block_of(id, &para(2, 1)).await;
    let url = format!("/book/{id}/annotations");
    let create = |s: &str, so: u32, e: &str, eo: u32, note: &str| serde_json::json!({"start_block": s, "start_offset": so, "end_block": e, "end_offset": eo, "note": note});
    for (body, expected) in [
        (create(&p12, 5, &p11, 1, ""), "ends before it starts"),
        (create(&p11, 5, &p11, 5, ""), "empty"),
        (create("b9999", 0, "b9999", 1, ""), "not a text block"),
        (create(&p11, 0, &p11, 9999, ""), "outside block"),
        (create(&p11, 0, &p11, 21, ""), "the limit is 20"),
        (create(&p11, 0, &p11, 3, "toolong"), "the limit is 5"),
        (create(&p11, 0, &p21, 3, ""), "within one part"),
        (create("12", 0, &p11, 3, ""), "invalid block id"),
        (serde_json::json!({"start_block": p11}), "missing"),
        (serde_json::json!({"block": "b9999"}), "not a text block"),
    ] {
        let reply = server.json(&url, body.clone()).await;
        assert_eq!(
            reply.status,
            StatusCode::UNPROCESSABLE_ENTITY,
            "{body}: {}",
            reply.body
        );
        let error = reply.json()["error"].as_str().unwrap().to_owned();
        assert!(error.contains(expected), "{body}: {error}");
    }
    // Exactly at the limits is fine.
    let reply = server.json(&url, create(&p11, 0, &p11, 20, "12345")).await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    // Malformed JSON, unknown books, too large bodies.
    let reply = server
        .post(&url, "application/json", "{".to_owned(), Some(ORIGIN))
        .await;
    assert!(reply.status.is_client_error(), "{}", reply.status);
    let reply = server
        .json("/book/999/annotations", serde_json::json!({"block": p11}))
        .await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    let huge = serde_json::json!({"block": p11, "note": "x".repeat(200_000)});
    let reply = server.json(&url, huge).await;
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{}",
        reply.body
    );
    // A note over the limit through the card's form: an error page.
    let mark = store::for_book(&lib.userdata.lock(), &lib.key_of(id)).unwrap()[0]
        .id
        .clone();
    let reply = server
        .form(&format!("/annotation/{mark}/note"), &[("note", "toolong")])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert!(reply.body.contains("the limit is 5"), "{}", reply.body);
}

#[tokio::test]
async fn write_routes_need_an_allowed_origin() {
    let lib = TestLibrary::new();
    lib.write("book.fb2", &book("The Book", "Writer", 1, false, &[]));
    lib.scan();
    let id = lib.id_of("book.fb2");
    let server = lib.server(|_| {});
    let uuid = "67e55044-10b1-426f-9247-bb680e5fe0c8";
    for uri in [
        format!("/book/{id}/annotations"),
        format!("/annotation/{uuid}/note"),
        format!("/annotation/{uuid}/delete"),
        format!("/book/{id}/position"),
    ] {
        for origin in [None, Some("http://evil.example"), Some("null")] {
            let reply = server
                .post(
                    &uri,
                    "application/json",
                    r#"{"block":"b2"}"#.to_owned(),
                    origin,
                )
                .await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri} {origin:?}");
        }
    }
    assert!(
        store::for_book(&lib.userdata.lock(), &lib.key_of(id))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn from_mark_survives_the_redirect() {
    let lib = TestLibrary::new();
    lib.write("book.fb2", &book("The Book", "Writer", 3, false, &[]));
    lib.scan();
    let id = lib.id_of("book.fb2");
    let server = lib.server(|_| {});
    let p21 = server.block_of(id, &para(2, 1)).await;
    let reply = server
        .get(&format!("/book/{id}/read?at={p21}&from=mark"))
        .await;
    assert!(reply.status.is_redirection());
    assert_eq!(
        reply.location(),
        format!("/book/{id}/read?part=2&from=mark#{p21}")
    );
    let reply = server.get(&format!("/book/{id}/read?at={p21}")).await;
    assert_eq!(reply.location(), format!("/book/{id}/read?part=2#{p21}"));
    // The reader drops `from` on prev/next: reading on saves the position again.
    let page = server
        .ok(&format!("/book/{id}/read?part=2&from=mark"))
        .await;
    assert!(
        page.contains(&format!("href=\"/book/{id}/read?part=3\" rel=\"next\"")),
        "{page}"
    );
}

#[tokio::test]
async fn reading_position() {
    let lib = TestLibrary::new();
    lib.write("book.fb2", &book("The Book", "Writer", 3, true, &[]));
    lib.scan();
    let id = lib.id_of("book.fb2");
    let server = lib.server(|_| {});
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(card.contains(">Read</a>") && !card.contains("Continue reading"));

    let p21 = server.block_of(id, &para(2, 1)).await;
    let url = format!("/book/{id}/position");
    let reply = server.json(&url, serde_json::json!({"block": p21})).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.body);
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(
        card.contains(&format!(
            "href=\"/book/{id}/read?at={p21}\">Continue reading"
        )),
        "{card}"
    );
    assert!(card.contains(&format!("href=\"/book/{id}/read\">Read from the start")));
    // 1 title + 4 paragraphs of chapter 1 before it, of 15 main blocks.
    assert!(card.contains("read 3"), "{card}");

    // Notes are never a position.
    let note = server.block_of(id, "A footnote text.").await;
    let reply = server.json(&url, serde_json::json!({"block": note})).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(
        card.contains(&format!("read?at={p21}\">Continue")),
        "{card}"
    );
    // The last block of the main text: finished.
    let last = server.block_of(id, &para(3, 4)).await;
    server.json(&url, serde_json::json!({"block": last})).await;
    assert!(server.ok(&format!("/book/{id}")).await.contains("finished"));
    // Invalid blocks.
    let reply = server
        .json(&url, serde_json::json!({"block": "b9999"}))
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);

    // Another split setting: block ids, and so the position, stay.
    let whole = lib.server(|c| c.reader.split = SplitMode::None);
    let card = whole.ok(&format!("/book/{id}")).await;
    assert!(
        card.contains(&format!("read?at={last}\">Continue")),
        "{card}"
    );

    // `remember_position = false`: hidden, and nothing is saved.
    let off = lib.server(|c| c.reader.remember_position = false);
    let card = off.ok(&format!("/book/{id}")).await;
    assert!(!card.contains("Continue reading") && !card.contains("finished"));
    let read = off.ok(&format!("/book/{id}/read")).await;
    assert!(read.contains("data-position=\"\""), "{read}");
    off.json(&url, serde_json::json!({"block": p21})).await;
    let saved = store::position(&lib.userdata.lock(), &lib.key_of(id))
        .unwrap()
        .unwrap();
    assert_eq!(saved.block.to_string(), last, "not saved while disabled");
}

#[tokio::test]
async fn annotations_follow_moves_and_reattach_across_containers() {
    let lib = TestLibrary::new();
    let data = book("The Book", "Writer", 3, false, &[]);
    lib.write("book.fb2", &data);
    lib.scan();
    let id = lib.id_of("book.fb2");
    let server = lib.server(|_| {});
    let p32 = server.block_of(id, &para(3, 2)).await;
    server.mark(id, &p32, 15, 7, "kept").await;
    server
        .json(
            &format!("/book/{id}/position"),
            serde_json::json!({"block": p32}),
        )
        .await;

    // Renamed: same content key, same id, nothing to do.
    std::fs::rename(lib.path("book.fb2"), lib.path("renamed.fb2")).unwrap();
    lib.scan();
    assert_eq!(lib.id_of("renamed.fb2"), id);
    assert!(
        server
            .ok(&format!("/book/{id}"))
            .await
            .contains("Quotes (1)")
    );

    // Moved into an archive: a new id and content key; the card re-attaches the
    // orphans by title and authors.
    lib.write_zip("archive.zip", "inner/book.fb2", &data);
    std::fs::remove_file(lib.path("renamed.fb2")).unwrap();
    lib.scan();
    let zip_id = lib.id_of("archive.zip");
    assert_ne!(zip_id, id);
    assert_eq!(lib.export_files().len(), 1, "the orphan keeps its file");
    assert!(lib.export_text().contains("Place in the book not found"));
    let card = server.ok(&format!("/book/{zip_id}")).await;
    assert!(card.contains("Quotes (1)"), "{card}");
    assert!(
        card.contains(&format!("read?at={p32}\">Continue")),
        "position follows: {card}"
    );
    assert!(!card.contains("place not found"));
    let marks = store::for_book(&lib.userdata.lock(), &lib.key_of(zip_id)).unwrap();
    assert_eq!(marks[0].quote.text, "the sly");
    assert_eq!(marks[0].book_id, zip_id);
    let md = lib.export_text();
    assert!(!md.contains("Place in the book not found"), "{md}");
    assert!(md.contains(&format!("/book/{zip_id}/read?at={p32}&from=mark")));
}

#[tokio::test]
async fn replaced_books_reattach_above_the_threshold_only() {
    let lib = TestLibrary::new();
    lib.write("book.fb2", &book("The Book", "Writer", 2, false, &[]));
    // Same title and authors, different content (so a different content key).
    lib.write("other.fb2", &book("The Book", "Writer", 3, false, &[]));
    lib.scan();
    let id = lib.id_of("book.fb2");
    let server = lib.server(|_| {});
    // Five marks, one per paragraph of chapter 1 and the first of chapter 2.
    let mut blocks = Vec::new();
    for (c, n) in [(1, 1), (1, 2), (1, 3), (1, 4), (2, 1)] {
        let block = server.block_of(id, &para(c, n)).await;
        // Long enough to be found without its context (which the edit changes).
        server.mark(id, &block, 0, 20, "").await;
        blocks.push(block);
    }
    let card = server.ok(&format!("/book/{id}")).await;
    assert!(card.contains("Quotes (5)"));

    // Replaced in place with a new edition: a paragraph inserted at the start (the
    // offsets move) and one marked paragraph rewritten: 4 of 5 = 80 % are found.
    let edition = |edits: &[(usize, usize, &str)]| {
        let data = book("The Book", "Writer", 2, false, edits);
        String::from_utf8(data)
            .unwrap()
            .replacen(
                "<title><p>Chapter 1</p></title>",
                "<title><p>Chapter 1</p></title><p>Preface added.</p>",
                1,
            )
            .into_bytes()
    };
    lib.write("book.fb2", &edition(&[(1, 3, "Completely rewritten.")]));
    lib.scan();
    assert_eq!(lib.id_of("book.fb2"), id, "replaced in place: same id");
    let read = server.ok(&format!("/book/{id}/read")).await;
    assert_eq!(read.matches("class=\"mark-item\"").count(), 5, "{read}");
    assert_eq!(read.matches("data-lost").count(), 1);
    assert!(read.contains("place not found"));
    let marks = store::for_book(&lib.userdata.lock(), &lib.key_of(id)).unwrap();
    let found: Vec<_> = marks.iter().filter(|m| !m.anchor_lost).collect();
    assert_eq!(found.len(), 4);
    // The found marks moved one block down.
    let shifted: Vec<String> = found
        .iter()
        .map(|m| m.anchor.start_block.to_string())
        .collect();
    assert!(!shifted.contains(&blocks[0]), "{shifted:?}");
    let lost = marks.iter().find(|m| m.anchor_lost).unwrap();
    assert!(
        lost.quote.text.starts_with("Paragraph 1.3"),
        "{}",
        lost.quote.text
    );
    assert_eq!(
        lost.anchor.start_block.ordinal(),
        1,
        "start of its old section"
    );

    // Below the threshold nothing moves: the other copy (same title and authors)
    // replaced by a different text keeps its marks orphaned.
    let other = lib.id_of("other.fb2");
    let p = server.block_of(other, &para(1, 1)).await;
    for offset in [0, 20, 40] {
        server.mark(other, &p, offset, 5, "").await;
    }
    let old_key = lib.key_of(other);
    lib.write(
        "other.fb2",
        &book(
            "The Book",
            "Writer",
            2,
            false,
            &[(1, 1, "Nothing like before.")],
        ),
    );
    lib.scan();
    let card = server.ok(&format!("/book/{other}")).await;
    assert!(card.contains("Quotes (0)"), "{card}");
    assert_eq!(
        store::for_book(&lib.userdata.lock(), &old_key)
            .unwrap()
            .len(),
        3,
        "kept"
    );
    // ...and it is not attached to the first book either (same title and authors).
    let read = server.ok(&format!("/book/{id}/read")).await;
    assert_eq!(read.matches("class=\"mark-item\"").count(), 5);
}

#[tokio::test]
async fn removed_books_are_cleaned_up_restored_and_purged() {
    let lib = TestLibrary::new();
    let data = book("Gone Book", "Writer", 1, false, &[]);
    lib.write("gone.fb2", &data);
    lib.write("stay.fb2", &book("Stay", "Writer", 1, false, &[]));
    lib.scan();
    let id = lib.id_of("gone.fb2");
    let key = lib.key_of(id);
    let server = lib.server(|_| {});
    let p = server.block_of(id, &para(1, 1)).await;
    let kept = server.mark(id, &p, 0, 9, "").await;
    let tomb = server.mark(id, &p, 10, 3, "").await;
    server
        .json(&format!("/annotation/{tomb}/delete"), serde_json::json!({}))
        .await;
    assert_eq!(lib.export_files().len(), 1);

    // A scan without the cleanup (a cancelled one) changes nothing.
    std::fs::remove_file(lib.path("gone.fb2")).unwrap();
    lib.scan_only();
    assert_eq!(
        store::for_book(&lib.userdata.lock(), &key).unwrap().len(),
        1
    );

    // The cleanup after a complete scan: soft-deleted, the export file goes.
    lib.scan();
    assert!(
        store::for_book(&lib.userdata.lock(), &key)
            .unwrap()
            .is_empty()
    );
    assert!(lib.export_files().is_empty());

    // The book comes back: restored.
    std::fs::create_dir_all(lib.path("back")).unwrap();
    lib.write("back/gone.fb2", &data);
    lib.scan();
    let restored = store::for_book(&lib.userdata.lock(), &key).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, kept);
    assert_eq!(lib.export_files().len(), 1);

    // Removed while a book with the same title and authors exists (a move awaiting
    // re-attaching): kept.
    std::fs::remove_dir_all(lib.path("back")).unwrap();
    lib.write(
        "edition.fb2",
        &book("Gone Book", "Writer", 1, false, &[(1, 4, "New.")]),
    );
    lib.scan();
    assert_eq!(
        store::for_book(&lib.userdata.lock(), &key).unwrap().len(),
        1
    );

    // Without it: removed, then purged after the retention period.
    std::fs::remove_file(lib.path("edition.fb2")).unwrap();
    lib.scan();
    assert!(
        store::for_book(&lib.userdata.lock(), &key)
            .unwrap()
            .is_empty()
    );
    let index = open_writer(&lib.library.db_path()).unwrap();
    let now = shelfgauge::annotations::now();
    let summary = cleanup_after_scan(
        &index,
        &lib.userdata.lock(),
        &lib.exporter,
        30,
        now + 29 * 86_400,
    )
    .unwrap();
    assert_eq!(summary.purged, 0);
    let summary = cleanup_after_scan(
        &index,
        &lib.userdata.lock(),
        &lib.exporter,
        30,
        now + 31 * 86_400,
    )
    .unwrap();
    assert_eq!(summary.purged, 1);
    let user = lib.userdata.lock();
    assert!(store::get(&user, &kept).unwrap().is_none());
    assert!(
        store::get(&user, &tomb).unwrap().is_some(),
        "user tombstones are never purged"
    );
}

#[tokio::test]
async fn zero_retention_purges_at_once() {
    let lib = TestLibrary::new();
    lib.write("gone.fb2", &book("Gone", "Writer", 1, false, &[]));
    lib.scan();
    let id = lib.id_of("gone.fb2");
    let server = lib.server(|_| {});
    let p = server.block_of(id, &para(1, 1)).await;
    let mark = server.mark(id, &p, 0, 9, "").await;
    std::fs::remove_file(lib.path("gone.fb2")).unwrap();
    lib.scan_only();
    let index = open_writer(&lib.library.db_path()).unwrap();
    let summary = cleanup_after_scan(
        &index,
        &lib.userdata.lock(),
        &lib.exporter,
        0,
        shelfgauge::annotations::now(),
    )
    .unwrap();
    assert_eq!((summary.removed, summary.purged), (1, 1));
    assert!(store::get(&lib.userdata.lock(), &mark).unwrap().is_none());
}

#[tokio::test]
async fn export_all_rewrites_files_and_removes_only_marked_ones() {
    let lib = TestLibrary::new();
    lib.write("a.fb2", &book("Book A", "Writer", 1, false, &[]));
    lib.write("b.fb2", &book("Book B", "Writer", 1, false, &[]));
    lib.scan();
    let server = lib.server(|_| {});
    for path in ["a.fb2", "b.fb2"] {
        let id = lib.id_of(path);
        let p = server.block_of(id, &para(1, 1)).await;
        server.mark(id, &p, 0, 9, "").await;
        server.bookmark(id, &p).await;
    }
    let dir = lib.library.annotations_dir();
    let files = lib.export_files();
    assert_eq!(files.len(), 2);
    std::fs::remove_file(dir.join(&files[0])).unwrap();
    std::fs::write(dir.join(&files[1]), "edited by hand").unwrap();
    std::fs::write(dir.join("stale (deadbeef).md"), format!("{MARKER}\n\nold")).unwrap();
    std::fs::write(dir.join("mine.md"), "my own notes").unwrap();

    let index = open_writer(&lib.library.db_path()).unwrap();
    let summary = export_all(&index, &lib.userdata.lock(), &lib.exporter).unwrap();
    assert_eq!(
        (summary.files, summary.annotations, summary.removed),
        (2, 4, 1)
    );
    let mut expected = files.clone();
    expected.push("mine.md".to_owned());
    expected.sort();
    assert_eq!(lib.export_files(), expected);
    assert!(
        std::fs::read_to_string(dir.join(&files[1]))
            .unwrap()
            .starts_with(MARKER)
    );

    // The same through the command line.
    std::fs::remove_file(dir.join(&files[1])).unwrap();
    drop(index);
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_shelfgauge"))
        .args(["annotations", "export"])
        .arg(lib.dir.path())
        .env_remove("RUST_LOG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Exported 4 annotations of 2 books"),
        "{stdout}"
    );
    assert_eq!(lib.export_files(), expected);
}

#[tokio::test]
async fn currently_reading_list() {
    let lib = TestLibrary::new();
    for name in ["a", "b", "c"] {
        lib.write(
            &format!("{name}.fb2"),
            &book(&format!("Book {name}"), "Writer", 2, false, &[]),
        );
    }
    lib.write("gone.fb2", &book("Book gone", "Writer", 2, false, &[]));
    lib.scan();
    let ids: Vec<i64> = ["a", "b", "c", "gone"]
        .iter()
        .map(|n| lib.id_of(&format!("{n}.fb2")))
        .collect();
    let server = lib.server(|c| c.library.reading_list_size = 2);
    let home = server.ok("/").await;
    assert!(!home.contains("Currently reading"), "no positions, no list");

    // Positions through the route, then made older or newer in the store.
    let p = server.block_of(ids[0], &para(1, 2)).await;
    for (i, at) in ids.iter().zip([10, 30, 20, 40]) {
        let reply = server
            .json(
                &format!("/book/{i}/position"),
                serde_json::json!({"block": p}),
            )
            .await;
        assert_eq!(reply.status, StatusCode::NO_CONTENT);
        lib.userdata
            .lock()
            .execute(
                "UPDATE reading_positions SET updated_at = ?2 WHERE book_key = ?1",
                rusqlite::params![lib.key_of(*i), at],
            )
            .unwrap();
    }
    std::fs::remove_file(lib.path("gone.fb2")).unwrap();
    lib.scan_only(); // Removed from the index (its position is not cleaned up yet).

    let home = server.ok("/").await;
    assert!(home.contains("Currently reading"), "{home}");
    let at = |title: &str| home.find(&format!(">{title}</a>"));
    // Most recent first, limited to 2; the removed book is not listed.
    assert!(at("Book b").unwrap() < at("Book c").unwrap(), "{home}");
    assert!(at("Book a").is_none() && at("Book gone").is_none());
    assert!(home.contains(&format!(
        "href=\"/book/{}/read?at={p}\">Continue</a>",
        ids[1]
    )));
    assert!(
        home.contains("read 1") && home.contains("1970-01-01"),
        "{home}"
    );

    // Mark as finished: off the list, "finished" on the card.
    let reply = server
        .form(&format!("/book/{}/finished", ids[1]), &[])
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/");
    let home = server.ok("/").await;
    assert!(
        !home.contains(">Book b</a>") && home.contains(">Book a</a>"),
        "{home}"
    );
    assert!(
        server
            .ok(&format!("/book/{}", ids[1]))
            .await
            .contains("finished")
    );

    // Clear the list: the positions stay on the cards.
    let reply = server.form("/reading/clear", &[]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(!server.ok("/").await.contains("Currently reading"));
    let card = server.ok(&format!("/book/{}", ids[2])).await;
    assert!(card.contains("Continue reading"), "{card}");
    // Reading again puts the book back.
    let p2 = server.block_of(ids[2], &para(2, 1)).await;
    server
        .json(
            &format!("/book/{}/position", ids[2]),
            serde_json::json!({"block": p2}),
        )
        .await;
    assert!(server.ok("/").await.contains(">Book c</a>"));

    // Writes need the Origin check.
    for uri in [
        format!("/book/{}/finished", ids[2]),
        "/reading/clear".to_owned(),
    ] {
        let reply = server
            .post(
                &uri,
                "application/x-www-form-urlencoded",
                String::new(),
                None,
            )
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri}");
    }
    assert!(server.ok("/").await.contains(">Book c</a>"));

    // Turned off: by size 0 or by not remembering positions.
    let none = lib.server(|c| c.library.reading_list_size = 0);
    assert!(!none.ok("/").await.contains("Currently reading"));
    let off = lib.server(|c| c.reader.remember_position = false);
    assert!(!off.ok("/").await.contains("Currently reading"));
}
