//! The hub server end to end through its router: libraries and hub invite codes,
//! member invites, sign-in, passwords, devices, the owner's pairing, blobs, library
//! isolation, quotas, expiry, takedown, sync and the limits of every route.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{HeaderMap, Method, Request, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use shelfgauge::hub::config::{HubConfig, Registration};
use shelfgauge::hub::{Hub, HubError, HubState};
use shelfgauge::publication::format::PublicationId;
use tower::ServiceExt;

const HOST: &str = "localhost:8743";

/// The start of the test clock (Unix seconds).
const T0: i64 = 1_800_000_000;

/// The header naming the account a library's token syncs.
const ACCOUNT: &str = "x-shelfgauge-account";

/// A hub in a temporary directory with a settable clock.
struct TestHub {
    dir: tempfile::TempDir,
    state: HubState,
    app: Router,
    clock: Arc<AtomicI64>,
}

/// An answer: status, headers, body.
struct Answer {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Answer {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| {
            panic!("{e}: {:?}", String::from_utf8_lossy(&self.body));
        })
    }

    fn error(&self) -> String {
        self.json()["error"].as_str().unwrap().to_owned()
    }
}

/// A library on the hub: its public id, token and owner account.
#[derive(Clone)]
struct Lib {
    id: String,
    token: String,
    owner: String,
    handle: String,
}

/// A device token of an account.
#[derive(Clone)]
struct Device {
    account: String,
    id: i64,
    token: String,
}

impl TestHub {
    /// A hub with open registration (no per-address library limit) and `edit`
    /// applied to the config.
    fn new(edit: impl FnOnce(&mut HubConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = HubConfig::default();
        config.libraries.registration = Registration::Open;
        config.libraries.new_libraries_per_address_per_day = 0;
        edit(&mut config);
        let hub = Hub::with_config(dir.path(), config).unwrap();
        let clock = Arc::new(AtomicI64::new(T0));
        let state = {
            let clock = Arc::clone(&clock);
            HubState::with_clock(hub, move || clock.load(Ordering::Relaxed))
        };
        let app = shelfgauge::hub::router(state.clone())
            .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50000))));
        Self {
            dir,
            state,
            app,
            clock,
        }
    }

    fn open() -> Self {
        Self::new(|_| {})
    }

    fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs, Ordering::Relaxed);
    }

    fn now(&self) -> i64 {
        self.clock.load(Ordering::Relaxed)
    }

    async fn call(&self, method: Method, uri: &str, token: Option<&str>, body: Body) -> Answer {
        self.call_with(method, uri, token, body, |r| r).await
    }

    async fn call_with(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        body: Body,
        edit: impl FnOnce(axum::http::request::Builder) -> axum::http::request::Builder,
    ) -> Answer {
        let mut request = edit(Request::builder().method(method).uri(uri));
        if !request
            .headers_ref()
            .is_some_and(|h| h.contains_key(header::HOST))
        {
            request = request.header(header::HOST, HOST);
        }
        if let Some(token) = token {
            request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        let request = request.body(body).unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        Answer {
            status,
            headers,
            body: body.to_vec(),
        }
    }

    async fn get(&self, uri: &str, token: Option<&str>) -> Answer {
        self.call(Method::GET, uri, token, Body::empty()).await
    }

    /// A JSON request with `method`.
    async fn send(&self, method: Method, uri: &str, token: Option<&str>, body: Value) -> Answer {
        self.call_with(method, uri, token, Body::from(body.to_string()), |r| {
            r.header(header::CONTENT_TYPE, "application/json")
        })
        .await
    }

    async fn post(&self, uri: &str, token: Option<&str>, body: Value) -> Answer {
        self.send(Method::POST, uri, token, body).await
    }

    async fn put(&self, uri: &str, token: &str, data: Vec<u8>) -> Answer {
        self.call(Method::PUT, uri, Some(token), Body::from(data))
            .await
    }

    async fn delete(&self, uri: &str, token: &str) -> Answer {
        self.call(Method::DELETE, uri, Some(token), Body::empty())
            .await
    }

    /// A sync request of a library's token for the account `account`.
    async fn sync_as(&self, lib: &Lib, account: &str, body: Option<Value>) -> Answer {
        let account = account.to_owned();
        match body {
            Some(body) => {
                self.call_with(
                    Method::POST,
                    "/api/sync",
                    Some(&lib.token),
                    Body::from(body.to_string()),
                    move |r| {
                        r.header(header::CONTENT_TYPE, "application/json")
                            .header(ACCOUNT, account)
                    },
                )
                .await
            }
            None => {
                self.call_with(
                    Method::GET,
                    "/api/sync",
                    Some(&lib.token),
                    Body::empty(),
                    move |r| r.header(ACCOUNT, account),
                )
                .await
            }
        }
    }

    /// Creates the library `handle` (with the hub invite code `invite` if given).
    async fn try_create_library(&self, invite: Option<&str>, handle: &str) -> Answer {
        let mut body = json!({ "handle": handle, "owner_name": "admin" });
        if let Some(invite) = invite {
            body["invite"] = json!(invite);
        }
        self.post("/api/libraries", None, body).await
    }

    async fn library(&self, handle: &str) -> Lib {
        let answer = self.try_create_library(None, handle).await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
        let json = answer.json();
        Lib {
            id: json["library"].as_str().unwrap().to_owned(),
            token: json["token"].as_str().unwrap().to_owned(),
            owner: json["owner"].as_str().unwrap().to_owned(),
            handle: handle.to_owned(),
        }
    }

    /// A member invite of `lib` (for a new person, or a reset of `account`); its
    /// secret.
    async fn invite(&self, lib: &Lib, account: Option<&str>) -> String {
        let mut body = json!({ "payload": b64(b"sealed payload") });
        if let Some(account) = account {
            body["account"] = json!(account);
        }
        let answer = self
            .post("/api/library/invites", Some(&lib.token), body)
            .await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
        answer.json()["secret"].as_str().unwrap().to_owned()
    }

    async fn redeem(&self, secret: &str, name: Option<&str>, password: u8) -> Answer {
        let mut body = json!({ "secret": secret, "password": password_body(password) });
        if let Some(name) = name {
            body["name"] = json!(name);
        }
        self.post("/api/invites/redeem", None, body).await
    }

    /// A new member `name` of `lib` with the password `password`, signed in on one
    /// device.
    async fn member(&self, lib: &Lib, name: &str, password: u8) -> Device {
        let secret = self.invite(lib, None).await;
        let answer = self.redeem(&secret, Some(name), password).await;
        assert_eq!(answer.status, StatusCode::CREATED, "{}", answer.json());
        issued(&answer)
    }

    async fn login(&self, address: &str, password: u8) -> Answer {
        self.post(
            "/api/login",
            None,
            json!({ "address": address, "login_key": b64(&[password; 32]) }),
        )
        .await
    }

    /// Pairs a phone with the owner account of `lib`.
    async fn pair_owner(&self, lib: &Lib) -> Device {
        let secret = self
            .post("/api/pairings", Some(&lib.token), json!({}))
            .await;
        assert_eq!(secret.status, StatusCode::CREATED);
        let secret = secret.json()["secret"].as_str().unwrap().to_owned();
        let paired = self
            .post("/api/pair", None, json!({ "secret": secret }))
            .await;
        assert_eq!(paired.status, StatusCode::CREATED);
        issued(&paired)
    }

    /// Publishes a manifest of `len` bytes as publication `id` of `lib`.
    async fn publish(&self, lib: &Lib, id: &str, len: usize) -> Answer {
        self.put(&format!("/api/pub/{id}/manifest"), &lib.token, blob(len, 0))
            .await
    }

    fn blob_files(&self) -> usize {
        walk(&self.dir.path().join("blobs"))
    }
}

fn walk(dir: &std::path::Path) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .map(|e| {
            let path = e.unwrap().path();
            if path.is_dir() { walk(&path) } else { 1 }
        })
        .sum()
}

fn issued(answer: &Answer) -> Device {
    let json = answer.json();
    Device {
        account: json["account"].as_str().unwrap().to_owned(),
        id: json["device"].as_i64().unwrap(),
        token: json["token"].as_str().unwrap().to_owned(),
    }
}

/// A password as the browser sends it: login key, salt and bundle all of `byte`.
fn password_body(byte: u8) -> Value {
    json!({
        "login_key": b64(&[byte; 32]),
        "salt": b64(&[byte; 16]),
        "kdf": { "memory_kib": 19456, "iterations": 2 },
        "bundle": b64(&[byte; 40]),
    })
}

/// A well-formed sealed blob (format version 2, no header) of `len` bytes after the
/// framing.
fn blob(len: usize, fill: u8) -> Vec<u8> {
    let mut data = vec![0, 2, 0, 0];
    data.extend_from_slice(&[7; 12]);
    data.extend(std::iter::repeat_n(fill, len.max(16)));
    data
}

fn pub_id(byte: u8) -> String {
    PublicationId::new([byte; 16]).to_string()
}

fn b64(data: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(data)
}

#[tokio::test]
async fn info_is_public_and_tells_the_limits() {
    let hub = TestHub::new(|c| c.limits.publication_expiry_days = 30);
    let info = hub.get("/api/info", None).await;
    assert_eq!(info.status, StatusCode::OK);
    let json = info.json();
    assert_eq!(json["service"], "shelfgauge-hub");
    assert_eq!(json["formats"], json!({ "min": 2, "max": 2 }));
    assert_eq!(json["registration"], "open");
    assert_eq!(json["limits"]["max_blob_bytes"], 64 * 1024 * 1024);
    assert_eq!(json["limits"]["publication_expiry_days"], 30);
    assert_eq!(json["limits"]["max_library_accounts"], 16);
    assert_eq!(json["limits"]["member_invite_max_days"], 7);
    assert_eq!(json["quotas"]["max_devices"], 5);
    assert_eq!(
        json["passwords"],
        json!({ "algorithm": "argon2id", "memory_kib": 19456, "iterations": 2, "parallelism": 1 })
    );
    assert_eq!(info.headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn creating_a_library_needs_a_one_time_invite_by_default() {
    let hub = TestHub::new(|c| c.libraries = Default::default());
    let refused = hub.try_create_library(None, "smiths").await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.error(), "invite_required");
    let (code, expires) = hub.state.hub().invite(hub.now()).unwrap();
    assert_eq!(expires, hub.now() + 7 * 86_400);
    let wrong = hub.try_create_library(Some("not-the-code"), "smiths").await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let created = hub.try_create_library(Some(code.as_str()), "smiths").await;
    assert_eq!(created.status, StatusCode::CREATED);
    let again = hub.try_create_library(Some(code.as_str()), "joneses").await;
    assert_eq!(again.error(), "invite_required", "used up");
    let late = hub.state.hub().invite(hub.now()).unwrap().0;
    hub.advance(7 * 86_400);
    let expired = hub.try_create_library(Some(late.as_str()), "joneses").await;
    assert_eq!(expired.error(), "invite_required");
}

#[tokio::test]
async fn handles_are_unique_and_follow_the_format() {
    let hub = TestHub::new(|c| c.libraries.max_libraries = 2);
    let lib = hub.library("Smiths").await;
    assert_eq!(lib.handle, "Smiths");
    let taken = hub.try_create_library(None, " smiths ").await;
    assert_eq!(taken.status, StatusCode::CONFLICT);
    assert_eq!(taken.error(), "handle_taken");
    for bad in ["ab", "a b c", "-abc", "смиты", &"x".repeat(33)] {
        let answer = hub.try_create_library(None, bad).await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST, "{bad}");
    }
    let bad_owner = hub
        .post(
            "/api/libraries",
            None,
            json!({ "handle": "joneses", "owner_name": "a" }),
        )
        .await;
    assert_eq!(bad_owner.status, StatusCode::BAD_REQUEST);
    hub.library("joneses").await;
    let full = hub.try_create_library(None, "third").await;
    assert_eq!(full.status, StatusCode::FORBIDDEN);
    assert_eq!(full.error(), "quota_exceeded");
}

#[tokio::test]
async fn the_api_needs_a_valid_token() {
    let hub = TestHub::open();
    for uri in ["/api/account", "/api/devices", "/api/pub", "/api/sync"] {
        let answer = hub.get(uri, None).await;
        assert_eq!(answer.status, StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(answer.headers[header::WWW_AUTHENTICATE], "Bearer");
        assert_eq!(answer.error(), "unauthorized");
        let wrong = hub.get(uri, Some("made-up")).await;
        assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{uri}");
    }
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let library = hub.get("/api/account", Some(&lib.token)).await.json();
    assert_eq!(library["library"], lib.id.as_str());
    assert_eq!(library["handle"], "smiths");
    assert_eq!(library["owner"], lib.owner.as_str());
    assert_eq!(library["usage"]["accounts"], 2);
    assert!(library.get("account").is_none());
    let member = hub.get("/api/account", Some(&olga.token)).await.json();
    assert_eq!(member["library"], lib.id.as_str());
    assert_eq!(member["account"], olga.account.as_str());
    assert_eq!(member["name"], "olga");
    assert_eq!(member["role"], "reader");
    assert_eq!(member["device"], olga.id);
    assert_eq!(member["has_password"], true);
}

#[tokio::test]
async fn library_tokens_and_member_devices_keep_to_their_routes() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let id = pub_id(1);
    assert_eq!(hub.publish(&lib, &id, 10).await.status, StatusCode::CREATED);
    // A member reads, never publishes or deletes.
    let upload = hub
        .put(
            &format!("/api/pub/{}/manifest", pub_id(2)),
            &olga.token,
            blob(10, 0),
        )
        .await;
    assert_eq!(upload.status, StatusCode::FORBIDDEN);
    assert_eq!(upload.error(), "forbidden");
    let replace = hub
        .put(&format!("/api/pub/{id}/manifest"), &olga.token, blob(10, 1))
        .await;
    assert_eq!(replace.status, StatusCode::FORBIDDEN);
    assert_eq!(
        hub.delete(&format!("/api/pub/{id}"), &olga.token)
            .await
            .status,
        StatusCode::FORBIDDEN
    );
    for (method, uri) in [
        (Method::GET, "/api/library/accounts"),
        (Method::POST, "/api/library/invites"),
        (Method::POST, "/api/pairings"),
        (Method::DELETE, "/api/library"),
        (Method::GET, "/api/library/owner/devices"),
    ] {
        let answer = hub
            .send(
                method.clone(),
                uri,
                Some(&olga.token),
                json!({ "payload": "" }),
            )
            .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    // The library's token is not a device.
    for (method, uri) in [
        (Method::GET, "/api/devices"),
        (Method::GET, "/api/bundle"),
        (Method::DELETE, "/api/account"),
    ] {
        let answer = hub
            .send(
                method.clone(),
                uri,
                Some(&lib.token),
                json!({ "login_key": b64(&[0; 32]) }),
            )
            .await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{method} {uri}");
    }
    let got = hub
        .get(&format!("/api/pub/{id}/manifest"), Some(&olga.token))
        .await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.body, blob(10, 0));
}

#[tokio::test]
async fn an_invite_creates_one_account_and_is_spent_only_by_it() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let created = hub
        .post(
            "/api/library/invites",
            Some(&lib.token),
            json!({ "payload": b64(b"sealed"), "days": 30 }),
        )
        .await
        .json();
    // The lifetime is capped by `member_invite_max_days`.
    assert_eq!(created["expires_at"], hub.now() + 7 * 86_400);
    let secret = created["secret"].as_str().unwrap();
    let invite_id = created["id"].as_str().unwrap();
    let preview = hub
        .post("/api/invites/preview", None, json!({ "secret": secret }))
        .await;
    assert_eq!(preview.status, StatusCode::OK);
    assert_eq!(
        preview.json(),
        json!({
            "kind": "new",
            "handle": "smiths",
            "name": null,
            "payload": b64(b"sealed"),
            "expires_at": hub.now() + 7 * 86_400,
        })
    );
    // Opening it twice spends nothing; a taken name neither.
    hub.post("/api/invites/preview", None, json!({ "secret": secret }))
        .await;
    let taken = hub.redeem(secret, Some("Admin"), 1).await;
    assert_eq!(taken.status, StatusCode::CONFLICT);
    assert_eq!(taken.error(), "name_taken");
    let bad = hub.redeem(secret, Some("o"), 1).await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    let weak = hub
        .post(
            "/api/invites/redeem",
            None,
            json!({
                "secret": secret,
                "name": "olga",
                "password": {
                    "login_key": b64(&[1; 32]),
                    "salt": b64(&[1; 16]),
                    "kdf": { "memory_kib": 19455, "iterations": 2 },
                    "bundle": b64(&[1; 40]),
                },
            }),
        )
        .await;
    assert_eq!(weak.status, StatusCode::BAD_REQUEST, "below the minimum");
    let redeemed = hub.redeem(secret, Some(" Olga "), 1).await;
    assert_eq!(redeemed.status, StatusCode::CREATED);
    let olga = issued(&redeemed);
    assert_eq!(redeemed.json()["library"], lib.id.as_str());
    let again = hub.redeem(secret, Some("olga2"), 1).await;
    assert_eq!(again.status, StatusCode::FORBIDDEN);
    assert_eq!(again.error(), "invalid_secret");
    let preview = hub
        .post("/api/invites/preview", None, json!({ "secret": secret }))
        .await;
    assert_eq!(preview.error(), "invalid_secret");

    let list = hub
        .get("/api/library/accounts", Some(&lib.token))
        .await
        .json();
    let accounts = list["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    assert_eq!(accounts[0]["id"], lib.owner.as_str());
    assert_eq!(accounts[0]["role"], "owner");
    assert_eq!(accounts[0]["has_password"], false);
    assert_eq!(accounts[1]["id"], olga.account.as_str());
    assert_eq!(accounts[1]["name"], "olga");
    assert_eq!(accounts[1]["invite"], invite_id);
    assert_eq!(accounts[1]["devices"], 1);
    assert_eq!(accounts[1]["last_seen_at"], hub.now());

    // An expired invite and a withdrawn one spend nothing.
    let late = hub.invite(&lib, None).await;
    hub.advance(7 * 86_400);
    assert_eq!(
        hub.redeem(&late, Some("ivan"), 2).await.error(),
        "invalid_secret"
    );
    let withdrawn = hub
        .post(
            "/api/library/invites",
            Some(&lib.token),
            json!({ "payload": b64(b"p") }),
        )
        .await
        .json();
    let path = format!("/api/library/invites/{}", withdrawn["id"].as_str().unwrap());
    assert_eq!(
        hub.delete(&path, &lib.token).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.delete(&path, &lib.token).await.status,
        StatusCode::NOT_FOUND
    );
    let secret = withdrawn["secret"].as_str().unwrap();
    assert_eq!(
        hub.redeem(secret, Some("ivan"), 2).await.error(),
        "invalid_secret"
    );
}

#[tokio::test]
async fn a_library_holds_at_most_sixteen_accounts_with_pending_invites() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    hub.member(&lib, "olga", 1).await;
    for _ in 0..14 {
        hub.invite(&lib, None).await;
    }
    let full = hub
        .post(
            "/api/library/invites",
            Some(&lib.token),
            json!({ "payload": b64(b"p") }),
        )
        .await;
    assert_eq!(full.status, StatusCode::FORBIDDEN);
    assert_eq!(full.error(), "quota_exceeded");
    // A reset is no new account.
    let olga = hub
        .get("/api/library/accounts", Some(&lib.token))
        .await
        .json()["accounts"][1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    hub.invite(&lib, Some(&olga)).await;
    let owner = hub
        .post(
            "/api/library/invites",
            Some(&lib.token),
            json!({ "payload": b64(b"p"), "account": lib.owner }),
        )
        .await;
    assert_eq!(
        owner.status,
        StatusCode::FORBIDDEN,
        "the owner is not reset"
    );
    let other = hub.library("joneses").await;
    let foreign = hub
        .post(
            "/api/library/invites",
            Some(&other.token),
            json!({ "payload": b64(b"p"), "account": olga }),
        )
        .await;
    assert_eq!(
        foreign.status,
        StatusCode::NOT_FOUND,
        "another library's account"
    );
}

#[tokio::test]
async fn unknown_addresses_get_the_same_fake_salt_every_time() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    hub.member(&lib, "olga", 1).await;
    let hub = &hub;
    let prelogin = |address: &'static str| async move {
        hub.post("/api/prelogin", None, json!({ "address": address }))
            .await
    };
    let real = prelogin("olga@smiths").await.json();
    assert_eq!(real["salt"], b64(&[1; 16]));
    assert_eq!(real["kdf"]["memory_kib"], 19456);
    let unknown_name = prelogin("nobody@smiths").await.json();
    let unknown_handle = prelogin("olga@nowhere").await.json();
    let no_password = prelogin("admin@smiths").await.json();
    for fake in [&unknown_name, &unknown_handle, &no_password] {
        let salt = URL_SAFE_NO_PAD
            .decode(fake["salt"].as_str().unwrap())
            .unwrap();
        assert_eq!(salt.len(), 16, "the shape of a real salt");
        assert_eq!(fake["kdf"], real["kdf"]);
        assert_ne!(fake["salt"], real["salt"]);
    }
    assert_ne!(unknown_name["salt"], unknown_handle["salt"]);
    assert_eq!(prelogin("nobody@smiths").await.json(), unknown_name);
    assert_eq!(prelogin(" NoBody@Smiths ").await.json(), unknown_name);
    assert_eq!(
        prelogin("nobody").await.status,
        StatusCode::BAD_REQUEST,
        "the format is checked"
    );
}

#[tokio::test]
async fn signing_in_adds_a_device_and_tells_nothing_about_addresses() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let phone = hub.login("Olga@Smiths", 1).await;
    assert_eq!(phone.status, StatusCode::CREATED);
    let phone = issued(&phone);
    assert_eq!(phone.account, olga.account);
    assert_ne!(phone.token, olga.token);
    for (address, password) in [
        ("olga@smiths", 2),
        ("nobody@smiths", 1),
        ("olga@nowhere", 1),
        ("admin@smiths", 0),
    ] {
        let answer = hub.login(address, password).await;
        assert_eq!(answer.status, StatusCode::FORBIDDEN, "{address}");
        assert_eq!(answer.error(), "invalid_login");
    }
    let short_key = hub
        .post(
            "/api/login",
            None,
            json!({ "address": "olga@smiths", "login_key": b64(&[1; 31]) }),
        )
        .await;
    assert_eq!(short_key.status, StatusCode::BAD_REQUEST);
    let devices = hub.get("/api/devices", Some(&olga.token)).await.json();
    let devices = devices["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0]["current"], true);
    assert_eq!(devices[1]["id"], phone.id);
}

#[tokio::test]
async fn failed_sign_ins_slow_down_and_never_lock() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    hub.member(&lib, "ivan", 2).await;
    for _ in 0..4 {
        assert_eq!(hub.login("olga@smiths", 9).await.error(), "invalid_login");
    }
    let slowed = hub.login("olga@smiths", 1).await;
    assert_eq!(slowed.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(slowed.headers[header::RETRY_AFTER], "1");
    // Another address from the same client, and the signed-in device, go on.
    assert_eq!(
        hub.login("ivan@smiths", 2).await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        hub.get("/api/sync", Some(&olga.token)).await.status,
        StatusCode::OK
    );
    hub.advance(1);
    assert_eq!(hub.login("olga@smiths", 9).await.error(), "invalid_login");
    let longer = hub.login("olga@smiths", 1).await;
    assert_eq!(longer.headers[header::RETRY_AFTER], "2");
    hub.advance(2);
    assert_eq!(
        hub.login("olga@smiths", 1).await.status,
        StatusCode::CREATED
    );
    // The success cleared the address.
    assert_eq!(hub.login("olga@smiths", 9).await.error(), "invalid_login");
}

#[tokio::test]
async fn the_sixth_device_lists_the_devices_and_removes_one_first() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    for _ in 0..4 {
        assert_eq!(
            hub.login("olga@smiths", 1).await.status,
            StatusCode::CREATED
        );
    }
    let sixth = hub.login("olga@smiths", 1).await;
    assert_eq!(sixth.status, StatusCode::CONFLICT);
    assert_eq!(sixth.error(), "device_limit");
    let listed = sixth.json()["devices"].as_array().unwrap().len();
    assert_eq!(listed, 5);
    // Only after the password: a wrong one lists nothing.
    let wrong = hub.login("olga@smiths", 2).await;
    assert_eq!(wrong.error(), "invalid_login");
    assert!(wrong.json().get("devices").is_none());
    let replaced = hub
        .post(
            "/api/login",
            None,
            json!({
                "address": "olga@smiths",
                "login_key": b64(&[1; 32]),
                "remove_device": olga.id,
            }),
        )
        .await;
    assert_eq!(replaced.status, StatusCode::CREATED);
    assert_eq!(
        hub.get("/api/sync", Some(&olga.token)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn changes_that_could_take_the_account_away_ask_the_current_password() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let phone = issued(&hub.login("olga@smiths", 1).await);
    let tablet = issued(&hub.login("olga@smiths", 1).await);

    let bundle = hub.get("/api/bundle", Some(&phone.token)).await.json();
    assert_eq!(bundle["salt"], b64(&[1; 16]));
    assert_eq!(bundle["bundle"], b64(&[1; 40]));
    assert_eq!(bundle["kdf"]["iterations"], 2);

    // A new password.
    let change = |key: u8| {
        hub.post(
            "/api/password",
            Some(&phone.token),
            json!({ "login_key": b64(&[key; 32]), "new": password_body(3) }),
        )
    };
    assert_eq!(change(9).await.error(), "invalid_login");
    assert_eq!(change(1).await.status, StatusCode::NO_CONTENT);
    assert_eq!(hub.login("olga@smiths", 1).await.error(), "invalid_login");
    assert_eq!(
        hub.login("olga@smiths", 3).await.status,
        StatusCode::CREATED
    );
    // The other devices stay signed in and are told.
    let pulled = hub.get("/api/sync", Some(&olga.token)).await.json();
    assert_eq!(
        pulled["password_changed"],
        json!({ "at": hub.now(), "via": "device", "device": phone.id })
    );

    // Another device and a name: the current password; the own device: nothing.
    let hub = &hub;
    let token = phone.token.clone();
    let remove = |key: Option<u8>, id: i64| {
        let token = token.clone();
        async move {
            let body = match key {
                Some(key) => Body::from(json!({ "login_key": b64(&[key; 32]) }).to_string()),
                None => Body::empty(),
            };
            hub.call_with(
                Method::DELETE,
                &format!("/api/devices/{id}"),
                Some(&token),
                body,
                |r| r.header(header::CONTENT_TYPE, "application/json"),
            )
            .await
        }
    };
    assert_eq!(
        remove(None, tablet.id).await.status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(remove(Some(1), tablet.id).await.error(), "invalid_login");
    assert_eq!(
        remove(Some(3), tablet.id).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        remove(Some(3), tablet.id).await.status,
        StatusCode::NOT_FOUND
    );
    let rename = |key: u8, name: &'static str| {
        hub.post(
            "/api/name",
            Some(&olga.token),
            json!({ "login_key": b64(&[key; 32]), "name": name }),
        )
    };
    assert_eq!(rename(1, "olga.s").await.error(), "invalid_login");
    assert_eq!(rename(3, "admin").await.error(), "name_taken");
    assert_eq!(rename(3, "olga.s").await.status, StatusCode::NO_CONTENT);
    assert_eq!(
        hub.login("olga.s@smiths", 3).await.status,
        StatusCode::CREATED
    );
    assert_eq!(hub.login("olga@smiths", 3).await.error(), "invalid_login");
    // The old name is free at once; the old address then belongs to someone else.
    let other = hub.member(&lib, "olga", 4).await;
    assert_ne!(other.account, olga.account);
    assert_eq!(hub.login("olga@smiths", 3).await.error(), "invalid_login");

    // Leaving asks the password; the own device signs out without one.
    let leave = |key: u8| {
        hub.send(
            Method::DELETE,
            "/api/account",
            Some(&olga.token),
            json!({ "login_key": b64(&[key; 32]) }),
        )
    };
    assert_eq!(leave(1).await.error(), "invalid_login");
    assert_eq!(remove(None, phone.id).await.status, StatusCode::NO_CONTENT);
    assert_eq!(
        hub.get("/api/sync", Some(&phone.token)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(leave(3).await.status, StatusCode::NO_CONTENT);
    assert_eq!(
        hub.get("/api/sync", Some(&olga.token)).await.status,
        StatusCode::UNAUTHORIZED,
        "a left account answers like any unknown token"
    );
    let names: Vec<String> = hub
        .get("/api/library/accounts", Some(&lib.token))
        .await
        .json()["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names, ["admin", "olga"]);
}

#[tokio::test]
async fn wrong_current_passwords_slow_the_account_down() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let rename = |key: u8| {
        hub.post(
            "/api/name",
            Some(&olga.token),
            json!({ "login_key": b64(&[key; 32]), "name": "olga.s" }),
        )
    };
    for _ in 0..4 {
        assert_eq!(rename(9).await.error(), "invalid_login");
    }
    assert_eq!(rename(1).await.status, StatusCode::TOO_MANY_REQUESTS);
    hub.advance(1);
    assert_eq!(rename(1).await.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn a_reset_link_sets_a_new_password_and_keeps_everything_else() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    hub.post(
        "/api/sync",
        Some(&olga.token),
        json!({ "records": [{ "kind": "annotation", "id": "a1", "data": b64(b"note") }] }),
    )
    .await;
    let secret = hub.invite(&lib, Some(&olga.account)).await;
    let preview = hub
        .post("/api/invites/preview", None, json!({ "secret": secret }))
        .await
        .json();
    assert_eq!(preview["kind"], "reset");
    assert_eq!(preview["name"], "olga");
    hub.advance(60);
    // The name in a reset is ignored: the address stays.
    let reset = hub.redeem(&secret, Some("other"), 5).await;
    assert_eq!(reset.status, StatusCode::CREATED);
    assert_eq!(issued(&reset).account, olga.account);
    assert_eq!(hub.login("olga@smiths", 1).await.error(), "invalid_login");
    assert_eq!(
        hub.login("olga@smiths", 5).await.status,
        StatusCode::CREATED
    );
    let pulled = hub.get("/api/sync", Some(&olga.token)).await.json();
    assert_eq!(pulled["records"].as_array().unwrap().len(), 1);
    assert_eq!(
        pulled["password_changed"],
        json!({ "at": hub.now(), "via": "reset", "device": null })
    );
}

#[tokio::test]
async fn the_library_manages_its_members() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let phone = issued(&hub.login("olga@smiths", 1).await);
    let ivan = hub.member(&lib, "ivan", 2).await;
    let other = hub.library("joneses").await;
    let account = |id: &str| format!("/api/library/accounts/{id}");

    // Another library's token reaches none of them.
    assert_eq!(
        hub.delete(&account(&olga.account), &other.token)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        hub.delete(&format!("{}/devices", account(&olga.account)), &other.token)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    // Sign out all devices: the account stays, its devices go.
    let signed_out = hub
        .delete(&format!("{}/devices", account(&olga.account)), &lib.token)
        .await;
    assert_eq!(signed_out.json()["removed"], 2);
    for token in [&olga.token, &phone.token] {
        assert_eq!(
            hub.get("/api/sync", Some(token)).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        hub.login("olga@smiths", 1).await.status,
        StatusCode::CREATED
    );
    // Removing a member deletes the account; the owner is removed only with the library.
    assert_eq!(
        hub.delete(&account(&ivan.account), &lib.token).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.get("/api/sync", Some(&ivan.token)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(hub.login("ivan@smiths", 2).await.error(), "invalid_login");
    assert_eq!(
        hub.delete(&account(&lib.owner), &lib.token).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        hub.delete(&account("not-an-id"), &lib.token).await.status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_changed_handle_changes_every_address_and_frees_the_old_one() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let other = hub.library("joneses").await;
    let hub = &hub;
    let change = |token: String, handle: &'static str| async move {
        hub.post(
            "/api/library/handle",
            Some(&token),
            json!({ "handle": handle }),
        )
        .await
    };
    assert_eq!(
        change(lib.token.clone(), "joneses").await.error(),
        "handle_taken"
    );
    assert_eq!(
        change(lib.token.clone(), "smith-family").await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.login("olga@smith-family", 1).await.status,
        StatusCode::CREATED
    );
    assert_eq!(hub.login("olga@smiths", 1).await.error(), "invalid_login");
    // Devices stay signed in; the old handle is free for anyone.
    assert_eq!(
        hub.get("/api/sync", Some(&olga.token)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        change(other.token.clone(), "smiths").await.status,
        StatusCode::NO_CONTENT
    );
    hub.member(&other, "olga", 7).await;
    assert_eq!(hub.login("olga@smiths", 1).await.error(), "invalid_login");
    assert_eq!(
        change(olga.token.clone(), "mine").await.status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn the_owners_phone_pairs_and_its_password_is_set_by_the_library() {
    let hub = TestHub::new(|c| c.quotas.max_devices = 2);
    let lib = hub.library("smiths").await;
    let phone = hub.pair_owner(&lib).await;
    assert_eq!(phone.account, lib.owner);
    let account = hub.get("/api/account", Some(&phone.token)).await.json();
    assert_eq!(account["role"], "owner");
    assert_eq!(account["has_password"], false);

    let secret = hub
        .post("/api/pairings", Some(&lib.token), json!({}))
        .await
        .json();
    assert_eq!(secret["expires_at"], hub.now() + 600);
    let secret = secret["secret"].as_str().unwrap().to_owned();
    let body = json!({ "secret": secret, "device_name": b64(b"encrypted name") });
    let tablet = hub.post("/api/pair", None, body.clone()).await;
    assert_eq!(tablet.status, StatusCode::CREATED);
    let tablet = issued(&tablet);
    let reused = hub.post("/api/pair", None, body).await;
    assert_eq!(reused.error(), "invalid_secret");
    // At the device limit the phone is told; there is no sixth device flow here.
    let third = hub
        .post("/api/pairings", Some(&lib.token), json!({}))
        .await
        .json()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    let refused = hub
        .post("/api/pair", None, json!({ "secret": third }))
        .await;
    assert_eq!(refused.error(), "quota_exceeded");
    let late = hub
        .post("/api/pairings", Some(&lib.token), json!({}))
        .await
        .json()["secret"]
        .as_str()
        .unwrap()
        .to_owned();
    hub.advance(600);
    assert_eq!(
        hub.post("/api/pair", None, json!({ "secret": late }))
            .await
            .error(),
        "invalid_secret"
    );

    // The library lists and removes the owner's devices without a password.
    let devices = hub
        .get("/api/library/owner/devices", Some(&lib.token))
        .await
        .json();
    let devices = devices["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[1]["name"], b64(b"encrypted name"));
    assert_eq!(
        hub.delete(
            &format!("/api/library/owner/devices/{}", tablet.id),
            &lib.token
        )
        .await
        .status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.get("/api/sync", Some(&tablet.token)).await.status,
        StatusCode::UNAUTHORIZED
    );

    // The owner's password and name, set by the library.
    assert_eq!(hub.login("admin@smiths", 6).await.error(), "invalid_login");
    let set = hub
        .send(
            Method::PUT,
            "/api/library/owner/password",
            Some(&lib.token),
            password_body(6),
        )
        .await;
    assert_eq!(set.status, StatusCode::NO_CONTENT);
    assert_eq!(
        hub.login("admin@smiths", 6).await.status,
        StatusCode::CREATED
    );
    let renamed = hub
        .post(
            "/api/library/owner/name",
            Some(&lib.token),
            json!({ "name": "anna" }),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::NO_CONTENT);
    assert_eq!(
        hub.get("/api/library/owner/devices", Some(&lib.token))
            .await
            .json()["devices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(hub.login("anna@smiths", 6).await.error(), "device_limit");
    // The owner cannot leave; a device asks the library for nothing of this.
    let leave = hub
        .send(
            Method::DELETE,
            "/api/account",
            Some(&phone.token),
            json!({ "login_key": b64(&[6; 32]) }),
        )
        .await;
    assert_eq!(leave.status, StatusCode::FORBIDDEN);
    let pulled = hub.get("/api/sync", Some(&phone.token)).await.json();
    assert_eq!(pulled["password_changed"]["via"], "library");
}

#[tokio::test]
async fn the_library_syncs_any_of_its_accounts_by_name() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let other = hub.library("joneses").await;
    let missing = hub
        .post("/api/sync", Some(&lib.token), json!({ "records": [] }))
        .await;
    assert_eq!(missing.status, StatusCode::BAD_REQUEST);
    let pushed = hub
        .sync_as(
            &lib,
            &olga.account,
            Some(json!({ "records": [
                { "kind": "library_key", "id": "k1", "data": b64(b"key") },
                { "kind": "profile", "id": "p", "data": null },
            ] })),
        )
        .await;
    assert_eq!(pushed.status, StatusCode::OK);
    assert_eq!(pushed.json()["seq"], 2);
    let pulled = hub.get("/api/sync", Some(&olga.token)).await.json();
    assert_eq!(pulled["records"][0]["kind"], "library_key");
    assert_eq!(pulled["records"][0]["data"], b64(b"key"));
    hub.post(
        "/api/sync",
        Some(&olga.token),
        json!({ "records": [{ "kind": "annotation", "id": "a", "data": b64(b"mine") }] }),
    )
    .await;
    let seen = hub.sync_as(&lib, &olga.account, None).await.json();
    assert_eq!(seen["records"].as_array().unwrap().len(), 3);
    assert!(seen.get("password_changed").is_none());
    // Its own owner account too; never an account of another library.
    assert_eq!(
        hub.sync_as(&lib, &lib.owner, None).await.status,
        StatusCode::OK
    );
    assert_eq!(
        hub.sync_as(&other, &olga.account, None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        hub.sync_as(&lib, "garbage", None).await.status,
        StatusCode::NOT_FOUND
    );
    // A device's own account is fixed by its token.
    let own = hub
        .call_with(
            Method::GET,
            "/api/sync",
            Some(&olga.token),
            Body::empty(),
            |r| r.header(ACCOUNT, other.owner.clone()),
        )
        .await
        .json();
    assert_eq!(own["records"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn blobs_round_trip_and_are_deleted_with_their_publication() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let id = pub_id(1);
    let manifest = blob(300, b'm');
    let part = blob(1000, b'p');
    let image = blob(20, b'i');
    for (path, data) in [("manifest", &manifest), ("p/0", &part), ("i/0", &image)] {
        let put = hub
            .put(&format!("/api/pub/{id}/{path}"), &lib.token, data.clone())
            .await;
        assert_eq!(put.status, StatusCode::CREATED, "{path}");
    }
    let replaced = hub
        .put(&format!("/api/pub/{id}/p/0"), &lib.token, part.clone())
        .await;
    assert_eq!(replaced.status, StatusCode::NO_CONTENT);
    for token in [&lib.token, &olga.token] {
        let got = hub.get(&format!("/api/pub/{id}/p/0"), Some(token)).await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.body, part);
        assert_eq!(
            got.headers[header::CONTENT_TYPE],
            "application/octet-stream"
        );
        assert_eq!(got.headers[header::CACHE_CONTROL], "private, no-store");
    }
    assert_eq!(hub.blob_files(), 3);

    let list = hub.get("/api/pub", Some(&olga.token)).await.json();
    let publications = list["publications"].as_array().unwrap();
    assert_eq!(publications.len(), 1);
    assert_eq!(publications[0]["id"], id.as_str());
    assert_eq!(publications[0]["blobs"], 3);
    let total = manifest.len() + part.len() + image.len();
    assert_eq!(publications[0]["bytes"], total);
    let usage = hub.get("/api/account", Some(&lib.token)).await.json();
    assert_eq!(usage["usage"]["bytes"], total);

    for path in ["p/1", "x/0", "p/01", "../manifest"] {
        let missing = hub
            .get(&format!("/api/pub/{id}/{path}"), Some(&lib.token))
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        hub.get("/api/pub/NOT-AN-ID/manifest", Some(&lib.token))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let deleted = hub.delete(&format!("/api/pub/{id}"), &lib.token).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        hub.get(&format!("/api/pub/{id}/manifest"), Some(&olga.token))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(hub.blob_files(), 0);
    assert_eq!(
        hub.delete(&format!("/api/pub/{id}"), &lib.token)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_library_never_reaches_another_librarys_data() {
    let hub = TestHub::open();
    let owner = hub.library("smiths").await;
    let olga = hub.member(&owner, "olga", 1).await;
    let other = hub.library("joneses").await;
    let stranger = hub.member(&other, "ivan", 2).await;
    let id = pub_id(2);
    let data = blob(100, 1);
    hub.put(
        &format!("/api/pub/{id}/manifest"),
        &owner.token,
        data.clone(),
    )
    .await;
    hub.post(
        "/api/sync",
        Some(&olga.token),
        json!({ "records": [{ "kind": "annotation", "id": "a1", "data": b64(b"secret") }] }),
    )
    .await;

    for token in [&other.token, &stranger.token] {
        let get = hub
            .get(&format!("/api/pub/{id}/manifest"), Some(token))
            .await;
        assert_eq!(get.status, StatusCode::NOT_FOUND);
        assert_eq!(get.error(), "not_found", "the same answer as a missing id");
        let list = hub.get("/api/pub", Some(token)).await.json();
        assert_eq!(list["publications"], json!([]));
    }
    let put = hub
        .put(&format!("/api/pub/{id}/manifest"), &other.token, blob(5, 9))
        .await;
    assert_eq!(put.status, StatusCode::NOT_FOUND, "never overwritten");
    let put_new_path = hub
        .put(&format!("/api/pub/{id}/p/0"), &other.token, blob(5, 9))
        .await;
    assert_eq!(put_new_path.status, StatusCode::NOT_FOUND);
    assert_eq!(
        hub.delete(&format!("/api/pub/{id}"), &other.token)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    let sync = hub.get("/api/sync", Some(&stranger.token)).await.json();
    assert_eq!(sync["records"], json!([]));
    assert_eq!(sync["seq"], 0);
    let devices = hub.get("/api/devices", Some(&stranger.token)).await.json();
    assert_eq!(devices["devices"].as_array().unwrap().len(), 1);
    let other_device = hub
        .send(
            Method::DELETE,
            &format!("/api/devices/{}", olga.id),
            Some(&stranger.token),
            json!({ "login_key": b64(&[2; 32]) }),
        )
        .await;
    assert_eq!(
        other_device.status,
        StatusCode::NOT_FOUND,
        "another account's device"
    );

    let still = hub
        .get(&format!("/api/pub/{id}/manifest"), Some(&olga.token))
        .await;
    assert_eq!(still.body, data);
}

#[tokio::test]
async fn quotas_limit_publications_and_bytes_per_library_and_devices_per_account() {
    let hub = TestHub::new(|c| {
        c.quotas.max_publications = 1;
        c.quotas.max_bytes = 1000;
        c.quotas.max_devices = 2;
    });
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let put = |id: String, path: &'static str, len: usize| {
        let token = lib.token.clone();
        let hub = &hub;
        async move {
            hub.put(&format!("/api/pub/{id}/{path}"), &token, blob(len, 0))
                .await
        }
    };
    assert_eq!(
        put(pub_id(1), "manifest", 400).await.status,
        StatusCode::CREATED
    );
    let second = put(pub_id(2), "manifest", 10).await;
    assert_eq!(second.status, StatusCode::FORBIDDEN);
    assert_eq!(second.error(), "quota_exceeded");
    let too_much = put(pub_id(1), "p/0", 700).await;
    assert_eq!(too_much.error(), "quota_exceeded");
    assert_eq!(hub.blob_files(), 1, "a refused blob is not written");
    // Replacing a blob counts only the difference.
    assert_eq!(
        put(pub_id(1), "manifest", 900).await.status,
        StatusCode::NO_CONTENT
    );
    // A member's records count against the library.
    let records = hub
        .post(
            "/api/sync",
            Some(&olga.token),
            json!({ "records": [{ "kind": "position", "id": "p", "data": b64(&[0; 200]) }] }),
        )
        .await;
    assert_eq!(records.error(), "quota_exceeded", "records count too");

    assert_eq!(
        hub.login("olga@smiths", 1).await.status,
        StatusCode::CREATED
    );
    assert_eq!(hub.login("olga@smiths", 1).await.error(), "device_limit");
    // Each account has its own device limit.
    hub.member(&lib, "ivan", 2).await;
    assert_eq!(
        hub.login("ivan@smiths", 2).await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn unopened_publications_expire() {
    let hub = TestHub::new(|c| c.limits.publication_expiry_days = 10);
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    for byte in [1, 2] {
        hub.publish(&lib, &pub_id(byte), 10).await;
    }
    hub.advance(9 * 86_400);
    // Opening (fetching the manifest) by a member keeps the first one.
    hub.get(
        &format!("/api/pub/{}/manifest", pub_id(1)),
        Some(&olga.token),
    )
    .await;
    assert_eq!(hub.state.hub().expire(hub.now()).unwrap().0, 0);
    hub.advance(86_400);
    assert_eq!(hub.state.hub().expire(hub.now()).unwrap().0, 1);
    let list = hub.get("/api/pub", Some(&lib.token)).await.json();
    let ids: Vec<&str> = list["publications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [pub_id(1)]);
    assert_eq!(hub.blob_files(), 1);
    // An expired id may be published again (only takedowns are remembered).
    let again = hub.publish(&lib, &pub_id(2), 10).await;
    assert_eq!(again.status, StatusCode::CREATED);
}

#[tokio::test]
async fn expiry_is_off_by_default() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    hub.member(&lib, "olga", 1).await;
    hub.publish(&lib, &pub_id(1), 10).await;
    hub.advance(10_000 * 86_400);
    assert_eq!(hub.state.hub().expire(hub.now()).unwrap(), (0, 0));
}

#[tokio::test]
async fn taken_down_publications_are_gone_for_good() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let id = pub_id(5);
    hub.publish(&lib, &id, 10).await;
    let parsed: PublicationId = id.parse().unwrap();
    assert!(hub.state.hub().takedown(&parsed, hub.now()).unwrap());
    assert_eq!(hub.blob_files(), 0);
    let get = hub
        .get(&format!("/api/pub/{id}/manifest"), Some(&lib.token))
        .await;
    assert_eq!(get.status, StatusCode::GONE);
    assert_eq!(get.error(), "taken_down");
    let put = hub.publish(&lib, &id, 10).await;
    assert_eq!(put.status, StatusCode::GONE);
    assert!(!hub.state.hub().takedown(&parsed, hub.now()).unwrap());
    let libraries = hub.state.hub().libraries().unwrap();
    assert_eq!(libraries.len(), 1);
    assert_eq!(libraries[0].usage.publications, 0);
}

#[tokio::test]
async fn sync_orders_changes_and_pages_with_since() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let phone = hub.pair_owner(&lib).await;
    let pushed = hub
        .sync_as(
            &lib,
            &lib.owner,
            Some(json!({ "records": [
                { "kind": "shelf_item", "id": "s1", "data": b64(b"shelf") },
                { "kind": "annotation", "id": "a1", "data": b64(b"one") },
                { "kind": "annotation", "id": "a2", "data": b64(b"two") },
                { "kind": "position", "id": "p1", "data": b64(b"pos") },
            ] })),
        )
        .await;
    assert_eq!(pushed.status, StatusCode::OK);
    assert_eq!(pushed.json(), json!({ "seqs": [1, 2, 3, 4], "seq": 4 }));

    let page = hub
        .get("/api/sync?since=0&limit=3", Some(&phone.token))
        .await
        .json();
    assert_eq!(page["more"], true);
    assert_eq!(page["seq"], 3);
    let ids: Vec<&str> = page["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["s1", "a1", "a2"]);
    assert_eq!(page["records"][1]["data"], b64(b"one"));
    assert_eq!(page["records"][1]["updated_at"], hub.now());
    let rest = hub
        .get("/api/sync?since=3&limit=3", Some(&phone.token))
        .await
        .json();
    assert_eq!(rest["more"], false);
    assert_eq!(rest["seq"], 4);
    assert_eq!(rest["records"].as_array().unwrap().len(), 1);

    // An update moves the record to the end; a deletion is a tombstone.
    hub.post(
        "/api/sync",
        Some(&phone.token),
        json!({ "records": [
            { "kind": "annotation", "id": "a1", "data": b64(b"edited") },
            { "kind": "annotation", "id": "a2", "data": null },
        ] }),
    )
    .await;
    let after = hub
        .get("/api/sync?since=4", Some(&phone.token))
        .await
        .json();
    assert_eq!(after["seq"], 6);
    assert_eq!(
        after["records"],
        json!([
            { "kind": "annotation", "id": "a1", "seq": 5, "data": b64(b"edited"), "updated_at": hub.now() },
            { "kind": "annotation", "id": "a2", "seq": 6, "data": null, "updated_at": hub.now() },
        ])
    );
    let all = hub.sync_as(&lib, &lib.owner, None).await.json();
    assert_eq!(
        all["records"].as_array().unwrap().len(),
        4,
        "one row per record"
    );
    let ahead = hub
        .get("/api/sync?since=100", Some(&phone.token))
        .await
        .json();
    assert_eq!(
        (ahead["seq"].clone(), ahead["more"].clone()),
        (json!(100), json!(false))
    );
    assert_eq!(
        hub.get("/api/sync?since=x", Some(&phone.token))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn sync_rejects_malformed_records() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let device = hub.member(&lib, "olga", 1).await;
    let push = |records: Value| {
        hub.post(
            "/api/sync",
            Some(&device.token),
            json!({ "records": records }),
        )
    };
    for (records, error) in [
        (
            json!([{ "kind": "note", "id": "a", "data": null }]),
            "bad_request",
        ),
        (
            json!([{ "kind": "annotation", "id": "a/b", "data": null }]),
            "bad_request",
        ),
        (
            json!([{ "kind": "annotation", "id": "", "data": null }]),
            "bad_request",
        ),
        (
            json!([{ "kind": "annotation", "id": "a", "data": "!!" }]),
            "bad_request",
        ),
        (
            json!([{ "kind": "annotation", "id": "a", "extra": 1 }]),
            "bad_request",
        ),
    ] {
        let answer = push(records.clone()).await;
        assert_eq!(answer.error(), error, "{records}");
    }
    let many: Vec<Value> = (0..501)
        .map(|i| json!({ "kind": "annotation", "id": format!("a{i}"), "data": null }))
        .collect();
    assert_eq!(
        push(json!(many)).await.status,
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let big = b64(&vec![0; 1024 * 1024 + 1]);
    let answer = push(json!([{ "kind": "annotation", "id": "a", "data": big }])).await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    // Nothing of a refused batch is stored.
    let sync = hub.get("/api/sync", Some(&device.token)).await.json();
    assert_eq!(sync["seq"], 0);
}

#[tokio::test]
async fn bodies_are_limited_per_route() {
    let hub = TestHub::new(|c| c.limits.max_blob_bytes = 2048);
    let lib = hub.library("smiths").await;
    let id = pub_id(3);
    let too_big = hub
        .put(&format!("/api/pub/{id}/p/0"), &lib.token, blob(2048, 0))
        .await;
    assert_eq!(too_big.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(too_big.error(), "too_large");
    let fits = hub
        .put(
            &format!("/api/pub/{id}/p/0"),
            &lib.token,
            blob(2048 - 16, 0),
        )
        .await;
    assert_eq!(fits.status, StatusCode::CREATED);
    for malformed in [vec![], vec![0, 1, 2], blob(20, 0)[..20].to_vec(), {
        let mut wrong_version = blob(20, 0);
        wrong_version[1] = 9;
        wrong_version
    }] {
        let answer = hub
            .put(&format!("/api/pub/{id}/p/1"), &lib.token, malformed)
            .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    }
    let long_name = b64(&vec![0; 64 * 1024]);
    let answer = hub
        .post(
            "/api/login",
            None,
            json!({ "address": "olga@smiths", "login_key": b64(&[0; 32]), "device_name": long_name }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    let big_payload = hub
        .post(
            "/api/library/invites",
            Some(&lib.token),
            json!({ "payload": b64(&vec![0; 8193]) }),
        )
        .await;
    assert_eq!(big_payload.status, StatusCode::PAYLOAD_TOO_LARGE);
    let answer = hub
        .call_with(
            Method::POST,
            "/api/libraries",
            None,
            Body::from("{not json"),
            |r| r.header(header::CONTENT_TYPE, "application/json"),
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    assert!(
        !String::from_utf8_lossy(&answer.body).contains("not json"),
        "the body is not echoed"
    );
}

#[tokio::test]
async fn blobs_and_pushes_have_larger_limits_than_json_routes() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let part = hub
        .put(
            &format!("/api/pub/{}/p/0", pub_id(6)),
            &lib.token,
            blob(200 * 1024, 0),
        )
        .await;
    assert_eq!(part.status, StatusCode::CREATED);
    let record = b64(&vec![0; 300 * 1024]);
    let pushed = hub
        .sync_as(
            &lib,
            &lib.owner,
            Some(json!({ "records": [{ "kind": "annotation", "id": "a", "data": record }] })),
        )
        .await;
    assert_eq!(pushed.status, StatusCode::OK);
    let pairing = hub
        .call_with(
            Method::POST,
            "/api/pairings",
            Some(&lib.token),
            Body::from(vec![b' '; 65 * 1024]),
            |r| r,
        )
        .await;
    assert_eq!(pairing.status, StatusCode::CREATED, "pairings read no body");
}

#[tokio::test]
async fn requests_are_rate_limited() {
    let hub = TestHub::new(|c| {
        c.limits.requests_per_minute = 2;
        c.limits.public_requests_per_hour = 2;
    });
    let lib = hub.library("smiths").await;
    for _ in 0..2 {
        assert_eq!(
            hub.get("/api/pub", Some(&lib.token)).await.status,
            StatusCode::OK
        );
    }
    let limited = hub.get("/api/pub", Some(&lib.token)).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers[header::RETRY_AFTER], "30");
    // Without a token: one budget for all loopback peers (a proxy on this machine);
    // the two libraries took it.
    let other = hub.library("joneses").await;
    let secret = hub.invite(&other, None).await;
    let olga = hub.redeem(&secret, Some("olga"), 1).await;
    assert_eq!(olga.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(olga.error(), "rate_limited");
    // The info route is not limited.
    assert_eq!(hub.get("/api/info", None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn devices_and_the_library_token_have_separate_buckets() {
    let hub = TestHub::new(|c| c.limits.requests_per_minute = 2);
    let lib = hub.library("smiths").await;
    // The invite takes one request of the library's token.
    let olga = hub.member(&lib, "olga", 1).await;
    assert_eq!(
        hub.get("/api/pub", Some(&lib.token)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        hub.get("/api/pub", Some(&olga.token)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        hub.get("/api/pub", Some(&lib.token)).await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn open_registration_limits_new_libraries_per_address() {
    let hub = TestHub::new(|c| {
        c.server.trusted_proxies = vec!["127.0.0.1".into()];
        c.libraries.new_libraries_per_address_per_day = 2;
    });
    let create = |client: &'static str, handle: &'static str| {
        let body = json!({ "handle": handle, "owner_name": "admin" });
        hub.call_with(
            Method::POST,
            "/api/libraries",
            None,
            Body::from(body.to_string()),
            move |r| {
                r.header(header::CONTENT_TYPE, "application/json")
                    .header("x-forwarded-for", client)
            },
        )
    };
    // A refused request (a bad body) does not count.
    let bad = hub
        .call_with(
            Method::POST,
            "/api/libraries",
            None,
            Body::from("{}"),
            |r| {
                r.header(header::CONTENT_TYPE, "application/json")
                    .header("x-forwarded-for", "192.0.2.1")
            },
        )
        .await;
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(create("192.0.2.1", "aaa").await.status, StatusCode::CREATED);
    assert_eq!(create("192.0.2.1", "bbb").await.status, StatusCode::CREATED);
    let third = create("192.0.2.1", "ccc").await;
    assert_eq!(third.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(third.error(), "library_limit");
    assert_eq!(third.headers[header::RETRY_AFTER], "86400");
    assert_eq!(create("192.0.2.2", "ddd").await.status, StatusCode::CREATED);
    // IPv6: one /64 is one client.
    assert_eq!(
        create("2001:db8::1", "eee").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        create("2001:db8::2", "fff").await.status,
        StatusCode::CREATED
    );
    assert_eq!(
        create("2001:db8::3", "ggg").await.status,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        create("2001:db8:0:1::1", "hhh").await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn members_joining_from_one_address_are_not_counted_as_new_libraries() {
    let hub = TestHub::new(|c| c.libraries.new_libraries_per_address_per_day = 1);
    let lib = hub.library("smiths").await;
    for name in ["olga", "ivan", "anna"] {
        hub.member(&lib, name, 1).await;
        let address = format!("{name}@smiths");
        assert_eq!(hub.login(&address, 1).await.status, StatusCode::CREATED);
    }
    assert_eq!(
        hub.try_create_library(None, "joneses").await.error(),
        "library_limit"
    );
}

#[tokio::test]
async fn invite_codes_are_not_counted_per_address() {
    let hub = TestHub::new(|c| {
        c.libraries.registration = Registration::Invite;
        c.libraries.new_libraries_per_address_per_day = 1;
    });
    for handle in ["one", "two", "three"] {
        let (code, _) = hub.state.hub().invite(hub.now()).unwrap();
        let answer = hub.try_create_library(Some(code.as_str()), handle).await;
        assert_eq!(answer.status, StatusCode::CREATED);
    }
}

#[tokio::test]
async fn a_full_hub_refuses_new_data_and_says_so() {
    let hub = TestHub::new(|c| c.limits.max_total_bytes = 1000);
    assert_eq!(hub.get("/api/info", None).await.json()["accepting"], true);
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let put = |path: &'static str, len: usize| {
        let token = lib.token.clone();
        let hub = &hub;
        async move {
            hub.put(
                &format!("/api/pub/{}/{path}", pub_id(1)),
                &token,
                blob(len, 0),
            )
            .await
        }
    };
    assert_eq!(put("manifest", 900).await.status, StatusCode::CREATED);
    assert_eq!(put("p/0", 200).await.error(), "hub_full");
    let full = hub
        .post(
            "/api/sync",
            Some(&olga.token),
            json!({ "records": [{ "kind": "position", "id": "p", "data": b64(&[0; 200]) }] }),
        )
        .await;
    assert_eq!(full.status, StatusCode::INSUFFICIENT_STORAGE);
    assert_eq!(full.error(), "hub_full");
    assert_eq!(hub.blob_files(), 1, "a refused blob is not written");
    // A new state for the info route (its answer is cached for a minute); the smallest
    // test blob takes 32 bytes with its framing, so the hub is then exactly full.
    let again = TestHub::new(|c| c.limits.max_total_bytes = 32);
    let lib = again.library("smiths").await;
    again.publish(&lib, &pub_id(1), 10).await;
    assert_eq!(
        again.get("/api/info", None).await.json()["accepting"],
        false
    );
}

#[tokio::test]
async fn a_library_quota_replaces_the_hub_value() {
    let hub = TestHub::new(|c| c.quotas.max_publications = 1);
    let lib = hub.library("smiths").await;
    let first = hub.state.hub().libraries().unwrap()[0].id;
    let own = shelfgauge::hub::LibraryQuotas {
        max_publications: Some(2),
        max_bytes: Some(5000),
    };
    assert!(hub.state.hub().set_library_quotas(first, &own).unwrap());
    for n in [1, 2] {
        let answer = hub.publish(&lib, &pub_id(n), 10).await;
        assert_eq!(answer.status, StatusCode::CREATED);
    }
    let third = hub.publish(&lib, &pub_id(3), 10).await;
    assert_eq!(third.error(), "quota_exceeded");
    let account = hub.get("/api/account", Some(&lib.token)).await.json();
    assert_eq!(account["quotas"]["max_publications"], 2);
    assert_eq!(account["quotas"]["max_bytes"], 5000);
    let info = hub.get("/api/info", None).await.json();
    assert_eq!(info["quotas"]["max_publications"], 1, "the hub's defaults");
}

#[tokio::test]
async fn deleted_and_idle_libraries_and_members_lose_everything() {
    let hub = TestHub::new(|c| c.libraries.expiry_days = 30);
    let idle = hub.library("idle").await;
    let kept = hub.library("kept").await;
    let olga = hub.member(&kept, "olga", 1).await;
    let ivan = hub.member(&kept, "ivan", 2).await;
    let gone = hub.library("gone").await;
    let gone_member = hub.member(&gone, "anna", 3).await;
    hub.publish(&idle, &pub_id(1), 10).await;
    hub.publish(&gone, &pub_id(2), 10).await;
    assert_eq!(hub.blob_files(), 2);
    // The library deletes itself.
    assert_eq!(
        hub.delete("/api/library", &gone.token).await.status,
        StatusCode::NO_CONTENT
    );
    for token in [&gone.token, &gone_member.token] {
        assert_eq!(
            hub.get("/api/pub", Some(token)).await.status,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(hub.blob_files(), 1);
    assert_eq!(
        hub.try_create_library(None, "gone").await.status,
        StatusCode::CREATED,
        "the handle is free again"
    );
    // The library token keeps the library and its owner, but no member; a member's
    // own device keeps them.
    hub.advance(20 * 86_400);
    hub.get("/api/account", Some(&kept.token)).await;
    hub.get("/api/sync", Some(&ivan.token)).await;
    hub.advance(10 * 86_400);
    let ids: Vec<i64> = hub
        .state
        .hub()
        .libraries()
        .unwrap()
        .iter()
        .map(|l| l.id)
        .collect();
    // `idle` and the new `gone` (created at the start) go; one member of `kept` goes.
    assert_eq!(hub.state.hub().expire(hub.now()).unwrap(), (0, 3));
    assert_eq!(hub.state.hub().libraries().unwrap().len(), ids.len() - 2);
    assert_eq!(
        hub.get("/api/pub", Some(&idle.token)).await.status,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        hub.get("/api/sync", Some(&olga.token)).await.status,
        StatusCode::UNAUTHORIZED
    );
    for token in [&kept.token, &ivan.token] {
        assert_eq!(
            hub.get("/api/pub", Some(token)).await.status,
            StatusCode::OK
        );
    }
    assert_eq!(hub.blob_files(), 0);
    let kept_id = hub.state.hub().libraries().unwrap()[0].id;
    assert!(hub.state.hub().delete_library(kept_id).unwrap());
    assert!(!hub.state.hub().delete_library(kept_id).unwrap());
    assert_eq!(
        hub.get("/api/sync", Some(&ivan.token)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn a_hub_with_accounts_of_0_20_starts_only_after_hub_migrate() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hub.toml"), "").unwrap();
    // The schema of 0.20 with one account, one publication and its blob file.
    {
        let conn = rusqlite::Connection::open(dir.path().join("hub.sqlite")).unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY);
             CREATE TABLE publications (id INTEGER PRIMARY KEY);
             INSERT INTO accounts (id) VALUES (1);
             INSERT INTO publications (id) VALUES (1);
             PRAGMA user_version = 3;",
        )
        .unwrap();
    }
    std::fs::create_dir_all(dir.path().join("blobs/ab")).unwrap();
    std::fs::write(dir.path().join("blobs/ab/manifest"), b"x").unwrap();
    let err = Hub::open(dir.path(), false).unwrap_err();
    assert!(matches!(
        err,
        HubError::OldAccounts {
            accounts: 1,
            publications: 1
        }
    ));
    let message = err.to_string();
    assert!(message.contains("shelfgauge hub migrate"), "{message}");
}

#[test]
fn an_old_accounts_section_stops_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("hub.toml"),
        "[accounts]\nregistration = \"open\"\n",
    )
    .unwrap();
    let err = Hub::open(dir.path(), false).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("[libraries]"), "{message}");
}

#[tokio::test]
async fn a_trusted_proxy_gives_each_client_its_own_budget() {
    let hub = TestHub::new(|c| {
        c.server.trusted_proxies = vec!["127.0.0.1".into()];
        c.limits.public_requests_per_hour = 1;
    });
    let pair = |forwarded: Option<&'static str>| {
        hub.call_with(
            Method::POST,
            "/api/pair",
            None,
            Body::from(json!({ "secret": "x" }).to_string()),
            move |r| {
                let r = r.header(header::CONTENT_TYPE, "application/json");
                match forwarded {
                    Some(value) => r.header("x-forwarded-for", value),
                    None => r,
                }
            },
        )
    };
    let status = |answer: Answer| answer.status;
    assert_eq!(status(pair(Some("192.0.2.1")).await), StatusCode::FORBIDDEN);
    assert_eq!(
        status(pair(Some("192.0.2.1")).await),
        StatusCode::TOO_MANY_REQUESTS
    );
    // A value the client sent itself is left of the one the proxy appended.
    assert_eq!(
        status(pair(Some("192.0.2.2, 192.0.2.1")).await),
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(status(pair(Some("192.0.2.2")).await), StatusCode::FORBIDDEN);
    // Without a usable value: the shared budget, not the proxy's address.
    assert_eq!(status(pair(None).await), StatusCode::FORBIDDEN);
    assert_eq!(
        status(pair(Some("garbage")).await),
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn forwarded_values_of_an_untrusted_peer_are_ignored() {
    let hub = TestHub::new(|c| c.limits.public_requests_per_hour = 1);
    for (value, expected) in [
        ("192.0.2.1", StatusCode::FORBIDDEN),
        ("192.0.2.2", StatusCode::TOO_MANY_REQUESTS),
    ] {
        let answer = hub
            .call_with(
                Method::POST,
                "/api/pair",
                None,
                Body::from(json!({ "secret": "x" }).to_string()),
                |r| {
                    r.header(header::CONTENT_TYPE, "application/json")
                        .header("x-forwarded-for", value)
                },
            )
            .await;
        assert_eq!(answer.status, expected, "{value}");
    }
}

/// One GET against a hub opened in `dir` with `config`.
async fn get_from(dir: &std::path::Path, config: HubConfig, uri: &str) -> Answer {
    let hub = Hub::with_config(dir, config).unwrap();
    let app = shelfgauge::hub::router(HubState::new(hub))
        .layer(MockConnectInfo(SocketAddr::from(([127, 0, 0, 1], 50000))));
    let request = Request::builder()
        .uri(uri)
        .header(header::HOST, HOST)
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    Answer {
        status: response.status(),
        headers: response.headers().clone(),
        body: response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    }
}

#[tokio::test]
async fn the_terms_page_shows_the_operators_text_escaped() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("terms.txt"),
        "Books stay encrypted.\r\nWe cannot read them.\n\n<script>alert(1)</script> & more\n",
    )
    .unwrap();
    let mut config = HubConfig::default();
    config.operator.name = "Example <hub>".into();
    config.operator.contact = "abuse@example.org".into();
    config.server.base_path = "/hub".into();
    config.validate().unwrap();
    let page = get_from(dir.path(), config.clone(), "/hub/terms").await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-cache");
    let html = String::from_utf8(page.body).unwrap();
    assert!(
        html.contains("<p>Books stay encrypted.\nWe cannot read them.</p>"),
        "{html}"
    );
    assert!(
        html.contains("&#60;script&#62;alert(1)&#60;/script&#62; &#38; more"),
        "{html}"
    );
    assert!(!html.contains("<script>alert"), "{html}");
    assert!(html.contains("Example &#60;hub&#62;"), "{html}");
    assert!(
        html.contains(r#"href="mailto:abuse@example.org""#),
        "{html}"
    );
    assert!(
        !html.contains("assets/hub.js"),
        "the page runs without the app"
    );
    let info = get_from(dir.path(), config, "/hub/api/info").await.json();
    assert_eq!(info["operator"]["name"], "Example <hub>");
    assert_eq!(info["operator"]["contact"], "abuse@example.org");
    assert_eq!(info["operator"]["terms"], true);

    let empty = tempfile::tempdir().unwrap();
    let mut config = HubConfig::default();
    config.operator.contact = "javascript:alert(1)".into();
    let page = get_from(empty.path(), config.clone(), "/terms").await;
    let html = String::from_utf8(page.body).unwrap();
    assert!(html.contains("This hub has no terms of use."), "{html}");
    assert!(
        !html.contains("href=\"javascript"),
        "only mailto and https links: {html}"
    );
    let info = get_from(empty.path(), config, "/api/info").await.json();
    assert_eq!(info["operator"]["terms"], false);
    assert_eq!(info["operator"]["name"], "");
}

#[test]
fn a_terms_file_too_large_or_not_text_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("terms.txt");
    std::fs::write(&path, vec![b'a'; 64 * 1024 + 1]).unwrap();
    assert!(Hub::with_config(dir.path(), HubConfig::default()).is_err());
    std::fs::write(&path, [0xff, 0xfe]).unwrap();
    assert!(Hub::with_config(dir.path(), HubConfig::default()).is_err());
    std::fs::write(&path, " \n").unwrap();
    let hub = Hub::with_config(dir.path(), HubConfig::default()).unwrap();
    assert_eq!(hub.terms(), None, "only whitespace is no terms");
}

#[tokio::test]
async fn unknown_hosts_are_rejected() {
    let hub = TestHub::new(|c| c.server.allowed_hosts = vec!["hub.example.org".into()]);
    let foreign = hub
        .call_with(Method::GET, "/api/info", None, Body::empty(), |r| {
            r.header(header::HOST, "evil.example")
        })
        .await;
    assert_eq!(foreign.status, StatusCode::FORBIDDEN);
    let proxied = hub
        .call_with(Method::GET, "/api/info", None, Body::empty(), |r| {
            r.header(header::HOST, "hub.example.org")
        })
        .await;
    assert_eq!(proxied.status, StatusCode::OK);
}

#[tokio::test]
async fn the_reader_shell_tells_nothing_about_publications() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let id = pub_id(4);
    hub.publish(&lib, &id, 10).await;
    let existing = hub.get(&format!("/r/{id}"), None).await;
    let missing = hub.get(&format!("/r/{}", pub_id(9)), None).await;
    assert_eq!(existing.status, StatusCode::OK);
    assert_eq!(existing.body, missing.body);
    let csp = existing.headers[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap();
    assert!(
        csp.contains("script-src 'self' 'wasm-unsafe-eval'"),
        "{csp}"
    );
    assert!(csp.contains("img-src 'self' blob:"), "{csp}");
    assert!(!csp.contains("unsafe-inline"), "{csp}");
    assert_eq!(existing.headers[header::REFERRER_POLICY], "no-referrer");
    assert_eq!(existing.headers[header::X_FRAME_OPTIONS], "DENY");
    assert_eq!(
        hub.get("/r/not-an-id", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(hub.get("/pair", None).await.status, StatusCode::OK);
    for page in ["/signin", "/invite"] {
        assert_eq!(hub.get(page, None).await.status, StatusCode::OK, "{page}");
    }
    assert_eq!(hub.get("/", None).await.status, StatusCode::OK);
    let css = hub.get("/assets/hub.css", None).await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
    // The reader module: WebAssembly needs its own type to compile while streaming.
    let wasm = hub.get("/assets/pkg/shelfgauge_bg.wasm", None).await;
    assert_eq!(wasm.status, StatusCode::OK);
    assert_eq!(wasm.headers[header::CONTENT_TYPE], "application/wasm");
    assert_eq!(&wasm.body[..4], b"\0asm");
    let glue = hub.get("/assets/pkg/shelfgauge.js", None).await;
    assert_eq!(glue.status, StatusCode::OK);
    let page = String::from_utf8(existing.body).unwrap();
    assert!(page.contains("href=\"/assets/hub.css\""), "{page}");
    // The reading view: the instance's reading page, empty until the app fills it.
    assert!(page.contains(r#"<article class="reader""#), "{page}");
    assert!(
        page.contains(r#"<script type="module" src="/assets/hub.js">"#),
        "{page}"
    );
    assert!(page.contains("data-local-settings"), "{page}");
    assert!(
        !page.contains("reader.js"),
        "reader.js starts once the book is in"
    );
    let home = String::from_utf8(hub.get("/", None).await.body).unwrap();
    assert!(home.contains(r#"id="hub-app""#), "{home}");
    assert!(!home.contains("<article"), "{home}");
    // The library's reading files are shared; nothing else of the library is.
    for (name, kind) in [
        ("style.css", "text/css; charset=utf-8"),
        ("reader.css", "text/css; charset=utf-8"),
        ("theme.js", "text/javascript; charset=utf-8"),
        ("app.js", "text/javascript; charset=utf-8"),
        ("reader.js", "text/javascript; charset=utf-8"),
        ("icon.svg", "image/svg+xml"),
        ("hub.js", "text/javascript; charset=utf-8"),
        ("store.js", "text/javascript; charset=utf-8"),
        ("crypto.js", "text/javascript; charset=utf-8"),
    ] {
        let file = hub.get(&format!("/assets/{name}"), None).await;
        assert_eq!(file.status, StatusCode::OK, "{name}");
        assert_eq!(file.headers[header::CONTENT_TYPE], kind, "{name}");
    }
    assert_eq!(
        hub.get("/assets/hub/hub.js", None).await.status,
        StatusCode::NOT_FOUND
    );
    assert!(
        !page.contains("<script>") && !page.contains("style="),
        "no inline code"
    );
    let unknown = hub.get("/nothing", None).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.error(), "not_found");
}

#[tokio::test]
async fn the_base_path_prefixes_every_route() {
    let hub = TestHub::new(|c| c.server.base_path = "/hub".into());
    assert_eq!(hub.get("/hub/api/info", None).await.status, StatusCode::OK);
    assert_eq!(
        hub.get("/api/info", None).await.status,
        StatusCode::NOT_FOUND
    );
    let page = hub.get(&format!("/hub/r/{}", pub_id(1)), None).await;
    let page = String::from_utf8(page.body).unwrap();
    assert!(page.contains("/hub/assets/hub.css"), "{page}");
    // The app finds the API and the assets under the base path.
    assert!(page.contains(r#"<body data-base="/hub">"#), "{page}");
    assert!(page.contains(r#"src="/hub/assets/hub.js""#), "{page}");
    assert_eq!(hub.get("/hub", None).await.status, StatusCode::OK);
    assert_eq!(
        hub.get("/hub/assets/reader.js", None).await.status,
        StatusCode::OK
    );
}

#[tokio::test]
async fn the_service_worker_and_manifest_make_the_reader_installable() {
    for (base, prefix) in [("/", ""), ("/hub", "/hub")] {
        let hub = TestHub::new(|c| c.server.base_path = base.into());
        let sw = hub.get(&format!("{prefix}/sw.js"), None).await;
        assert_eq!(sw.status, StatusCode::OK);
        assert_eq!(
            sw.headers[header::CONTENT_TYPE],
            "text/javascript; charset=utf-8"
        );
        assert_eq!(sw.headers[header::CACHE_CONTROL], "no-cache");
        let script = String::from_utf8(sw.body).unwrap();
        assert!(
            script.contains(&format!(r#"const BASE = "{prefix}";"#)),
            "{script}"
        );
        // Every file the shell needs offline is precached, with a version.
        for file in [
            "hub.js",
            "store.js",
            "crypto.js",
            "reader.js",
            "style.css",
            "pkg/shelfgauge_bg.wasm",
        ] {
            assert!(
                script.contains(&format!("`${{BASE}}/assets/{file}`")),
                "{file}: {script}"
            );
        }
        let version = script
            .split(r#"const VERSION = ""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap();
        assert_eq!(version.len(), 16, "{version}");
        // The same build gives the same version.
        let again =
            String::from_utf8(hub.get(&format!("{prefix}/sw.js"), None).await.body).unwrap();
        assert_eq!(script, again);

        // `cache.addAll` is all or nothing: every precached URL must answer.
        let mut urls = vec![
            format!("{prefix}/"),
            format!("{prefix}/r/00000000000000000000000000000000"),
        ];
        assert!(script.contains("const HOME = `${BASE}/`;"), "{script}");
        assert!(
            script.contains("const READER = `${BASE}/r/00000000000000000000000000000000`;"),
            "{script}"
        );
        let files = script
            .split("const FILES = [")
            .nth(1)
            .and_then(|rest| rest.split("];").next())
            .unwrap();
        for line in files.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let file = line
                .strip_prefix("`${BASE}/assets/")
                .and_then(|l| l.strip_suffix("`,"))
                .unwrap_or_else(|| panic!("{line}"));
            urls.push(format!("{prefix}/assets/{file}"));
        }
        assert!(urls.len() > 10, "{urls:?}");
        for url in &urls {
            assert_eq!(hub.get(url, None).await.status, StatusCode::OK, "{url}");
        }

        let manifest = hub
            .get(&format!("{prefix}/manifest.webmanifest"), None)
            .await;
        assert_eq!(manifest.status, StatusCode::OK);
        assert_eq!(
            manifest.headers[header::CONTENT_TYPE],
            "application/manifest+json"
        );
        let json: serde_json::Value = serde_json::from_slice(&manifest.body).unwrap();
        assert_eq!(json["start_url"], format!("{prefix}/"));
        assert_eq!(json["scope"], format!("{prefix}/"));
        assert_eq!(json["display"], "standalone");
        // Chrome refuses to install without 192 and 512 pixel raster icons.
        for size in ["192x192", "512x512"] {
            let icon = json["icons"]
                .as_array()
                .unwrap()
                .iter()
                .find(|i| i["sizes"] == size && i["type"] == "image/png")
                .unwrap_or_else(|| panic!("no {size} PNG icon: {json}"));
            let src = icon["src"].as_str().unwrap();
            assert!(src.starts_with(&format!("{prefix}/assets/")), "{src}");
            let png = hub.get(src, None).await;
            assert_eq!(png.status, StatusCode::OK, "{src}");
            assert_eq!(png.headers[header::CONTENT_TYPE], "image/png");
            let side: u32 = size.split_once('x').unwrap().0.parse().unwrap();
            // The PNG header's width and height.
            assert_eq!(&png.body[..8], b"\x89PNG\r\n\x1a\n");
            assert_eq!(png.body[16..20], side.to_be_bytes());
            assert_eq!(png.body[20..24], side.to_be_bytes());
        }
        let page = String::from_utf8(hub.get(&format!("{prefix}/"), None).await.body).unwrap();
        assert!(
            page.contains(&format!(
                r#"<link rel="manifest" href="{prefix}/manifest.webmanifest">"#
            )),
            "{page}"
        );
        // The shelf's top bar leads to This device; the reading view has neither that
        // nor the home link (its sidebar has My books).
        assert!(page.contains(r#"class="icon-button home-link""#), "{page}");
        assert!(
            page.contains(r#"class="icon-button device-link""#),
            "{page}"
        );
        let read = hub
            .get(&format!("{prefix}/r/{}", "0".repeat(32)), None)
            .await;
        assert_eq!(read.status, StatusCode::OK);
        let read = String::from_utf8(read.body).unwrap();
        assert!(read.contains(r#"class="toc-book""#), "{read}");
        assert!(!read.contains("home-link"), "{read}");
        assert!(!read.contains("device-link"), "{read}");
    }
}

#[tokio::test]
async fn book_files_open_through_the_installed_app() {
    for (base, prefix) in [("/", ""), ("/hub", "/hub")] {
        let hub = TestHub::new(|c| c.server.base_path = base.into());
        let manifest = hub
            .get(&format!("{prefix}/manifest.webmanifest"), None)
            .await;
        let json: serde_json::Value = serde_json::from_slice(&manifest.body).unwrap();
        let handler = &json["file_handlers"][0];
        assert_eq!(handler["action"], format!("{prefix}/open"));
        assert_eq!(
            handler["accept"]["application/vnd.shelfgauge.book"],
            json!([".sgbook", ".sgb"])
        );
        let share = &json["share_target"];
        assert_eq!(share["action"], format!("{prefix}/share"));
        assert_eq!(share["method"], "POST");
        assert_eq!(share["enctype"], "multipart/form-data");
        assert_eq!(
            share["params"]["files"][0]["accept"],
            json!(["application/vnd.shelfgauge.book", ".sgbook", ".sgb"])
        );

        // `/open` is the shell (the app takes the file), served offline as the shelf.
        let open = hub.get(&format!("{prefix}/open"), None).await;
        assert_eq!(open.status, StatusCode::OK);
        let page = String::from_utf8(open.body).unwrap();
        assert!(page.contains(r#"id="hub-app""#), "{page}");
        let sw = String::from_utf8(hub.get(&format!("{prefix}/sw.js"), None).await.body).unwrap();
        assert!(sw.contains("pair|open|device|signin|invite|r"), "{sw}");
        assert!(sw.contains("`${BASE}/share`"), "{sw}");

        // A share no Service Worker took: the body is not read, the shelf says so.
        let shared = hub
            .call_with(
                Method::POST,
                &format!("{prefix}/share"),
                None,
                Body::from(vec![0u8; 4 << 20]),
                |r| r.header(header::CONTENT_TYPE, "multipart/form-data; boundary=x"),
            )
            .await;
        assert_eq!(shared.status, StatusCode::SEE_OTHER);
        assert_eq!(
            shared.headers[header::LOCATION],
            format!("{prefix}/?shared=lost")
        );
    }
}

#[test]
fn a_hub_is_created_only_when_asked() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("hub");
    let err = Hub::open(&data, false).unwrap_err();
    assert!(matches!(err, HubError::NotAHub(_)), "{err}");
    assert!(!data.exists());
    let opened = Hub::open(&data, true).unwrap();
    assert!(opened.created);
    assert!(data.join("hub.toml").is_file());
    assert!(data.join("hub.sqlite").is_file());
    drop(opened);
    let reopened = Hub::open(&data, false).unwrap();
    assert!(!reopened.created);
    assert_eq!(reopened.hub.config(), &HubConfig::default());
}

#[tokio::test]
async fn a_signed_in_device_names_itself_only() {
    let hub = TestHub::open();
    let lib = hub.library("smiths").await;
    let olga = hub.member(&lib, "olga", 1).await;
    let phone = issued(&hub.login("olga@smiths", 1).await);
    let name = |id: i64, token: String| {
        let hub = &hub;
        async move {
            hub.send(
                Method::PUT,
                &format!("/api/devices/{id}/name"),
                Some(&token),
                json!({ "name": b64(b"sealed phone name") }),
            )
            .await
        }
    };
    assert_eq!(
        name(phone.id, phone.token.clone()).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        name(olga.id, phone.token.clone()).await.status,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        name(phone.id, lib.token.clone()).await.status,
        StatusCode::FORBIDDEN
    );
    let devices = hub.get("/api/devices", Some(&olga.token)).await.json();
    assert_eq!(devices["devices"][1]["name"], b64(b"sealed phone name"));
    assert_eq!(devices["devices"][0]["name"], serde_json::Value::Null);
}
