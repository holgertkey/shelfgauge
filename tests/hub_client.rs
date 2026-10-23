//! The instance's side of the hub end to end: a real hub on a loopback port and the
//! library's router, through My profile → Phone and other devices (register, the
//! recovery code, pairing, devices, joining with the recovery code, leaving).

mod common;

use std::sync::Arc;

use axum::http::{Method, StatusCode, header};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use common::hub::{Lib, TestHub, pairing_link};
use shelfgauge::hub_client::keys::{self, AccountKey, RecoveryCode};

#[tokio::test]
async fn without_a_hub_the_page_says_how_to_set_one() {
    let lib = Lib::new("", "Home");
    let page = lib.get("/profile/hub").await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(page.body.contains("No hub set"), "{}", page.body);
    let reply = lib.post("/profile/hub", &[("action", "register")]).await;
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(reply.body.contains("No hub is set"));
    let profile = lib.get("/profile").await;
    assert!(profile.body.contains("href=\"/profile/hub\""));
}

#[tokio::test]
async fn registering_shows_the_recovery_code_until_it_is_saved() {
    let hub = TestHub::start().await;
    let lib = Lib::new(&hub.url, "Home library");
    let page = lib.get("/profile/hub").await;
    assert!(page.body.contains("Invite code"), "{}", page.body);
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");

    // The hub is invite-only.
    let refused = lib.post("/profile/hub", &[("action", "register")]).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("invite code"), "{}", refused.body);
    assert!(lib.account().is_none());

    let code = lib.register(&hub.invite()).await;
    let parsed = RecoveryCode::parse(&code).unwrap();
    let account = lib.account().unwrap();
    assert_eq!(parsed.account(), account.account);
    assert_eq!(parsed.key(), &account.key);
    assert!(account.recovery_pending);

    let page = lib.get("/profile/hub/recovery").await;
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
    assert!(page.body.contains("<svg class=\"qr\""));

    let page = lib.get("/profile/hub").await;
    assert!(page.body.contains("Save your recovery code"));
    assert!(page.body.contains("Home library"), "{}", page.body);
    assert!(page.body.contains("(this library)"));

    // A second registration is refused.
    let again = lib
        .post(
            "/profile/hub",
            &[("action", "register"), ("invite", &hub.invite())],
        )
        .await;
    assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY);

    let saved = lib.post("/profile/hub", &[("action", "saved")]).await;
    assert_eq!(saved.location(), "/profile/hub?done=saved");
    assert_eq!(
        lib.get("/profile/hub/recovery").await.status,
        StatusCode::NOT_FOUND
    );
    let page = lib.get("/profile/hub").await;
    assert!(!page.body.contains("Save your recovery code"));
    assert!(!page.body.contains(&code));
}

#[tokio::test]
async fn pairing_links_carry_the_account_key_and_pair_a_device() {
    let hub = TestHub::start().await;
    let lib = Lib::new(&hub.url, "Home library");
    lib.register(&hub.invite()).await;
    let account = lib.account().unwrap();

    let page = lib.post("/profile/hub", &[("action", "pair")]).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(page.headers[header::CACHE_CONTROL], "no-store");
    assert!(page.body.contains("<svg class=\"qr\""));
    let (secret, key) = pairing_link(&page.body, &hub.url);
    assert_eq!(key, account.key.to_text());

    // The phone pairs with the secret and names itself with the account key.
    let key = AccountKey::new(URL_SAFE_NO_PAD.decode(&key).unwrap().try_into().unwrap());
    let name = URL_SAFE_NO_PAD.encode(keys::seal_device_name(&key, "Phone").unwrap());
    let (status, paired) = hub
        .api(
            Method::POST,
            "pair",
            None,
            Some(serde_json::json!({ "secret": secret, "device_name": name })),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(paired["account"], account.account.to_string());
    let phone = paired["device"].as_i64().unwrap();
    // Single use.
    let (status, _) = hub
        .api(
            Method::POST,
            "pair",
            None,
            Some(serde_json::json!({ "secret": secret })),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let page = lib.get("/profile/hub").await;
    assert!(
        page.body.contains("<strong>Phone</strong>"),
        "{}",
        page.body
    );
    assert_eq!(hub.device_count(), 2);

    let revoked = lib
        .post(
            "/profile/hub",
            &[("action", "revoke"), ("device", &phone.to_string())],
        )
        .await;
    assert_eq!(revoked.location(), "/profile/hub?done=revoked");
    let page = lib.get("/profile/hub").await;
    assert!(!page.body.contains("<strong>Phone</strong>"));
    assert_eq!(hub.device_count(), 1);
    let token = paired["token"].as_str().unwrap();
    let (status, _) = hub.api(Method::GET, "devices", Some(token), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_second_library_joins_with_the_recovery_code() {
    let hub = TestHub::start().await;
    let home = Lib::new(&hub.url, "Home library");
    let code = home.register(&hub.invite()).await;

    let office = Lib::new(&hub.url, "Office");
    // One digit of the payload changed (the last digit also holds padding bits).
    let i = 3 + 5 * 10 + 2;
    let wrong = if &code[i..=i] == "A" { "B" } else { "A" };
    let typo = format!("{}{wrong}{}", &code[..i], &code[i + 1..]);
    let refused = office
        .post("/profile/hub", &[("action", "join"), ("code", &typo)])
        .await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("typing error"), "{}", refused.body);

    let joined = office
        .post(
            "/profile/hub",
            &[("action", "join"), ("code", &code.to_lowercase())],
        )
        .await;
    assert_eq!(
        joined.location(),
        "/profile/hub?done=joined",
        "{}",
        joined.body
    );
    let account = office.account().unwrap();
    assert_eq!(account.account, home.account().unwrap().account);
    assert_eq!(account.key, home.account().unwrap().key);
    assert!(!account.recovery_pending);

    let page = office.get("/profile/hub").await;
    assert!(
        page.body.contains("<strong>Home library</strong>"),
        "{}",
        page.body
    );
    assert!(
        page.body
            .contains("<strong>Office</strong> <span class=\"muted\">(this library)")
    );
}

#[tokio::test]
async fn leaving_forgets_the_account_and_revokes_this_library() {
    let hub = TestHub::start().await;
    let home = Lib::new(&hub.url, "Home library");
    let code = home.register(&hub.invite()).await;

    // Not before the recovery code is saved: it is the only copy.
    let page = home.get("/profile/hub").await;
    assert!(!page.body.contains("Leave the hub account"));
    let refused = home.post("/profile/hub", &[("action", "leave")]).await;
    assert_eq!(refused.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(refused.body.contains("Save the recovery code first"));
    assert!(home.account().is_some());
    home.post("/profile/hub", &[("action", "saved")]).await;
    assert!(
        home.get("/profile/hub")
            .await
            .body
            .contains("Leave the hub account")
    );

    let office = Lib::new(&hub.url, "Office");
    office
        .post("/profile/hub", &[("action", "join"), ("code", &code)])
        .await;
    assert_eq!(hub.device_count(), 2);
    let token = office.account().unwrap().token;

    let left = office.post("/profile/hub", &[("action", "leave")]).await;
    assert_eq!(left.location(), "/profile/hub?done=left");
    assert!(office.account().is_none());
    assert_eq!(hub.device_count(), 1);
    let (status, _) = hub
        .api(Method::GET, "devices", Some(token.as_str()), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let page = office.get("/profile/hub").await;
    assert!(page.body.contains("Join your account"));
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
        .post("/profile/hub", &[("action", "register"), ("invite", "x")])
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

    async fn serve(router: axum::Router) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, router).await });
        port
    }

    let hits = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&hits);
    let target = serve(axum::Router::new().route(
        "/api/devices",
        get(move || {
            counted.fetch_add(1, Ordering::Relaxed);
            async { axum::Json(serde_json::json!({ "devices": [] })) }
        }),
    ))
    .await;
    let location = format!("http://127.0.0.1:{target}/api/devices");
    let redirecting = serve(axum::Router::new().route(
        "/api/devices",
        get(move || async move { (StatusCode::FOUND, [(header::LOCATION, location.clone())]) }),
    ))
    .await;

    let url = HubUrl::parse(&format!("http://127.0.0.1:{redirecting}")).unwrap();
    let result = tokio::task::spawn_blocking(move || {
        HubClient::new(url).devices(&Secret::from_text("token"))
    })
    .await
    .unwrap();
    match result {
        Err(ClientError::Unreachable(text)) => assert!(text.contains("redirects"), "{text}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(hits.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn each_profile_has_its_own_hub_account() {
    use shelfgauge::profiles::session::{self, Level};
    use shelfgauge::profiles::{ProfileId, Role, store as profiles};

    let hub = TestHub::start().await;
    let lib = Lib::new(&hub.url, "Home library");
    let (anna, owner_session, anna_session) = {
        let conn = lib.userdata.lock();
        let anna = profiles::create(&conn, "Anna", 1, Role::Reader, 0)
            .unwrap()
            .id;
        (
            anna,
            session::create(&conn, ProfileId::OWNER, Level::Local, false, 0).unwrap(),
            session::create(&conn, anna, Level::Local, false, 0).unwrap(),
        )
    };
    lib.act_as(Some(owner_session.as_str()));
    lib.register(&hub.invite()).await;

    lib.act_as(Some(anna_session.as_str()));
    let page = lib.get("/profile/hub").await;
    assert!(page.body.contains("New hub account"), "{}", page.body);
    assert!(!page.body.contains("Home library"));
    assert_eq!(
        lib.get("/profile/hub/recovery").await.status,
        StatusCode::NOT_FOUND
    );
    for action in ["leave", "pair", "saved"] {
        let reply = lib.post("/profile/hub", &[("action", action)]).await;
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY, "{action}");
    }
    assert!(lib.account_of(anna).is_none());

    // Anna registers her own account.
    lib.register(&hub.invite()).await;
    let (mine, hers) = (lib.account().unwrap(), lib.account_of(anna).unwrap());
    assert_ne!(mine.account, hers.account);
    assert_ne!(mine.key, hers.key);
    assert_eq!(hub.state.hub().accounts().unwrap().len(), 2);
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
    assert!(card.body.contains("Open on phone"), "{}", card.body);

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
    assert!(card.body.contains("The book is on your shelf"));
    assert!(card.body.contains("Remove from shelf"));
    assert!(card.body.contains(&format!("{}/r/{id}", hub.url)));

    // A device opens the manifest with the book key from the shelf item.
    let account = lib.account().unwrap();
    let token = account.token.as_str();
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
                assert_eq!(value["format_version"], 1);
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
        move || sync::push(&userdata, &account, &HubClient::new(url))
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(pushed, 1);
    assert_eq!(lib.outbox(), 0);
    let records = hub.records(account.token.as_str()).await;
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
    let token = lib.account().unwrap().token;

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
    assert!(card.body.contains("Open on phone"));
    assert!(card.body.contains("The book is off your shelf"));
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
    let token = lib.account().unwrap().token;
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

    lib.get("/profile/hub").await;
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
    assert!(!card.body.contains("Open on phone"));
    assert_eq!(lib.publish(book).await, "protected");
    assert!(lib.shelf().is_empty());
    let token = lib.account().unwrap().token;
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
    let token = lib.account().unwrap().token;
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
    let push = |account: shelfgauge::hub_client::store::HubAccount| {
        let userdata = std::sync::Arc::clone(&lib.userdata);
        let url = HubUrl::parse(&hub.url).unwrap();
        tokio::task::spawn_blocking(move || {
            sync::push(&userdata, &account, &HubClient::new(url)).unwrap()
        })
    };
    assert_eq!(push(account.clone()).await.unwrap(), 0, "nothing to push");
    assert_eq!(lib.outbox(), 0);
    let records = hub.records(account.token.as_str()).await;
    assert_eq!(records.len(), 3);
    assert!(
        records.iter().all(|(_, _, data)| data.is_some()),
        "{records:?}"
    );

    // Taking the book off the shelf deletes them all, also those of the removed book.
    assert_eq!(lib.shelf_action(book, "remove").await, "removed");
    let records = hub.records(account.token.as_str()).await;
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
    let token = lib.account().unwrap().token;
    let (_, list) = hub
        .api(Method::GET, "pub", Some(token.as_str()), None)
        .await;
    assert_eq!(list["publications"].as_array().unwrap().len(), 1);
    // Publishing a published book again keeps its copy.
    let before = lib.shelf()[0].publication;
    assert_eq!(lib.publish(book).await, "published");
    assert_eq!(lib.shelf()[0].publication, before);
}

/// Two libraries on one account with the same book, `home` has it on the shelf with a
/// bookmark and `office` has pulled it: `(hub, home, office, book id, bookmark id)`.
async fn shared() -> (TestHub, Lib, Lib, i64, String) {
    let hub = TestHub::start().await;
    let home = Lib::new(&hub.url, "Home library");
    let code = home.register(&hub.invite()).await;
    home.post("/profile/hub", &[("action", "saved")]).await;
    let book = home.book("book.txt");
    let mark = home.bookmark(book, "b2").await;
    assert_eq!(home.publish(book).await, "published");
    let office = Lib::new(&hub.url, "Office");
    office.join(&code).await;
    office.sync().await;
    let office_book = office.book("book.txt");
    assert_eq!(office_book, book, "same library layout");
    (hub, home, office, book, mark)
}

#[tokio::test]
async fn two_libraries_share_the_shelf_and_the_marks() {
    let (_hub, home, office, book, mark) = shared().await;
    let shelf = office.shelf();
    assert_eq!(shelf.len(), 1);
    assert!(!shelf[0].own);
    assert_eq!(shelf[0].instance, "Home library");
    assert_eq!(shelf[0].publication, home.shelf()[0].publication);
    let card = office.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("from Home library"), "{}", card.body);
    assert!(card.body.contains("Bookmarks (1)"));
    let pulled: i64 = office.value(&format!(
        "SELECT count(*) FROM annotations WHERE id = '{mark}' AND book_id = {book}"
    ));
    assert_eq!(pulled, 1, "attached to the office's own copy of the book");
    assert_eq!(office.outbox(), 0, "a merge is not pushed back");
    let exported = std::fs::read_dir(office.library.annotations_dir())
        .map(|d| d.count())
        .unwrap_or(0);
    assert!(exported > 0, "the export follows the merge");

    // The office's bookmark and position reach home.
    office.bookmark(book, "b4").await;
    let reply = office
        .json(
            &format!("/book/{book}/position"),
            serde_json::json!({ "block": "b5" }),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
    assert_eq!(office.outbox(), 2);
    office.sync().await;
    assert_eq!(office.outbox(), 0);
    home.sync().await;
    let card = home.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("Bookmarks (2)"), "{}", card.body);
    assert!(card.body.contains("read?at=b5"), "{}", card.body);
    assert_eq!(home.outbox(), 0);
}

#[tokio::test]
async fn a_newer_edit_beats_a_deletion_and_a_newer_deletion_beats_an_edit() {
    let (_hub, home, office, _book, mark) = shared().await;
    let t = shelfgauge::annotations::now();

    // Office deletes at t, home edits the note at t + 10: the edit wins everywhere.
    office.sql(&format!(
        "UPDATE annotations SET deleted_at = {t}, deleted_reason = 'user', updated_at = {t}
          WHERE id = '{mark}'"
    ));
    home.sql(&format!(
        "UPDATE annotations SET note = 'kept', updated_at = {} WHERE id = '{mark}'",
        t + 10
    ));
    office.sync().await;
    home.sync().await;
    office.sync().await;
    for lib in [&home, &office] {
        let (note, deleted): (String, Option<i64>) = lib
            .userdata
            .lock()
            .query_row(
                &format!("SELECT note, deleted_at FROM annotations WHERE id = '{mark}'"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((note.as_str(), deleted), ("kept", None));
    }

    // Now the deletion is newer: it wins everywhere.
    office.sql(&format!(
        "UPDATE annotations SET deleted_at = {0}, deleted_reason = 'user', updated_at = {0}
          WHERE id = '{mark}'",
        t + 30
    ));
    home.sql(&format!(
        "UPDATE annotations SET note = 'lost', updated_at = {} WHERE id = '{mark}'",
        t + 20
    ));
    home.sync().await;
    office.sync().await;
    home.sync().await;
    for lib in [&home, &office] {
        let reason: String = lib.value(&format!(
            "SELECT deleted_reason FROM annotations WHERE id = '{mark}'"
        ));
        assert_eq!(reason, "user");
        assert_eq!(lib.outbox(), 0);
    }
}

#[tokio::test]
async fn edits_at_the_same_second_end_the_same_everywhere() {
    let (_hub, home, office, _book, mark) = shared().await;
    let t = shelfgauge::annotations::now() + 5;
    home.sql(&format!(
        "UPDATE annotations SET note = 'from home', updated_at = {t} WHERE id = '{mark}'"
    ));
    office.sql(&format!(
        "UPDATE annotations SET note = 'from office', updated_at = {t} WHERE id = '{mark}'"
    ));
    home.sync().await;
    office.sync().await;
    home.sync().await;
    let query = format!("SELECT note FROM annotations WHERE id = '{mark}'");
    let (a, b): (String, String) = (home.value(&query), office.value(&query));
    assert_eq!(a, b, "both libraries agree");
    // The bytewise larger plaintext wins: "from office" > "from home".
    assert_eq!(a, "from office");
}

#[tokio::test]
async fn removing_on_one_library_takes_the_book_off_the_others() {
    let (_hub, home, office, book, mark) = shared().await;
    assert_eq!(home.shelf_action(book, "remove").await, "removed");
    office.sync().await;
    assert!(office.shelf().is_empty());
    let kept: i64 = office.value(&format!(
        "SELECT count(*) FROM annotations WHERE id = '{mark}' AND deleted_at IS NULL"
    ));
    assert_eq!(kept, 1, "local marks stay");
    let card = office.get(&format!("/book/{book}")).await;
    assert!(card.body.contains("Open on phone"));
}

#[tokio::test]
async fn records_of_a_book_this_library_lacks_are_kept_for_later() {
    let hub = TestHub::start().await;
    let home = Lib::new(&hub.url, "Home library");
    let code = home.register(&hub.invite()).await;
    let book = home.book("book.txt");
    let mark = home.bookmark(book, "b2").await;
    assert_eq!(home.publish(book).await, "published");

    let empty = Lib::with_books(&hub.url, "Laptop", &[]);
    empty.join(&code).await;
    empty.sync().await;
    assert_eq!(
        empty.shelf().len(),
        1,
        "the shelf lists the book from Home library"
    );
    let orphan: i64 = empty.value(&format!(
        "SELECT book_id FROM annotations WHERE id = '{mark}' AND deleted_at IS NULL"
    ));
    assert_eq!(orphan, 0, "no book here yet");
    assert_eq!(empty.outbox(), 0);

    // A scan's cleanup keeps them while the book is on the shelf, even with no
    // retention (regression: they were marked removed and purged).
    let exporter = shelfgauge::annotations::export::Exporter::new(
        empty.library.annotations_dir(),
        "http://127.0.0.1:8742/",
    );
    shelfgauge::annotations::attach::cleanup_library(&empty.library, &empty.userdata, &exporter, 0)
        .unwrap();
    let kept: i64 = empty.value(&format!(
        "SELECT count(*) FROM annotations WHERE id = '{mark}' AND deleted_at IS NULL"
    ));
    assert_eq!(kept, 1);
}

#[tokio::test]
async fn a_sync_notices_expired_publications_and_reports_errors() {
    use shelfgauge::hub_client::store::ShelfState;

    let hub = TestHub::start().await;
    let lib = Lib::registered(&hub).await;
    let book = lib.book("book.txt");
    assert_eq!(lib.publish(book).await, "published");
    let token = lib.account().unwrap().token;
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
    let home = Lib::new(&hub.url, "Home library");
    let code = home.register(&hub.invite()).await;
    let book = home.book("book.txt");
    assert_eq!(home.publish(book).await, "published");
    let office = Lib::new(&hub.url, "Office");
    office.join(&code).await;

    let worker = shelfgauge::web::hub_sync::start(office.state.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while office.shelf().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the worker did not sync"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    worker.abort();
    assert!(office.account().unwrap().synced_at.is_some());
}

#[tokio::test]
async fn marks_at_places_the_book_does_not_have_do_not_break_its_pages() {
    // A buggy device could push them; the hub cannot forge them (AEAD).
    let (_hub, home, office, book, _mark) = shared().await;
    let t = shelfgauge::annotations::now() + 5;
    home.sql(&format!(
        "INSERT INTO annotations (id, profile_id, book_key, book_id, start_block, start_offset,
             end_block, end_offset, text, prefix, suffix, book_title, book_authors, note,
             created_at, updated_at)
         SELECT 'far-away', 1, book_key, book_id, 99999, 0, 99999, 4, 'gone', '', '', 'T', 'A',
                'far', {t}, {t} FROM annotations LIMIT 1;
         INSERT INTO annotations (id, profile_id, book_key, book_id, start_block, start_offset,
             end_block, end_offset, text, prefix, suffix, book_title, book_authors, note,
             created_at, updated_at)
         SELECT 'backwards', 1, book_key, book_id, 2, 10, 2, 3, 'odd', '', '', 'T', 'A',
                'odd', {t}, {t} FROM annotations LIMIT 1;"
    ));
    home.sync().await;
    office.sync().await;
    let merged: i64 =
        office.value("SELECT count(*) FROM annotations WHERE id IN ('far-away', 'backwards')");
    assert_eq!(merged, 2);
    for uri in [
        format!("/book/{book}"),
        format!("/book/{book}/annotations"),
        format!("/book/{book}/read"),
        format!("/book/{book}/read?part=2"),
    ] {
        let reply = office.get(&uri).await;
        assert_eq!(reply.status, StatusCode::OK, "{uri}: {}", reply.body);
    }
}

#[tokio::test]
async fn two_profiles_sync_with_separate_accounts() {
    use shelfgauge::profiles::session::{self, Level};
    use shelfgauge::profiles::{ProfileId, Role, store as profiles};

    let hub = TestHub::start().await;
    let lib = Lib::new(&hub.url, "Home library");
    let (anna, owner_session, anna_session) = {
        let conn = lib.userdata.lock();
        let anna = profiles::create(&conn, "Anna", 1, Role::Reader, 0)
            .unwrap()
            .id;
        (
            anna,
            session::create(&conn, ProfileId::OWNER, Level::Local, false, 0).unwrap(),
            session::create(&conn, anna, Level::Local, false, 0).unwrap(),
        )
    };
    let book = lib.book("book.txt");
    let mut marks = Vec::new();
    for session in [&owner_session, &anna_session] {
        lib.act_as(Some(session.as_str()));
        lib.register(&hub.invite()).await;
        lib.post("/profile/hub", &[("action", "saved")]).await;
        marks.push(lib.bookmark(book, "b2").await);
        assert_eq!(lib.publish(book).await, "published");
        lib.sync().await;
    }
    for session in [&owner_session, &anna_session] {
        lib.act_as(Some(session.as_str()));
        lib.sync().await;
    }
    for (profile, mark) in [(ProfileId::OWNER, &marks[0]), (anna, &marks[1])] {
        let ids: Vec<String> = {
            let conn = lib.userdata.lock();
            let mut stmt = conn
                .prepare("SELECT id FROM annotations WHERE profile_id = ?1")
                .unwrap();
            stmt.query_map([profile.0], |r| r.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(ids, std::slice::from_ref(mark), "{profile:?}");
        let shelf = shelfgauge::hub_client::store::shelf(&lib.userdata.lock(), profile).unwrap();
        assert_eq!(shelf.len(), 1);
        assert!(shelf[0].own);
    }
    let (owner_account, anna_account) = (lib.account().unwrap(), lib.account_of(anna).unwrap());
    assert_ne!(owner_account.account, anna_account.account);
    // Each account's hub holds only its own records.
    let records = hub.records(anna_account.token.as_str()).await;
    assert!(
        records
            .iter()
            .any(|(k, id, _)| k == "annotation" && *id == marks[1])
    );
    assert!(!records.iter().any(|(_, id, _)| *id == marks[0]));
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
