//! Profiles end to end: one profile changes nothing, the picker and sessions, PINs and
//! their lockout, roles, separate reading per profile, the settings in the profile,
//! the per-profile export and the family activity, on a scanned temporary library.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use shelfgauge::annotations::export::Exporter;
use shelfgauge::annotations::store::{self, UserData};
use shelfgauge::config::Config;
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::library::Library;
use shelfgauge::profiles::session::Level;
use shelfgauge::profiles::{ProfileId, password, store as profiles};
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::all_books;
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

const HOST: &str = "127.0.0.1:8742";
const ORIGIN: &str = "http://127.0.0.1:8742";

fn fb2(title: &str) -> Vec<u8> {
    let mut body = String::new();
    for c in 1..=3 {
        body.push_str(&format!("<section><title><p>Chapter {c}</p></title>"));
        for n in 1..=4 {
            body.push_str(&format!(
                "<p>Paragraph {c}.{n} of {title}: the quick fox jumps over the lazy dog.</p>"
            ));
        }
        body.push_str("</section>");
    }
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook><description><title-info><author><first-name>Anna</first-name><last-name>Writer</last-name></author>
<book-title>{title}</book-title></title-info></description><body>{body}</body></FictionBook>"#
    )
    .into_bytes()
}

struct Lib {
    _dir: tempfile::TempDir,
    library: Library,
    userdata: Arc<UserData>,
    exporter: Exporter,
    router: Router,
    scan: Arc<BackgroundScan>,
    book: i64,
}

impl Drop for Lib {
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
    fn location(&self) -> &str {
        self.headers[header::LOCATION].to_str().unwrap()
    }

    /// The session token set by the reply, if any (`Some("")` when it is cleared).
    fn cookie(&self) -> Option<String> {
        self.headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find_map(|v| v.strip_prefix("shelfgauge-profile="))
            .map(|v| v.split(';').next().unwrap().to_owned())
    }

    fn set_cookie_header(&self) -> String {
        self.headers[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_owned()
    }
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("book.fb2"), fb2("Shared Book")).unwrap();
        let mut conn = open_writer(&library.db_path()).unwrap();
        Scanner {
            library: &library,
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
        let book = all_books(&conn).unwrap()[0].id;
        let userdata = Arc::new(UserData::open(&library.userdata_path()).unwrap());
        let exporter = Exporter::new(library.annotations_dir(), "http://127.0.0.1:8742/");
        let config = Config::default();
        let pool = open_read_pool(&library.db_path(), 2).unwrap();
        let scan = Arc::new(BackgroundScan::new(library.clone(), Vec::new()));
        let state = AppState::new(library.clone(), &config, pool, Arc::clone(&scan))
            .with_annotations(Arc::clone(&userdata), exporter.clone());
        Self {
            _dir: dir,
            library,
            userdata,
            exporter,
            router: common::local(web::router(state)),
            scan,
            book,
        }
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

    async fn get(&self, uri: &str, token: Option<&str>) -> Reply {
        let mut request = Request::get(uri).header(header::HOST, HOST);
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("shelfgauge-profile={token}"));
        }
        self.send(request.body(Body::empty()).unwrap()).await
    }

    async fn post_with(
        &self,
        uri: &str,
        token: Option<&str>,
        content_type: &str,
        body: &str,
        origin: Option<&str>,
    ) -> Reply {
        let mut request = Request::post(uri)
            .header(header::HOST, HOST)
            .header(header::CONTENT_TYPE, content_type);
        if let Some(origin) = origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("shelfgauge-profile={token}"));
        }
        self.send(request.body(Body::from(body.to_owned())).unwrap())
            .await
    }

    async fn form(&self, uri: &str, token: Option<&str>, body: &str) -> Reply {
        self.post_with(
            uri,
            token,
            "application/x-www-form-urlencoded",
            body,
            Some(ORIGIN),
        )
        .await
    }

    async fn json(&self, uri: &str, token: Option<&str>, body: &str) -> Reply {
        self.post_with(uri, token, "application/json", body, Some(ORIGIN))
            .await
    }

    /// Adds a profile as the owner (implied while it is the only profile, or with
    /// `token`); returns its id and the reply.
    async fn add(&self, token: Option<&str>, form: &str) -> (ProfileId, Reply) {
        let reply = self.form("/settings/profiles", token, form).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        let name = form
            .split('&')
            .find_map(|p| p.strip_prefix("name="))
            .unwrap();
        let id = profiles::by_name(&self.userdata.lock(), name)
            .unwrap()
            .unwrap_or_else(|| panic!("not added: {} {}", reply.location(), reply.body))
            .id;
        (id, reply)
    }

    /// Chooses profile `id` (without a PIN); returns its session token.
    async fn select(&self, id: ProfileId) -> String {
        let reply = self
            .form(&format!("/profiles/{id}/select"), None, "next=/")
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        reply.cookie().expect("a session cookie")
    }
}

fn html_tag(body: &str) -> &str {
    let start = body.find("<html").unwrap();
    &body[start..start + body[start..].find('>').unwrap()]
}

#[tokio::test]
async fn one_profile_needs_no_cookie_and_shows_no_profile_ui() {
    let lib = Lib::new();
    for uri in ["/", &format!("/book/{}", lib.book), "/stats", "/settings"] {
        let reply = lib.get(uri, None).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}");
        assert!(!reply.body.contains("profile-menu"), "{uri}");
        assert!(reply.cookie().is_none(), "{uri}");
    }
    let reply = lib.get(&format!("/book/{}/read", lib.book), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    // The picker has nothing to offer: back to `next`.
    let reply = lib.get("/profiles?next=/stats", None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/stats");
    // Writes need no cookie either.
    let reply = lib
        .json(
            &format!("/book/{}/annotations", lib.book),
            None,
            r#"{"block":"b2"}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::CREATED, "{}", reply.body);
    assert_eq!(
        store::for_book(&lib.userdata.lock(), ProfileId::OWNER, &key(&lib))
            .unwrap()
            .len(),
        1
    );
    // The owner's settings were never saved: the page asks for the adoption.
    let home = lib.get("/", None).await.body;
    let tag = html_tag(&home);
    assert!(tag.contains(" data-adopt"), "{tag}");
    assert!(!tag.contains("data-pcolor"), "{tag}");
    assert!(
        tag.contains(r#"data-settings="/profile/settings""#),
        "{tag}"
    );
}

fn key(lib: &Lib) -> Vec<u8> {
    let conn = open_writer(&lib.library.db_path()).unwrap();
    let book = shelfgauge::storage::books::book_by_id(&conn, lib.book)
        .unwrap()
        .unwrap();
    shelfgauge::annotations::attach::snapshot(&book)
        .unwrap()
        .key
}

#[tokio::test]
async fn settings_are_stored_cleaned_and_rendered_on_html() {
    let lib = Lib::new();
    let reply = lib
        .json(
            "/profile/settings",
            None,
            r#"{"theme":"sepia","font":"serif","size":"6","width":"bogus","x":"y","align":"left"}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.body);
    let owner = profiles::get(&lib.userdata.lock(), ProfileId::OWNER)
        .unwrap()
        .unwrap();
    assert_eq!(
        owner.settings.unwrap().to_json(),
        r#"{"font":"serif","size":"6","theme":"sepia"}"#
    );
    let home = lib.get("/", None).await.body;
    let tag = html_tag(&home);
    assert!(tag.contains(r#"data-theme="sepia""#), "{tag}");
    assert!(tag.contains(r#"data-rfont="serif""#), "{tag}");
    assert!(tag.contains(r#"data-rsize="6""#), "{tag}");
    assert!(
        !tag.contains("data-rwidth") && !tag.contains("data-ralign"),
        "{tag}"
    );
    assert!(!tag.contains("data-adopt"), "adopted once: {tag}");
    // Not JSON: refused, nothing changed.
    let reply = lib.json("/profile/settings", None, "theme=dark").await;
    assert!(reply.status.is_client_error());
    // The reading-settings reset keeps the theme.
    let reply = lib.form("/profile", None, "action=reset-reading").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let tag_page = lib.get("/", None).await.body;
    let tag = html_tag(&tag_page);
    assert!(
        tag.contains(r#"data-theme="sepia""#) && !tag.contains("data-rfont"),
        "{tag}"
    );
}

#[tokio::test]
async fn the_second_profile_keeps_the_admin_signed_in() {
    let lib = Lib::new();
    let (anna, reply) = lib.add(None, "name=Anna&color=2&role=reader").await;
    let owner_token = reply.cookie().expect("an owner session for the admin");
    let cookie = reply.set_cookie_header();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
        "{cookie}"
    );
    assert!(
        cookie.contains("Path=/;") && cookie.contains("Max-Age=34560000"),
        "{cookie}"
    );
    assert!(!cookie.contains("Secure"), "plain-HTTP localhost: {cookie}");
    assert_eq!(reply.location(), "/settings?profile=added#profiles");

    let page = lib.get("/settings", Some(&owner_token)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("profile-menu"));
    assert!(page.body.contains("Anna"));

    // Without a cookie: the picker for pages, 401 for fragments and writes.
    let reply = lib.get("/book/1?x=1", None).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/profiles?next=/book/1%3Fx%3D1");
    for uri in [
        "/scan/status",
        "/sidebar/authors",
        &format!("/book/{}/annotations", lib.book),
        &format!("/book/{}/find?q=fox", lib.book),
    ] {
        assert_eq!(
            lib.get(uri, None).await.status,
            StatusCode::UNAUTHORIZED,
            "{uri}"
        );
    }
    let reply = lib
        .json(
            &format!("/book/{}/position", lib.book),
            None,
            r#"{"block":"b2"}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    // A forged or foreign token is no session.
    assert_eq!(
        lib.get("/", Some("forged")).await.status,
        StatusCode::SEE_OTHER
    );

    let picker = lib.get("/profiles?next=/stats", None).await;
    assert_eq!(picker.status, StatusCode::OK);
    assert!(picker.body.contains("Who is reading?") && picker.body.contains("Anna"));
    assert!(picker.body.contains(r#"value="/stats""#));
    let token = lib.select(anna).await;
    assert_ne!(token, owner_token);
    let page = lib.get("/", Some(&token)).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(html_tag(&page.body).contains(r#"data-pcolor="2""#));
    assert!(
        !html_tag(&page.body).contains("data-adopt"),
        "only the owner adopts"
    );

    // Leaving ends the session.
    let reply = lib.form("/profiles/leave", Some(&token), "").await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert_eq!(reply.location(), "/profiles");
    assert_eq!(reply.cookie().as_deref(), Some(""));
    assert_eq!(
        lib.get("/", Some(&token)).await.status,
        StatusCode::SEE_OTHER
    );
}

#[tokio::test]
async fn next_must_stay_local() {
    let lib = Lib::new();
    let (anna, _) = lib.add(None, "name=Anna").await;
    for (next, expected) in [
        ("//evil.example/", "/"),
        ("https://evil.example/", "/"),
        ("/%5Cevil.example", "/"),
        ("evil", "/"),
        ("/stats", "/stats"),
    ] {
        let reply = lib
            .form(
                &format!("/profiles/{anna}/select"),
                None,
                &format!("next={next}"),
            )
            .await;
        assert_eq!(reply.location(), expected, "{next}");
    }
}

#[tokio::test]
async fn reading_is_separate_per_profile() {
    let lib = Lib::new();
    let (owner_token, anna) = {
        let (anna, reply) = lib.add(None, "name=Anna").await;
        (reply.cookie().unwrap(), anna)
    };
    let anna_token = lib.select(anna).await;
    let annotations = format!("/book/{}/annotations", lib.book);
    let mine = lib
        .json(&annotations, Some(&owner_token), r#"{"block":"b2"}"#)
        .await;
    assert_eq!(mine.status, StatusCode::CREATED);
    let mine: serde_json::Value = serde_json::from_str(&mine.body).unwrap();
    let id = mine["id"].as_str().unwrap();

    // Anna does not see it, and cannot edit or delete it.
    let lists = lib.get(&annotations, Some(&anna_token)).await.body;
    assert!(!lists.contains(id), "{lists}");
    let note = lib
        .json(
            &format!("/annotation/{id}/note"),
            Some(&anna_token),
            r#"{"note":"x"}"#,
        )
        .await;
    assert_eq!(note.status, StatusCode::NOT_FOUND);
    let delete = lib
        .json(&format!("/annotation/{id}/delete"), Some(&anna_token), "{}")
        .await;
    assert_eq!(delete.status, StatusCode::NOT_FOUND);
    assert!(
        lib.get(&annotations, Some(&owner_token))
            .await
            .body
            .contains(id)
    );

    // Positions and "Currently reading" are hers alone.
    let reply = lib
        .json(
            &format!("/book/{}/position", lib.book),
            Some(&anna_token),
            r#"{"block":"b9"}"#,
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert!(
        lib.get("/", Some(&anna_token))
            .await
            .body
            .contains("Currently reading")
    );
    assert!(
        !lib.get("/", Some(&owner_token))
            .await
            .body
            .contains("Currently reading")
    );
    {
        let user = lib.userdata.lock();
        assert!(store::position(&user, anna, &key(&lib)).unwrap().is_some());
        assert!(
            store::position(&user, ProfileId::OWNER, &key(&lib))
                .unwrap()
                .is_none()
        );
    }

    // Statistics: "Your reading" with the own figures.
    let stats = lib.get("/stats", Some(&anna_token)).await.body;
    assert!(stats.contains("Your reading"), "{stats}");

    // The export: the owner's file in the root, Anna's in her folder.
    let owner_files: Vec<_> = std::fs::read_dir(lib.library.annotations_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.is_file())
        .collect();
    assert_eq!(owner_files.len(), 1);
    lib.json(&annotations, Some(&anna_token), r#"{"block":"b3"}"#)
        .await;
    let folder = lib.exporter.profile_dir(anna, "Anna");
    assert!(folder.ends_with(format!("profiles/Anna ({anna})")));
    assert_eq!(std::fs::read_dir(&folder).unwrap().count(), 1);
    // Renaming moves the folder; deleting the profile keeps it.
    let reply = lib
        .form(
            &format!("/settings/profiles/{anna}/rename"),
            Some(&owner_token),
            "name=Ann",
        )
        .await;
    assert_eq!(reply.location(), "/settings?profile=renamed#profiles");
    let renamed = lib.exporter.profile_dir(anna, "Ann");
    assert!(renamed.is_dir() && !folder.exists());
    let reply = lib
        .form(
            &format!("/settings/profiles/{anna}/delete"),
            Some(&owner_token),
            "name=ann",
        )
        .await;
    assert_eq!(reply.location(), "/settings?profile=deleted#profiles");
    assert!(renamed.is_dir(), "the export folder is kept");
    assert!(
        store::positions_for_key(&lib.userdata.lock(), &key(&lib))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn pins_lock_out_and_changes_renew_sessions() {
    let lib = Lib::new();
    let (_, reply) = lib.add(None, "name=Kid").await;
    let owner_token = reply.cookie().unwrap();
    let (anna, _) = lib.add(Some(&owner_token), "name=Anna&pin=2468").await;
    let hash = profiles::pin_hash(&lib.userdata.lock(), anna)
        .unwrap()
        .unwrap();
    assert!(hash.starts_with("$argon2id$"), "{hash}");
    assert!(!hash.contains("2468"));

    let reply = lib
        .form(&format!("/profiles/{anna}/select"), None, "next=/stats")
        .await;
    assert_eq!(
        reply.location(),
        format!("/profiles/{anna}/pin?next=%2Fstats")
    );
    assert!(reply.cookie().is_none());
    let page = lib
        .get(&format!("/profiles/{anna}/pin?next=/stats"), None)
        .await;
    assert!(
        page.body.contains(r#"inputmode="numeric""#) && page.body.contains(r#"autocomplete="off""#)
    );

    let pin = |value: &str| format!("pin={value}&next=/stats");
    let uri = format!("/profiles/{anna}/pin");
    let reply = lib.form(&uri, None, &pin("1111")).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("4 tries left"), "{}", reply.body);
    let reply = lib.form(&uri, None, &pin("12a")).await;
    assert!(
        reply.body.contains("3 tries left"),
        "a malformed PIN counts too"
    );
    let reply = lib.form(&uri, None, &pin("2468")).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "success resets");
    assert_eq!(reply.location(), "/stats");
    let first = reply.cookie().unwrap();
    for left in (1..5).rev() {
        let reply = lib.form(&uri, None, &pin("0000")).await;
        assert!(
            reply.body.contains(&format!("{left} tr")),
            "{left}: {}",
            reply.body
        );
    }
    let reply = lib.form(&uri, None, &pin("0000")).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        reply.body.contains("Try again in 30 seconds"),
        "{}",
        reply.body
    );
    // Locked: even the right PIN waits.
    let reply = lib.form(&uri, None, &pin("2468")).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);

    // A PIN change keeps this session with a new token and ends the others.
    let other =
        shelfgauge::profiles::session::create(&lib.userdata.lock(), anna, Level::Local, false, 0)
            .unwrap();
    let reply = lib
        .form(
            "/profile",
            Some(&first),
            "action=set-pin&current_pin=0000&new_pin=1357&confirm_pin=1357",
        )
        .await;
    // The profile is locked: the current PIN cannot be checked now.
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    // Lift the lockout the way an admin's removal does, then set a PIN.
    lib.form(
        &format!("/settings/profiles/{anna}/remove-pin"),
        Some(&owner_token),
        "",
    )
    .await;
    assert!(
        !profiles::get(&lib.userdata.lock(), anna)
            .unwrap()
            .unwrap()
            .has_pin
    );
    assert_eq!(
        lib.get("/", Some(&first)).await.status,
        StatusCode::SEE_OTHER,
        "an admin's removal signs the profile out everywhere"
    );
    let token = lib.select(anna).await;
    let other2 = {
        let conn = lib.userdata.lock();
        shelfgauge::profiles::session::create(&conn, anna, Level::Local, false, 0).unwrap()
    };
    let reply = lib
        .form(
            "/profile",
            Some(&token),
            "action=set-pin&new_pin=1357&confirm_pin=1357",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    let renewed = reply.cookie().expect("a renewed token");
    assert_ne!(renewed, token);
    assert_eq!(lib.get("/", Some(&renewed)).await.status, StatusCode::OK);
    assert_eq!(
        lib.get("/", Some(&token)).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        lib.get("/", Some(other2.as_str())).await.status,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        lib.get("/", Some(other.as_str())).await.status,
        StatusCode::SEE_OTHER
    );
    // Wrong current PIN, bad format, different confirmation.
    for (form, text) in [
        (
            "action=set-pin&current_pin=9999&new_pin=1234&confirm_pin=1234",
            "Wrong PIN",
        ),
        (
            "action=set-pin&current_pin=1357&new_pin=12&confirm_pin=12",
            "4 to 8 digits",
        ),
        (
            "action=set-pin&current_pin=1357&new_pin=1234&confirm_pin=4321",
            "differ",
        ),
    ] {
        let reply = lib.form("/profile", Some(&renewed), form).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{form}");
        assert!(reply.body.contains(text), "{form}: {}", reply.body);
    }
    // Removing it needs the current PIN.
    let reply = lib
        .form(
            "/profile",
            Some(&renewed),
            "action=remove-pin&current_pin=1357",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(
        !profiles::get(&lib.userdata.lock(), anna)
            .unwrap()
            .unwrap()
            .has_pin
    );
}

#[tokio::test]
async fn readers_get_no_admin_pages_and_the_owner_stays() {
    let lib = Lib::new();
    let (anna, reply) = lib.add(None, "name=Anna&role=reader").await;
    let owner_token = reply.cookie().unwrap();
    let token = lib.select(anna).await;
    let home = lib.get("/", Some(&token)).await.body;
    assert!(!home.contains("settings-link") && !home.contains("Rescan the library"));
    assert!(home.contains("My profile") && !home.contains("#profiles\""));
    for (method, uri) in [
        ("GET", "/settings"),
        ("POST", "/settings"),
        ("POST", "/scan"),
        ("POST", "/settings/export"),
        ("POST", "/settings/profiles"),
        ("POST", "/settings/profiles/1/delete"),
        ("GET", "/settings/profiles/1/delete"),
    ] {
        let reply = if method == "GET" {
            lib.get(uri, Some(&token)).await
        } else {
            lib.form(uri, Some(&token), "name=x").await
        };
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{method} {uri}");
        assert!(reply.body.contains("Not allowed"), "{uri}");
    }
    // Error pages outside the profile middleware use the profile too.
    lib.json("/profile/settings", Some(&token), r#"{"theme":"navy"}"#)
        .await;
    for uri in ["/no/such/page", "/profiles/99/pin"] {
        let missing = lib.get(uri, Some(&token)).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{uri}");
        assert!(
            html_tag(&missing.body).contains(r#"data-theme="navy""#),
            "{uri}"
        );
        assert!(!missing.body.contains("settings-link"), "{uri}");
    }
    let unknown = lib.get("/no/such/page", None).await;
    assert!(!unknown.body.contains("settings-link") && !unknown.body.contains("profile-menu"));
    let owner_404 = lib.get("/no/such/page", Some(&owner_token)).await;
    assert!(owner_404.body.contains("settings-link") && owner_404.body.contains("profile-menu"));

    // A reader edits their own profile.
    let reply = lib
        .form("/profile", Some(&token), "action=details&name=Anny&color=3")
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(
        profiles::get(&lib.userdata.lock(), anna)
            .unwrap()
            .unwrap()
            .color,
        3
    );
    let reply = lib
        .form("/profile", Some(&token), "action=details&name=me")
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("already has this name"));

    // The owner: never deleted or demoted; names unique ignoring case.
    let reply = lib
        .form(
            "/settings/profiles/1/role",
            Some(&owner_token),
            "role=reader",
        )
        .await;
    assert_eq!(reply.location(), "/settings?profile=owner#profiles");
    let reply = lib
        .form("/settings/profiles/1/delete", Some(&owner_token), "name=Me")
        .await;
    assert_eq!(reply.location(), "/settings?profile=owner#profiles");
    let reply = lib
        .form("/settings/profiles", Some(&owner_token), "name=ANNY")
        .await;
    assert_eq!(reply.location(), "/settings?profile=duplicate#profiles");
    assert_eq!(
        lib.get("/settings/profiles/1/delete", Some(&owner_token))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let confirm = lib
        .get(
            &format!("/settings/profiles/{anna}/delete"),
            Some(&owner_token),
        )
        .await;
    assert!(
        confirm.body.contains("Type the profile’s name"),
        "{}",
        confirm.body
    );
    let reply = lib
        .form(
            &format!("/settings/profiles/{anna}/delete"),
            Some(&owner_token),
            "name=wrong",
        )
        .await;
    assert!(reply.location().ends_with("delete?mismatch=1"));
    assert!(profiles::get(&lib.userdata.lock(), anna).unwrap().is_some());
}

/// The only admin password stays while config.toml turns logins with passwords on for
/// the next start (this server runs without them): its removal, making its profile a
/// reader and deleting the profile are refused, on Settings → Profiles and My profile.
#[tokio::test]
async fn the_last_admin_password_stays_while_the_next_start_needs_it() {
    let lib = Lib::new();
    let (anna, reply) = lib.add(None, "name=Anna&role=admin").await;
    let owner_token = reply.cookie().unwrap();
    let anna_token = lib.select(anna).await;
    let hash = password::hash("anna's long password").unwrap();
    profiles::set_password_hash(&lib.userdata.lock(), anna, Some(&hash), 0).unwrap();
    let anna_url = |action: &str| format!("/settings/profiles/{anna}/{action}");

    // Without logins with passwords nothing is kept.
    let settings = lib.get("/settings", Some(&owner_token)).await;
    assert!(
        !settings.body.contains("The only admin password")
            && settings.body.contains(&anna_url("remove-password")),
        "{}",
        settings.body
    );

    std::fs::write(lib.library.config_path(), "[server]\nauth = \"password\"\n").unwrap();
    let settings = lib.get("/settings", Some(&owner_token)).await;
    assert!(
        settings.body.contains("The only admin password")
            && !settings.body.contains(&anna_url("remove-password")),
        "{}",
        settings.body
    );
    for (action, body) in [
        ("remove-password", ""),
        ("role", "role=reader"),
        ("delete", "name=Anna"),
    ] {
        let reply = lib.form(&anna_url(action), Some(&owner_token), body).await;
        assert_eq!(
            reply.location(),
            "/settings?profile=last-admin-password#profiles",
            "{action}"
        );
    }
    let kept = profiles::get(&lib.userdata.lock(), anna).unwrap().unwrap();
    assert!(kept.is_admin() && kept.has_password);
    let notice = lib
        .get("/settings?profile=last-admin-password", Some(&owner_token))
        .await;
    assert!(
        notice.body.contains("no admin with a password"),
        "{}",
        notice.body
    );
    // Anna on My profile: no Remove password, and a post of it is refused.
    let page = lib.get("/profile", Some(&anna_token)).await;
    assert!(
        page.body.contains("This is the only admin password")
            && !page.body.contains(r#"value="remove-password""#),
        "{}",
        page.body
    );
    let reply = lib
        .form(
            "/profile",
            Some(&anna_token),
            "action=remove-password&current_password=anna%27s+long+password",
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        reply.body.contains("no admin with a password"),
        "{}",
        reply.body
    );

    // Once the owner has a password too, Anna's may go.
    profiles::set_password_hash(&lib.userdata.lock(), ProfileId::OWNER, Some(&hash), 0).unwrap();
    let reply = lib
        .form(&anna_url("remove-password"), Some(&owner_token), "")
        .await;
    assert_eq!(
        reply.location(),
        "/settings?profile=password-removed#profiles"
    );
    // The owner's is the last now; with logins with passwords off in the file, it goes.
    std::fs::write(lib.library.config_path(), "[server]\nauth = \"none\"\n").unwrap();
    let reply = lib
        .form(
            "/profile",
            Some(&owner_token),
            "action=remove-password&current_password=anna%27s+long+password",
        )
        .await;
    assert_eq!(reply.location(), "/profile?done=password-removed");
}

#[tokio::test]
async fn profile_posts_need_our_origin() {
    let lib = Lib::new();
    let (anna, reply) = lib.add(None, "name=Anna").await;
    let owner_token = reply.cookie().unwrap();
    for (uri, token) in [
        (format!("/profiles/{anna}/select"), None),
        (format!("/profiles/{anna}/pin"), None),
        ("/profiles/leave".to_owned(), Some(owner_token.as_str())),
        ("/profile".to_owned(), Some(owner_token.as_str())),
        ("/profile/settings".to_owned(), Some(owner_token.as_str())),
        ("/settings/profiles".to_owned(), Some(owner_token.as_str())),
    ] {
        for origin in [None, Some("http://evil.example")] {
            let reply = lib
                .post_with(
                    &uri,
                    token,
                    "application/x-www-form-urlencoded",
                    "next=/",
                    origin,
                )
                .await;
            assert_eq!(reply.status, StatusCode::FORBIDDEN, "{uri} {origin:?}");
            assert!(reply.cookie().is_none());
        }
    }
}

#[tokio::test]
async fn family_activity_is_opt_in_and_never_annotations() {
    let lib = Lib::new();
    let (anna, reply) = lib.add(None, "name=Anna").await;
    let owner_token = reply.cookie().unwrap();
    let anna_token = lib.select(anna).await;
    lib.json(
        &format!("/book/{}/position", lib.book),
        Some(&anna_token),
        r#"{"block":"b9"}"#,
    )
    .await;
    lib.json(
        &format!("/book/{}/annotations", lib.book),
        Some(&anna_token),
        r#"{"block":"b5","note":"Anna's secret note"}"#,
    )
    .await;
    let card = format!("/book/{}", lib.book);
    assert!(
        !lib.get("/", Some(&owner_token))
            .await
            .body
            .contains("Family is reading")
    );
    assert!(
        !lib.get(&card, Some(&owner_token))
            .await
            .body
            .contains("Also reading")
    );

    let reply = lib
        .form(
            "/profile",
            Some(&anna_token),
            "action=details&name=Anna&share=on",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    let home = lib.get("/", Some(&owner_token)).await.body;
    assert!(
        home.contains("Family is reading") && home.contains("Shared Book"),
        "{home}"
    );
    assert!(home.contains("Anna"));
    let page = lib.get(&card, Some(&owner_token)).await.body;
    assert!(page.contains("Also reading:"), "{page}");
    assert!(!page.contains("secret note"));
    // Her own pages do not list her as family.
    assert!(
        !lib.get("/", Some(&anna_token))
            .await
            .body
            .contains("Family is reading")
    );
}

/// Regression: parallel wrong PINs used to all pass the lockout check before the first
/// failure was counted, so 20 requests at once got 20 guesses. Tries are counted first.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_pin_guesses_are_limited_too() {
    let lib = Lib::new();
    let (_, reply) = lib.add(None, "name=Kid").await;
    let owner_token = reply.cookie().unwrap();
    let (anna, _) = lib.add(Some(&owner_token), "name=Anna&pin=2468").await;
    let mut set = tokio::task::JoinSet::new();
    for guess in 0..20 {
        let router = lib.router.clone();
        let request = Request::post(format!("/profiles/{anna}/pin"))
            .header(header::HOST, HOST)
            .header(header::ORIGIN, ORIGIN)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from(format!("pin={:04}&next=/", 1000 + guess)))
            .unwrap();
        set.spawn(async move { router.oneshot(request).await.unwrap().status() });
    }
    let statuses = set.join_all().await;
    let guessed = statuses
        .iter()
        .filter(|s| **s != StatusCode::TOO_MANY_REQUESTS)
        .count();
    assert!(guessed <= 5, "{guessed} guesses got through: {statuses:?}");
    assert!(
        statuses
            .iter()
            .all(|s| *s == StatusCode::UNPROCESSABLE_ENTITY || *s == StatusCode::TOO_MANY_REQUESTS)
    );
}
