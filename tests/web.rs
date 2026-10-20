//! Web server tests: security checks, base_path, assets, error pages and startup.

mod common;

use std::io::{Read, Write};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use http_body_util::BodyExt;
use shelfgauge::config::Config;
use shelfgauge::library::Library;
use shelfgauge::scanner::BackgroundScan;
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState, Auth, WebError};
use tower::ServiceExt;

struct TestServer {
    _dir: tempfile::TempDir,
    scan: Arc<BackgroundScan>,
    router: Router,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.scan.shutdown();
    }
}

fn server(base_path: &str) -> TestServer {
    let mut config = Config::default();
    config.server.base_path = base_path.to_owned();
    config.server.allowed_hosts = vec!["books.example.org".to_owned()];
    config.validate().unwrap();
    server_with(config)
}

fn server_with(config: Config) -> TestServer {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open(dir.path()).unwrap();
    open_writer(&library.db_path()).unwrap();
    let pool = open_read_pool(&library.db_path(), 2).unwrap();
    let scan = Arc::new(BackgroundScan::new(library.clone(), Vec::new()));
    let state = AppState::new(library, &config, pool, Arc::clone(&scan));
    TestServer {
        _dir: dir,
        scan,
        router: common::local(web::router(state)),
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl TestServer {
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
        self.send(request(Method::GET, uri, &[])).await
    }
}

const HOST: &str = "127.0.0.1:8742";

fn request(method: Method, uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method(method).uri(uri);
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        builder = builder.header(header::HOST, HOST);
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(Body::empty()).unwrap()
}

fn header(reply: &Reply, name: header::HeaderName) -> &str {
    reply
        .headers
        .get(&name)
        .unwrap_or_else(|| panic!("no {name} header"))
        .to_str()
        .unwrap()
}

#[tokio::test]
async fn library_page_renders_the_layout() {
    let server = server("/");
    let reply = server.get("/").await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        header(&reply, header::CONTENT_TYPE),
        "text/html; charset=utf-8"
    );
    for part in [
        "<header class=\"topbar\">",
        "id=\"sidebar\"",
        "<main class=\"main\">",
        "No books yet.",
        "href=\"/assets/style.css?v=",
        "src=\"/assets/app.js?v=",
        "action=\"/scan\"",
        "<div class=\"sidebar-resizer\" role=\"separator\"",
    ] {
        assert!(
            reply.body.contains(part),
            "{part} missing in:\n{}",
            reply.body
        );
    }
}

#[tokio::test]
async fn loopback_hosts_are_served() {
    let server = server("/");
    for host in ["127.0.0.1:8742", "localhost:8742", "[::1]:8742"] {
        let reply = server
            .send(request(Method::GET, "/", &[("host", host)]))
            .await;
        assert_eq!(reply.status, StatusCode::OK, "{host}");
    }
}

/// Behaviour change of 0.16.0: a request through an `allowed_hosts` name comes from
/// another device (a reverse proxy on this computer connects from loopback). Without
/// `auth = "password"` it is refused with a hint, on every path; with it, it needs a
/// login. Before, it was served without one.
#[tokio::test]
async fn allowed_hosts_need_password_auth() {
    let server = server("/");
    for uri in [
        "/",
        "/profiles",
        "/login",
        "/assets/style.css",
        "/no/such/page",
    ] {
        let reply = server
            .send(request(Method::GET, uri, &[("host", "books.example.org")]))
            .await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri}");
        assert!(
            reply.body.contains("auth = \"password\"") && reply.body.contains("set-password"),
            "{}",
            reply.body
        );
    }

    let mut config = Config::default();
    config.server.allowed_hosts = vec!["books.example.org".to_owned()];
    config.server.auth = shelfgauge::config::AuthMode::Password;
    let server = server_with(config);
    let reply = server
        .send(request(Method::GET, "/", &[("host", "books.example.org")]))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(header(&reply, header::LOCATION), "/login?next=/");
    // This computer is unchanged.
    let reply = server
        .send(request(Method::GET, "/", &[("host", "127.0.0.1:8742")]))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn rebinding_hosts_are_rejected_everywhere() {
    let server = server("/");
    for uri in ["/", "/no/such/page", "/assets/style.css"] {
        for host in [
            "evil.example:8742",
            "evil.example",
            "localhost:80",
            "127.0.0.1",
        ] {
            let reply = server
                .send(request(Method::GET, uri, &[("host", host)]))
                .await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri} {host}");
            assert!(reply.body.contains("allowed_hosts"), "{}", reply.body);
            assert!(!reply.body.contains("evil"), "the host is not echoed back");
        }
    }
}

#[tokio::test]
async fn request_without_host_is_rejected() {
    let server = server("/");
    let request = Request::builder().uri("/").body(Body::empty()).unwrap();
    assert_eq!(server.send(request).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn state_changing_requests_need_an_allowed_origin() {
    let server = server("/");
    for origin in [
        None,
        Some("null"),
        Some("http://evil.example"),
        Some("http://localhost:8743"),
    ] {
        let headers: Vec<(&str, &str)> = origin.iter().map(|o| ("origin", *o)).collect();
        let reply = server.send(request(Method::POST, "/scan", &headers)).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{origin:?}");
    }
    assert!(
        !server.scan.is_running(),
        "a rejected request starts no scan"
    );

    for origin in ["http://127.0.0.1:8742", "https://books.example.org"] {
        let reply = server
            .send(request(Method::POST, "/scan", &[("origin", origin)]))
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{origin}");
        assert_eq!(header(&reply, header::LOCATION), "/");
    }
}

#[tokio::test]
async fn origin_is_checked_on_unknown_paths_too() {
    let server = server("/");
    let reply = server
        .send(request(Method::POST, "/no/such/page", &[]))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    let reply = server
        .send(request(
            Method::DELETE,
            "/",
            &[("origin", "http://evil.example")],
        ))
        .await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn get_requests_need_no_origin() {
    let server = server("/");
    let reply = server
        .send(request(
            Method::GET,
            "/",
            &[("origin", "http://evil.example")],
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn base_path_prefixes_routes_and_links() {
    let server = server("/library/books/");
    let reply = server.get("/library/books/").await;
    assert_eq!(reply.status, StatusCode::OK);
    for part in [
        "href=\"/library/books/assets/style.css?v=",
        "src=\"/library/books/assets/app.js?v=",
        "action=\"/library/books/scan\"",
        "href=\"/library/books/\"",
    ] {
        assert!(
            reply.body.contains(part),
            "{part} missing in:\n{}",
            reply.body
        );
    }
    assert!(!reply.body.contains("\"/assets/"), "{}", reply.body);

    let reply = server.get("/library/books").await;
    assert_eq!(reply.status, StatusCode::TEMPORARY_REDIRECT);
    assert_eq!(header(&reply, header::LOCATION), "/library/books/");

    for uri in [
        "/",
        "/assets/style.css",
        "/scan",
        "/library",
        "/library/books/x",
    ] {
        assert_eq!(server.get(uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
    let asset = server.get("/library/books/assets/style.css").await;
    assert_eq!(asset.status, StatusCode::OK);

    let reply = server
        .send(request(
            Method::POST,
            "/library/books/scan",
            &[("origin", "http://127.0.0.1:8742")],
        ))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(header(&reply, header::LOCATION), "/library/books/");
}

#[tokio::test]
async fn not_found_page_uses_the_layout_and_base_path() {
    let server = server("/books");
    let reply = server.get("/books/no/such/page").await;
    assert_eq!(reply.status, StatusCode::NOT_FOUND);
    assert_eq!(
        header(&reply, header::CONTENT_TYPE),
        "text/html; charset=utf-8"
    );
    assert!(reply.body.contains("Not found"), "{}", reply.body);
    assert!(reply.body.contains("href=\"/books/assets/style.css?v="));
    assert!(reply.body.contains("href=\"/books/\""));
    assert!(!reply.body.contains("Rescan the library</button>"));
}

#[tokio::test]
async fn wrong_method_is_not_allowed() {
    let server = server("/");
    assert_eq!(
        server.get("/scan").await.status,
        StatusCode::METHOD_NOT_ALLOWED
    );
}

#[tokio::test]
async fn assets_have_type_and_cache_headers() {
    let server = server("/");
    let page = server.get("/").await;
    let css_url = page
        .body
        .split("href=\"")
        .find_map(|s| s.split('"').next().filter(|u| u.contains("style.css")))
        .unwrap()
        .to_owned();

    let reply = server.get(&css_url).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        header(&reply, header::CONTENT_TYPE),
        "text/css; charset=utf-8"
    );
    assert_eq!(
        header(&reply, header::CACHE_CONTROL),
        "public, max-age=31536000, immutable"
    );
    assert!(reply.body.contains(".sidebar"));
    let etag = header(&reply, header::ETAG).to_owned();

    let reply = server.get("/assets/style.css?v=0000000000000000").await;
    assert_eq!(header(&reply, header::CACHE_CONTROL), "no-cache");
    let reply = server.get("/assets/style.css").await;
    assert_eq!(header(&reply, header::CACHE_CONTROL), "no-cache");

    for if_none_match in [
        etag.as_str(),
        &format!("W/{etag}"),
        &format!("\"x\", {etag}"),
    ] {
        let reply = server
            .send(request(
                Method::GET,
                "/assets/style.css",
                &[("if-none-match", if_none_match)],
            ))
            .await;
        assert_eq!(reply.status, StatusCode::NOT_MODIFIED, "{if_none_match}");
        assert!(reply.body.is_empty());
    }
    let reply = server
        .send(request(
            Method::GET,
            "/assets/style.css",
            &[("if-none-match", "\"other\"")],
        ))
        .await;
    assert_eq!(reply.status, StatusCode::OK);

    let reply = server.get("/assets/app.js").await;
    assert_eq!(
        header(&reply, header::CONTENT_TYPE),
        "text/javascript; charset=utf-8"
    );
    let reply = server.get("/assets/icon.svg").await;
    assert_eq!(header(&reply, header::CONTENT_TYPE), "image/svg+xml");
}

#[tokio::test]
async fn unknown_assets_are_not_found() {
    let server = server("/");
    for uri in [
        "/assets/missing.css",
        "/assets/../Cargo.toml",
        "/assets/%2e%2e/Cargo.toml",
        "/assets/",
    ] {
        assert_eq!(server.get(uri).await.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn security_headers_on_every_response() {
    let server = server("/");
    for (uri, host) in [
        ("/", HOST),
        ("/no/such/page", HOST),
        ("/assets/style.css", HOST),
        ("/", "evil.example"),
    ] {
        let reply = server
            .send(request(Method::GET, uri, &[("host", host)]))
            .await;
        assert_eq!(header(&reply, header::X_CONTENT_TYPE_OPTIONS), "nosniff");
        assert_eq!(header(&reply, header::X_FRAME_OPTIONS), "DENY");
        assert!(
            header(&reply, header::CONTENT_SECURITY_POLICY).contains("default-src 'self'"),
            "{uri}"
        );
    }
}

#[test]
fn busy_port_is_a_clear_error() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let mut config = Config::default();
    config.server.port = taken.local_addr().unwrap().port();
    let err = web::bind(&config.server, Auth::None).unwrap_err();
    assert!(matches!(err, WebError::PortInUse(_)), "{err:?}");
    let message = err.to_string();
    assert!(
        message.contains(&format!("port {} ", config.server.port)),
        "{message}"
    );
    assert!(message.contains("--port"), "{message}");
    assert!(message.contains("server.port"), "{message}");
}

#[test]
fn non_loopback_bind_is_refused_before_listening() {
    let mut config = Config::default();
    for bind in ["0.0.0.0", "::"] {
        config.server.bind = bind.parse().unwrap();
        let err = web::bind(&config.server, Auth::None).unwrap_err();
        assert!(matches!(err, WebError::UnsafeBind(_)), "{bind}: {err:?}");
        assert!(err.to_string().contains("loopback"), "{err}");
    }
}

#[test]
fn display_url() {
    let mut config = Config::default();
    config.server.base_path = "/books".to_owned();
    config.validate().unwrap();
    let urls = web::Urls::new(&config.server.base_path);
    assert_eq!(
        web::display_url(&config.server, &urls),
        "http://127.0.0.1:8742/books/"
    );
    config.server.bind = "::1".parse().unwrap();
    assert_eq!(
        web::display_url(&config.server, &urls),
        "http://[::1]:8742/books/"
    );
}

/// The real server over TCP: serves a request and stops on the shutdown signal.
#[tokio::test(flavor = "multi_thread")]
async fn serves_over_tcp_until_shutdown() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = Config::default();
    config.server.port = port;
    let test = server_with(config);
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(web::serve(listener, test.router.clone(), async {
        let _ = stopped.await;
    }));

    let response = tokio::task::spawn_blocking(move || {
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        write!(
            stream,
            "GET / HTTP/1.1\r\nHost: localhost:{port}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    })
    .await
    .unwrap();
    assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
    assert!(response.contains("No books yet."), "{response}");

    stop.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .expect("the server stops after the shutdown signal")
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn rescan_indexes_in_the_background() {
    let server = server("/");
    let root = server._dir.path();
    std::fs::copy("tests/fixtures/fb2/minimal.fb2", root.join("book.fb2")).unwrap();
    let reply = server
        .send(request(
            Method::POST,
            "/scan",
            &[("origin", "http://localhost:8742")],
        ))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while server.scan.is_running() {
        assert!(
            std::time::Instant::now() < deadline,
            "the scan did not finish"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let reply = server.get("/").await;
    assert!(reply.body.contains("1 book · "), "{}", reply.body);
    assert!(!reply.body.contains("Scanning"), "{}", reply.body);
}

#[test]
fn background_scan_refuses_to_start_after_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open(dir.path()).unwrap();
    let scan = BackgroundScan::new(library, Vec::new());
    scan.shutdown();
    assert!(!scan.start(shelfgauge::scanner::ScanTrigger::Rescan));
    assert!(!scan.is_running());
}
