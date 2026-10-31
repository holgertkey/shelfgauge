//! The Showcase (Phase 31): the folders an admin opens for the library's catalog on the
//! hub. Marks by path, open recursively (zip archives as folders), renamed folders
//! closed and reported, duplicates through the open copy; the controls only for admins
//! of a library on a hub.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use rusqlite::Connection;
use shelfgauge::annotations::export::Exporter;
use shelfgauge::annotations::store::UserData;
use shelfgauge::config::Config;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::hub::{AccountId, Secret};
use shelfgauge::hub_client::HubUrl;
use shelfgauge::hub_client::keys::AccountKey;
use shelfgauge::hub_client::showcase::{self, MarkState};
use shelfgauge::hub_client::store;
use shelfgauge::library::Library;
use shelfgauge::profiles::{ProfileId, store as profiles};
use shelfgauge::render::load::content_key;
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::{BookStatus, all_books, book_by_id, live_books_by_key};
use shelfgauge::storage::folders::FolderPath;
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:8742";
const ORIGIN: &str = "http://127.0.0.1:8742";

fn fb2(title: &str) -> Vec<u8> {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook><description><title-info><author><first-name>Anna</first-name><last-name>Writer</last-name></author>
<book-title>{title}</book-title></title-info></description><body><section><p>Text of {title}.</p></section></body></FictionBook>"#
    )
    .into_bytes()
}

fn dir(p: &str) -> FolderPath {
    FolderPath::Dir(p.into())
}

fn zip(a: &str, i: &str) -> FolderPath {
    FolderPath::Archive {
        archive: a.into(),
        inner: i.into(),
    }
}

fn write(root: &Path, path: &str, bytes: &[u8]) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// The library:
///
/// ```text
/// top.fb2                       Top
/// sf/other.fb2                  Other
/// sf/lem/solaris.fb2            Solaris
/// sf/all.zip: a.fb2             Zip Top
///             x/b.fb2           Zip X
///             x/y/c.fb2         Zip Y
/// closed/copy.fb2               Other (the same bytes as sf/other.fb2)
/// ```
fn build_library(root: &Path) {
    write(root, "top.fb2", &fb2("Top"));
    write(root, "sf/other.fb2", &fb2("Other"));
    write(root, "sf/lem/solaris.fb2", &fb2("Solaris"));
    write(
        root,
        "sf/all.zip",
        &common::zip(&[
            ("a.fb2", fb2("Zip Top")),
            ("x/b.fb2", fb2("Zip X")),
            ("x/y/c.fb2", fb2("Zip Y")),
        ]),
    );
    write(root, "closed/copy.fb2", &fb2("Other"));
}

struct Lib {
    dir: tempfile::TempDir,
    library: Library,
    userdata: Arc<UserData>,
    router: Router,
    scan: Arc<BackgroundScan>,
}

impl Drop for Lib {
    fn drop(&mut self) {
        self.scan.shutdown();
    }
}

struct Reply {
    status: StatusCode,
    location: Option<String>,
    body: String,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        build_library(dir.path());
        let library = Library::open(dir.path()).unwrap();
        let mut lib = Self {
            userdata: Arc::new(UserData::open(&library.userdata_path()).unwrap()),
            router: Router::new(),
            scan: Arc::new(BackgroundScan::new(library.clone(), Vec::new())),
            library,
            dir,
        };
        lib.rescan();
        let pool = open_read_pool(&lib.library.db_path(), 2).unwrap();
        let exporter = Exporter::new(lib.library.annotations_dir(), "http://127.0.0.1:8742/");
        let state = AppState::new(
            lib.library.clone(),
            &Config::default(),
            pool,
            Arc::clone(&lib.scan),
        )
        .with_annotations(Arc::clone(&lib.userdata), exporter);
        lib.router = common::local(web::router(state));
        lib
    }

    fn rescan(&self) {
        let mut conn = open_writer(&self.library.db_path()).unwrap();
        Scanner {
            library: &self.library,
            exclude: &[],
            registry: &Registry::with_builtin_formats(),
            reread: false,
            limits: ReadLimits::default(),
            threads: 1,
            progress: &ScanProgress::default(),
            cancel: &AtomicBool::new(false),
        }
        .run(&mut conn)
        .unwrap();
    }

    fn index(&self) -> Connection {
        Connection::open(self.library.db_path()).unwrap()
    }

    /// Every live book: id, title, container path.
    fn books(&self) -> Vec<(i64, String, String)> {
        let index = self.index();
        all_books(&index)
            .unwrap()
            .into_iter()
            .map(|b| book_by_id(&index, b.id).unwrap().unwrap())
            .filter(|b| b.status != BookStatus::Removed)
            .map(|b| (b.id, b.title, b.container_path))
            .collect()
    }

    /// Book ids by title (duplicates in path order).
    fn ids(&self, title: &str) -> Vec<i64> {
        let mut books: Vec<_> = self.books().into_iter().filter(|b| b.1 == title).collect();
        books.sort_by(|a, b| a.2.cmp(&b.2));
        books.into_iter().map(|b| b.0).collect()
    }

    /// The titles on the Showcase now, sorted.
    fn titles(&self) -> Vec<String> {
        let marks = showcase::marks(&self.userdata.lock()).unwrap();
        let all = self.books();
        let mut titles: Vec<String> = showcase::showcase_books(&self.index(), &marks)
            .unwrap()
            .iter()
            .map(|b| all.iter().find(|x| x.0 == b.id).unwrap().1.clone())
            .collect();
        titles.sort();
        titles
    }

    fn mark(&self, folder: &FolderPath, open: bool) {
        showcase::set_mark(&self.userdata.lock(), folder, open, 1).unwrap();
    }

    fn put_on_hub(&self) {
        store::save_library(
            &self.userdata.lock(),
            &HubUrl::parse("https://hub.example.org").unwrap(),
            AccountId::new([3; 16]),
            "home",
            &Secret::from_text("token"),
            &AccountKey::new([4; 32]),
            ProfileId::OWNER,
            AccountId::new([2; 16]),
            &AccountKey::new([5; 32]),
            "admin",
            0,
        )
        .unwrap();
    }

    async fn send(&self, request: Request<Body>) -> Reply {
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_owned());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            location,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&self, uri: &str, token: Option<&str>) -> Reply {
        let mut request = Request::get(uri)
            .header(header::HOST, HOST)
            .header(header::COOKIE, "shelfgauge-sidebar=folders");
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("shelfgauge-profile={token}"));
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    async fn post(&self, uri: &str, token: Option<&str>, body: &str, json: bool) -> Reply {
        let mut request = Request::post(uri)
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if json {
            request = request.header(header::ACCEPT, "application/json");
        }
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("shelfgauge-profile={token}"));
        }
        self.send(request.body(Body::from(body.to_owned())).unwrap())
            .await
    }
}

#[test]
fn nothing_is_on_the_showcase_by_default() {
    let lib = Lib::new();
    assert!(lib.titles().is_empty());
    assert_eq!(lib.books().len(), 7);
}

#[test]
fn an_open_folder_is_open_recursively_with_its_archives() {
    let lib = Lib::new();
    lib.mark(&dir("sf"), true);
    assert_eq!(
        lib.titles(),
        ["Other", "Solaris", "Zip Top", "Zip X", "Zip Y"]
    );
    // A folder added later inside an open one is open too.
    write(lib.dir.path(), "sf/new/later.fb2", &fb2("Later"));
    lib.rescan();
    assert!(lib.titles().contains(&"Later".to_owned()));
    // The whole library.
    lib.mark(&dir(""), true);
    assert_eq!(lib.titles().len(), 8);
}

#[test]
fn folders_inside_archives_are_folders_too() {
    let lib = Lib::new();
    lib.mark(&zip("sf/all.zip", "x"), true);
    assert_eq!(lib.titles(), ["Zip X", "Zip Y"]);
    lib.mark(&zip("sf/all.zip", "x"), false);
    lib.mark(&zip("sf/all.zip", ""), true);
    assert_eq!(lib.titles(), ["Zip Top", "Zip X", "Zip Y"]);
    // The archive's directory alone does not open the archive.
    lib.mark(&zip("sf/all.zip", ""), false);
    lib.mark(&zip("sf/all.zip", "x/y"), true);
    assert_eq!(lib.titles(), ["Zip Y"]);
}

#[test]
fn a_sub_folder_keeps_its_own_mark_when_the_parent_closes() {
    let lib = Lib::new();
    lib.mark(&dir("sf/lem"), true);
    lib.mark(&dir("sf"), true);
    let marks = showcase::marks(&lib.userdata.lock()).unwrap();
    assert_eq!(
        showcase::folder_state(&marks, &dir("sf/lem")),
        MarkState::ThroughParent
    );
    lib.mark(&dir("sf"), false);
    assert_eq!(lib.titles(), ["Solaris"]);
}

#[test]
fn a_renamed_folder_is_closed_and_reported() {
    let lib = Lib::new();
    lib.mark(&dir("sf/lem"), true);
    lib.mark(&zip("sf/all.zip", "x"), true);
    let index = lib.index();
    let solaris = lib.ids("Solaris")[0];
    let marks = showcase::marks(&lib.userdata.lock()).unwrap();
    assert!(showcase::on_showcase(&index, &marks, solaris).unwrap());
    assert!(showcase::missing_marks(&index, &marks).unwrap().is_empty());
    drop(index);

    std::fs::rename(
        lib.dir.path().join("sf/lem"),
        lib.dir.path().join("sf/stanislaw"),
    )
    .unwrap();
    lib.rescan();
    assert_eq!(lib.titles(), ["Zip X", "Zip Y"]);
    let index = lib.index();
    let solaris = lib.ids("Solaris")[0];
    assert!(!showcase::on_showcase(&index, &marks, solaris).unwrap());
    assert_eq!(
        showcase::missing_marks(&index, &marks).unwrap(),
        [dir("sf/lem")]
    );
    // The root is never missing, even in an empty library.
    assert!(
        showcase::missing_marks(&index, &[dir("")])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_duplicate_is_on_the_showcase_through_its_open_copy() {
    let lib = Lib::new();
    lib.mark(&dir("sf"), true);
    // In path order: `closed/copy.fb2`, then `sf/other.fb2`.
    let [closed_copy, open_copy] = lib.ids("Other")[..] else {
        panic!("two copies of Other");
    };
    let index = lib.index();
    let marks = showcase::marks(&lib.userdata.lock()).unwrap();
    assert!(showcase::on_showcase(&index, &marks, open_copy).unwrap());
    assert!(!showcase::on_showcase(&index, &marks, closed_copy).unwrap());
    let listed: Vec<i64> = showcase::showcase_books(&index, &marks)
        .unwrap()
        .iter()
        .map(|b| b.id)
        .collect();
    assert!(listed.contains(&open_copy) && !listed.contains(&closed_copy));

    // A request names the content: it is on the Showcase whichever copy is the newest
    // (the closed one or the open one).
    let key = |id: i64| content_key(&book_by_id(&index, id).unwrap().unwrap()).unwrap();
    assert_eq!(key(open_copy), key(closed_copy));
    let newest = live_books_by_key(&index, &key(open_copy)).unwrap()[0];
    for marks in [vec![dir("sf")], vec![dir("closed")]] {
        assert!(
            showcase::content_on_showcase(&index, &marks, &key(open_copy)).unwrap(),
            "{marks:?}, newest {}",
            if newest == open_copy {
                "open"
            } else {
                "closed"
            }
        );
    }
    assert!(!showcase::content_on_showcase(&index, &[dir("other")], &key(open_copy)).unwrap());
    let top = lib.ids("Top")[0];
    assert!(!showcase::content_on_showcase(&index, &marks, &key(top)).unwrap());
}

#[tokio::test]
async fn turning_the_hub_off_keeps_the_marks() {
    let lib = Lib::new();
    lib.put_on_hub();
    let saved = lib.post("/showcase", None, "path=sf&open=1", true).await;
    assert_eq!(saved.status, StatusCode::OK);
    // Deleting the library on its hub is how the hub is turned off.
    store::forget_library(&lib.userdata.lock()).unwrap();
    assert_eq!(
        lib.get("/showcase", None).await.body,
        r#"{"enabled":false,"marks":[{"path":"sf"}]}"#
    );
    assert!(
        !lib.get("/folder?path=sf", None)
            .await
            .body
            .contains("id=\"showcase\"")
    );
    assert_eq!(
        lib.post("/showcase", None, "path=sf", true).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(showcase::marks(&lib.userdata.lock()).unwrap(), [dir("sf")]);
    // Back on a hub, the folder is open as before.
    lib.put_on_hub();
    assert!(
        lib.get("/folder?path=sf", None)
            .await
            .body
            .contains(r#"name="open" value="1" checked>"#)
    );
}

#[tokio::test]
async fn the_controls_need_an_admin_and_a_hub() {
    let lib = Lib::new();
    // Not on a hub: the sidebar's script asks and is told so (no checkboxes), the
    // folder page has no box, and marks cannot be set.
    let page = lib.get("/folder?path=sf", None).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("data-showcase=\"/showcase\""));
    assert!(!page.body.contains("id=\"showcase\""));
    let marks = lib.get("/showcase", None).await;
    assert_eq!(marks.status, StatusCode::OK);
    assert_eq!(marks.body, r#"{"enabled":false,"marks":[]}"#);
    let refused = lib.post("/showcase", None, "path=sf&open=1", true).await;
    assert_eq!(refused.status, StatusCode::BAD_REQUEST);
    assert!(lib.titles().is_empty());

    lib.put_on_hub();
    let page = lib.get("/folder?path=sf", None).await;
    assert!(page.body.contains("id=\"showcase\""));
    // Only in the folder mode.
    let request = Request::get("/folder?path=sf")
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    assert!(!lib.send(request).await.body.contains("data-showcase"));
    assert!(
        page.body.contains(r#"name="open" value="1">"#),
        "closed by default"
    );

    // The script's way: JSON back with the marks.
    let saved = lib
        .post("/showcase", None, "zip=sf%2Fall.zip&in=x&open=1", true)
        .await;
    assert_eq!(saved.status, StatusCode::OK, "{}", saved.body);
    assert_eq!(
        saved.body,
        r#"{"enabled":true,"marks":[{"zip":"sf/all.zip","in":"x"}]}"#
    );
    // The form's way: back to the folder page with a notice.
    let saved = lib.post("/showcase", None, "path=sf&open=1", false).await;
    assert_eq!(saved.status, StatusCode::SEE_OTHER);
    assert_eq!(
        saved.location.as_deref(),
        Some("/folder?path=sf&showcase=saved#showcase")
    );
    let page = lib.get("/folder?path=sf&showcase=saved", None).await;
    assert!(page.body.contains("Saved."));
    assert!(page.body.contains(r#"name="open" value="1" checked>"#));
    // A sub-folder shows open through its parent, without a form.
    let page = lib.get("/folder?path=sf%2Flem", None).await;
    assert!(page.body.contains("Open through its parent"));
    assert!(page.body.contains("checked disabled"));
    assert!(!page.body.contains(r#"name="open""#));
    // Unchecked: the box is absent from the form, the mark goes.
    lib.post("/showcase", None, "path=sf", false).await;
    assert_eq!(lib.titles(), ["Zip X", "Zip Y"]);
}

#[tokio::test]
async fn readers_get_no_controls() {
    let lib = Lib::new();
    lib.put_on_hub();
    let reply = lib
        .post(
            "/settings/profiles",
            None,
            "name=Anna&color=2&role=reader",
            false,
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let anna = profiles::by_name(&lib.userdata.lock(), "Anna")
        .unwrap()
        .unwrap()
        .id;
    // Choosing the profile sets its session cookie.
    let token = {
        let response = lib
            .router
            .clone()
            .oneshot(
                Request::post(format!("/profiles/{anna}/select"))
                    .header(header::HOST, HOST)
                    .header(header::ORIGIN, ORIGIN)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("next=/"))
                    .unwrap(),
            )
            .await
            .unwrap();
        response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .strip_prefix("shelfgauge-profile=")
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    };
    let page = lib.get("/folder?path=sf", Some(&token)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.body.contains("data-showcase"));
    assert!(!page.body.contains("id=\"showcase\""));
    assert_eq!(
        lib.get("/showcase", Some(&token)).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        lib.post("/showcase", Some(&token), "path=sf&open=1", true)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    assert!(lib.titles().is_empty());
}

#[tokio::test]
async fn the_root_page_names_marks_whose_folder_is_gone() {
    let lib = Lib::new();
    lib.put_on_hub();
    lib.mark(&dir("sf/lem"), true);
    std::fs::rename(
        lib.dir.path().join("sf/lem"),
        lib.dir.path().join("sf/stanislaw"),
    )
    .unwrap();
    lib.rescan();
    let page = lib.get("/folder", None).await;
    assert!(
        page.body.contains("no longer in the library"),
        "{}",
        page.body
    );
    assert!(page.body.contains("<code>sf/lem</code>"));
    lib.post("/showcase", None, "path=sf%2Flem", false).await;
    let page = lib.get("/folder", None).await;
    assert!(!page.body.contains("no longer in the library"));
}
