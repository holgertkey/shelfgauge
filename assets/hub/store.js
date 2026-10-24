// The hub reader's state in this browser and its calls to the hub.
//
// IndexedDB "shelfgauge-hub":
// - "state": "pairing" → { account, device, token, aes, mac } (aes and mac are
//   non-extractable CryptoKeys of the account key: scripts can use them, not read
//   them), "seq" → the cursor of the last pull;
// - "records": the account's sync records, still encrypted: { kind, id, seq, data }
//   (data a Uint8Array, null for a record the hub deleted); changes made here are
//   written here first;
// - "outbox": { kind, id } of the records changed here and not pushed yet;
// - "copies": books opened from book files (bookfile.js): { publication,
//   content_key, title, authors, book_key (sealed under the account key, as in the
//   file's sg.json), imported_at }; "files": their sealed blobs { publication,
//   path, blob }.
// Plaintext is never stored: records are opened when a page needs them.
//
// Merging follows the instance (src/hub_client/merge.rs): per record the newer
// `updated_at` wins, on equal times the bytewise larger plaintext; a record changed
// here that loses is replaced and leaves the outbox. Record plaintexts are written
// exactly as the instance writes them (keys sorted, compact JSON; pinned by
// tests/fixtures/publication/record_plaintexts.json), since ties compare bytes.
import * as c from "./crypto.js";

const DB_NAME = "shelfgauge-hub";
const DB_VERSION = 3;
// The most records in one push, and the largest record the hub takes.
const PUSH_BATCH = 500;
const MAX_RECORD_BYTES = 1024 * 1024;

/** The hub's base path ("" at the root), from the page. */
export const base = document.body.dataset.base || "";

let dbPromise = null;

function db() {
  if (!dbPromise) {
    dbPromise = new Promise((resolve, reject) => {
      const open = indexedDB.open(DB_NAME, DB_VERSION);
      open.onupgradeneeded = (event) => {
        const d = open.result;
        if (event.oldVersion < 1) {
          d.createObjectStore("state");
          d.createObjectStore("records", { keyPath: ["kind", "id"] });
        }
        if (event.oldVersion < 2) {
          d.createObjectStore("outbox", { keyPath: ["kind", "id"] });
        }
        if (event.oldVersion < 3) {
          d.createObjectStore("copies", { keyPath: "publication" });
          d.createObjectStore("files", { keyPath: ["publication", "path"] });
        }
      };
      open.onsuccess = () => {
        const d = open.result;
        // A newer version of the app (another tab) upgrades the database: this tab
        // lets go and reloads into the new version.
        d.onversionchange = () => {
          d.close();
          location.reload();
        };
        resolve(d);
      };
      open.onerror = () => reject(open.error);
      // An older tab holds the database open and did not let go.
      open.onblocked = () => reject(new Error("Close the other shelfgauge tabs and reload this page."));
    });
  }
  return dbPromise;
}

function done(request) {
  return new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
}

async function tx(stores, mode, fn) {
  const t = (await db()).transaction(stores, mode);
  const finished = new Promise((resolve, reject) => {
    t.oncomplete = resolve;
    t.onerror = () => reject(t.error);
    t.onabort = () => reject(t.error || new Error("aborted"));
  });
  const result = await fn(t);
  await finished;
  return result;
}

/** The pairing of this browser, or null. */
export function pairing() {
  return tx(["state"], "readonly", (t) => done(t.objectStore("state").get("pairing"))).then((p) => p || null);
}

/** Forgets the pairing and everything synced (revoked token, or another account). */
export async function forget() {
  await dropBlobs(() => true);
  return tx(["state", "records", "outbox", "copies", "files"], "readwrite", (t) => {
    for (const name of ["state", "records", "outbox", "copies", "files"]) {
      t.objectStore(name).clear();
    }
  });
}

/** Thrown when the hub no longer accepts this device's token. */
export class Unpaired extends Error {}

/** Thrown when the hub is out of reach behind its proxy (a 5xx answer). */
export class Unreachable extends Error {}

/** A call to the hub's API with the device token. */
export async function api(path, options = {}, token) {
  const headers = Object.assign({}, options.headers);
  if (token) {
    headers.Authorization = `Bearer ${token}`;
  }
  const response = await fetch(`${base}/api/${path}`, Object.assign({}, options, { headers, cache: "no-store" }));
  if (response.status === 401) {
    throw new Unpaired("This device is no longer paired.");
  }
  if (response.status >= 500) {
    throw new Unreachable(`The hub is not available (${response.status}).`);
  }
  return response;
}

/** A short name for this browser, e.g. "Chrome on Android" (shown in the library). */
export function deviceName() {
  const ua = navigator.userAgent;
  const browser = [["Edg/", "Edge"], ["Firefox/", "Firefox"], ["Chrome/", "Chrome"], ["Safari/", "Safari"]]
    .find(([needle]) => ua.includes(needle));
  const system = [["iPhone", "iPhone"], ["iPad", "iPad"], ["Android", "Android"], ["Mac OS X", "Mac"], ["Windows", "Windows"], ["Linux", "Linux"]]
    .find(([needle]) => ua.includes(needle));
  return `${browser ? browser[1] : "Browser"}${system ? ` on ${system[1]}` : ""}`;
}

/**
 * Pairs this browser with the pairing code of a QR link (`<secret>.<account key>`).
 * Throws an Error with a message for the reader.
 */
export async function pair(code) {
  const dot = code.indexOf(".");
  let raw;
  try {
    raw = c.unbase64url(code.slice(dot + 1));
  } catch (e) {
    raw = null;
  }
  const secret = code.slice(0, dot);
  if (dot < 1 || !raw || raw.length !== 32 || !/^[A-Za-z0-9_-]+$/.test(secret)) {
    throw new Error("This pairing link is incomplete. Scan the QR code again, or copy the whole link.");
  }
  // Non-extractable from here on; the raw bytes are dropped.
  const aes = await c.aesKey(raw);
  const mac = await c.macKey(raw);
  const name = c.base64url(await c.sealDeviceName(aes, deviceName()));
  raw.fill(0);
  const response = await api("pair", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ secret, device_name: name }),
  });
  if (response.status === 403) {
    const answer = await response.json().catch(() => ({}));
    throw new Error(answer.error === "quota_exceeded"
      ? "Your account has as many devices as this hub allows. Remove one in your library first."
      : "This pairing code has expired or was used already. Show a new one in your library.");
  }
  if (response.status === 429) {
    throw new Error("Too many attempts. Wait a minute and try again.");
  }
  if (!response.ok) {
    throw new Error(`The hub refused the pairing (${response.status}).`);
  }
  const issued = await response.json();
  const previous = await pairing();
  await tx(["state", "records", "outbox", "copies", "files"], "readwrite", (t) => {
    const state = t.objectStore("state");
    if (!previous || previous.account !== issued.account) {
      state.clear();
      for (const name of ["records", "outbox", "copies", "files"]) {
        t.objectStore(name).clear();
      }
    }
    state.put({ account: issued.account, device: issued.device, token: issued.token, aes, mac }, "pairing");
  });
}

/** Whether `(time, bytes)` of `a` is greater than that of `b`. */
function newer(a, b) {
  if (a.time !== b.time) {
    return a.time > b.time;
  }
  const n = Math.min(a.bytes.length, b.bytes.length);
  for (let i = 0; i < n; i++) {
    if (a.bytes[i] !== b.bytes[i]) {
      return a.bytes[i] > b.bytes[i];
    }
  }
  return a.bytes.length > b.bytes.length;
}

/** `(updated_at, plaintext)` of a sealed record, or null if it does not open. */
async function stamp(key, kind, id, data) {
  try {
    const bytes = await c.openRecord(key, kind, id, data);
    const time = Number(JSON.parse(new TextDecoder().decode(bytes)).updated_at);
    return { time: Number.isFinite(time) ? time : 0, bytes };
  } catch (e) {
    return null;
  }
}

function sameBytes(a, b) {
  if (a === b) {
    return true;
  }
  if (!a || !b || a.length !== b.length) {
    return false;
  }
  return a.every((x, i) => x === b[i]);
}

/**
 * Whether the hub's record `remote` replaces this device's `local` copy (both
 * `{ kind, id, data }`, data sealed or null), by the instance's rule: the newer
 * `updated_at` wins, on equal times the bytewise larger plaintext (an echo of the
 * same bytes changes nothing). A record the hub deleted (`null`: its book left the
 * shelf) always wins; one that does not open with the account key never does.
 * `key` is the account's AES key.
 */
export async function remoteWins(key, remote, local) {
  if (!remote.data) {
    return true;
  }
  const r = await stamp(key, remote.kind, remote.id, remote.data);
  if (!r) {
    return false;
  }
  if (!local || !local.data) {
    return true;
  }
  const l = await stamp(key, local.kind, local.id, local.data);
  return !l || newer(r, l);
}

/**
 * Pulls the records changed on the hub since the last pull and merges them into the
 * records store. Returns the number of records that changed here.
 */
export async function pull(p) {
  let seq = (await tx(["state"], "readonly", (t) => done(t.objectStore("state").get("seq")))) || 0;
  let changed = 0;
  for (;;) {
    const response = await api(`sync?since=${seq}`, {}, p.token);
    if (!response.ok) {
      throw new Error(`sync: ${response.status}`);
    }
    const answer = await response.json();
    const incoming = answer.records.map((r) => ({
      kind: r.kind, id: r.id, seq: r.seq, data: r.data === null ? null : c.unbase64url(r.data),
    }));
    // Decisions need decrypting, which cannot happen inside a transaction: decide
    // first, then write only where the local record is still what was compared.
    const keys = incoming.map((r) => [r.kind, r.id]);
    const [locals, pending] = await tx(["records", "outbox"], "readonly", (t) => Promise.all([
      Promise.all(keys.map((k) => done(t.objectStore("records").get(k)))),
      Promise.all(keys.map((k) => done(t.objectStore("outbox").get(k)))),
    ]));
    const take = [];
    for (let i = 0; i < incoming.length; i++) {
      const remote = incoming[i];
      const local = locals[i];
      if (!(await remoteWins(p.aes, remote, local))) {
        continue;
      }
      take.push({ remote, local: local ? local.data : undefined, pending: Boolean(pending[i]) });
    }
    await tx(["state", "records", "outbox"], "readwrite", async (t) => {
      const records = t.objectStore("records");
      const outbox = t.objectStore("outbox");
      const now = await Promise.all(take.map(({ remote }) => done(records.get([remote.kind, remote.id]))));
      take.forEach(({ remote, local, pending: wasPending }, i) => {
        const current = now[i] ? now[i].data : undefined;
        if (!sameBytes(current, local)) {
          return; // Changed here meanwhile: the next pull decides again.
        }
        if (!sameBytes(current, remote.data)) {
          changed += 1;
        }
        records.put(remote);
        if (wasPending) {
          outbox.delete([remote.kind, remote.id]);
        }
      });
      t.objectStore("state").put(answer.seq, "seq");
    });
    seq = answer.seq;
    if (!answer.more) {
      return changed;
    }
  }
}

// Browsers limit the bodies of `keepalive` requests (sent while a page goes away) to
// 64 KiB in all.
const KEEPALIVE_BYTES = 60 * 1024;

/**
 * Pushes the records changed here. Returns the number pushed; a record changed again
 * while it was on its way stays in the outbox. With `keepalive` (the page is being
 * hidden or left) small pushes outlive the page.
 */
export async function push(p, { keepalive = false } = {}) {
  const entries = await tx(["outbox"], "readonly", (t) => done(t.objectStore("outbox").getAll()));
  let pushed = 0;
  for (let at = 0; at < entries.length; at += PUSH_BATCH) {
    const batch = entries.slice(at, at + PUSH_BATCH);
    const records = (await tx(["records"], "readonly", (t) =>
      Promise.all(batch.map((e) => done(t.objectStore("records").get([e.kind, e.id]))))))
      .filter((r) => r && r.data && r.data.length <= MAX_RECORD_BYTES);
    if (!records.length) {
      continue;
    }
    const body = JSON.stringify({ records: records.map((r) => ({ kind: r.kind, id: r.id, data: c.base64url(r.data) })) });
    const response = await api("sync", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body,
      keepalive: keepalive && body.length <= KEEPALIVE_BYTES,
    }, p.token);
    if (!response.ok) {
      throw new Error(`push: ${response.status}`);
    }
    const answer = await response.json();
    await tx(["records", "outbox"], "readwrite", async (t) => {
      const store = t.objectStore("records");
      const now = await Promise.all(records.map((r) => done(store.get([r.kind, r.id]))));
      records.forEach((r, i) => {
        if (now[i] && sameBytes(now[i].data, r.data)) {
          store.put(Object.assign({}, r, { seq: answer.seqs[i] }));
          t.objectStore("outbox").delete([r.kind, r.id]);
          pushed += 1;
        }
      });
    });
  }
  return pushed;
}

/** Whether changes made here wait to be pushed. */
export async function pending() {
  return (await tx(["outbox"], "readonly", (t) => done(t.objectStore("outbox").count()))) > 0;
}

/**
 * Record JSON as the instance writes it: keys sorted, compact, raw UTF-8 (serde_json
 * and JSON.stringify escape the same characters).
 */
export function canonical(value) {
  const sorted = {};
  for (const key of Object.keys(value).sort()) {
    sorted[key] = value[key];
  }
  return JSON.stringify(sorted);
}

/** Writes record `id` of `kind` changed here (sealed), to be pushed. */
export async function write(p, kind, id, value) {
  const plain = new TextEncoder().encode(canonical(value));
  const data = await c.sealRecord(p.aes, kind, id, plain);
  await tx(["records", "outbox"], "readwrite", (t) => {
    t.objectStore("records").put({ kind, id, seq: null, data });
    t.objectStore("outbox").put({ kind, id });
  });
}

/** The opened record `id` of `kind`, or null. */
export async function get(p, kind, id) {
  const r = await tx(["records"], "readonly", (t) => done(t.objectStore("records").get([kind, id])));
  if (!r || !r.data) {
    return null;
  }
  try {
    return JSON.parse(new TextDecoder().decode(await c.openRecord(p.aes, kind, id, r.data)));
  } catch (e) {
    return null;
  }
}

/** The opened records of `kind` (deleted and unreadable ones left out). */
export async function records(p, kind) {
  const all = await tx(["records"], "readonly", (t) =>
    done(t.objectStore("records").getAll(IDBKeyRange.bound([kind, ""], [kind, "￿"]))));
  const out = [];
  for (const r of all) {
    if (!r.data) {
      continue;
    }
    try {
      const plain = await c.openRecord(p.aes, r.kind, r.id, r.data);
      out.push({ id: r.id, value: JSON.parse(new TextDecoder().decode(plain)) });
    } catch (e) {
      // Not sealed for this account at this place: ignored, as the instance does.
    }
  }
  return out;
}

/**
 * The shelf: the account's shelf items whose record id matches their content key,
 * each with its position (if any).
 */
export async function shelf(p) {
  const items = [];
  const positions = new Map((await records(p, "position")).map((r) => [r.id, r.value]));
  for (const { id, value } of await records(p, "shelf_item")) {
    try {
      const contentKey = c.unbase64url(value.content_key);
      if ((await c.shelfId(p.mac, contentKey)) !== id || typeof value.publication !== "string") {
        continue;
      }
      const position = positions.get(await c.positionId(p.mac, contentKey));
      items.push(Object.assign({}, value, { position: position || null }));
    } catch (e) {
      // A broken item: left out.
    }
  }
  return items;
}

// The Service Worker's cache of publication blobs (sw.js).
const BLOBS = "shelfgauge-blobs";

/** The publication id of a cached blob's URL, or null. */
function blobPublication(url) {
  const m = new URL(url).pathname.match(/\/api\/pub\/([0-9a-f]{32})\//);
  return m ? m[1] : null;
}

/** Deletes the cached blobs of every publication `drop(id)` says. */
export async function dropBlobs(drop) {
  if (!("caches" in self)) {
    return;
  }
  const cache = await caches.open(BLOBS);
  for (const request of await cache.keys()) {
    const id = blobPublication(request.url);
    if (id && drop(id)) {
      await cache.delete(request);
    }
  }
}

/** The publications with a cached manifest and every part (readable offline). */
export async function offline() {
  const found = new Set();
  if (!("caches" in self)) {
    return found;
  }
  const cache = await caches.open(BLOBS);
  for (const request of await cache.keys()) {
    const id = blobPublication(request.url);
    if (id) {
      found.add(id);
    }
  }
  const complete = new Set();
  for (const id of found) {
    if (await tx(["state"], "readonly", (t) => done(t.objectStore("state").get(`offline:${id}`)))) {
      complete.add(id);
    }
  }
  return complete;
}

/** Whether the blob at `path` of publication `id` is in the cache. */
export async function cached(id, path) {
  if (!("caches" in self)) {
    return false;
  }
  return Boolean(await (await caches.open(BLOBS)).match(`${location.origin}${base}/api/pub/${id}/${path}`));
}

/** Notes that every blob of publication `id` is cached. */
export function markOffline(id) {
  return tx(["state"], "readwrite", (t) => t.objectStore("state").put(true, `offline:${id}`));
}

/** Fetches blob `path` of publication `id` (raw ciphertext); null if it is gone. */
export async function blob(p, id, path) {
  const response = await api(`pub/${id}/${path}`, {}, p.token);
  if (response.status === 404 || response.status === 410) {
    return null;
  }
  if (!response.ok) {
    throw new Error(`${path}: ${response.status}`);
  }
  return new Uint8Array(await response.arrayBuffer());
}

/**
 * Merges the sealed records of a book file's snapshot (`{ kind, id, data }`) into the
 * records store, by the pull rule; they are the library's own records, so they are
 * not pushed. A record this device knows as deleted (its book left the shelf) is not
 * brought back, unless `revive` (the book goes back on the shelf with the file): then
 * it replaces the deletion and is pushed.
 */
export async function mergeSnapshot(p, incoming, { revive }) {
  const keys = incoming.map((r) => [r.kind, r.id]);
  const locals = await tx(["records"], "readonly", (t) =>
    Promise.all(keys.map((k) => done(t.objectStore("records").get(k)))));
  const take = [];
  for (let i = 0; i < incoming.length; i++) {
    const remote = incoming[i];
    const local = locals[i];
    if (local && !local.data) {
      if (revive && await stamp(p.aes, remote.kind, remote.id, remote.data)) {
        take.push({ remote, local: null, push: true });
      }
      continue;
    }
    if (await remoteWins(p.aes, remote, local)) {
      take.push({ remote, local: local ? local.data : undefined, push: false });
    }
  }
  await tx(["records", "outbox"], "readwrite", async (t) => {
    const records = t.objectStore("records");
    const now = await Promise.all(take.map(({ remote }) => done(records.get([remote.kind, remote.id]))));
    take.forEach(({ remote, local, push }, i) => {
      const current = now[i] ? now[i].data : undefined;
      if (!sameBytes(current, local)) {
        return; // Changed meanwhile.
      }
      records.put({ kind: remote.kind, id: remote.id, seq: null, data: remote.data });
      if (push) {
        t.objectStore("outbox").put({ kind: remote.kind, id: remote.id });
      }
    });
  });
}

/** Keeps a book file's copy: its row and its sealed blobs (`[{ path, blob }]`). */
export function saveCopy(copy, blobs) {
  return tx(["copies", "files"], "readwrite", (t) => {
    t.objectStore("copies").put(copy);
    const files = t.objectStore("files");
    for (const { path, blob } of blobs) {
      files.put({ publication: copy.publication, path, blob });
    }
  });
}

/** The books opened from files on this device. */
export function copies() {
  return tx(["copies"], "readonly", (t) => done(t.objectStore("copies").getAll()));
}

/** The copy of publication `id`, or null. */
export function copy(id) {
  return tx(["copies"], "readonly", (t) => done(t.objectStore("copies").get(id))).then((c) => c || null);
}

/** The sealed blob at `path` of copy `id`; null if it is not here. */
export async function copyBlob(id, path) {
  const row = await tx(["files"], "readonly", (t) => done(t.objectStore("files").get([id, path])));
  return row ? new Uint8Array(await row.blob.arrayBuffer()) : null;
}

/** Deletes the copies (and their blobs) that `drop(copy)` says. */
export async function dropCopies(drop) {
  const gone = (await copies()).filter(drop).map((c) => c.publication);
  if (!gone.length) {
    return;
  }
  await tx(["copies", "files"], "readwrite", (t) => {
    const files = t.objectStore("files");
    for (const id of gone) {
      t.objectStore("copies").delete(id);
      files.delete(IDBKeyRange.bound([id, ""], [id, "\uffff"]));
    }
  });
}

/** Asks the browser to keep this site's data under storage pressure (best effort). */
export async function navigatorPersist() {
  try {
    if (navigator.storage && navigator.storage.persist && !(await navigator.storage.persisted())) {
      await navigator.storage.persist();
    }
  } catch (e) {
    // Not offered here.
  }
}
