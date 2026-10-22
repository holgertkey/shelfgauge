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
