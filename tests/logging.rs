//! Logging end to end: request spans and end events, the never-logged rule (queries,
//! bodies, credentials), spans carried to blocking and parser threads, the JSON format
//! and untrusted names that try to forge log lines.

mod common;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode, header};
use common::Capture;
use http_body_util::BodyExt;
use shelfgauge::config::Config;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::library::Library;
use shelfgauge::logging::LogFormat;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::all_books;
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

/// An FB2 book with unbalanced tags: parsing it logs "repaired unbalanced tags".
const UNBALANCED: &[u8] = include_bytes!("fixtures/fb2/unbalanced.fb2");

const HOST: &str = "localhost:8742";
const ORIGIN: &str = "http://localhost:8742";

/// A scanned library with one book, and its app.
struct TestServer {
    _dir: tempfile::TempDir,
    router: Router,
    scan: Arc<BackgroundScan>,
    book: i64,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.scan.shutdown();
    }
}

/// A library with the book `data` at `path` (or in `zip` under the entry `path`).
fn server(path: &str, zip: Option<&str>, data: &[u8]) -> TestServer {
    // Before anything logs: see `common::Quiet`.
    drop(Capture::default().dispatch("off", LogFormat::Text, false));
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open(dir.path()).unwrap();
    match zip {
        Some(zip) => {
            let file = std::fs::File::create(dir.path().join(zip)).unwrap();
            let mut out = zip::ZipWriter::new(file);
            out.start_file(path, zip::write::SimpleFileOptions::default())
                .unwrap();
            out.write_all(data).unwrap();
            out.finish().unwrap();
        }
        None => std::fs::write(dir.path().join(path), data).unwrap(),
    }
    let mut conn = open_writer(&library.db_path()).unwrap();
    let registry = Registry::with_builtin_formats();
    Scanner {
        library: &library,
        exclude: &[],
        registry: &registry,
        reread: false,
        limits: ReadLimits::default(),
        threads: 1,
        progress: &ScanProgress::default(),
        cancel: &AtomicBool::new(false),
    }
    .run(&mut conn)
    .unwrap();
    let books = all_books(&conn).unwrap();
    assert_eq!(books.len(), 1);
    let config = Config::default();
    let pool = open_read_pool(&library.db_path(), 2).unwrap();
    let scan = Arc::new(BackgroundScan::new(library.clone(), Vec::new()));
    // No request of these tests is logged as slow, even on a slow machine.
    let state = AppState::new(library, &config, pool, Arc::clone(&scan))
        .with_slow_request(std::time::Duration::from_secs(60));
    TestServer {
        _dir: dir,
        router: web::router(state),
        scan,
        book: books[0].id,
    }
}

impl TestServer {
    async fn send(&self, request: Request<Body>) -> (StatusCode, String) {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    async fn get(&self, uri: &str) -> (StatusCode, String) {
        self.send(request(Method::GET, uri, &[], Body::empty()))
            .await
    }

    async fn post(&self, uri: &str, content_type: &str, body: String) -> (StatusCode, String) {
        let headers = [
            (header::ORIGIN.as_str(), ORIGIN),
            (header::CONTENT_TYPE.as_str(), content_type),
        ];
        self.send(request(Method::POST, uri, &headers, Body::from(body)))
            .await
    }
}

fn request(method: Method, uri: &str, headers: &[(&str, &str)], body: Body) -> Request<Body> {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, HOST);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body).unwrap()
}

#[tokio::test]
async fn requests_log_their_route_template_status_and_latency() {
    let server = server("book.fb2", None, UNBALANCED);
    let capture = Capture::default();
    let _log = tracing::dispatcher::set_default(&capture.dispatch(
        "shelfgauge=debug",
        LogFormat::Text,
        false,
    ));
    assert_eq!(
        server.get(&format!("/book/{}", server.book)).await.0,
        StatusCode::OK
    );
    assert_eq!(server.get("/no/such/page").await.0, StatusCode::NOT_FOUND);

    let log = capture.text();
    let card = log
        .lines()
        .find(|l| l.contains("route=/book/{id}}") && l.contains(" status=200 "))
        .unwrap_or_else(|| panic!("no end event of the book card in\n{log}"));
    assert!(card.contains(" DEBUG request{id="), "{card}");
    assert!(card.contains(" method=GET route=/book/{id}}"), "{card}");
    assert!(card.contains(" latency_ms="), "{card}");
    assert!(
        log.lines()
            .any(|l| l.contains("route=-}") && l.contains(" status=404 ")),
        "{log}"
    );
    assert!(!log.contains("/no/such/page"), "{log}");
    assert!(
        !log.contains('\x1b'),
        "no colours when not on a terminal:\n{log}"
    );
}

/// Requests with secrets in the query, the body and the credential headers, at the
/// most verbose levels and in both formats, log none of them.
#[tokio::test]
async fn queries_bodies_and_credentials_are_never_logged() {
    for format in [LogFormat::Text, LogFormat::Json] {
        for filter in ["info,shelfgauge=trace", "trace"] {
            let server = server("book.fb2", None, UNBALANCED);
            let capture = Capture::default();
            let _log = tracing::dispatcher::set_default(&capture.dispatch(filter, format, true));
            let book = server.book;

            let (status, _) = server.get(&format!("/book/{book}/find?q=SECRETfind")).await;
            assert_eq!(status, StatusCode::OK);
            server
                .get(&format!("/book/{book}/read?find=SECRETread&hit=1"))
                .await;
            server.get("/search?q=SECRETsearch").await;
            let (status, created) = server
                .post(
                    &format!("/book/{book}/annotations"),
                    "application/json",
                    r#"{"block":"b2","note":"SECRETnote"}"#.to_owned(),
                )
                .await;
            assert_eq!(status, StatusCode::CREATED, "{created}");
            let created: serde_json::Value = serde_json::from_str(&created).unwrap();
            let annotation = created["id"].as_str().unwrap();
            server
                .post(
                    &format!("/annotation/{annotation}/note"),
                    "application/x-www-form-urlencoded",
                    "note=SECRETedit".to_owned(),
                )
                .await;
            server
                .post(
                    "/settings",
                    "application/x-www-form-urlencoded",
                    "library.page_size=SECRETform".to_owned(),
                )
                .await;
            server
                .send(request(
                    Method::GET,
                    "/",
                    &[
                        (header::AUTHORIZATION.as_str(), "Bearer SECRETauth"),
                        (header::COOKIE.as_str(), "sg=SECRETcookie"),
                    ],
                    Body::empty(),
                ))
                .await;

            let log = capture.text();
            assert!(
                log.contains("/book/{id}/find") && log.contains("/annotation/{id}/note"),
                "the requests are logged ({format:?}, {filter}):\n{log}"
            );
            assert!(
                !log.contains("SECRET"),
                "a secret is logged ({format:?}, {filter}):\n{log}"
            );
        }
    }
}

/// Lines logged on the blocking thread of `AppState::db`/`user_db` and on the parser
/// thread carry the request span.
#[tokio::test]
async fn blocking_and_parser_threads_keep_the_request_span() {
    let server = server("book.fb2", None, UNBALANCED);
    let capture = Capture::default();
    let _log = tracing::dispatcher::set_default(&capture.dispatch(
        "shelfgauge=debug",
        LogFormat::Text,
        true,
    ));
    // `user_db`, then the parser thread: the book is parsed for the first time.
    let (status, _) = server
        .post(
            &format!("/book/{}/annotations", server.book),
            "application/json",
            r#"{"block":"b2"}"#.to_owned(),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    // `db`: the statistics, timed by their span.
    assert_eq!(server.get("/stats").await.0, StatusCode::OK);

    let log = capture.text();
    let repaired = log
        .lines()
        .find(|l| l.contains("repaired unbalanced tags"))
        .unwrap_or_else(|| panic!("no parser line in\n{log}"));
    assert!(
        repaired.contains("request{id=") && repaired.contains("route=/book/{id}/annotations}"),
        "{repaired}"
    );
    assert!(repaired.contains("book{path=\"book.fb2\"}"), "{repaired}");
    let stats = log
        .lines()
        .find(|l| l.contains("library_stats: ") && l.contains("close"))
        .unwrap_or_else(|| panic!("no timing of the statistics in\n{log}"));
    assert!(stats.contains("route=/stats}"), "{stats}");
    assert!(stats.contains("time.busy="), "{stats}");
}

#[tokio::test]
async fn json_logs_are_one_object_per_line_with_the_span_chain() {
    let server = server("book.fb2", None, UNBALANCED);
    let capture = Capture::default();
    let _log = tracing::dispatcher::set_default(&capture.dispatch(
        "shelfgauge=debug",
        LogFormat::Json,
        true,
    ));
    server.get(&format!("/book/{}/read", server.book)).await;
    server.get("/missing").await;

    let log = capture.text();
    assert!(log.lines().count() >= 3, "{log}");
    let mut routes = Vec::new();
    for line in log.lines() {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert!(value.is_object(), "{line}");
        assert!(
            value["level"].is_string() && value["fields"].is_object(),
            "{line}"
        );
        if let Some(status) = value["fields"]["status"].as_u64() {
            let spans = value["spans"].as_array().unwrap();
            assert_eq!(spans[0]["name"], "request", "{line}");
            routes.push((spans[0]["route"].as_str().unwrap().to_owned(), status));
        }
    }
    assert_eq!(
        routes,
        [("/book/{id}/read".to_owned(), 200), ("-".to_owned(), 404)]
    );
    assert!(
        log.contains("\"message\":\"repaired unbalanced tags\""),
        "{log}"
    );
}

/// Archive entry names are untrusted: a newline in one must not start a second log
/// line in the `text` format. (Header values cannot hold a newline: `http` refuses it.)
#[tokio::test]
async fn untrusted_values_cannot_forge_log_lines() {
    let server = server("a\nINFO forged entry.fb2", Some("shelf.zip"), UNBALANCED);
    let capture = Capture::default();
    let _log = tracing::dispatcher::set_default(&capture.dispatch(
        "shelfgauge=debug",
        LogFormat::Text,
        false,
    ));
    let (status, _) = server.get(&format!("/book/{}/read", server.book)).await;
    assert_eq!(status, StatusCode::OK);
    let log = capture.text();
    assert!(
        log.contains(r#"path="shelf.zip/a\nINFO forged entry.fb2""#),
        "the entry name is escaped:\n{log}"
    );
    for line in log.lines() {
        assert!(
            line.starts_with(|c: char| c.is_ascii_digit()),
            "a line that is not a log event: {line:?}\n{log}"
        );
    }
}
