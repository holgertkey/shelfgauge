//! The hub server end to end through its router: accounts and invites, pairing,
//! recovery, devices, blobs, account isolation, quotas, expiry, takedown, sync and
//! the limits of every route.

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
use shelfgauge::hub::{Hub, HubError, HubState, Secret};
use shelfgauge::publication::format::PublicationId;
use tower::ServiceExt;

const HOST: &str = "localhost:8743";

/// The start of the test clock (Unix seconds).
const T0: i64 = 1_800_000_000;

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

/// A device token of an account.
#[derive(Clone)]
struct Device {
    account: String,
    id: i64,
    token: String,
}

impl TestHub {
    /// A hub with open registration and `edit` applied to the config.
    fn new(edit: impl FnOnce(&mut HubConfig)) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut config = HubConfig::default();
        config.accounts.registration = Registration::Open;
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

    async fn post(&self, uri: &str, token: Option<&str>, body: Value) -> Answer {
        self.call_with(
            Method::POST,
            uri,
            token,
            Body::from(body.to_string()),
            |r| r.header(header::CONTENT_TYPE, "application/json"),
        )
        .await
    }

    async fn put(&self, uri: &str, token: &str, data: Vec<u8>) -> Answer {
        self.call(Method::PUT, uri, Some(token), Body::from(data))
            .await
    }

    async fn delete(&self, uri: &str, token: &str) -> Answer {
        self.call(Method::DELETE, uri, Some(token), Body::empty())
            .await
    }

    /// Registers an account (with `invite` if given); returns its first device.
    async fn try_register(&self, invite: Option<&str>, recovery: &Secret) -> Answer {
        let mut body = json!({ "recovery_hash": URL_SAFE_NO_PAD.encode(recovery.hash()) });
        if let Some(invite) = invite {
            body["invite"] = json!(invite);
        }
        self.post("/api/account", None, body).await
    }

    async fn register(&self) -> Device {
        let answer = self
            .try_register(None, &Secret::from_text("recovery"))
            .await;
        assert_eq!(answer.status, StatusCode::CREATED);
        issued(&answer)
    }

    /// Pairs a new device with the account of `device`.
    async fn pair(&self, device: &Device) -> Device {
        let secret = self
            .post("/api/pairings", Some(&device.token), json!({}))
            .await;
        assert_eq!(secret.status, StatusCode::CREATED);
        let secret = secret.json()["secret"].as_str().unwrap().to_owned();
        let paired = self
            .post("/api/pair", None, json!({ "secret": secret }))
            .await;
        assert_eq!(paired.status, StatusCode::CREATED);
        issued(&paired)
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

/// A well-formed sealed blob (format version 1) of `len` bytes after the header.
fn blob(len: usize, fill: u8) -> Vec<u8> {
    let mut data = vec![0, 1];
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
    assert_eq!(json["formats"], json!({ "min": 1, "max": 1 }));
    assert_eq!(json["registration"], "open");
    assert_eq!(json["limits"]["max_blob_bytes"], 64 * 1024 * 1024);
    assert_eq!(json["limits"]["publication_expiry_days"], 30);
    assert_eq!(info.headers[header::CACHE_CONTROL], "no-store");
}

#[tokio::test]
async fn registration_needs_a_one_time_invite_by_default() {
    let hub = TestHub::new(|c| c.accounts = Default::default());
    let recovery = Secret::from_text("r");
    let refused = hub.try_register(None, &recovery).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.error(), "invite_required");
    let (code, expires) = hub.state.hub().invite(hub.now()).unwrap();
    assert_eq!(expires, hub.now() + 7 * 86_400);
    let wrong = hub.try_register(Some("not-the-code"), &recovery).await;
    assert_eq!(wrong.status, StatusCode::FORBIDDEN);
    let created = hub.try_register(Some(code.as_str()), &recovery).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let again = hub.try_register(Some(code.as_str()), &recovery).await;
    assert_eq!(again.error(), "invite_required", "used up");
    let late = hub.state.hub().invite(hub.now()).unwrap().0;
    hub.advance(7 * 86_400);
    let expired = hub.try_register(Some(late.as_str()), &recovery).await;
    assert_eq!(expired.error(), "invite_required");
}

#[tokio::test]
async fn max_accounts_closes_registration() {
    let hub = TestHub::new(|c| c.accounts.max_accounts = 1);
    hub.register().await;
    let refused = hub.try_register(None, &Secret::from_text("x")).await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.error(), "quota_exceeded");
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
    let device = hub.register().await;
    let account = hub.get("/api/account", Some(&device.token)).await;
    assert_eq!(account.status, StatusCode::OK);
    assert_eq!(account.json()["account"], device.account.as_str());
    assert_eq!(account.json()["usage"]["devices"], 1);
}

#[tokio::test]
async fn pairing_secrets_are_single_use_and_expire() {
    let hub = TestHub::open();
    let instance = hub.register().await;
    let phone = hub.pair(&instance).await;
    assert_eq!(phone.account, instance.account);
    assert_ne!(phone.token, instance.token);
    assert_eq!(
        hub.get("/api/account", Some(&phone.token)).await.status,
        StatusCode::OK
    );

    let secret = hub
        .post("/api/pairings", Some(&instance.token), json!({}))
        .await;
    let json = secret.json();
    assert_eq!(json["expires_at"], hub.now() + 600);
    let secret = json["secret"].as_str().unwrap().to_owned();
    let body = json!({ "secret": secret, "device_name": b64(b"encrypted name") });
    assert_eq!(
        hub.post("/api/pair", None, body.clone()).await.status,
        StatusCode::CREATED
    );
    let reused = hub.post("/api/pair", None, body).await;
    assert_eq!(reused.status, StatusCode::FORBIDDEN);
    assert_eq!(reused.error(), "invalid_secret");

    let late = hub
        .post("/api/pairings", Some(&instance.token), json!({}))
        .await;
    let late = late.json()["secret"].as_str().unwrap().to_owned();
    hub.advance(600);
    let expired = hub.post("/api/pair", None, json!({ "secret": late })).await;
    assert_eq!(expired.error(), "invalid_secret");

    let devices = hub.get("/api/devices", Some(&instance.token)).await.json();
    let devices = devices["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 3);
    assert_eq!(devices[0]["current"], true);
    assert_eq!(devices[2]["name"], b64(b"encrypted name"));
}

#[tokio::test]
async fn recovery_needs_the_account_and_its_secret() {
    let hub = TestHub::open();
    let recovery = Secret::generate().unwrap();
    let first = issued(&hub.try_register(None, &recovery).await);
    let other = hub.register().await;
    let attempt = |account: &str, secret: &str| {
        hub.post(
            "/api/recover",
            None,
            json!({ "account": account, "secret": secret }),
        )
    };
    assert_eq!(
        attempt(&first.account, "wrong").await.error(),
        "invalid_secret"
    );
    assert_eq!(
        attempt(&other.account, recovery.as_str()).await.error(),
        "invalid_secret",
        "the secret belongs to another account"
    );
    assert_eq!(
        attempt("nonsense", recovery.as_str()).await.status,
        StatusCode::BAD_REQUEST
    );
    let recovered = attempt(&first.account, recovery.as_str()).await;
    assert_eq!(recovered.status, StatusCode::CREATED);
    assert_eq!(issued(&recovered).account, first.account);
}

#[tokio::test]
async fn revoked_devices_lose_access() {
    let hub = TestHub::open();
    let instance = hub.register().await;
    let phone = hub.pair(&instance).await;
    let stranger = hub.register().await;
    let hub = &hub;
    let revoke = |token: String, id: i64| async move {
        hub.delete(&format!("/api/devices/{id}"), &token).await
    };
    assert_eq!(
        revoke(stranger.token.clone(), phone.id).await.status,
        StatusCode::NOT_FOUND,
        "another account's device"
    );
    assert_eq!(
        revoke(instance.token.clone(), 999).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        hub.delete("/api/devices/x", &instance.token).await.status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        revoke(instance.token.clone(), phone.id).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.get("/api/sync", Some(&phone.token)).await.status,
        StatusCode::UNAUTHORIZED
    );
    let devices = hub.get("/api/devices", Some(&instance.token)).await.json();
    assert_eq!(devices["devices"].as_array().unwrap().len(), 1);
    // A device can sign itself out.
    assert_eq!(
        revoke(instance.token.clone(), instance.id).await.status,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        hub.get("/api/account", Some(&instance.token)).await.status,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn blobs_round_trip_and_are_deleted_with_their_publication() {
    let hub = TestHub::open();
    let device = hub.register().await;
    let id = pub_id(1);
    let manifest = blob(300, b'm');
    let part = blob(1000, b'p');
    let image = blob(20, b'i');
    for (path, data) in [("manifest", &manifest), ("p/0", &part), ("i/0", &image)] {
        let put = hub
            .put(
                &format!("/api/pub/{id}/{path}"),
                &device.token,
                data.clone(),
            )
            .await;
        assert_eq!(put.status, StatusCode::CREATED, "{path}");
    }
    let replaced = hub
        .put(&format!("/api/pub/{id}/p/0"), &device.token, part.clone())
        .await;
    assert_eq!(replaced.status, StatusCode::NO_CONTENT);
    let got = hub
        .get(&format!("/api/pub/{id}/p/0"), Some(&device.token))
        .await;
    assert_eq!(got.status, StatusCode::OK);
    assert_eq!(got.body, part);
    assert_eq!(
        got.headers[header::CONTENT_TYPE],
        "application/octet-stream"
    );
    assert_eq!(got.headers[header::CACHE_CONTROL], "private, no-store");
    assert_eq!(hub.blob_files(), 3);

    let list = hub.get("/api/pub", Some(&device.token)).await.json();
    let publications = list["publications"].as_array().unwrap();
    assert_eq!(publications.len(), 1);
    assert_eq!(publications[0]["id"], id.as_str());
    assert_eq!(publications[0]["blobs"], 3);
    let total = manifest.len() + part.len() + image.len();
    assert_eq!(publications[0]["bytes"], total);
    let usage = hub.get("/api/account", Some(&device.token)).await.json();
    assert_eq!(usage["usage"]["bytes"], total);

    for path in ["p/1", "x/0", "p/01", "../manifest"] {
        let missing = hub
            .get(&format!("/api/pub/{id}/{path}"), Some(&device.token))
            .await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{path}");
    }
    assert_eq!(
        hub.get("/api/pub/NOT-AN-ID/manifest", Some(&device.token))
            .await
            .status,
        StatusCode::NOT_FOUND
    );

    let deleted = hub.delete(&format!("/api/pub/{id}"), &device.token).await;
    assert_eq!(deleted.status, StatusCode::NO_CONTENT);
    assert_eq!(
        hub.get(&format!("/api/pub/{id}/manifest"), Some(&device.token))
            .await
            .status,
        StatusCode::NOT_FOUND
    );
    assert_eq!(hub.blob_files(), 0);
    assert_eq!(
        hub.delete(&format!("/api/pub/{id}"), &device.token)
            .await
            .status,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn an_account_never_reaches_another_accounts_data() {
    let hub = TestHub::open();
    let owner = hub.register().await;
    let other = hub.register().await;
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
        Some(&owner.token),
        json!({ "records": [{ "kind": "annotation", "id": "a1", "data": b64(b"secret") }] }),
    )
    .await;

    let get = hub
        .get(&format!("/api/pub/{id}/manifest"), Some(&other.token))
        .await;
    assert_eq!(get.status, StatusCode::NOT_FOUND);
    assert_eq!(get.error(), "not_found", "the same answer as a missing id");
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
    let list = hub.get("/api/pub", Some(&other.token)).await.json();
    assert_eq!(list["publications"], json!([]));
    let sync = hub.get("/api/sync", Some(&other.token)).await.json();
    assert_eq!(sync["records"], json!([]));
    assert_eq!(sync["seq"], 0);

    let still = hub
        .get(&format!("/api/pub/{id}/manifest"), Some(&owner.token))
        .await;
    assert_eq!(still.body, data);
}

#[tokio::test]
async fn quotas_limit_publications_bytes_and_devices() {
    let hub = TestHub::new(|c| {
        c.quotas.max_publications = 1;
        c.quotas.max_bytes = 1000;
        c.quotas.max_devices = 2;
    });
    let device = hub.register().await;
    let put = |id: String, path: &'static str, len: usize| {
        let token = device.token.clone();
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
    let records = hub
        .post(
            "/api/sync",
            Some(&device.token),
            json!({ "records": [{ "kind": "position", "id": "p", "data": b64(&[0; 200]) }] }),
        )
        .await;
    assert_eq!(records.error(), "quota_exceeded", "records count too");

    hub.pair(&device).await;
    let secret = hub
        .post("/api/pairings", Some(&device.token), json!({}))
        .await;
    let secret = secret.json()["secret"].as_str().unwrap().to_owned();
    let third = hub
        .post("/api/pair", None, json!({ "secret": secret }))
        .await;
    assert_eq!(third.error(), "quota_exceeded");
}

#[tokio::test]
async fn unopened_publications_expire() {
    let hub = TestHub::new(|c| c.limits.publication_expiry_days = 10);
    let device = hub.register().await;
    for byte in [1, 2] {
        hub.put(
            &format!("/api/pub/{}/manifest", pub_id(byte)),
            &device.token,
            blob(10, 0),
        )
        .await;
    }
    hub.advance(9 * 86_400);
    // Opening (fetching the manifest) keeps the first one; parts do not count.
    hub.get(
        &format!("/api/pub/{}/manifest", pub_id(1)),
        Some(&device.token),
    )
    .await;
    assert_eq!(hub.state.hub().expire(hub.now()).unwrap(), 0);
    hub.advance(86_400);
    assert_eq!(hub.state.hub().expire(hub.now()).unwrap(), 1);
    let list = hub.get("/api/pub", Some(&device.token)).await.json();
    let ids: Vec<&str> = list["publications"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [pub_id(1)]);
    assert_eq!(hub.blob_files(), 1);
    // An expired id may be published again (only takedowns are remembered).
    let again = hub
        .put(
            &format!("/api/pub/{}/manifest", pub_id(2)),
            &device.token,
            blob(10, 0),
        )
        .await;
    assert_eq!(again.status, StatusCode::CREATED);
}

#[tokio::test]
async fn expiry_is_off_by_default() {
    let hub = TestHub::open();
    let device = hub.register().await;
    hub.put(
        &format!("/api/pub/{}/manifest", pub_id(1)),
        &device.token,
        blob(10, 0),
    )
    .await;
    hub.advance(10_000 * 86_400);
    assert_eq!(hub.state.hub().expire(hub.now()).unwrap(), 0);
}

#[tokio::test]
async fn taken_down_publications_are_gone_for_good() {
    let hub = TestHub::open();
    let device = hub.register().await;
    let id = pub_id(5);
    hub.put(
        &format!("/api/pub/{id}/manifest"),
        &device.token,
        blob(10, 0),
    )
    .await;
    let parsed: PublicationId = id.parse().unwrap();
    assert!(hub.state.hub().takedown(&parsed, hub.now()).unwrap());
    assert_eq!(hub.blob_files(), 0);
    let get = hub
        .get(&format!("/api/pub/{id}/manifest"), Some(&device.token))
        .await;
    assert_eq!(get.status, StatusCode::GONE);
    assert_eq!(get.error(), "taken_down");
    let put = hub
        .put(
            &format!("/api/pub/{id}/manifest"),
            &device.token,
            blob(10, 0),
        )
        .await;
    assert_eq!(put.status, StatusCode::GONE);
    assert!(!hub.state.hub().takedown(&parsed, hub.now()).unwrap());
    let accounts = hub.state.hub().accounts().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].usage.publications, 0);
}

#[tokio::test]
async fn sync_orders_changes_and_pages_with_since() {
    let hub = TestHub::open();
    let instance = hub.register().await;
    let phone = hub.pair(&instance).await;
    let push = |token: String, records: Value| {
        let hub = &hub;
        async move {
            hub.post("/api/sync", Some(&token), json!({ "records": records }))
                .await
        }
    };
    let pushed = push(
        instance.token.clone(),
        json!([
            { "kind": "shelf_item", "id": "s1", "data": b64(b"shelf") },
            { "kind": "annotation", "id": "a1", "data": b64(b"one") },
            { "kind": "annotation", "id": "a2", "data": b64(b"two") },
            { "kind": "position", "id": "p1", "data": b64(b"pos") },
        ]),
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
    push(
        phone.token.clone(),
        json!([
            { "kind": "annotation", "id": "a1", "data": b64(b"edited") },
            { "kind": "annotation", "id": "a2", "data": null },
        ]),
    )
    .await;
    let after = hub
        .get("/api/sync?since=4", Some(&instance.token))
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
    let all = hub.get("/api/sync", Some(&instance.token)).await.json();
    assert_eq!(
        all["records"].as_array().unwrap().len(),
        4,
        "one row per record"
    );
    let ahead = hub
        .get("/api/sync?since=100", Some(&instance.token))
        .await
        .json();
    assert_eq!(
        (ahead["seq"].clone(), ahead["more"].clone()),
        (json!(100), json!(false))
    );
    assert_eq!(
        hub.get("/api/sync?since=x", Some(&instance.token))
            .await
            .status,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn sync_rejects_malformed_records() {
    let hub = TestHub::open();
    let device = hub.register().await;
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
    let device = hub.register().await;
    let id = pub_id(3);
    let too_big = hub
        .put(&format!("/api/pub/{id}/p/0"), &device.token, blob(2048, 0))
        .await;
    assert_eq!(too_big.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(too_big.error(), "too_large");
    let fits = hub
        .put(
            &format!("/api/pub/{id}/p/0"),
            &device.token,
            blob(2048 - 14, 0),
        )
        .await;
    assert_eq!(fits.status, StatusCode::CREATED);
    for malformed in [vec![], vec![0, 1, 2], blob(20, 0)[..20].to_vec(), {
        let mut wrong_version = blob(20, 0);
        wrong_version[1] = 9;
        wrong_version
    }] {
        let answer = hub
            .put(&format!("/api/pub/{id}/p/1"), &device.token, malformed)
            .await;
        assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    }
    let long_name = b64(&vec![0; 64 * 1024]);
    let answer = hub
        .post(
            "/api/account",
            None,
            json!({ "recovery_hash": b64(&[0; 32]), "device_name": long_name }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::PAYLOAD_TOO_LARGE);
    let answer = hub
        .post(
            "/api/account",
            None,
            json!({ "recovery_hash": b64(&[0; 31]) }),
        )
        .await;
    assert_eq!(answer.status, StatusCode::BAD_REQUEST);
    let answer = hub
        .call_with(
            Method::POST,
            "/api/account",
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
    let device = hub.register().await;
    let part = hub
        .put(
            &format!("/api/pub/{}/p/0", pub_id(6)),
            &device.token,
            blob(200 * 1024, 0),
        )
        .await;
    assert_eq!(part.status, StatusCode::CREATED);
    let record = b64(&vec![0; 300 * 1024]);
    let pushed = hub
        .post(
            "/api/sync",
            Some(&device.token),
            json!({ "records": [{ "kind": "annotation", "id": "a", "data": record }] }),
        )
        .await;
    assert_eq!(pushed.status, StatusCode::OK);
    let pairing = hub
        .call_with(
            Method::POST,
            "/api/pairings",
            Some(&device.token),
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
    let device = hub.register().await;
    assert_eq!(
        hub.get("/api/pub", Some(&device.token)).await.status,
        StatusCode::OK
    );
    assert_eq!(
        hub.get("/api/pub", Some(&device.token)).await.status,
        StatusCode::OK
    );
    let limited = hub.get("/api/pub", Some(&device.token)).await;
    assert_eq!(limited.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(limited.headers[header::RETRY_AFTER], "30");
    // Without a token: one budget for all loopback peers (a proxy on this machine).
    let second = hub.try_register(None, &Secret::from_text("x")).await;
    assert_eq!(second.status, StatusCode::CREATED);
    let third = hub.post("/api/pair", None, json!({ "secret": "x" })).await;
    assert_eq!(third.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(third.error(), "rate_limited");
    // The info route is not limited.
    assert_eq!(hub.get("/api/info", None).await.status, StatusCode::OK);
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
    let device = hub.register().await;
    let id = pub_id(4);
    hub.put(
        &format!("/api/pub/{id}/manifest"),
        &device.token,
        blob(10, 0),
    )
    .await;
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
    assert_eq!(hub.get("/", None).await.status, StatusCode::OK);
    let css = hub.get("/assets/hub.css", None).await;
    assert_eq!(css.status, StatusCode::OK);
    assert_eq!(css.headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
    let page = String::from_utf8(existing.body).unwrap();
    assert!(page.contains("href=\"/assets/hub.css\""), "{page}");
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
    assert!(
        String::from_utf8(page.body)
            .unwrap()
            .contains("/hub/assets/hub.css")
    );
    assert_eq!(hub.get("/hub", None).await.status, StatusCode::OK);
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
