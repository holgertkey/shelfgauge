//! The hub's JSON API under `/api/`.
//!
//! Binary values (device names, record data, login keys, salts, key bundles, invite
//! payloads) travel as unpadded base64url strings; blobs as raw request and response
//! bodies. `info` is open; creating a library, `prelogin`, `login`, the invite routes
//! and `pair` need no token and count against the per-address budget
//! ([`limit_public`]); every other route needs a token ([`require_token`]): a
//! library's or a device's, and says which it takes. A library's token acts for one of
//! its accounts in `sync` by naming it in the [`ACCOUNT_HEADER`]. Request bodies are
//! limited per route.

use std::time::Instant;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::rejection::{BytesRejection, JsonRejection, QueryRejection};
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::handler::Handler;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::address::{normalize_label, parse_address};
use super::config::{MIN_PASSWORD_ITERATIONS, MIN_PASSWORD_MEMORY_KIB, Registration};
use super::limits::{LoginKey, client_key};
use super::store::{
    self, Auth, DeviceAuth, DeviceInfo, Kdf, NewDevice, Password, RecordChange, Redeem, Role,
};
use super::{AccountId, HubError, HubState, Secret};
use crate::publication::format::{BlobPath, Envelope, PublicationId, SUPPORTED_VERSIONS};
use crate::web::security::peer_addr;

/// The body limit of the small JSON routes.
const JSON_BODY_LIMIT: usize = 64 * 1024;

/// The largest record ciphertext.
pub const MAX_RECORD_BYTES: usize = 1024 * 1024;

/// The most records in one push.
pub const MAX_SYNC_RECORDS: usize = 500;

/// The body limit of a push.
pub const SYNC_BODY_LIMIT: usize = 16 * 1024 * 1024;

/// The header naming the account (its public id) a library's token syncs.
pub const ACCOUNT_HEADER: &str = "x-shelfgauge-account";

/// The default and the largest page of a pull.
const SYNC_PAGE: u32 = 500;
const SYNC_PAGE_MAX: u32 = 1000;

/// The largest encrypted device name.
const MAX_NAME_BYTES: usize = 1024;

/// The length of a login key.
const LOGIN_KEY_LEN: usize = 32;

/// The largest sealed key bundle.
const MAX_BUNDLE_BYTES: usize = 4096;

/// The largest sealed invite payload.
const MAX_PAYLOAD_BYTES: usize = 8192;

/// The default lifetime of a member invite, in days (capped by the hub's
/// `member_invite_max_days`).
const MEMBER_INVITE_DAYS: u32 = 7;

/// The record kinds.
const KINDS: &[&str] = &[
    "shelf_item",
    "annotation",
    "position",
    "library_key",
    "profile",
];

/// The API routes.
pub(super) fn routes(state: &HubState) -> Router<HubState> {
    let token = || from_fn_with_state(state.clone(), require_token);
    let public = || from_fn_with_state(state.clone(), limit_public);
    let small = || DefaultBodyLimit::max(JSON_BODY_LIMIT);
    let blob_limit = usize::try_from(state.config().limits.max_blob_bytes).unwrap_or(usize::MAX);

    let open = Router::new().route(&state.path("/api/info"), get(info));
    let unauthenticated = Router::new()
        .route(&state.path("/api/libraries"), post(create_library))
        .route(&state.path("/api/prelogin"), post(prelogin))
        .route(&state.path("/api/login"), post(login))
        .route(&state.path("/api/invites/preview"), post(preview_invite))
        .route(&state.path("/api/invites/redeem"), post(redeem_invite))
        .route(&state.path("/api/pair"), post(pair))
        .layer(small())
        .route_layer(public());
    let authenticated = Router::new()
        .route(&state.path("/api/account"), get(account).delete(leave))
        .route(&state.path("/api/bundle"), get(bundle))
        .route(&state.path("/api/password"), post(change_password))
        .route(&state.path("/api/name"), post(change_name))
        .route(&state.path("/api/devices"), get(devices))
        .route(&state.path("/api/devices/{id}"), delete(remove_device))
        .route(&state.path("/api/library"), delete(delete_library))
        .route(&state.path("/api/library/handle"), post(change_handle))
        .route(&state.path("/api/library/accounts"), get(members))
        .route(
            &state.path("/api/library/accounts/{id}"),
            delete(delete_member),
        )
        .route(
            &state.path("/api/library/accounts/{id}/devices"),
            delete(sign_out_member),
        )
        .route(&state.path("/api/library/invites"), post(create_invite))
        .route(
            &state.path("/api/library/invites/{id}"),
            delete(withdraw_invite),
        )
        .route(
            &state.path("/api/library/owner/password"),
            put(set_owner_password),
        )
        .route(&state.path("/api/library/owner/name"), post(set_owner_name))
        .route(
            &state.path("/api/library/owner/devices"),
            get(owner_devices),
        )
        .route(
            &state.path("/api/library/owner/devices/{id}"),
            delete(remove_owner_device),
        )
        .route(&state.path("/api/pairings"), post(create_pairing))
        .route(&state.path("/api/pub"), get(publications))
        .route(&state.path("/api/pub/{id}"), delete(delete_publication))
        .route(
            &state.path("/api/pub/{id}/{*blob}"),
            get(get_blob).put(put_blob.layer(DefaultBodyLimit::max(blob_limit))),
        )
        .route(
            &state.path("/api/sync"),
            get(pull).post(push.layer(DefaultBodyLimit::max(SYNC_BODY_LIMIT))),
        )
        .layer(small())
        .route_layer(token());
    open.merge(unauthenticated).merge(authenticated)
}

/// Middleware: the library or device token of `Authorization: Bearer`; records
/// `library` (and a device's `account`) on the request span and takes one request
/// from the token's rate limit.
async fn require_token(State(state): State<HubState>, mut req: Request, next: Next) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return HubError::Unauthorized.into_response();
    };
    let now = state.now();
    let found = state
        .db(move |conn| store::authenticate(conn, &token, now))
        .await;
    let auth = match found {
        Ok(Some(auth)) => auth,
        Ok(None) => return HubError::Unauthorized.into_response(),
        Err(e) => return e.into_response(),
    };
    let span = tracing::Span::current();
    span.record("library", auth.library());
    // Device ids are positive; a library's token takes the negative of its id.
    let bucket = match auth {
        Auth::Device(device) => {
            span.record("account", device.account);
            device.device
        }
        Auth::Library(library) => -library,
    };
    if let Err(wait) = state.0.devices.take(bucket, Instant::now()) {
        tracing::debug!("rate limited a token");
        return HubError::RateLimited(wait).into_response();
    }
    req.extensions_mut().insert(auth);
    next.run(req).await
}

/// The device of a request that only a signed-in device may make.
fn device_only(auth: Auth) -> Result<DeviceAuth, HubError> {
    match auth {
        Auth::Device(device) => Ok(device),
        Auth::Library(_) => Err(HubError::Forbidden("only a signed-in device may do this")),
    }
}

/// The library of a request that only its own token may make.
fn library_only(auth: Auth) -> Result<i64, HubError> {
    match auth {
        Auth::Library(library) => Ok(library),
        Auth::Device(_) => Err(HubError::Forbidden("only the library may do this")),
    }
}

/// The `X-Forwarded-For` header name.
const X_FORWARDED_FOR: &str = "x-forwarded-for";

/// Middleware: the per-address budget of the routes without a token, keyed by
/// [`client_key`] (never logged).
async fn limit_public(State(state): State<HubState>, mut req: Request, next: Next) -> Response {
    let forwarded: Option<Vec<&str>> = (req.headers().get_all(X_FORWARDED_FOR).iter())
        .map(|value| value.to_str().ok())
        .collect();
    let address = client_key(
        peer_addr(&req).map(|a| a.ip()),
        forwarded.as_deref(),
        &state.0.trusted_proxies,
    );
    if let Err(wait) = state.0.public.begin(address, Instant::now()) {
        tracing::debug!("rate limited a request without a token");
        return HubError::RateLimited(wait).into_response();
    }
    req.extensions_mut().insert(ClientKey(address));
    next.run(req).await
}

/// The key of the request's client for the per-address limits (set by
/// [`limit_public`]).
#[derive(Debug, Clone, Copy)]
struct ClientKey(Option<std::net::IpAddr>);

/// The token of an `Authorization: Bearer <token>` header.
fn bearer(headers: &HeaderMap) -> Option<Secret> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty() && token.len() <= 256)
        .then(|| Secret::from_text(token))
}

/// The body of a JSON route, or the error to answer (never echoing the body).
fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, HubError> {
    match body {
        Ok(Json(value)) => Ok(value),
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            Err(HubError::TooLarge)
        }
        Err(_) => Err(HubError::BadRequest("invalid JSON body")),
    }
}

/// An optional JSON body: `None` when the body is empty.
fn optional_json<T: DeserializeOwned>(
    body: Result<Bytes, BytesRejection>,
) -> Result<Option<T>, HubError> {
    let body = match body {
        Ok(body) => body,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return Err(HubError::TooLarge);
        }
        Err(_) => return Err(HubError::BadRequest("cannot read the body")),
    };
    if body.is_empty() {
        return Ok(None);
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|_| HubError::BadRequest("invalid JSON body"))
}

/// Decodes an unpadded base64url field of at most `max` bytes.
fn decode(value: &str, max: usize, what: &'static str) -> Result<Vec<u8>, HubError> {
    // Four characters per three bytes: reject long text before decoding it.
    if value.len() > max.div_ceil(3) * 4 {
        return Err(HubError::TooLarge);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| HubError::BadRequest(what))?;
    if bytes.len() > max {
        return Err(HubError::TooLarge);
    }
    Ok(bytes)
}

fn encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// A device name field (encrypted by the client), if any.
fn device_name(name: Option<&str>) -> Result<Option<Vec<u8>>, HubError> {
    name.map(|n| decode(n, MAX_NAME_BYTES, "invalid device name"))
        .transpose()
}

/// A login key field (exactly [`LOGIN_KEY_LEN`] bytes).
fn login_key(value: &str) -> Result<Vec<u8>, HubError> {
    let key = decode(value, LOGIN_KEY_LEN, "invalid login key")?;
    if key.len() != LOGIN_KEY_LEN {
        return Err(HubError::BadRequest("invalid login key"));
    }
    Ok(key)
}

/// A name or handle field in its stored form.
fn label(value: &str, what: &'static str) -> Result<String, HubError> {
    normalize_label(value).ok_or(HubError::BadRequest(what))
}

/// A public id from the path or a body; malformed ones are "not found".
fn public_id(id: &str) -> Result<AccountId, HubError> {
    id.parse().map_err(|_| HubError::NotFound)
}

/// A device list as the API sends it; `current` marks the calling device.
pub(super) fn device_list(devices: &[DeviceInfo], current: Option<i64>) -> serde_json::Value {
    let list: Vec<_> = devices
        .iter()
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name.as_deref().map(encode),
                "created_at": d.created_at,
                "last_seen_at": d.last_seen_at,
                "current": Some(d.id) == current,
            })
        })
        .collect();
    serde_json::Value::Array(list)
}

/// The answer to a new device token.
fn issued(device: NewDevice) -> Response {
    let body = json!({
        "library": device.library_id.to_string(),
        "account": device.account_id.to_string(),
        "device": device.device,
        "token": device.token.as_str(),
    });
    (StatusCode::CREATED, Json(body)).into_response()
}

/// `GET /api/info`: what this hub supports and allows, and whether it accepts new
/// data (`accepting`: below `max_total_bytes`).
async fn info(State(state): State<HubState>) -> Result<Json<serde_json::Value>, HubError> {
    let accepting = state.accepting().await?;
    let config = state.config();
    Ok(Json(json!({
        "service": "shelfgauge-hub",
        "version": crate::VERSION,
        "formats": SUPPORTED_VERSIONS,
        "registration": config.libraries.registration.as_str(),
        "accepting": accepting,
        "operator": {
            "name": config.operator.name,
            "contact": config.operator.contact,
            "terms": state.hub().terms().is_some(),
        },
        "passwords": {
            "algorithm": "argon2id",
            "memory_kib": config.passwords.memory_kib,
            "iterations": config.passwords.iterations,
            "parallelism": 1,
        },
        "limits": {
            "max_blob_bytes": config.limits.max_blob_bytes,
            "max_record_bytes": MAX_RECORD_BYTES,
            "max_sync_records": MAX_SYNC_RECORDS,
            "max_sync_body_bytes": SYNC_BODY_LIMIT,
            "publication_expiry_days": config.limits.publication_expiry_days,
            "pairing_ttl_secs": store::PAIRING_TTL,
            "max_library_accounts": store::MAX_LIBRARY_ACCOUNTS,
            "member_invite_max_days": config.libraries.member_invite_max_days,
        },
        "quotas": {
            "max_bytes": config.quotas.max_bytes,
            "max_publications": config.quotas.max_publications,
            "max_devices": config.quotas.max_devices,
        },
    })))
}

/// The body of `POST /api/libraries`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewLibraryBody {
    invite: Option<String>,
    handle: String,
    owner_name: String,
}

/// `POST /api/libraries`: a new library with its token and its owner account.
async fn create_library(
    State(state): State<HubState>,
    Extension(ClientKey(client)): Extension<ClientKey>,
    body: Result<Json<NewLibraryBody>, JsonRejection>,
) -> Result<Response, HubError> {
    let body = json_body(body)?;
    let handle = label(&body.handle, "invalid handle")?;
    let owner_name = label(&body.owner_name, "invalid name")?;
    // Invite codes are their own limit; only open registration counts addresses.
    let open = state.config().libraries.registration == Registration::Open;
    if open {
        state
            .0
            .new_libraries
            .check(client, Instant::now())
            .map_err(HubError::LibraryLimit)?;
    }
    let invite = body.invite.as_deref().map(Secret::from_text);
    let config = state.config().libraries.clone();
    let now = state.now();
    let library = state
        .db(move |conn| {
            store::create_library(conn, &config, invite.as_ref(), &handle, &owner_name, now)
        })
        .await?;
    if open {
        state.0.new_libraries.count(client);
    }
    let span = tracing::Span::current();
    span.record("library", library.library);
    span.record("account", library.owner);
    tracing::info!(library = library.library, "library created");
    let body = json!({
        "library": library.library_id.to_string(),
        "token": library.token.as_str(),
        "owner": library.owner_id.to_string(),
    });
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

/// The body of `POST /api/prelogin`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreloginBody {
    address: String,
}

fn kdf_json(kdf: Kdf) -> serde_json::Value {
    json!({
        "algorithm": "argon2id",
        "memory_kib": kdf.memory_kib,
        "iterations": kdf.iterations,
        "parallelism": 1,
    })
}

/// `POST /api/prelogin`: the salt and Argon2id parameters of an address (a stable
/// fake salt for an unknown one).
async fn prelogin(
    State(state): State<HubState>,
    body: Result<Json<PreloginBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, HubError> {
    let body = json_body(body)?;
    let (name, handle) = parse_address(&body.address)
        .ok_or(HubError::BadRequest("invalid address (name@library)"))?;
    let defaults = state.config().passwords.clone();
    let prelogin = state
        .db(move |conn| store::prelogin(conn, &name, &handle, &defaults))
        .await?;
    Ok(Json(json!({
        "salt": encode(&prelogin.salt),
        "kdf": kdf_json(prelogin.kdf),
    })))
}

/// The body of `POST /api/login`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginBody {
    address: String,
    login_key: String,
    device_name: Option<String>,
    remove_device: Option<i64>,
}

/// The [`LoginKey`] of an address.
fn address_key(name: &str, handle: &str) -> LoginKey {
    LoginKey::Address(Sha256::digest(format!("{name}@{handle}").as_bytes()).into())
}

/// `POST /api/login`: signs in with the login key and adds this browser as a device;
/// at the device limit `409 device_limit` lists the devices, and the same request with
/// `remove_device` removes one first. Failures slow the address and the client down.
async fn login(
    State(state): State<HubState>,
    Extension(ClientKey(client)): Extension<ClientKey>,
    body: Result<Json<LoginBody>, JsonRejection>,
) -> Result<Response, HubError> {
    let body = json_body(body)?;
    let (name, handle) = parse_address(&body.address)
        .ok_or(HubError::BadRequest("invalid address (name@library)"))?;
    let key = login_key(&body.login_key)?;
    let device_name = device_name(body.device_name.as_deref())?;
    let mut keys = vec![address_key(&name, &handle)];
    keys.extend(client.map(LoginKey::Client));
    let now = state.now();
    state
        .0
        .logins
        .check(&keys, now)
        .map_err(HubError::RateLimited)?;
    let max_devices = state.config().quotas.max_devices;
    let remove = body.remove_device;
    let result = state
        .db(move |conn| {
            store::login(
                conn,
                &name,
                &handle,
                &key,
                device_name.as_deref(),
                remove,
                max_devices,
                now,
            )
        })
        .await;
    let device = match result {
        Ok(device) => device,
        Err(HubError::InvalidLogin) => {
            state.0.logins.fail(&keys, now);
            tracing::info!("failed sign-in");
            return Err(HubError::InvalidLogin);
        }
        Err(e) => return Err(e),
    };
    state.0.logins.succeed(keys[0]);
    let span = tracing::Span::current();
    span.record("library", device.library);
    span.record("account", device.account);
    tracing::info!(
        account = device.account,
        device = device.device,
        "signed in"
    );
    Ok(issued(device))
}

/// A password as a client sends it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PasswordBody {
    login_key: String,
    salt: String,
    kdf: KdfBody,
    bundle: String,
}

/// Argon2id parameters as a client sends them.
#[derive(Debug, Deserialize)]
struct KdfBody {
    memory_kib: u32,
    iterations: u32,
}

impl PasswordBody {
    /// The password to store; parameters below the built-in minimum are refused.
    fn parse(&self) -> Result<Password, HubError> {
        let key = login_key(&self.login_key)?;
        let salt = decode(&self.salt, store::SALT_LEN, "invalid salt")?;
        if salt.len() != store::SALT_LEN {
            return Err(HubError::BadRequest("invalid salt"));
        }
        if self.kdf.memory_kib < MIN_PASSWORD_MEMORY_KIB
            || self.kdf.iterations < MIN_PASSWORD_ITERATIONS
        {
            return Err(HubError::BadRequest("the password parameters are too weak"));
        }
        let bundle = decode(&self.bundle, MAX_BUNDLE_BYTES, "invalid bundle")?;
        if bundle.is_empty() {
            return Err(HubError::BadRequest("invalid bundle"));
        }
        Ok(Password {
            login_hash: store::login_hash(&key),
            salt,
            kdf: Kdf {
                memory_kib: self.kdf.memory_kib,
                iterations: self.kdf.iterations,
            },
            bundle,
        })
    }
}

/// The body of `POST /api/invites/preview`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InviteSecret {
    secret: String,
}

/// `POST /api/invites/preview`: what a member invite is for, without spending it.
async fn preview_invite(
    State(state): State<HubState>,
    body: Result<Json<InviteSecret>, JsonRejection>,
) -> Result<Json<serde_json::Value>, HubError> {
    let body = json_body(body)?;
    let secret = Secret::from_text(&body.secret);
    let now = state.now();
    let preview = state
        .db(move |conn| store::preview_invite(conn, &secret, now))
        .await?;
    Ok(Json(json!({
        "kind": if preview.name.is_some() { "reset" } else { "new" },
        "handle": preview.handle,
        "name": preview.name,
        "payload": encode(&preview.payload),
        "expires_at": preview.expires_at,
    })))
}

/// The body of `POST /api/invites/redeem`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RedeemBody {
    secret: String,
    name: Option<String>,
    password: PasswordBody,
    device_name: Option<String>,
    remove_device: Option<i64>,
}

/// `POST /api/invites/redeem`: spends a member invite: a new account (`409
/// name_taken` leaves the invite unspent) or a new password of the account it resets,
/// and this browser as its device.
async fn redeem_invite(
    State(state): State<HubState>,
    body: Result<Json<RedeemBody>, JsonRejection>,
) -> Result<Response, HubError> {
    let body = json_body(body)?;
    let secret = Secret::from_text(&body.secret);
    let name = body
        .name
        .as_deref()
        .map(|n| label(n, "invalid name"))
        .transpose()?;
    let password = body.password.parse()?;
    let device_name = device_name(body.device_name.as_deref())?;
    let remove = body.remove_device;
    let max_devices = state.config().quotas.max_devices;
    let now = state.now();
    let device = state
        .db(move |conn| {
            let redeem = Redeem {
                name: name.as_deref(),
                password: &password,
                device_name: device_name.as_deref(),
                remove_device: remove,
            };
            store::redeem_invite(conn, &secret, &redeem, max_devices, now)
        })
        .await?;
    let span = tracing::Span::current();
    span.record("library", device.library);
    span.record("account", device.account);
    tracing::info!(
        account = device.account,
        device = device.device,
        "invite redeemed"
    );
    Ok(issued(device))
}

/// The body of `POST /api/pair`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairRequest {
    secret: String,
    device_name: Option<String>,
}

/// `POST /api/pair`: a pairing secret for a device token of the owner account.
async fn pair(
    State(state): State<HubState>,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Result<Response, HubError> {
    let body = json_body(body)?;
    let secret = Secret::from_text(&body.secret);
    let name = device_name(body.device_name.as_deref())?;
    let max_devices = state.config().quotas.max_devices;
    let now = state.now();
    let device = state
        .db(move |conn| store::pair(conn, &secret, max_devices, name.as_deref(), now))
        .await?;
    let span = tracing::Span::current();
    span.record("library", device.library);
    span.record("account", device.account);
    tracing::info!(
        account = device.account,
        device = device.device,
        "device paired"
    );
    Ok(issued(device))
}

/// `GET /api/account`: for a device its account (address parts, role) and the
/// library's usage and quotas; for a library's token the library's.
async fn account(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let defaults = state.config().quotas.clone();
    let library = auth.library();
    let (ids, usage, quotas, owner) = state
        .db(move |conn| {
            let owner = store::owner_account(conn, library)?;
            Ok((
                store::library_ids(conn, library)?,
                store::usage(conn, library)?,
                store::effective_quotas(conn, library, &defaults)?,
                store::account_ids(conn, owner)?.0,
            ))
        })
        .await?;
    let mut body = json!({
        "library": ids.0.to_string(),
        "handle": ids.1,
        "owner": owner.to_string(),
        "usage": {
            "bytes": usage.bytes,
            "publications": usage.publications,
            "accounts": usage.accounts,
            "devices": usage.devices,
            "records": usage.records,
        },
        "quotas": {
            "max_bytes": quotas.max_bytes,
            "max_publications": quotas.max_publications,
            "max_devices": quotas.max_devices,
        },
    });
    if let Auth::Device(device) = auth {
        let (id, name, role, has_password) = state
            .db(move |conn| {
                let (id, name, role) = store::account_ids(conn, device.account)?;
                let has_password = store::bundle(conn, device.account).is_ok();
                Ok((id, name, role, has_password))
            })
            .await?;
        body["account"] = json!(id.to_string());
        body["name"] = json!(name);
        body["role"] = json!(role.as_str());
        body["device"] = json!(device.device);
        body["has_password"] = json!(has_password);
    }
    Ok(Json(body))
}

/// The body of the routes a device confirms with the current password.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirm {
    login_key: String,
}

/// Checks the current password of the device's account, counting failures against
/// the account (a growing delay, never a lock).
async fn confirm(state: &HubState, device: DeviceAuth, login_key: &str) -> Result<(), HubError> {
    let key = self::login_key(login_key)?;
    let keys = [LoginKey::Account(device.account)];
    let now = state.now();
    state
        .0
        .logins
        .check(&keys, now)
        .map_err(HubError::RateLimited)?;
    let result = state
        .db(move |conn| store::check_password(conn, device.account, &key))
        .await;
    match result {
        Ok(()) => {
            state.0.logins.succeed(keys[0]);
            Ok(())
        }
        Err(HubError::InvalidLogin) => {
            state.0.logins.fail(&keys, now);
            tracing::info!(account = device.account, "wrong current password");
            Err(HubError::InvalidLogin)
        }
        Err(e) => Err(e),
    }
}

/// `DELETE /api/account`: the device's account leaves its library (asks the current
/// password); the owner account cannot.
async fn leave(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    body: Result<Json<Confirm>, JsonRejection>,
) -> Result<StatusCode, HubError> {
    let device = device_only(auth)?;
    if device.role == Role::Owner {
        return Err(HubError::Forbidden(
            "the owner account ends only with its library",
        ));
    }
    let body = json_body(body)?;
    confirm(&state, device, &body.login_key).await?;
    state
        .db(move |conn| store::delete_account(conn, device.account))
        .await?;
    tracing::info!(account = device.account, "left the library");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/bundle`: the salt, parameters and sealed key bundle of the device's
/// account (for a password change; `404` without a password).
async fn bundle(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let device = device_only(auth)?;
    let (prelogin, bundle) = state
        .db(move |conn| store::bundle(conn, device.account))
        .await?;
    Ok(Json(json!({
        "salt": encode(&prelogin.salt),
        "kdf": kdf_json(prelogin.kdf),
        "bundle": encode(&bundle),
    })))
}

/// The body of `POST /api/password`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangePassword {
    login_key: String,
    new: PasswordBody,
}

/// `POST /api/password`: replaces the password of the device's account (asks the
/// current one); the account's other devices are told on their next sync.
async fn change_password(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    body: Result<Json<ChangePassword>, JsonRejection>,
) -> Result<StatusCode, HubError> {
    let device = device_only(auth)?;
    let body = json_body(body)?;
    let password = body.new.parse()?;
    confirm(&state, device, &body.login_key).await?;
    let now = state.now();
    state
        .db(move |conn| {
            store::set_password(
                conn,
                device.account,
                &password,
                "device",
                Some(device.device),
                now,
            )
        })
        .await?;
    tracing::info!(account = device.account, "password changed");
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/name`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangeName {
    login_key: String,
    name: String,
}

/// `POST /api/name`: changes the name part of the device's account address (asks
/// the current password); the old name is free at once.
async fn change_name(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    body: Result<Json<ChangeName>, JsonRejection>,
) -> Result<StatusCode, HubError> {
    let device = device_only(auth)?;
    let body = json_body(body)?;
    let name = label(&body.name, "invalid name")?;
    confirm(&state, device, &body.login_key).await?;
    state
        .db(move |conn| store::set_name(conn, device.account, &name))
        .await?;
    tracing::info!(account = device.account, "name changed");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/devices`: the devices of the device's account.
async fn devices(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let device = device_only(auth)?;
    let list = state
        .db(move |conn| store::devices(conn, device.account))
        .await?;
    Ok(Json(
        json!({ "devices": device_list(&list, Some(device.device)) }),
    ))
}

/// `DELETE /api/devices/{id}`: removes a device of the device's account: the calling
/// one without more, another one with the current password (`{"login_key": …}`).
async fn remove_device(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    Path(id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> Result<StatusCode, HubError> {
    let device = device_only(auth)?;
    let id: i64 = id.parse().map_err(|_| HubError::NotFound)?;
    if id != device.device {
        let body: Confirm =
            optional_json(body)?.ok_or(HubError::BadRequest("the current password is needed"))?;
        confirm(&state, device, &body.login_key).await?;
    }
    let removed = state
        .db(move |conn| store::remove_device(conn, device.account, id))
        .await?;
    if !removed {
        return Err(HubError::NotFound);
    }
    tracing::info!(account = device.account, device = id, "device removed");
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/library`: the library deletes itself on the hub with all its
/// accounts, devices, publications and records.
async fn delete_library(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let ids = state
        .db(move |conn| store::delete_library(conn, library))
        .await?
        .ok_or(HubError::NotFound)?;
    state
        .files(move |blobs| {
            for id in &ids {
                blobs.remove(id)?;
            }
            Ok(())
        })
        .await?;
    tracing::info!(library, "library deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/library/handle`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HandleBody {
    handle: String,
}

/// `POST /api/library/handle`: changes the library's handle (every member's
/// address); the old one is free at once.
async fn change_handle(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    body: Result<Json<HandleBody>, JsonRejection>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let body = json_body(body)?;
    let handle = label(&body.handle, "invalid handle")?;
    state
        .db(move |conn| store::set_handle(conn, library, &handle))
        .await?;
    tracing::info!(library, "handle changed");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/library/accounts`: the library's accounts with their addresses' names,
/// device counts and last syncs.
async fn members(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let library = library_only(auth)?;
    let list = state.db(move |conn| store::members(conn, library)).await?;
    let accounts: Vec<_> = list
        .into_iter()
        .map(|m| {
            json!({
                "id": m.id.to_string(),
                "name": m.name,
                "role": m.role.as_str(),
                "has_password": m.has_password,
                "devices": m.devices,
                "last_seen_at": m.last_seen_at,
                "created_at": m.created_at,
                "invite": m.invite_id.map(|id| id.to_string()),
            })
        })
        .collect();
    Ok(Json(json!({ "accounts": accounts })))
}

/// The account `id` (public, from the path) of the library.
async fn member(state: &HubState, library: i64, id: &str) -> Result<i64, HubError> {
    let id = public_id(id)?;
    state
        .db(move |conn| store::library_account(conn, library, &id))
        .await
}

/// `DELETE /api/library/accounts/{id}`: removes a member's account with its devices
/// and records (not the owner's).
async fn delete_member(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    Path(id): Path<String>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let account = member(&state, library, &id).await?;
    state
        .db(move |conn| store::delete_account(conn, account))
        .await?;
    tracing::info!(library, account, "member removed");
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/library/accounts/{id}/devices`: signs every device of an account out.
async fn sign_out_member(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, HubError> {
    let library = library_only(auth)?;
    let account = member(&state, library, &id).await?;
    let removed = state
        .db(move |conn| store::remove_all_devices(conn, account))
        .await?;
    tracing::info!(library, account, count = removed, "all devices signed out");
    Ok(Json(json!({ "removed": removed })))
}

/// The body of `POST /api/library/invites`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewInvite {
    account: Option<String>,
    payload: String,
    days: Option<u32>,
}

/// `POST /api/library/invites`: a member invite for a new person, or with `account`
/// a password reset of that member; the secret is answered once.
async fn create_invite(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    body: Result<Json<NewInvite>, JsonRejection>,
) -> Result<Response, HubError> {
    let library = library_only(auth)?;
    let body = json_body(body)?;
    let payload = decode(&body.payload, MAX_PAYLOAD_BYTES, "invalid payload")?;
    let max = state.config().libraries.member_invite_max_days;
    let days = body.days.unwrap_or(MEMBER_INVITE_DAYS).clamp(1, max);
    let account = match body.account.as_deref() {
        Some(id) => Some(member(&state, library, id).await?),
        None => None,
    };
    let now = state.now();
    let (secret, id, expires_at) = state
        .db(move |conn| store::create_member_invite(conn, library, account, &payload, days, now))
        .await?;
    tracing::info!(library, reset = account.is_some(), "member invite created");
    let body = json!({
        "id": id.to_string(),
        "secret": secret.as_str(),
        "expires_at": expires_at,
    });
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

/// `DELETE /api/library/invites/{id}`: withdraws a pending member invite.
async fn withdraw_invite(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    Path(id): Path<String>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let id = public_id(&id)?;
    let deleted = state
        .db(move |conn| store::delete_member_invite(conn, library, &id))
        .await?;
    if !deleted {
        return Err(HubError::NotFound);
    }
    tracing::info!(library, "member invite withdrawn");
    Ok(StatusCode::NO_CONTENT)
}

/// `PUT /api/library/owner/password`: sets the owner account's password without an
/// old one (the library holds that account's key anyway).
async fn set_owner_password(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    body: Result<Json<PasswordBody>, JsonRejection>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let password = json_body(body)?.parse()?;
    let now = state.now();
    state
        .db(move |conn| {
            let owner = store::owner_account(conn, library)?;
            store::set_password(conn, owner, &password, "library", None, now)
        })
        .await?;
    tracing::info!(library, "owner password set");
    Ok(StatusCode::NO_CONTENT)
}

/// The body of `POST /api/library/owner/name`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnerName {
    name: String,
}

/// `POST /api/library/owner/name`: changes the owner account's name.
async fn set_owner_name(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    body: Result<Json<OwnerName>, JsonRejection>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let name = label(&json_body(body)?.name, "invalid name")?;
    state
        .db(move |conn| {
            let owner = store::owner_account(conn, library)?;
            store::set_name(conn, owner, &name)
        })
        .await?;
    tracing::info!(library, "owner name changed");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/library/owner/devices`: the owner account's devices.
async fn owner_devices(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let library = library_only(auth)?;
    let list = state
        .db(move |conn| store::devices(conn, store::owner_account(conn, library)?))
        .await?;
    Ok(Json(json!({ "devices": device_list(&list, None) })))
}

/// `DELETE /api/library/owner/devices/{id}`: removes a device of the owner account.
async fn remove_owner_device(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    Path(id): Path<String>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let id: i64 = id.parse().map_err(|_| HubError::NotFound)?;
    let removed = state
        .db(move |conn| store::remove_device(conn, store::owner_account(conn, library)?, id))
        .await?;
    if !removed {
        return Err(HubError::NotFound);
    }
    tracing::info!(library, device = id, "owner device removed");
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/pairings`: a one-time pairing secret for the owner account.
async fn create_pairing(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
) -> Result<Response, HubError> {
    let library = library_only(auth)?;
    let now = state.now();
    let secret = state
        .db(move |conn| store::create_pairing(conn, store::owner_account(conn, library)?, now))
        .await?;
    tracing::info!(library, "pairing secret created");
    let body = json!({ "secret": secret.as_str(), "expires_at": now + store::PAIRING_TTL });
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

/// `GET /api/pub`: the library's publications.
async fn publications(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let library = auth.library();
    let list = state
        .db(move |conn| store::publications(conn, library))
        .await?;
    let publications: Vec<_> = list
        .into_iter()
        .map(|p| {
            json!({
                "id": p.id.to_string(),
                "created_at": p.created_at,
                "last_opened_at": p.last_opened_at,
                "bytes": p.bytes,
                "blobs": p.blobs,
            })
        })
        .collect();
    Ok(Json(json!({ "publications": publications })))
}

/// A publication id from the path; malformed ids are "not found".
fn publication_id(id: &str) -> Result<PublicationId, HubError> {
    id.parse().map_err(|_| HubError::NotFound)
}

/// A publication id and blob path from the path; malformed ones are "not found".
fn blob_address((id, blob): (String, String)) -> Result<(PublicationId, BlobPath), HubError> {
    let path = blob.parse().map_err(|_| HubError::NotFound)?;
    Ok((publication_id(&id)?, path))
}

/// `PUT /api/pub/{id}/{blob}`: stores a blob of the library's publication (the first
/// blob creates it); only the library's token publishes. The body must be a sealed
/// blob of a supported format version.
async fn put_blob(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    Path(address): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let (id, path) = blob_address(address)?;
    let max = state.config().limits.max_blob_bytes;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    if declared.is_some_and(|len| len > max) {
        return Err(HubError::TooLarge);
    }
    let body = match body {
        Ok(body) => body,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return Err(HubError::TooLarge);
        }
        Err(_) => return Err(HubError::BadRequest("cannot read the body")),
    };
    if body.len() as u64 > max {
        return Err(HubError::TooLarge);
    }
    if Envelope::parse(&body).is_err() {
        return Err(HubError::BadRequest(
            "not a sealed blob of a supported format version",
        ));
    }
    let quotas = state.config().quotas.clone();
    let total = state.config().limits.max_total_bytes;
    let now = state.now();
    let size = body.len() as u64;
    let reservation = state
        .db(move |conn| store::reserve_blob(conn, library, &id, path, size, &quotas, total, now))
        .await?;
    if let Err(e) = state
        .files(move |blobs| blobs.write(&id, path, &body))
        .await
    {
        state
            .db(move |conn| store::release_blob(conn, reservation, path))
            .await?;
        return Err(e);
    }
    Ok(if reservation.replaced() {
        StatusCode::NO_CONTENT
    } else {
        StatusCode::CREATED
    })
}

/// `GET /api/pub/{id}/{blob}`: a blob of a publication of the token's library.
async fn get_blob(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    Path(address): Path<(String, String)>,
) -> Result<Response, HubError> {
    let (id, path) = blob_address(address)?;
    let library = auth.library();
    let now = state.now();
    state
        .db(move |conn| store::open_blob(conn, library, &id, path, now))
        .await?;
    let data = state
        .files(move |blobs| blobs.read(&id, path))
        .await?
        .ok_or(HubError::NotFound)?;
    let headers = [
        (
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        ),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, no-store"),
        ),
    ];
    Ok((headers, data).into_response())
}

/// `DELETE /api/pub/{id}`: deletes the library's publication with all its blobs (for
/// every member); only the library's token.
async fn delete_publication(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    Path(id): Path<String>,
) -> Result<StatusCode, HubError> {
    let library = library_only(auth)?;
    let id = publication_id(&id)?;
    state
        .db(move |conn| store::delete_publication(conn, library, &id))
        .await?;
    state.files(move |blobs| blobs.remove(&id)).await?;
    tracing::info!(library, "publication deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// The query of `GET /api/sync`.
#[derive(Debug, Deserialize)]
struct PullQuery {
    since: Option<i64>,
    limit: Option<u32>,
}

/// A record as sent to clients.
#[derive(Debug, Serialize)]
struct RecordOut {
    kind: String,
    id: String,
    seq: i64,
    data: Option<String>,
    updated_at: i64,
}

/// The account a sync request is for: a device's own, or for a library's token the
/// one named in [`ACCOUNT_HEADER`] (an account of that library).
async fn sync_account(state: &HubState, auth: Auth, headers: &HeaderMap) -> Result<i64, HubError> {
    match auth {
        Auth::Device(device) => Ok(device.account),
        Auth::Library(library) => {
            let id = (headers.get(ACCOUNT_HEADER))
                .and_then(|v| v.to_str().ok())
                .ok_or(HubError::BadRequest("the account header is missing"))?;
            let account = member(state, library, id).await?;
            tracing::Span::current().record("account", account);
            Ok(account)
        }
    }
}

/// `GET /api/sync?since=N&limit=M`: the account's records changed after `since`, in
/// sequence order. `seq` is the cursor for the next pull: the last record's when
/// `more` is true, else the account's current sequence number. A device also gets
/// the account's last password change (`password_changed`).
async fn pull(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    query: Result<Query<PullQuery>, QueryRejection>,
) -> Result<Json<serde_json::Value>, HubError> {
    let Query(query) = query.map_err(|_| HubError::BadRequest("invalid query"))?;
    let account = sync_account(&state, auth, &headers).await?;
    let since = query.since.unwrap_or(0).max(0);
    let limit = query.limit.unwrap_or(SYNC_PAGE).clamp(1, SYNC_PAGE_MAX);
    let (records, more, current, changed) = state
        .db(move |conn| {
            let (records, more) = store::pull_records(conn, account, since, limit)?;
            Ok((
                records,
                more,
                store::current_seq(conn, account)?,
                store::password_change(conn, account)?,
            ))
        })
        .await?;
    let seq = match records.last() {
        Some(last) if more => last.seq,
        _ => current.max(since),
    };
    let records: Vec<RecordOut> = records
        .into_iter()
        .map(|r| RecordOut {
            kind: r.kind,
            id: r.id,
            seq: r.seq,
            data: r.data.as_deref().map(encode),
            updated_at: r.updated_at,
        })
        .collect();
    let mut body = json!({ "records": records, "seq": seq, "more": more });
    if let (Auth::Device(_), Some(changed)) = (auth, changed) {
        body["password_changed"] = json!({
            "at": changed.at,
            "via": changed.via,
            "device": changed.device,
        });
    }
    Ok(Json(body))
}

/// The body of `POST /api/sync`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushBody {
    records: Vec<RecordIn>,
}

/// A pushed record change; `data: null` deletes the record.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordIn {
    kind: String,
    id: String,
    data: Option<String>,
}

/// Whether `id` is a valid record id: 1 to 128 characters of `A-Z a-z 0-9 _ -`.
fn valid_record_id(id: &str) -> bool {
    (1..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// `POST /api/sync`: applies record changes of the account in order; answers their
/// sequence numbers.
async fn push(
    State(state): State<HubState>,
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    body: Result<Json<PushBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, HubError> {
    let body = json_body(body)?;
    let account = sync_account(&state, auth, &headers).await?;
    let library = auth.library();
    if body.records.len() > MAX_SYNC_RECORDS {
        return Err(HubError::TooLarge);
    }
    let mut changes = Vec::with_capacity(body.records.len());
    for record in body.records {
        if !KINDS.contains(&record.kind.as_str()) {
            return Err(HubError::BadRequest("unknown record kind"));
        }
        if !valid_record_id(&record.id) {
            return Err(HubError::BadRequest("invalid record id"));
        }
        let data = record
            .data
            .as_deref()
            .map(|d| decode(d, MAX_RECORD_BYTES, "invalid record data"))
            .transpose()?;
        changes.push(RecordChange {
            kind: record.kind,
            id: record.id,
            data,
        });
    }
    let quotas = state.config().quotas.clone();
    let total = state.config().limits.max_total_bytes;
    let now = state.now();
    let count = changes.len();
    let seqs = state
        .db(move |conn| store::push_records(conn, library, account, &changes, &quotas, total, now))
        .await?;
    tracing::debug!(account, count, "records pushed");
    let seq = seqs.last().copied();
    Ok(Json(json!({ "seqs": seqs, "seq": seq })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens_are_parsed() {
        let mut headers = HeaderMap::new();
        assert!(bearer(&headers).is_none());
        headers.insert(header::AUTHORIZATION, "Bearer abc".parse().unwrap());
        assert_eq!(bearer(&headers).unwrap().as_str(), "abc");
        headers.insert(header::AUTHORIZATION, "bearer  abc ".parse().unwrap());
        assert_eq!(bearer(&headers).unwrap().as_str(), "abc");
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert!(bearer(&headers).is_none());
        headers.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert!(bearer(&headers).is_none());
    }

    #[test]
    fn record_ids_are_restricted() {
        assert!(valid_record_id("0b4e7c2a-1f3d-4e5a-9b8c-7d6e5f4a3b2c"));
        assert!(valid_record_id(&"a".repeat(128)));
        assert!(!valid_record_id(""));
        assert!(!valid_record_id(&"a".repeat(129)));
        assert!(!valid_record_id("a/b"));
        assert!(!valid_record_id("a b"));
    }

    #[test]
    fn base64_fields_are_size_checked_before_decoding() {
        assert_eq!(decode(&encode(&[1, 2, 3]), 3, "x").unwrap(), [1, 2, 3]);
        assert!(matches!(
            decode(&encode(&[0; 4]), 3, "x"),
            Err(HubError::TooLarge)
        ));
        assert!(matches!(
            decode("!!", 3, "x"),
            Err(HubError::BadRequest("x"))
        ));
        assert!(matches!(
            decode(&"A".repeat(100), 3, "x"),
            Err(HubError::TooLarge)
        ));
    }
}
