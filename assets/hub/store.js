// The hub reader's state in this browser and its calls to the hub.
//
// IndexedDB "shelfgauge-hub":
// - "state": "pairing" → { account, device, token, aes, mac } (aes and mac are
//   non-extractable CryptoKeys of the account key: scripts can use them, not read
//   them), "seq" → the cursor of the last pull;
// - "records": the account's sync records as the hub has them, still encrypted:
//   { kind, id, seq, data } (data a Uint8Array, null for a deleted record).
// Plaintext is never stored: records are opened when a page needs them.
import * as c from "./crypto.js";

const DB_NAME = "shelfgauge-hub";
const DB_VERSION = 1;

/** The hub's base path ("" at the root), from the page. */
export const base = document.body.dataset.base || "";

let dbPromise = null;

function db() {
  if (!dbPromise) {
    dbPromise = new Promise((resolve, reject) => {
      const open = indexedDB.open(DB_NAME, DB_VERSION);
      open.onupgradeneeded = () => {
        const d = open.result;
        d.createObjectStore("state");
        d.createObjectStore("records", { keyPath: ["kind", "id"] });
      };
      open.onsuccess = () => resolve(open.result);
      open.onerror = () => reject(open.error);
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
export function forget() {
  return tx(["state", "records"], "readwrite", (t) => {
    t.objectStore("state").clear();
    t.objectStore("records").clear();
  });
}

/** Thrown when the hub no longer accepts this device's token. */
export class Unpaired extends Error {}

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
  await tx(["state", "records"], "readwrite", (t) => {
    const state = t.objectStore("state");
    if (!previous || previous.account !== issued.account) {
      state.clear();
      t.objectStore("records").clear();
    }
    state.put({ account: issued.account, device: issued.device, token: issued.token, aes, mac }, "pairing");
  });
}

/**
 * Pulls the records changed on the hub since the last pull into the records store.
 * Returns the number of records received.
 */
export async function pull(p) {
  let seq = (await tx(["state"], "readonly", (t) => done(t.objectStore("state").get("seq")))) || 0;
  let count = 0;
  for (;;) {
    const response = await api(`sync?since=${seq}`, {}, p.token);
    if (!response.ok) {
      throw new Error(`sync: ${response.status}`);
    }
    const answer = await response.json();
    await tx(["state", "records"], "readwrite", (t) => {
      const records = t.objectStore("records");
      for (const r of answer.records) {
        records.put({ kind: r.kind, id: r.id, seq: r.seq, data: r.data === null ? null : c.unbase64url(r.data) });
      }
      t.objectStore("state").put(answer.seq, "seq");
    });
    count += answer.records.length;
    seq = answer.seq;
    if (!answer.more) {
      return count;
    }
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
