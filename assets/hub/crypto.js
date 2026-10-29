// The hub reader's cryptography: publication blobs and sync records, with WebCrypto
// and DecompressionStream. The layouts are those of the instance (Rust):
// src/publication/seal.rs, crates/shelfgauge-core/src/publication/ and
// src/hub_client/keys.rs; tests/fixtures/publication/ holds the shared vectors.

// The publication format and record versions this reader opens. Format 2 (0.21) has a
// plaintext header in each blob: the manifest's holds the book key sealed under the
// publishing library's key (see KeyHeader in crates/shelfgauge-core).
export const FORMAT_VERSION = 2;
export const RECORD_VERSION = 1;
// The key scope this reader knows; another one needs a newer shelfgauge.
export const LIBRARY_SCOPE = "library";
// The largest blob header.
const MAX_HEADER_LEN = 4096;

const NONCE_LEN = 12;
const TAG_LEN = 16;
// A blob that inflates to more than this is refused (a hub blob is at most 64 MiB
// before inflating; this bounds a deflate bomb).
const INFLATE_LIMIT = 256 * 1024 * 1024;

const encoder = new TextEncoder();

/** Bytes as lowercase hex. */
export function hex(bytes) {
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

/** Hex to bytes; throws on anything else. */
export function unhex(text) {
  if (text.length % 2 || /[^0-9a-f]/i.test(text)) {
    throw new Error("not hex");
  }
  const out = new Uint8Array(text.length / 2);
  for (let i = 0; i < out.length; i++) {
    out[i] = parseInt(text.slice(i * 2, i * 2 + 2), 16);
  }
  return out;
}

/** Bytes as base64url without padding (the records' binary values). */
export function base64url(bytes) {
  let s = "";
  for (const b of bytes) {
    s += String.fromCharCode(b);
  }
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}

/** base64url (with or without padding) to bytes; throws on anything else. */
export function unbase64url(text) {
  if (/[^A-Za-z0-9_-]/.test(text.replace(/=+$/, ""))) {
    throw new Error("not base64url");
  }
  const s = atob(text.replace(/-/g, "+").replace(/_/g, "/").replace(/=+$/, ""));
  return Uint8Array.from(s, (c) => c.charCodeAt(0));
}

function concat(...parts) {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

function u16(n) {
  return new Uint8Array([n >> 8, n & 0xff]);
}

/** An AES-256-GCM key from 32 raw bytes (not extractable). */
export function aesKey(raw) {
  return crypto.subtle.importKey("raw", raw, "AES-GCM", false, ["encrypt", "decrypt"]);
}

/** The HMAC-SHA256 key of an account key (for record ids). */
export function macKey(raw) {
  return crypto.subtle.importKey("raw", raw, { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
}

/**
 * The associated data of a publication blob: version, publication id, path, a zero
 * byte, the plaintext header.
 */
export function blobAssociatedData(version, publicationId, path, header = new Uint8Array(0)) {
  return concat(u16(version), publicationId, encoder.encode(path), new Uint8Array([0]), header);
}

/**
 * A sealed blob taken apart: `{ version, header, nonce, ciphertext }`; throws for an
 * unknown version (with a message for the person) and a malformed blob.
 */
export function envelope(data) {
  if (data.length < 4) {
    throw new Error("truncated");
  }
  const version = (data[0] << 8) | data[1];
  if (version !== FORMAT_VERSION) {
    throw new Error(`unsupported version ${version}; ${version < FORMAT_VERSION ? "made by an older shelfgauge" : "a newer shelfgauge may be needed"}`);
  }
  const headerLen = (data[2] << 8) | data[3];
  if (headerLen > MAX_HEADER_LEN || data.length < 4 + headerLen + NONCE_LEN + TAG_LEN) {
    throw new Error("truncated");
  }
  return {
    version,
    header: data.subarray(4, 4 + headerLen),
    nonce: data.subarray(4 + headerLen, 4 + headerLen + NONCE_LEN),
    ciphertext: data.subarray(4 + headerLen + NONCE_LEN),
  };
}

/**
 * The key header of a manifest blob (not yet authenticated: the manifest's tag checks
 * it when it is opened): `{ library, key_scope, key_owner, key_id, book_key }`.
 */
export function keyHeader(manifest) {
  const header = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(envelope(manifest).header));
  if (typeof header.library !== "string" || typeof header.key_scope !== "string"
    || typeof header.key_owner !== "string" || !Number.isInteger(header.key_id)
    || typeof header.book_key !== "string") {
    throw new Error("a bad manifest header");
  }
  return header;
}

/**
 * The book key (an AES key) of publication `idHex` from its key header with the
 * library key generation (an AES key) the header names.
 */
export async function bookKeyFromHeader(header, libraryKey, idHex) {
  const raw = await openRecord(libraryKey, "book_key", idHex, unbase64url(header.book_key));
  return aesKey(raw);
}

/** The associated data of a record: "sg-record", version, kind, 0, id. */
export function recordAssociatedData(version, kind, id) {
  return concat(encoder.encode("sg-record"), u16(version), encoder.encode(kind), new Uint8Array([0]), encoder.encode(id));
}

async function decrypt(key, data, ad, versions) {
  if (data.length < 2 + NONCE_LEN + TAG_LEN) {
    throw new Error("truncated");
  }
  const version = (data[0] << 8) | data[1];
  if (!versions.includes(version)) {
    throw new Error(`unsupported version ${version}; a newer shelfgauge may be needed`);
  }
  const plain = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: data.subarray(2, 2 + NONCE_LEN), additionalData: ad(version) },
    key,
    data.subarray(2 + NONCE_LEN),
  );
  return new Uint8Array(plain);
}

async function encrypt(key, version, ad, plain, nonce) {
  const iv = nonce || crypto.getRandomValues(new Uint8Array(NONCE_LEN));
  const ciphertext = await crypto.subtle.encrypt({ name: "AES-GCM", iv, additionalData: ad }, key, plain);
  return concat(u16(version), iv, new Uint8Array(ciphertext));
}

/** The content of a padded plaintext: `[len u32 BE][data][zeros]`. */
export function unpad(padded) {
  if (padded.length < 4) {
    throw new Error("truncated");
  }
  const len = new DataView(padded.buffer, padded.byteOffset, 4).getUint32(0);
  if (len > padded.length - 4) {
    throw new Error("truncated");
  }
  return padded.subarray(4, 4 + len);
}

/** Inflates raw deflate (RFC 1951), refusing more than `limit` bytes. */
export async function inflate(data, limit = INFLATE_LIMIT) {
  const stream = new Blob([data]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
  const reader = stream.getReader();
  const chunks = [];
  let size = 0;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) {
      break;
    }
    size += value.length;
    if (size > limit) {
      await reader.cancel();
      throw new Error("too large");
    }
    chunks.push(value);
  }
  return concat(...chunks);
}

/**
 * Opens blob `path` ("manifest", "p/<n>", "i/<n>") of publication `publicationId`
 * (16 bytes) with its book key (an AES key): decrypt (the header authenticated),
 * unpad, inflate.
 */
export async function openBlob(bookKey, publicationId, path, blob) {
  const e = envelope(blob);
  const plain = await crypto.subtle.decrypt(
    { name: "AES-GCM", iv: e.nonce, additionalData: blobAssociatedData(e.version, publicationId, path, e.header) },
    bookKey,
    e.ciphertext,
  );
  return inflate(unpad(new Uint8Array(plain)));
}

/** Opens an invite's payload with its one-time key (an AES key): the parsed JSON. */
export async function openInvite(oneTimeKey, sealed) {
  return JSON.parse(new TextDecoder().decode(await openRecord(oneTimeKey, "invite", "", sealed)));
}

/** Seals the raw account key (32 bytes) under a password's wrapping key (the bundle). */
export function sealBundle(wrappingKey, accountKeyRaw) {
  return sealRecord(wrappingKey, "bundle", "", accountKeyRaw);
}

/** Opens a bundle with the wrapping key: the raw account key. */
export function openBundle(wrappingKey, sealed) {
  return openRecord(wrappingKey, "bundle", "", sealed);
}

/** Opens record `id` of `kind` with the account key (an AES key). */
export function openRecord(accountKey, kind, id, data) {
  return decrypt(accountKey, data, (version) => recordAssociatedData(version, kind, id), [RECORD_VERSION]);
}

/**
 * Seals the plaintext bytes of record `id` of `kind`. `nonce` only for fixed test
 * vectors: a nonce must never be used twice with one key.
 */
export function sealRecord(accountKey, kind, id, plain, nonce) {
  return encrypt(accountKey, RECORD_VERSION, recordAssociatedData(RECORD_VERSION, kind, id), plain, nonce);
}

/** Seals a device name (kind "device", empty id). */
export function sealDeviceName(accountKey, name) {
  return sealRecord(accountKey, "device", "", encoder.encode(name));
}

/** Opens a device name sealed by `sealDeviceName`. */
export async function openDeviceName(accountKey, sealed) {
  return new TextDecoder("utf-8", { fatal: true }).decode(await openRecord(accountKey, "device", "", sealed));
}

async function mac(key, label, contentKey) {
  const sig = await crypto.subtle.sign("HMAC", key, concat(encoder.encode(label), contentKey));
  return base64url(new Uint8Array(sig));
}

/** The record id of a book's position: HMAC(account key, "position" ‖ content key). */
export function positionId(macKey, contentKey) {
  return mac(macKey, "position", contentKey);
}

/** The record id of a book's shelf item: HMAC(account key, "shelf" ‖ content key). */
export function shelfId(macKey, contentKey) {
  return mac(macKey, "shelf", contentKey);
}
