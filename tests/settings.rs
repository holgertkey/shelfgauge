//! The settings page end to end: the config form and its file edits, validation,
//! conflicts, the live effect of a save, `[scan]` notices with the rescan that follows,
//! maintenance actions and the facts about the library, on a scanned temporary library.

mod common;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use shelfgauge::annotations::attach::{self, cleanup_library};
use shelfgauge::annotations::export::Exporter;
use shelfgauge::annotations::store::{self, UserData};
use shelfgauge::config::edit::{FIELDS, FieldValue};
use shelfgauge::config::{Config, ConfigOverrides, DEFAULT_CONFIG_TOML, LiveConfig};
use shelfgauge::container::ReadLimits;
use shelfgauge::library::Library;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::{BookStatus, all_books, book_by_id};
use shelfgauge::storage::scans::last_scan;
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

/// The tests that override the port with `--port 9100` reach it there.
const PORT_9100: ConfigOverrides = ConfigOverrides {
    port: Some(9100),
    bind: None,
};

/// An FB2 book by `author` with three chapters.
fn fb2(title: &str, author: &str) -> Vec<u8> {
    let mut body = String::new();
    for c in 1..=3 {
        body.push_str(&format!(
            "<section><title><p>Chapter {c}</p></title><p>The text of chapter {c} of {title}, \
             long enough to be a paragraph of a real book.</p></section>"
        ));
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook><description><title-info><author><last-name>{author}</last-name></author>
<book-title>{title}</book-title></title-info></description><body>{body}</body></FictionBook>"#
    )
    .into_bytes()
}

struct Harness {
    dir: tempfile::TempDir,
    library: Library,
    userdata: Arc<UserData>,
    live: LiveConfig,
    scan: Arc<BackgroundScan>,
    router: Router,
    /// The `Host` of requests (`127.0.0.1:<port>`).
    host: String,
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.scan.shutdown();
    }
}

/// A library with three FB2 books (one in `drafts/`) and a TXT book, `config.toml`
/// with `config_text`, scanned once, served as `shelfgauge` serves it.
fn harness(config_text: &str, overrides: ConfigOverrides) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let library = Library::open(dir.path()).unwrap();
    std::fs::write(dir.path().join("a.fb2"), fb2("Alpha", "Adams")).unwrap();
    std::fs::write(dir.path().join("b.fb2"), fb2("Beta", "Brown")).unwrap();
    std::fs::create_dir(dir.path().join("drafts")).unwrap();
    std::fs::write(dir.path().join("drafts/c.fb2"), fb2("Gamma", "Clark")).unwrap();
    std::fs::write(dir.path().join("pride.txt"), common::txt_gutenberg()).unwrap();
    std::fs::write(library.config_path(), config_text).unwrap();

    let mut config = Config::load_or_create(&library.config_path())
        .unwrap()
        .config;
    config.apply_overrides(&overrides).unwrap();
    {
        let mut conn = open_writer(&library.db_path()).unwrap();
        let registry = config.scan.registry();
        Scanner {
            library: &library,
            exclude: &config.scan.exclude,
            registry: &registry,
            reread: false,
            limits: ReadLimits::default(),
            threads: 2,
            progress: &ScanProgress::default(),
            cancel: &AtomicBool::new(false),
        }
        .run(&mut conn)
        .unwrap();
    }
    let userdata = Arc::new(UserData::open(&library.userdata_path()).unwrap());
    let exporter = Exporter::new(library.annotations_dir(), "http://127.0.0.1:8742/");
    let live = LiveConfig::new(config.clone());
    let scan = {
        let (library, userdata, exporter, live) = (
            library.clone(),
            Arc::clone(&userdata),
            exporter.clone(),
            live.clone(),
        );
        Arc::new(
            BackgroundScan::new(library.clone(), Vec::new())
                .with_config(live.clone())
                .after_complete_scan(move || {
                    let retention = live.get().annotations.removed_book_retention_days;
                    cleanup_library(&library, &userdata, &exporter, retention).unwrap();
                }),
        )
    };
    let pool = open_read_pool(&library.db_path(), 2).unwrap();
    let state = AppState::new(library.clone(), &config, pool, Arc::clone(&scan))
        .with_config(live.clone(), overrides)
        .with_annotations(Arc::clone(&userdata), exporter);
    Harness {
        dir,
        library,
        userdata,
        host: format!("127.0.0.1:{}", config.server.port),
        live,
        scan,
        router: web::router(state),
    }
}

#[derive(Debug)]
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: String,
}

impl Reply {
    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }

    /// The `rev` of the config form.
    fn rev(&self) -> String {
        let start = self
            .body
            .find(r#"name="rev" value=""#)
            .expect("a config form")
            + 18;
        self.body[start..start + 16].to_owned()
    }

    /// The `<main>` element.
    fn main(&self) -> &str {
        let start = self.body.find("<main").unwrap();
        &self.body[start..]
    }
}

impl Harness {
    /// The server's own origin.
    fn origin(&self) -> String {
        format!("http://{}", self.host)
    }

    fn path(&self, path: &str) -> PathBuf {
        self.dir.path().join(path)
    }

    fn config_file(&self) -> String {
        std::fs::read_to_string(self.library.config_path()).unwrap()
    }

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
            .header(header::HOST, &self.host)
            .body(Body::empty())
            .unwrap();
        self.send(request).await
    }

    async fn ok(&self, uri: &str) -> Reply {
        let reply = self.get(uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
        reply
    }

    async fn post(&self, uri: &str, fields: &[(String, String)], origin: Option<&str>) -> Reply {
        let body = fields
            .iter()
            .map(|(k, v)| {
                format!(
                    "{}={}",
                    utf8_percent_encode(k, NON_ALPHANUMERIC),
                    utf8_percent_encode(v, NON_ALPHANUMERIC)
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let mut request = Request::post(uri)
            .header(header::HOST, &self.host)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        self.send(request.body(Body::from(body)).unwrap()).await
    }

    /// Posts `fields` as a form from this server's own pages.
    async fn form(&self, uri: &str, fields: &[(&str, &str)]) -> Reply {
        let fields: Vec<(String, String)> = fields
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        self.post(uri, &fields, Some(&self.origin())).await
    }

    /// The settings form as the browser submits it for `config`, with `rev`.
    fn submission(config: &Config, rev: &str) -> Vec<(String, String)> {
        let mut fields = vec![("rev".to_owned(), rev.to_owned())];
        for field in FIELDS {
            match field.get(config) {
                FieldValue::Bool(false) => {}
                FieldValue::Bool(true) => fields.push((field.key.to_owned(), "true".to_owned())),
                FieldValue::List(list) if field.key == "scan.formats" => {
                    for format in list {
                        fields.push((field.key.to_owned(), format));
                    }
                }
                FieldValue::List(list) => {
                    fields.push((field.key.to_owned(), list.join("\r\n")));
                }
                value => fields.push((field.key.to_owned(), value.to_string())),
            }
        }
        fields
    }

    /// Opens the settings page and saves it with `change` applied to the form values of
    /// the running config; returns the reply.
    async fn save(&self, change: impl FnOnce(&mut Vec<(String, String)>)) -> Reply {
        let rev = self.ok("/settings").await.rev();
        let mut fields = Self::submission(&self.live.get(), &rev);
        change(&mut fields);
        self.post("/settings", &fields, Some(&self.origin())).await
    }

    async fn wait_for_scan(&self) {
        for _ in 0..500 {
            if !self.scan.is_running() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("the scan did not finish");
    }

    fn live_book(&self, path: &str) -> Option<i64> {
        let conn = open_writer(&self.library.db_path()).unwrap();
        all_books(&conn)
            .unwrap()
            .into_iter()
            .filter(|b| b.status != BookStatus::Removed)
            .map(|b| book_by_id(&conn, b.id).unwrap().unwrap())
            .find(|b| b.container_path == path)
            .map(|b| b.id)
    }
}

/// Sets the form field `key` to `value` (replacing all its values).
fn set(fields: &mut Vec<(String, String)>, key: &str, value: &str) {
    fields.retain(|(k, _)| k != key);
    fields.push((key.to_owned(), value.to_owned()));
}

const COMMENTED: &str = "# My library.\n\n[server]\nport = 8742\n\n[library]\n\
                         # Short pages.\npage_size = 100 # entries\nmystery = \"kept\"\n";

#[tokio::test]
async fn page_shows_the_file_values_and_the_running_server() {
    let h = harness(
        "[server]\nallowed_hosts = [\"<b>&x.example\"]\n\n[scan]\nexclude = [\"<i>\\\"q\\\"&\"]\n\
         typo = 1\n",
        PORT_9100,
    );
    let reply = h.ok("/settings").await;
    let main = reply.main();
    for text in [
        r#"name="library.page_size" value="100""#,
        "Number of entries per sidebar/list page.",
        "Default: 100",
        r#"value="section" checked"#,
        "most HTML files are not books",
        "--port overrides 8742 from config.toml",
        "<li>&#60;b&#62;&#38;x.example</li>",
        "&#60;i&#62;&#34;q&#34;&#38;</textarea>",
        "<code>scan.typo</code>",
        shelfgauge::VERSION,
        "Last scan",
        r#"<a href="/settings?sizes=1#about">Measure</a>"#,
    ] {
        assert!(main.contains(text), "{text}\n{main}");
    }
    assert!(!main.contains("<b>&x"), "{main}");
    assert!(!main.contains("config.toml was changed outside"), "{main}");
    // The gear in the top bar, in library and reading mode.
    assert!(
        reply
            .body
            .contains(r#"class="icon-button settings-link" href="/settings""#)
    );
    let id = h.live_book("a.fb2").unwrap();
    let read = h.ok(&format!("/book/{id}/read")).await;
    assert!(read.body.contains(r#"href="/settings""#));
    // Cache sizes on request.
    let sizes = h.ok("/settings?sizes=1").await;
    assert!(sizes.main().contains(" in 0 files"), "{}", sizes.main());
}

#[tokio::test]
async fn saving_edits_the_file_and_applies_at_once() {
    let h = harness(COMMENTED, ConfigOverrides::default());
    let id = h.live_book("a.fb2").unwrap();
    let read = h.ok(&format!("/book/{id}/read")).await;
    assert!(read.body.contains(r#"rel="next""#), "split by sections");
    assert!(
        read.body
            .contains(&format!(r#"data-position="/book/{id}/position""#))
    );
    assert!(!h.ok("/authors?page=1").await.main().contains("Page 1 of"));

    let reply = h
        .save(|f| {
            set(f, "library.page_size", "1");
            set(f, "reader.split", "none");
            f.retain(|(k, _)| k != "reader.remember_position");
        })
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/settings?saved=1");
    assert_eq!(
        h.config_file(),
        "# My library.\n\n[server]\nport = 8742\n\n[library]\n# Short pages.\n\
         page_size = 1 # entries\nmystery = \"kept\"\n\n[reader]\nsplit = \"none\"\n\
         remember_position = false\n"
    );
    let saved = h.ok("/settings?saved=1").await;
    assert!(saved.main().contains("Settings saved."));
    assert!(!saved.main().contains("New books are indexed"));

    // The next requests use the new values, without a restart.
    let config = h.live.get();
    assert_eq!(config.library.page_size, 1);
    let read = h.ok(&format!("/book/{id}/read")).await;
    assert!(!read.body.contains(r#"rel="next""#), "one part");
    assert!(read.body.contains(r#"data-position="""#));
    assert!(h.ok("/authors?page=1").await.main().contains("Page 1 of 3"));

    // A save without changes leaves the file as it is.
    let before = h.config_file();
    let reply = h.save(|_| {}).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(h.config_file(), before);
}

#[tokio::test]
async fn reading_list_size_applies_at_once() {
    let h = harness("", ConfigOverrides::default());
    let id = h.live_book("a.fb2").unwrap();
    let read = h.ok(&format!("/book/{id}/read")).await;
    let block = read.body.split(r#"id="b"#).nth(1).unwrap();
    let block = format!("b{}", &block[..block.find('"').unwrap()]);
    let request = Request::post(format!("/book/{id}/position"))
        .header(header::HOST, &h.host)
        .header(header::ORIGIN, h.origin())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "block": block }).to_string(),
        ))
        .unwrap();
    assert_eq!(h.send(request).await.status, StatusCode::NO_CONTENT);
    assert!(h.ok("/").await.main().contains("Currently reading"));
    let reply = h.save(|f| set(f, "library.reading_list_size", "0")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(!h.ok("/").await.main().contains("Currently reading"));
    assert_eq!(h.config_file(), "[library]\nreading_list_size = 0\n");
}

#[tokio::test]
async fn server_settings_cannot_be_changed_from_the_browser() {
    let h = harness(COMMENTED, PORT_9100);
    let before = h.config_file();
    let reply = h
        .save(|f| {
            set(f, "server.port", "1");
            set(f, "server.bind", "0.0.0.0");
            set(f, "server.allowed_hosts", "evil.example");
            set(f, "server.base_path", "/x");
            set(f, "library.page_size", "7");
        })
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let after = h.config_file();
    assert_eq!(after, before.replace("page_size = 100", "page_size = 7"));
    let config = h.live.get();
    assert_eq!(config.server.port, 9100, "the command-line override stays");
    assert_eq!(config.server.bind.to_string(), "127.0.0.1");
    assert!(config.server.allowed_hosts.is_empty());
    assert_eq!(config.library.page_size, 7);
}

#[tokio::test]
async fn invalid_values_are_field_errors_and_nothing_is_written() {
    let h = harness(COMMENTED, ConfigOverrides::default());
    let before = h.config_file();
    let reply = h
        .save(|f| {
            set(f, "library.page_size", "0");
            set(f, "library.reading_list_size", "many");
            set(f, "reader.split", "pages");
            set(f, "annotations.max_note_length", "2000000");
            f.retain(|(k, _)| k != "scan.formats");
            set(f, "scan.exclude", "ok/\r\n[z-a]");
        })
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    let main = reply.main();
    for text in [
        "Some values are invalid; nothing was saved.",
        "must be at least 1",
        "must be a whole number",
        "must be one of: none, section, size",
        "must be between 1 and 1000000",
        "choose at least one format",
        "line 2: invalid pattern",
        // The submitted values are shown again.
        r#"name="library.reading_list_size" value="many""#,
        "ok/\n[z-a]</textarea>",
    ] {
        assert!(main.contains(text), "{text}\n{main}");
    }
    assert_eq!(
        main.matches(r#"class="field invalid""#).count(),
        6,
        "{main}"
    );
    assert_eq!(h.config_file(), before);
    assert_eq!(h.live.get().library.page_size, 100);
}

#[tokio::test]
async fn posts_need_the_own_origin() {
    let h = harness("", ConfigOverrides::default());
    let rev = h.ok("/settings").await.rev();
    let mut fields = Harness::submission(&h.live.get(), &rev);
    set(&mut fields, "library.page_size", "5");
    for origin in [None, Some("http://evil.example")] {
        let reply = h.post("/settings", &fields, origin).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{origin:?}");
    }
    for action in ["reread", "content-cache", "export"] {
        let reply = h.post(&format!("/settings/{action}"), &[], None).await;
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{action}");
    }
    assert_eq!(h.config_file(), "");
}

#[tokio::test]
async fn a_file_changed_since_the_page_was_opened_is_not_overwritten() {
    let h = harness(COMMENTED, ConfigOverrides::default());
    let rev = h.ok("/settings").await.rev();
    let mut fields = Harness::submission(&h.live.get(), &rev);
    set(&mut fields, "library.page_size", "5");
    let edited = COMMENTED.replace("page_size = 100", "page_size = 42");
    std::fs::write(h.library.config_path(), &edited).unwrap();

    let reply = h.post("/settings", &fields, Some(&h.origin())).await;
    assert_eq!(reply.status, StatusCode::CONFLICT);
    let main = reply.main();
    assert!(main.contains("config.toml was changed since this page was opened"));
    assert!(
        main.contains(r#"name="library.page_size" value="42""#),
        "{main}"
    );
    assert_eq!(h.config_file(), edited);
    assert_eq!(h.live.get().library.page_size, 100);

    // The page notes the difference; saving applies the file as a whole.
    let page = h.ok("/settings").await;
    assert!(
        page.main()
            .contains("config.toml was changed outside this page")
    );
    let reply = h.save(|f| set(f, "library.page_size", "42")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(h.live.get().library.page_size, 42);
    assert_eq!(h.config_file(), edited);
}

#[tokio::test]
async fn errors_in_the_file_are_shown_and_block_saving() {
    let h = harness("", ConfigOverrides::default());
    // A key the form does not edit: fix it in the file.
    std::fs::write(h.library.config_path(), "[server]\nport = 0\n").unwrap();
    let page = h.ok("/settings").await;
    assert!(
        page.main()
            .contains("`server.port` must be between 1 and 65535"),
        "{}",
        page.main()
    );
    assert!(page.main().contains("Fix this in config.toml"));
    assert!(page.main().contains(r#"class="primary" disabled"#));
    let reply = h.save(|_| {}).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(h.config_file(), "[server]\nport = 0\n");

    // Not TOML.
    std::fs::write(h.library.config_path(), "[library\n").unwrap();
    let page = h.ok("/settings").await;
    assert!(page.main().contains("config.toml is not valid TOML"));

    // An editable key: shown at its field, and a save fixes it.
    std::fs::write(h.library.config_path(), "[library]\npage_size = 0\n").unwrap();
    let page = h.ok("/settings").await;
    assert!(
        page.main().contains(r#"class="field invalid""#),
        "{}",
        page.main()
    );
    assert!(
        page.main()
            .contains("must be greater than 0 (the value in config.toml")
    );
    let rev = page.rev();
    let mut fields = Harness::submission(&h.live.get(), &rev);
    set(&mut fields, "library.page_size", "10");
    let reply = h.post("/settings", &fields, Some(&h.origin())).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(h.config_file(), "[library]\npage_size = 10\n");
}

#[tokio::test]
async fn unreadable_or_read_only_files_give_errors_without_paths() {
    let h = harness("", ConfigOverrides::default());
    let root = h.dir.path().display().to_string();
    let rev = h.ok("/settings").await.rev();
    let mut permissions = std::fs::metadata(h.library.config_path())
        .unwrap()
        .permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(h.library.config_path(), permissions).unwrap();
    let mut fields = Harness::submission(&h.live.get(), &rev);
    set(&mut fields, "library.page_size", "5");
    let reply = h.post("/settings", &fields, Some(&h.origin())).await;
    assert_eq!(reply.status, StatusCode::INTERNAL_SERVER_ERROR);
    let notice = reply.main().split("</div>").next().unwrap();
    assert!(
        notice.contains("cannot write the config file (permission denied); nothing was saved."),
        "{notice}"
    );
    assert!(!notice.contains(&root));
    assert_eq!(h.config_file(), "");

    // Windows cannot delete a read-only file.
    let mut permissions = std::fs::metadata(h.library.config_path())
        .unwrap()
        .permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    permissions.set_readonly(false);
    std::fs::set_permissions(h.library.config_path(), permissions).unwrap();
    std::fs::remove_file(h.library.config_path()).unwrap();
    let page = h.ok("/settings").await;
    let notice = page.main().split("</div>").next().unwrap();
    assert!(notice.contains("cannot read the config file"), "{notice}");
    assert!(!notice.contains(&root));
    let reply = h.post("/settings", &fields, Some(&h.origin())).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(!h.library.config_path().exists());
}

#[tokio::test]
async fn the_body_limit_follows_a_raised_note_length() {
    let h = harness(
        "[annotations]\nmax_note_length = 5\n",
        ConfigOverrides::default(),
    );
    let id = h.live_book("a.fb2").unwrap();
    let read = h.ok(&format!("/book/{id}/read")).await;
    let block = read.body.split(r#"id="b"#).nth(1).unwrap();
    let block = format!("b{}", &block[..block.find('"').unwrap()]);
    let body = serde_json::json!({ "block": block, "note": "x".repeat(20_000) }).to_string();
    // Without `Content-Length` the body is cut while it is read; with it, the request
    // is refused at once.
    let request = |declared: bool| {
        let mut builder = Request::post(format!("/book/{id}/annotations"))
            .header(header::HOST, &h.host)
            .header(header::ORIGIN, h.origin())
            .header(header::CONTENT_TYPE, "application/json");
        if declared {
            builder = builder.header(header::CONTENT_LENGTH, body.len());
        }
        builder.body(Body::from(body.clone())).unwrap()
    };
    assert_eq!(
        h.send(request(false)).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(
        h.send(request(true)).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let reply = h
        .save(|f| set(f, "annotations.max_note_length", "30000"))
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(h.send(request(false)).await.status, StatusCode::CREATED);
    assert_eq!(h.send(request(true)).await.status, StatusCode::CREATED);
}

#[tokio::test]
async fn scan_changes_report_the_books_that_leave_and_the_rescan_removes_them() {
    let h = harness("", ConfigOverrides::default());
    let txt = h.live_book("pride.txt").unwrap();
    let read = h.ok(&format!("/book/{txt}/read")).await;
    let block = read.body.split(r#"id="b"#).nth(1).unwrap();
    let block = format!("b{}", &block[..block.find('"').unwrap()]);
    let request = Request::post(format!("/book/{txt}/annotations"))
        .header(header::HOST, &h.host)
        .header(header::ORIGIN, h.origin())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "block": block }).to_string(),
        ))
        .unwrap();
    assert_eq!(h.send(request).await.status, StatusCode::CREATED);
    let key = {
        let conn = open_writer(&h.library.db_path()).unwrap();
        attach::snapshot(&book_by_id(&conn, txt).unwrap().unwrap())
            .unwrap()
            .key
    };

    // Turning a format off and adding a directory pattern.
    let reply = h
        .save(|f| {
            f.retain(|(k, v)| !(k == "scan.formats" && v == "txt"));
            set(f, "scan.exclude", "drafts/");
        })
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/settings?saved=1");
    let page = h.ok("/settings?saved=1").await;
    assert!(
        page.main().contains(
            "2 books (1 in TXT format, 1 matching an exclude pattern) will be removed from \
             the index at the next scan; their bookmarks and quotes are kept for 30 days."
        ),
        "{}",
        page.main()
    );
    assert!(page.main().contains("Rescan now"));
    // Nothing is removed before the scan.
    assert!(h.live_book("pride.txt").is_some());

    let reply = h.form("/scan", &[("back", "settings")]).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/settings");
    h.wait_for_scan().await;
    assert!(h.live_book("pride.txt").is_none());
    assert!(h.live_book("drafts/c.fb2").is_none());
    assert!(h.live_book("a.fb2").is_some());
    let groups = store::groups(&h.userdata.lock()).unwrap();
    let group = groups.iter().find(|g| g.key == key).expect("kept");
    assert!(group.removed_at.is_some(), "kept for the retention period");
    assert!(!h.ok("/settings").await.main().contains("will be removed"));

    // Turning it on again: books come back at the next scan.
    let reply = h
        .save(|f| {
            f.push(("scan.formats".to_owned(), "txt".to_owned()));
            set(f, "scan.exclude", "");
        })
        .await;
    assert_eq!(reply.location(), "/settings?saved=1&more=1");
    let page = h.ok("/settings?saved=1&more=1").await;
    assert!(
        page.main()
            .contains("New books are indexed at the next scan.")
    );
    assert!(h.scan.start());
    h.wait_for_scan().await;
    assert_eq!(
        h.live_book("pride.txt"),
        Some(txt),
        "the same book comes back"
    );
}

#[tokio::test]
async fn retention_zero_says_deleted_at_once() {
    let h = harness(
        "[annotations]\nremoved_book_retention_days = 0\n\n[scan]\nformats = [\"fb2\"]\n",
        ConfigOverrides::default(),
    );
    assert!(
        h.live_book("pride.txt").is_none(),
        "scanned with the file's formats"
    );
    let reply = h.save(|f| set(f, "scan.exclude", "b.fb2")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let page = h.ok("/settings").await;
    assert!(
        page.main().contains(
            "1 book (1 matching an exclude pattern) will be removed from the index at the \
             next scan; their bookmarks and quotes are deleted at once."
        ),
        "{}",
        page.main()
    );
}

#[tokio::test]
async fn maintenance_actions() {
    let h = harness("", ConfigOverrides::default());
    // Re-read: every book is parsed again, also unchanged ones.
    let reply = h.form("/settings/reread", &[]).await;
    assert_eq!(reply.location(), "/settings?done=reread");
    h.wait_for_scan().await;
    {
        let conn = open_writer(&h.library.db_path()).unwrap();
        let scan = last_scan(&conn).unwrap().unwrap();
        assert_eq!((scan.parsed, scan.updated, scan.added), (4, 4, 0));
    }
    assert!(h.scan.start());
    h.wait_for_scan().await;
    {
        let conn = open_writer(&h.library.db_path()).unwrap();
        assert_eq!(
            last_scan(&conn).unwrap().unwrap().parsed,
            0,
            "a normal scan"
        );
    }

    // The content cache: filled by opening a book, emptied by the action.
    let id = h.live_book("a.fb2").unwrap();
    h.ok(&format!("/book/{id}/read")).await;
    let cached = || {
        walkdir::WalkDir::new(h.library.content_dir())
            .into_iter()
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_file())
            .count()
    };
    assert!(cached() > 0);
    let reply = h.form("/settings/content-cache", &[]).await;
    assert_eq!(reply.location(), "/settings?done=cache&n=1");
    assert_eq!(cached(), 0);
    let page = h.ok(reply.location()).await;
    assert!(
        page.main()
            .contains("The content cache was cleared (1 file removed).")
    );
    h.ok(&format!("/book/{id}/read")).await;
    assert_eq!(cached(), 1, "parsed again on the next open");

    // Export.
    let read = h.ok(&format!("/book/{id}/read")).await;
    let block = read.body.split(r#"id="b"#).nth(1).unwrap();
    let block = format!("b{}", &block[..block.find('"').unwrap()]);
    let request = Request::post(format!("/book/{id}/annotations"))
        .header(header::HOST, &h.host)
        .header(header::ORIGIN, h.origin())
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "block": block }).to_string(),
        ))
        .unwrap();
    assert_eq!(h.send(request).await.status, StatusCode::CREATED);
    std::fs::remove_dir_all(h.library.annotations_dir()).unwrap();
    let reply = h.form("/settings/export", &[]).await;
    assert_eq!(reply.location(), "/settings?done=export&n=1&books=1");
    assert_eq!(
        std::fs::read_dir(h.library.annotations_dir())
            .unwrap()
            .count(),
        1
    );
    let page = h.ok(reply.location()).await;
    assert!(page.main().contains("Exported 1 annotation of 1 book"));

    // Clearing the reading list comes back to the settings page.
    let reply = h.form("/reading/clear", &[("back", "settings")]).await;
    assert_eq!(reply.location(), "/settings");
    let reply = h.form("/reading/clear", &[]).await;
    assert_eq!(reply.location(), "/");
}

#[tokio::test]
async fn actions_are_disabled_while_a_scan_runs() {
    let h = harness("", ConfigOverrides::default());
    // Many books keep the scan busy for a moment.
    for i in 0..300 {
        std::fs::write(h.path(&format!("n{i}.fb2")), fb2(&format!("N{i}"), "Nash")).unwrap();
    }
    assert!(h.scan.start_reread());
    let page = h.ok("/settings").await;
    if h.scan.is_running() {
        assert!(page.main().contains("A scan is running"), "{}", page.main());
        assert!(page.main().contains("disabled>Re-read all books</button>"));
        let reply = h.form("/settings/reread", &[]).await;
        assert_eq!(reply.location(), "/settings?done=busy");
    }
    h.wait_for_scan().await;
}

#[test]
fn the_default_file_is_left_alone_by_a_save_without_changes() {
    let config = Config::from_toml(DEFAULT_CONFIG_TOML, std::path::Path::new("c"))
        .unwrap()
        .0;
    assert_eq!(
        shelfgauge::config::edit::edit(DEFAULT_CONFIG_TOML, &config).unwrap(),
        DEFAULT_CONFIG_TOML
    );
}

/// Format names are case-insensitive in the file: the form shows them checked, and a
/// save of another key leaves the line alone.
#[tokio::test]
async fn formats_in_other_case_are_kept() {
    let text = "[scan]\nformats = [\"TXT\", \"fb2\", \"txt\"]\n";
    let h = harness(text, ConfigOverrides::default());
    let page = h.ok("/settings").await;
    let main = page.main();
    assert!(main.contains(r#"value="txt" checked"#), "{main}");
    assert!(main.contains(r#"value="fb2" checked"#), "{main}");
    assert!(!main.contains(r#"value="epub" checked"#), "{main}");
    assert!(!main.contains("config.toml was changed outside"), "{main}");
    let reply = h.save(|f| set(f, "library.page_size", "9")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(
        h.config_file(),
        format!("{text}\n[library]\npage_size = 9\n")
    );
}
