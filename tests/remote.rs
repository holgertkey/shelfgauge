//! Remote access end to end: the reach rule (TCP peer, `allowed_hosts` names and the
//! presence of proxy headers, whose values are never trusted), the login with profile passwords, password sessions against
//! local ones, PINs never tried from other devices, the lockouts, `remote_admin`, the
//! `Secure` cookie flag and logging out, on a scanned temporary library.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use axum::Router;
use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode, header};
use http_body_util::BodyExt;
use shelfgauge::annotations::export::Exporter;
use shelfgauge::annotations::store::UserData;
use shelfgauge::config::{AuthMode, Config};
use shelfgauge::container::ReadLimits;
use shelfgauge::format::Registry;
use shelfgauge::library::Library;
use shelfgauge::profiles::session::{self, Level};
use shelfgauge::profiles::{ProfileId, Role, password, pin, store as profiles};
use shelfgauge::scanner::{BackgroundScan, ScanProgress, Scanner};
use shelfgauge::storage::books::all_books;
use shelfgauge::storage::{open_read_pool, open_writer};
use shelfgauge::web::{self, AppState};
use tower::ServiceExt;

/// The loopback name of this computer.
const LOCAL_HOST: &str = "127.0.0.1:8742";
/// The name of the reverse proxy (`allowed_hosts`).
const PROXY_HOST: &str = "books.example.org";
/// A phone on the private network, connecting directly to the bind address.
const PHONE: &str = "100.64.0.2:40000";
const BIND_HOST: &str = "100.64.0.1:8742";

const OWNER_PASSWORD: &str = "correct horse battery";

/// How a request reaches the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Via {
    /// A browser on this computer.
    Local,
    /// A phone connecting directly (a non-loopback peer).
    Phone,
    /// Through the reverse proxy on this computer (loopback peer, the proxy's name).
    Proxy,
}

impl Via {
    fn host(self) -> &'static str {
        match self {
            Self::Local => LOCAL_HOST,
            Self::Phone => BIND_HOST,
            Self::Proxy => PROXY_HOST,
        }
    }

    fn origin(self) -> String {
        match self {
            Self::Proxy => format!("https://{PROXY_HOST}"),
            other => format!("http://{}", other.host()),
        }
    }
}

struct Lib {
    _dir: tempfile::TempDir,
    userdata: Arc<UserData>,
    local: Router,
    phone: Router,
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

    fn set_cookie(&self) -> String {
        self.headers
            .get(header::SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default()
    }

    /// The session token set by the reply, if any.
    fn token(&self) -> Option<String> {
        self.set_cookie()
            .strip_prefix("shelfgauge-profile=")
            .map(|v| v.split(';').next().unwrap().to_owned())
            .filter(|t| !t.is_empty())
    }
}

fn fb2() -> Vec<u8> {
    br#"<?xml version="1.0" encoding="UTF-8"?>
<FictionBook><description><title-info><author><first-name>Anna</first-name><last-name>Writer</last-name></author>
<book-title>Remote Book</book-title></title-info></description><body><section><title><p>One</p></title>
<p>The quick fox jumps over the lazy dog.</p></section></body></FictionBook>"#
        .to_vec()
}

impl Lib {
    fn new() -> Self {
        Self::with(|_| {})
    }

    fn with(adjust: impl FnOnce(&mut Config)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let library = Library::open(dir.path()).unwrap();
        std::fs::write(dir.path().join("book.fb2"), fb2()).unwrap();
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
        let mut config = Config::default();
        config.server.bind = "100.64.0.1".parse().unwrap();
        config.server.allowed_hosts = vec![PROXY_HOST.to_owned()];
        config.server.auth = AuthMode::Password;
        adjust(&mut config);
        config.validate().unwrap();
        let pool = open_read_pool(&library.db_path(), 2).unwrap();
        let scan = Arc::new(BackgroundScan::new(library.clone(), Vec::new()));
        let state = AppState::new(library, &config, pool, Arc::clone(&scan))
            .with_annotations(Arc::clone(&userdata), exporter);
        let router = web::router(state);
        Self {
            _dir: dir,
            userdata,
            local: common::local(router.clone()),
            phone: common::from_peer(router, PHONE),
            scan,
            book,
        }
    }

    fn set_password(&self, id: ProfileId, value: &str) {
        let hash = password::hash(value).unwrap();
        profiles::set_password_hash(&self.userdata.lock(), id, Some(&hash), 0).unwrap();
    }

    fn add_profile(&self, name: &str, role: Role) -> ProfileId {
        profiles::create(&self.userdata.lock(), name, 1, role, 0)
            .unwrap()
            .id
    }

    async fn send(&self, via: Via, request: Request<Body>) -> Reply {
        let router = if via == Via::Phone {
            &self.phone
        } else {
            &self.local
        };
        let response = router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Reply {
            status,
            headers,
            body: String::from_utf8_lossy(&bytes).into_owned(),
        }
    }

    async fn get(&self, via: Via, uri: &str, token: Option<&str>) -> Reply {
        let mut request = Request::get(uri).header(header::HOST, via.host());
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("shelfgauge-profile={token}"));
        }
        self.send(via, request.body(Body::empty()).unwrap()).await
    }

    async fn post(&self, via: Via, uri: &str, token: Option<&str>, body: &str) -> Reply {
        self.post_with(via, uri, token, body, &[]).await
    }

    async fn post_with(
        &self,
        via: Via,
        uri: &str,
        token: Option<&str>,
        body: &str,
        extra: &[(&str, &str)],
    ) -> Reply {
        let mut request = Request::post(uri)
            .header(header::HOST, via.host())
            .header(header::ORIGIN, via.origin())
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded");
        if let Some(token) = token {
            request = request.header(header::COOKIE, format!("shelfgauge-profile={token}"));
        }
        for (name, value) in extra {
            request = request.header(*name, *value);
        }
        self.send(via, request.body(Body::from(body.to_owned())).unwrap())
            .await
    }

    /// Logs into `id` with `value` from `via`; returns the reply.
    async fn login(&self, via: Via, id: ProfileId, value: &str, remember: bool) -> Reply {
        let mut body = format!(
            "password={}&next=%2F",
            percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
        );
        if remember {
            body.push_str("&remember=1");
        }
        self.post(via, &format!("/login/{}", id.0), None, &body)
            .await
    }

    /// A password session of the owner, logged in from the phone.
    async fn phone_session(&self) -> String {
        let reply = self
            .login(Via::Phone, ProfileId::OWNER, OWNER_PASSWORD, true)
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
        reply.token().unwrap()
    }
}

#[tokio::test]
async fn this_computer_is_unchanged() {
    let lib = Lib::new();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    // One profile: no login, no profile UI, even with a password.
    let reply = lib.get(Via::Local, "/", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!reply.body.contains("profile-menu"), "{}", reply.body);
    // The login pages send this computer to the picker (which goes home with one
    // profile).
    for uri in ["/login", "/login/1"] {
        let reply = lib.get(Via::Local, uri, None).await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER, "{uri}");
        assert_eq!(reply.location(), "/profiles?next=/", "{uri}");
    }
    let reply = lib
        .login(Via::Local, ProfileId::OWNER, OWNER_PASSWORD, true)
        .await;
    assert_eq!(reply.location(), "/profiles?next=/");
    assert_eq!(
        reply.token(),
        None,
        "no password session from this computer"
    );
}

#[tokio::test]
async fn other_devices_need_a_login_on_every_route() {
    let lib = Lib::new();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    let book = lib.book;
    let pages = [
        "/".to_owned(),
        "/authors".to_owned(),
        "/author/1".to_owned(),
        "/folder".to_owned(),
        format!("/book/{book}"),
        format!("/book/{book}/read"),
        "/search?q=fox".to_owned(),
        "/errors".to_owned(),
        "/stats".to_owned(),
        "/books".to_owned(),
        "/duplicates".to_owned(),
        "/profile".to_owned(),
        "/settings".to_owned(),
    ];
    let fragments = [
        "/sidebar/authors".to_owned(),
        "/sidebar/folder".to_owned(),
        "/scan/status".to_owned(),
        format!("/cover/{book}"),
        format!("/book/{book}/annotations"),
        format!("/book/{book}/find?q=fox"),
    ];
    let writes = [
        "/scan".to_owned(),
        "/settings".to_owned(),
        "/settings/reread".to_owned(),
        "/settings/profiles".to_owned(),
        "/settings/profiles/1/remove-password".to_owned(),
        format!("/book/{book}/annotations"),
        format!("/book/{book}/position"),
        format!("/book/{book}/finished"),
        "/reading/clear".to_owned(),
        "/profile".to_owned(),
        "/profile/settings".to_owned(),
        "/profiles/leave".to_owned(),
        "/logout".to_owned(),
    ];
    for via in [Via::Phone, Via::Proxy] {
        for uri in &pages {
            let reply = lib.get(via, uri, None).await;
            assert_eq!(reply.status, StatusCode::SEE_OTHER, "{via:?} {uri}");
            assert!(
                reply.location().starts_with("/login?next="),
                "{via:?} {uri}"
            );
        }
        for uri in &fragments {
            let reply = lib.get(via, uri, None).await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{via:?} {uri}");
        }
        for uri in &writes {
            let reply = lib.post(via, uri, None, "").await;
            assert_eq!(reply.status, StatusCode::UNAUTHORIZED, "{via:?} POST {uri}");
        }
    }
    assert!(!lib.scan.is_running(), "no scan was started");
    // The assets stay public.
    let reply = lib.get(Via::Phone, "/assets/style.css", None).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn password_login_from_another_device() {
    let lib = Lib::new();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);

    // One profile: the login goes straight to its form.
    let reply = lib.get(Via::Phone, "/login?next=/stats", None).await;
    assert_eq!(reply.location(), "/login/1?next=/stats");
    let reply = lib.get(Via::Phone, "/login/1?next=/stats", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.body.contains(r#"autocomplete="current-password""#),
        "{}",
        reply.body
    );
    assert!(reply.body.contains("Remember this device"));
    assert!(
        reply.body.contains(r#"name="next" value="/stats""#),
        "{}",
        reply.body
    );

    let reply = lib
        .login(Via::Phone, ProfileId::OWNER, "wrong password", false)
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        reply.body.contains("Wrong password. 4 tries left."),
        "{}",
        reply.body
    );
    assert_eq!(reply.token(), None);

    // Without "Remember this device": a cookie for this browser session only; plain
    // HTTP over the private network: not `Secure`.
    let reply = lib
        .login(Via::Phone, ProfileId::OWNER, OWNER_PASSWORD, false)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/");
    let cookie = reply.set_cookie();
    assert!(
        !cookie.contains("Max-Age") && !cookie.contains("Secure"),
        "{cookie}"
    );
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"),
        "{cookie}"
    );
    let token = reply.token().unwrap();
    let found = session::lookup(
        &lib.userdata.lock(),
        &session::Token::from_cookie(&token),
        0,
    )
    .unwrap()
    .unwrap();
    assert_eq!((found.level, found.remote), (Level::Password, true));

    let reply = lib.get(Via::Phone, "/", Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    // No path of this computer on the library page.
    assert!(
        !reply.body.contains(r#"class="muted path""#),
        "{}",
        reply.body
    );
    // A single profile on another device still has the menu, with Sign out.
    assert!(reply.body.contains("Sign out"), "{}", reply.body);
    assert!(!reply.body.contains("Switch profile"));
    // The same session works through the proxy name and on this computer.
    assert_eq!(
        lib.get(Via::Proxy, "/", Some(&token)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        lib.get(Via::Local, "/", Some(&token)).await.status,
        StatusCode::OK
    );

    // Through the HTTPS proxy, remembered: `Secure` and about 400 days.
    let reply = lib
        .login(Via::Proxy, ProfileId::OWNER, OWNER_PASSWORD, true)
        .await;
    let cookie = reply.set_cookie();
    assert!(
        cookie.contains("; Secure") && cookie.contains("Max-Age=34560000"),
        "{cookie}"
    );

    // Logging out ends the session and goes to the login.
    let reply = lib.post(Via::Phone, "/logout", Some(&token), "").await;
    assert_eq!(reply.location(), "/login");
    assert!(reply.set_cookie().contains("Max-Age=0"));
    let reply = lib.get(Via::Phone, "/", Some(&token)).await;
    assert_eq!(reply.location(), "/login?next=/");
}

#[tokio::test]
async fn local_sessions_and_pins_never_work_remotely() {
    let lib = Lib::new();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    let anna = lib.add_profile("Anna", Role::Reader);
    let pin_hash = pin::hash("2468").unwrap();
    profiles::set_pin_hash(&lib.userdata.lock(), anna, Some(&pin_hash)).unwrap();

    // A local session (from the picker or a PIN) is not a login on another device.
    let local = session::create(
        &lib.userdata.lock(),
        ProfileId::OWNER,
        Level::Local,
        false,
        0,
    )
    .unwrap();
    let reply = lib.get(Via::Phone, "/", Some(local.as_str())).await;
    assert_eq!(reply.location(), "/login?next=/");
    assert_eq!(
        lib.get(Via::Local, "/", Some(local.as_str())).await.status,
        StatusCode::OK
    );

    // The picker, select and the PIN go to the password form; the PIN is not tried.
    let reply = lib.get(Via::Phone, "/profiles?next=/stats", None).await;
    assert_eq!(reply.location(), "/login?next=/stats");
    let reply = lib
        .post(
            Via::Phone,
            &format!("/profiles/{}/select", anna.0),
            None,
            "next=%2F",
        )
        .await;
    assert_eq!(reply.location(), format!("/login/{}?next=/", anna.0));
    assert_eq!(reply.token(), None);
    let reply = lib
        .get(Via::Phone, &format!("/profiles/{}/pin", anna.0), None)
        .await;
    assert_eq!(reply.location(), format!("/login/{}?next=/", anna.0));
    for _ in 0..10 {
        let reply = lib
            .post(
                Via::Phone,
                &format!("/profiles/{}/pin", anna.0),
                None,
                "pin=2468",
            )
            .await;
        assert_eq!(reply.status, StatusCode::SEE_OTHER);
        assert_eq!(reply.token(), None, "a right PIN from another device");
    }
    // The tries from the phone did not lock Anna out at the computer.
    let reply = lib
        .post(
            Via::Local,
            &format!("/profiles/{}/pin", anna.0),
            None,
            "pin=2468&next=%2F",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert!(reply.token().is_some());

    // Anna has no password: shown, but disabled, in the login list.
    let reply = lib.get(Via::Phone, "/login", None).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.body.contains("On this computer only"),
        "{}",
        reply.body
    );
    assert!(reply.body.contains(" disabled"), "{}", reply.body);
    let reply = lib
        .get(Via::Phone, &format!("/login/{}", anna.0), None)
        .await;
    assert!(reply.body.contains("has no password"), "{}", reply.body);
    // Logging in without a password fails (and counts) like a wrong password.
    let reply = lib.login(Via::Phone, anna, "anything at all", false).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("has no password"), "{}", reply.body);
    assert_eq!(reply.token(), None);
}

#[tokio::test]
async fn forwarded_headers_are_ignored() {
    let lib = Lib::new();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    let spoof = [
        ("x-forwarded-for", "127.0.0.1"),
        ("forwarded", "for=127.0.0.1;proto=https"),
        ("x-forwarded-proto", "https"),
        ("x-real-ip", "127.0.0.1"),
    ];
    // Another device claiming to be this computer is still another device.
    let mut request = Request::get("/").header(header::HOST, BIND_HOST);
    for (name, value) in spoof {
        request = request.header(name, value);
    }
    let reply = lib
        .send(Via::Phone, request.body(Body::empty()).unwrap())
        .await;
    assert_eq!(reply.location(), "/login?next=/");
    // A forwarded header only ever makes a request remote: a proxy on this computer
    // that rewrites `Host` to 127.0.0.1 (nginx's default) adds one of them.
    for name in web::security::PROXY_HEADERS {
        let request = Request::get("/")
            .header(header::HOST, LOCAL_HOST)
            .header(*name, "203.0.113.9")
            .body(Body::empty())
            .unwrap();
        let reply = lib.send(Via::Local, request).await;
        assert_eq!(reply.location(), "/login?next=/", "{name}");
    }
    // A header no proxy is known for changes nothing.
    let request = Request::get("/")
        .header(header::HOST, LOCAL_HOST)
        .header("x-forwarded-proto", "https")
        .body(Body::empty())
        .unwrap();
    assert_eq!(lib.send(Via::Local, request).await.status, StatusCode::OK);

    // `X-Forwarded-Proto` no longer makes a cookie `Secure` (0.15.0 read it); the
    // scheme of `Origin` does.
    let reply = lib
        .post_with(
            Via::Phone,
            "/login/1",
            None,
            &format!("password={}", OWNER_PASSWORD.replace(' ', "+")),
            &[("x-forwarded-proto", "https")],
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);
    assert!(
        !reply.set_cookie().contains("Secure"),
        "{}",
        reply.set_cookie()
    );
}

#[tokio::test]
async fn admin_pages_from_other_devices_follow_remote_admin() {
    let lib = Lib::new();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    let token = lib.phone_session().await;
    let reply = lib.get(Via::Phone, "/settings", Some(&token)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(
        reply.body.contains("On this computer only"),
        "{}",
        reply.body
    );
    assert!(reply.body.contains("remote_admin"), "{}", reply.body);
    let reply = lib.post(Via::Phone, "/scan", Some(&token), "").await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(!lib.scan.is_running());
    // The gear and Rescan are not offered there.
    let home = lib.get(Via::Phone, "/", Some(&token)).await;
    assert!(!home.body.contains("settings-link"), "{}", home.body);
    // On this computer the same session is an admin's.
    let reply = lib.get(Via::Local, "/settings", Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(
        reply.body.contains("Login from other devices"),
        "{}",
        reply.body
    );

    let lib = Lib::with(|config| config.server.remote_admin = true);
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    let token = lib.phone_session().await;
    let reply = lib.get(Via::Phone, "/settings", Some(&token)).await;
    assert_eq!(reply.status, StatusCode::OK);
    // `[server]` and `[hub]` change only on this computer, also for a remote admin.
    assert!(!reply.body.contains("/settings/server\""), "{}", reply.body);
    for (path, post) in [
        ("/settings/server", false),
        ("/settings/server", true),
        ("/settings/restart", true),
    ] {
        let reply = if post {
            lib.post(Via::Phone, path, Some(&token), "action=save")
                .await
        } else {
            lib.get(Via::Phone, path, Some(&token)).await
        };
        assert_eq!(reply.status, StatusCode::FORBIDDEN, "{path} post={post}");
    }
    // Not even an admin removes the password of this very login from another device.
    let reply = lib
        .post(
            Via::Phone,
            "/settings/profiles/1/remove-password",
            Some(&token),
            "",
        )
        .await;
    assert_eq!(reply.location(), "/settings?profile=own-password#profiles");
    assert!(
        profiles::get(&lib.userdata.lock(), ProfileId::OWNER)
            .unwrap()
            .unwrap()
            .has_password
    );
    assert_eq!(
        lib.get(Via::Phone, "/", Some(&token)).await.status,
        StatusCode::OK
    );
    let home = lib.get(Via::Phone, "/", Some(&token)).await;
    assert!(home.body.contains("settings-link"), "{}", home.body);
    // A reader stays a reader.
    let anna = lib.add_profile("Anna", Role::Reader);
    lib.set_password(anna, "anna's long password");
    let reply = lib
        .login(Via::Phone, anna, "anna's long password", false)
        .await;
    let anna_token = reply.token().unwrap();
    let reply = lib.get(Via::Phone, "/settings", Some(&anna_token)).await;
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert!(reply.body.contains("admin role"), "{}", reply.body);
}

#[tokio::test]
async fn wrong_passwords_lock_the_profile_and_a_direct_address() {
    let lib = Lib::new();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    for left in (1..5).rev() {
        let reply = lib
            .login(Via::Proxy, ProfileId::OWNER, "not the password", false)
            .await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        let tries = if left == 1 { "try" } else { "tries" };
        assert!(
            reply.body.contains(&format!("{left} {tries} left")),
            "{}",
            reply.body
        );
    }
    let reply = lib
        .login(Via::Proxy, ProfileId::OWNER, "not the password", false)
        .await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    // Locked: even the right password waits.
    let reply = lib
        .login(Via::Proxy, ProfileId::OWNER, OWNER_PASSWORD, false)
        .await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    assert!(
        reply.body.contains("Too many wrong passwords"),
        "{}",
        reply.body
    );
    let reply = lib.get(Via::Proxy, "/login/1", None).await;
    assert!(reply.body.contains("Too many wrong passwords") && reply.body.contains(" disabled"));

    // One directly connected client cannot walk through all profiles: after 20
    // failures it is refused for every profile.
    let lib = Lib::new();
    let ids: Vec<ProfileId> = (0..6)
        .map(|i| {
            let id = lib.add_profile(&format!("P{i}"), Role::Reader);
            lib.set_password(id, &format!("password of P{i}"));
            id
        })
        .collect();
    let mut failures = 0;
    'outer: for id in &ids {
        for _ in 0..4 {
            let reply = lib.login(Via::Phone, *id, "a wrong password", false).await;
            if failures == 20 {
                assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
                assert!(reply.body.contains("from this device"), "{}", reply.body);
                break 'outer;
            }
            assert_eq!(
                reply.status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "try {failures}"
            );
            failures += 1;
        }
    }
    assert_eq!(failures, 20);
    let reply = lib.login(Via::Phone, ids[5], "password of P5", false).await;
    assert_eq!(
        reply.status,
        StatusCode::TOO_MANY_REQUESTS,
        "the right one too"
    );
    // Under a name from `allowed_hosts` (a MagicDNS name) it is the same client.
    let request = Request::post(format!("/login/{}", ids[5].0))
        .header(header::HOST, PROXY_HOST)
        .header(header::ORIGIN, format!("https://{PROXY_HOST}"))
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Body::from("password=password+of+P5"))
        .unwrap();
    let reply = lib.send(Via::Phone, request).await;
    assert_eq!(reply.status, StatusCode::TOO_MANY_REQUESTS);
    // Behind the proxy on this computer all clients share one address: only the
    // per-profile lockout.
    let reply = lib.login(Via::Proxy, ids[5], "password of P5", false).await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
}

#[tokio::test]
async fn password_changes_and_removal() {
    let lib = Lib::new();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    let phone = lib.phone_session().await;
    let laptop = lib.phone_session().await;
    let at_home = session::create(
        &lib.userdata.lock(),
        ProfileId::OWNER,
        Level::Local,
        false,
        0,
    )
    .unwrap();

    // The current password is needed to change it.
    let reply = lib
        .post(
            Via::Phone,
            "/profile",
            Some(&phone),
            "action=set-password&current_password=nope&new_password=a+new+passphrase&confirm_password=a+new+passphrase",
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("Wrong password."), "{}", reply.body);
    let reply = lib
        .post(
            Via::Phone,
            "/profile",
            Some(&phone),
            "action=set-password&current_password=correct+horse+battery&new_password=short&confirm_password=short",
        )
        .await;
    assert!(
        reply.body.contains("at least 10 characters"),
        "{}",
        reply.body
    );
    let reply = lib
        .post(
            Via::Phone,
            "/profile",
            Some(&phone),
            "action=set-password&current_password=correct+horse+battery&new_password=a+new+passphrase&confirm_password=a+new+passphrase",
        )
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER, "{}", reply.body);
    assert_eq!(reply.location(), "/profile?done=password-set");
    // This device stays logged in, the other one logs in again, the local session stays.
    assert_eq!(
        lib.get(Via::Phone, "/", Some(&phone)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        lib.get(Via::Phone, "/", Some(&laptop)).await.location(),
        "/login?next=/"
    );
    assert_eq!(
        lib.get(Via::Local, "/", Some(at_home.as_str()))
            .await
            .status,
        StatusCode::OK
    );
    let reply = lib
        .login(Via::Phone, ProfileId::OWNER, "a new passphrase", false)
        .await;
    assert_eq!(reply.status, StatusCode::SEE_OTHER);

    // Not removable from another device: it would cut off this very login.
    let reply = lib
        .post(
            Via::Phone,
            "/profile",
            Some(&phone),
            "action=remove-password&current_password=a+new+passphrase",
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        reply.body.contains("only on the computer"),
        "{}",
        reply.body
    );
    // Nor is a PIN checked or set from there.
    let reply = lib
        .post(
            Via::Phone,
            "/profile",
            Some(&phone),
            "action=set-pin&new_pin=2468&confirm_pin=2468",
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        !profiles::get(&lib.userdata.lock(), ProfileId::OWNER)
            .unwrap()
            .unwrap()
            .has_pin
    );
    let page = lib.get(Via::Phone, "/profile", Some(&phone)).await;
    assert!(
        !page.body.contains(r#"value="remove-password""#),
        "{}",
        page.body
    );
    assert!(
        !page.body.contains(r#"id="pin""#),
        "no PIN section remotely"
    );

    // An admin on this computer removes another profile's password and its logins.
    let anna = lib.add_profile("Anna", Role::Reader);
    lib.set_password(anna, "anna's long password");
    let anna_token = lib
        .login(Via::Phone, anna, "anna's long password", true)
        .await
        .token()
        .unwrap();
    let reply = lib
        .post(
            Via::Local,
            &format!("/settings/profiles/{}/remove-password", anna.0),
            Some(at_home.as_str()),
            "",
        )
        .await;
    assert_eq!(
        reply.location(),
        "/settings?profile=password-removed#profiles"
    );
    assert!(
        !profiles::get(&lib.userdata.lock(), anna)
            .unwrap()
            .unwrap()
            .has_password
    );
    assert_eq!(
        lib.get(Via::Phone, "/", Some(&anna_token)).await.location(),
        "/login?next=/"
    );
    // On this computer the owner removes their own password.
    let reply = lib
        .post(
            Via::Local,
            "/profile",
            Some(at_home.as_str()),
            "action=remove-password&current_password=a+new+passphrase",
        )
        .await;
    assert_eq!(reply.location(), "/profile?done=password-removed");
    assert_eq!(
        lib.get(Via::Phone, "/", Some(&phone)).await.location(),
        "/login?next=/"
    );
}

/// An error page for another device without a login shows nothing of the owner's
/// profile (the error layer resolves the profile like the profile middleware).
#[tokio::test]
async fn error_pages_show_no_profile_without_a_login() {
    let lib = Lib::new();
    profiles::rename(&lib.userdata.lock(), ProfileId::OWNER, "Ownername").unwrap();
    lib.set_password(ProfileId::OWNER, OWNER_PASSWORD);
    for via in [Via::Phone, Via::Proxy] {
        let reply = lib.get(via, "/no/such/page", None).await;
        assert_eq!(reply.status, StatusCode::NOT_FOUND, "{via:?}");
        assert!(
            !reply.body.contains("Ownername") && !reply.body.contains("settings-link"),
            "{}",
            reply.body
        );
    }
    let reply = lib.get(Via::Local, "/no/such/page", None).await;
    assert!(reply.body.contains("settings-link"), "{}", reply.body);
}
