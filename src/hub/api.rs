//! The hub's JSON API under `/api/`.
//!
//! Binary values (device names, record data, the recovery hash) travel as unpadded
//! base64url strings; blobs as raw request and response bodies. Every route except
//! `info`, `account` (create), `pair` and `recover` needs a device token
//! ([`require_device`]); those three count against the per-address budget
//! ([`limit_public`]). Request bodies are limited per route.

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
use axum::routing::{delete, get, post};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::config::Registration;
use super::limits::client_key;
use super::store::{self, DeviceAuth, NewDevice, RecordChange};
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

/// The default and the largest page of a pull.
const SYNC_PAGE: u32 = 500;
const SYNC_PAGE_MAX: u32 = 1000;

/// The largest encrypted device name.
const MAX_NAME_BYTES: usize = 1024;

/// The record kinds.
const KINDS: &[&str] = &["shelf_item", "annotation", "position"];

/// The API routes.
pub(super) fn routes(state: &HubState) -> Router<HubState> {
    let device = || from_fn_with_state(state.clone(), require_device);
    let public = || from_fn_with_state(state.clone(), limit_public);
    let small = || DefaultBodyLimit::max(JSON_BODY_LIMIT);
    let blob_limit = usize::try_from(state.config().limits.max_blob_bytes).unwrap_or(usize::MAX);

    let open = Router::new().route(&state.path("/api/info"), get(info));
    let unauthenticated = Router::new()
        .route(&state.path("/api/account"), post(create_account))
        .route(&state.path("/api/pair"), post(pair))
        .route(&state.path("/api/recover"), post(recover))
        .layer(small())
        .route_layer(public());
    let authenticated = Router::new()
        .route(&state.path("/api/account"), get(account))
        .route(&state.path("/api/pairings"), post(create_pairing))
        .route(&state.path("/api/devices"), get(devices))
        .route(&state.path("/api/devices/{id}"), delete(revoke_device))
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
        .route_layer(device());
    open.merge(unauthenticated).merge(authenticated)
}

/// Middleware: the device token of `Authorization: Bearer`; records `account` on the
/// request span and takes one request from the device's rate limit.
async fn require_device(State(state): State<HubState>, mut req: Request, next: Next) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return HubError::Unauthorized.into_response();
    };
    let now = state.now();
    let found = state
        .db(move |conn| store::device_by_token(conn, &token, now))
        .await;
    let auth = match found {
        Ok(Some(auth)) => auth,
        Ok(None) => return HubError::Unauthorized.into_response(),
        Err(e) => return e.into_response(),
    };
    tracing::Span::current().record("account", auth.account);
    if let Err(wait) = state.0.devices.take(auth.device, Instant::now()) {
        tracing::debug!("rate limited a device");
        return HubError::RateLimited(wait).into_response();
    }
    req.extensions_mut().insert(auth);
    next.run(req).await
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

/// The answer to a new device token.
fn issued(device: NewDevice) -> Response {
    let body = json!({
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
        "registration": config.accounts.registration.as_str(),
        "accepting": accepting,
        "operator": {
            "name": config.operator.name,
            "contact": config.operator.contact,
            "terms": state.hub().terms().is_some(),
        },
        "limits": {
            "max_blob_bytes": config.limits.max_blob_bytes,
            "max_record_bytes": MAX_RECORD_BYTES,
            "max_sync_records": MAX_SYNC_RECORDS,
            "max_sync_body_bytes": SYNC_BODY_LIMIT,
            "publication_expiry_days": config.limits.publication_expiry_days,
            "pairing_ttl_secs": store::PAIRING_TTL,
        },
        "quotas": {
            "max_bytes": config.quotas.max_bytes,
            "max_publications": config.quotas.max_publications,
            "max_devices": config.quotas.max_devices,
        },
    })))
}

/// The body of `POST /api/account`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NewAccount {
    invite: Option<String>,
    recovery_hash: String,
    device_name: Option<String>,
}

/// `POST /api/account`: a new account and its first device token.
async fn create_account(
    State(state): State<HubState>,
    Extension(ClientKey(client)): Extension<ClientKey>,
    body: Result<Json<NewAccount>, JsonRejection>,
) -> Result<Response, HubError> {
    let body = json_body(body)?;
    // Invite codes are their own limit; only open registration counts addresses.
    let open = state.config().accounts.registration == Registration::Open;
    if open {
        state
            .0
            .new_accounts
            .check(client, Instant::now())
            .map_err(HubError::AccountLimit)?;
    }
    let hash: [u8; 32] = decode(&body.recovery_hash, 32, "invalid recovery hash")?
        .try_into()
        .map_err(|_| HubError::BadRequest("invalid recovery hash"))?;
    let name = device_name(body.device_name.as_deref())?;
    let invite = body.invite.as_deref().map(Secret::from_text);
    let accounts = state.config().accounts.clone();
    let now = state.now();
    let device = state
        .db(move |conn| {
            store::create_account(
                conn,
                &accounts,
                invite.as_ref(),
                &hash,
                name.as_deref(),
                now,
            )
        })
        .await?;
    if open {
        state.0.new_accounts.count(client);
    }
    tracing::Span::current().record("account", device.account);
    tracing::info!(
        account = device.account,
        device = device.device,
        "account created"
    );
    Ok(issued(device))
}

/// `GET /api/account`: the own account's usage and quotas.
async fn account(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let defaults = state.config().quotas.clone();
    let (id, usage, quotas) = state
        .db(move |conn| {
            Ok((
                store::account_id(conn, auth.account)?,
                store::usage(conn, auth.account)?,
                store::effective_quotas(conn, auth.account, &defaults)?,
            ))
        })
        .await?;
    Ok(Json(json!({
        "account": id.to_string(),
        "device": auth.device,
        "usage": {
            "bytes": usage.bytes,
            "publications": usage.publications,
            "devices": usage.devices,
            "records": usage.records,
        },
        "quotas": {
            "max_bytes": quotas.max_bytes,
            "max_publications": quotas.max_publications,
            "max_devices": quotas.max_devices,
        },
    })))
}

/// `POST /api/pairings`: a one-time pairing secret of the own account.
async fn create_pairing(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
) -> Result<Response, HubError> {
    let now = state.now();
    let secret = state
        .db(move |conn| store::create_pairing(conn, auth.account, now))
        .await?;
    tracing::info!(account = auth.account, "pairing secret created");
    let body = json!({ "secret": secret.as_str(), "expires_at": now + store::PAIRING_TTL });
    Ok((StatusCode::CREATED, Json(body)).into_response())
}

/// The body of `POST /api/pair`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PairRequest {
    secret: String,
    device_name: Option<String>,
}

/// `POST /api/pair`: a pairing secret for a device token.
async fn pair(
    State(state): State<HubState>,
    body: Result<Json<PairRequest>, JsonRejection>,
) -> Result<Response, HubError> {
    let body = json_body(body)?;
    let secret = Secret::from_text(&body.secret);
    let name = device_name(body.device_name.as_deref())?;
    let quotas = state.config().quotas.clone();
    let now = state.now();
    let device = state
        .db(move |conn| store::pair(conn, &secret, &quotas, name.as_deref(), now))
        .await?;
    tracing::Span::current().record("account", device.account);
    tracing::info!(
        account = device.account,
        device = device.device,
        "device paired"
    );
    Ok(issued(device))
}

/// The body of `POST /api/recover`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecoverRequest {
    account: String,
    secret: String,
    device_name: Option<String>,
}

/// `POST /api/recover`: the recovery secret for a device token.
async fn recover(
    State(state): State<HubState>,
    body: Result<Json<RecoverRequest>, JsonRejection>,
) -> Result<Response, HubError> {
    let body = json_body(body)?;
    let account: AccountId = body.account.parse()?;
    let secret = Secret::from_text(&body.secret);
    let name = device_name(body.device_name.as_deref())?;
    let quotas = state.config().quotas.clone();
    let now = state.now();
    let device = state
        .db(move |conn| store::recover(conn, account, &secret, &quotas, name.as_deref(), now))
        .await?;
    tracing::Span::current().record("account", device.account);
    tracing::info!(
        account = device.account,
        device = device.device,
        "account recovered"
    );
    Ok(issued(device))
}

/// `GET /api/devices`: the own account's devices.
async fn devices(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let list = state
        .db(move |conn| store::devices(conn, auth.account))
        .await?;
    let devices: Vec<_> = list
        .into_iter()
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name.as_deref().map(encode),
                "created_at": d.created_at,
                "last_seen_at": d.last_seen_at,
                "current": d.id == auth.device,
            })
        })
        .collect();
    Ok(Json(json!({ "devices": devices })))
}

/// `DELETE /api/devices/{id}`: revokes a device of the own account (also the own one).
async fn revoke_device(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
    Path(id): Path<String>,
) -> Result<StatusCode, HubError> {
    let id: i64 = id.parse().map_err(|_| HubError::NotFound)?;
    let now = state.now();
    let revoked = state
        .db(move |conn| store::revoke_device(conn, auth.account, id, now))
        .await?;
    if !revoked {
        return Err(HubError::NotFound);
    }
    tracing::info!(account = auth.account, device = id, "device revoked");
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/pub`: the own account's publications.
async fn publications(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
) -> Result<Json<serde_json::Value>, HubError> {
    let list = state
        .db(move |conn| store::publications(conn, auth.account))
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

/// `PUT /api/pub/{id}/{blob}`: stores a blob of the own account's publication (the
/// first blob creates it). The body must be a sealed blob of a supported format
/// version.
async fn put_blob(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
    Path(address): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<StatusCode, HubError> {
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
        .db(move |conn| {
            store::reserve_blob(conn, auth.account, &id, path, size, &quotas, total, now)
        })
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

/// `GET /api/pub/{id}/{blob}`: a blob of the own account's publication.
async fn get_blob(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
    Path(address): Path<(String, String)>,
) -> Result<Response, HubError> {
    let (id, path) = blob_address(address)?;
    let now = state.now();
    state
        .db(move |conn| store::open_blob(conn, auth.account, &id, path, now))
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

/// `DELETE /api/pub/{id}`: deletes the own account's publication with all its blobs.
async fn delete_publication(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
    Path(id): Path<String>,
) -> Result<StatusCode, HubError> {
    let id = publication_id(&id)?;
    state
        .db(move |conn| store::delete_publication(conn, auth.account, &id))
        .await?;
    state.files(move |blobs| blobs.remove(&id)).await?;
    tracing::info!(account = auth.account, "publication deleted");
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

/// `GET /api/sync?since=N&limit=M`: the own account's records changed after `since`,
/// in sequence order. `seq` is the cursor for the next pull: the last record's when
/// `more` is true, else the account's current sequence number.
async fn pull(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
    query: Result<Query<PullQuery>, QueryRejection>,
) -> Result<Json<serde_json::Value>, HubError> {
    let Query(query) = query.map_err(|_| HubError::BadRequest("invalid query"))?;
    let since = query.since.unwrap_or(0).max(0);
    let limit = query.limit.unwrap_or(SYNC_PAGE).clamp(1, SYNC_PAGE_MAX);
    let (records, more, current) = state
        .db(move |conn| {
            let (records, more) = store::pull_records(conn, auth.account, since, limit)?;
            Ok((records, more, store::current_seq(conn, auth.account)?))
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
    Ok(Json(
        json!({ "records": records, "seq": seq, "more": more }),
    ))
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

/// `POST /api/sync`: applies record changes of the own account in order; answers
/// their sequence numbers.
async fn push(
    State(state): State<HubState>,
    Extension(auth): Extension<DeviceAuth>,
    body: Result<Json<PushBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, HubError> {
    let body = json_body(body)?;
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
        .db(move |conn| store::push_records(conn, auth.account, &changes, &quotas, total, now))
        .await?;
    tracing::debug!(account = auth.account, count, "records pushed");
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
