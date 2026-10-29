// A person's accounts (Phase 30): signing in with an address `name@library` and a
// password, redeeming an invite (a new account, or a new password through a reset
// link), and what My account changes with the current password.
//
// The password never leaves this page: the WASM module derives the login key (whose
// SHA-256 the hub keeps) and the wrapping key (which seals the account key: the
// bundle) with Argon2id, never weaker than the built-in minimum. The derived keys
// stay in memory for one sign-in and are never stored.
import * as c from "./crypto.js";
import * as store from "./store.js";

let wasmModule = null;

/** The WASM module (Argon2id and the address rules). */
export async function wasm() {
  if (!wasmModule) {
    wasmModule = import("./pkg/shelfgauge.js").then(async (m) => {
      await m.default();
      return m;
    });
  }
  return wasmModule;
}

/** The built-in rules: `{ memory_kib, iterations, salt_len, min_chars }`. */
export async function rules() {
  return JSON.parse((await wasm()).passwordRules());
}

/** `name@library` in its stored form, or null if it is not an address. */
export async function normalizeAddress(text) {
  return (await wasm()).normalizeAddress(text) || null;
}

/** A name in its stored form, or null if it breaks the rules. */
export async function normalizeName(text) {
  return (await wasm()).normalizeLabel(text) || null;
}

/** The login key and the wrapping key (an AES key) of `password`. */
async function derive(password, salt, kdf) {
  const keys = (await wasm()).deriveKeys(password, salt, kdf.memory_kib, kdf.iterations);
  const login = keys.slice(0, 32);
  const wrapping = await c.aesKey(keys.subarray(32));
  keys.fill(0);
  return { login, wrapping };
}

/** The parameters for a new password: the hub's, never below the built-in minimum. */
async function newKdf() {
  const min = await rules();
  let hub = {};
  try {
    const response = await store.api("info");
    hub = (await response.json()).passwords || {};
  } catch (e) {
    // The minimum then.
  }
  return {
    memory_kib: Math.max(min.memory_kib, hub.memory_kib || 0),
    iterations: Math.max(min.iterations, hub.iterations || 0),
  };
}

/**
 * A new password as the hub stores it: `{ body, login }` with the request body
 * (login key, salt, parameters, the account key sealed under the wrapping key) and
 * the login key for a retry. `accountRaw` is the raw account key.
 */
async function newPassword(password, accountRaw) {
  const kdf = await newKdf();
  const salt = crypto.getRandomValues(new Uint8Array((await rules()).salt_len));
  const { login, wrapping } = await derive(password, salt, kdf);
  const bundle = await c.sealBundle(wrapping, accountRaw);
  return {
    login,
    body: {
      login_key: c.base64url(login),
      salt: c.base64url(salt),
      kdf,
      bundle: c.base64url(bundle),
    },
  };
}

/** A refusal of the hub as a message for the person. */
export class Refused extends Error {
  constructor(message, code, devices) {
    super(message);
    this.code = code;
    this.devices = devices || null;
  }
}

async function refusal(response) {
  const answer = await response.json().catch(() => ({}));
  const text = {
    invalid_login: "The address or the password is wrong.",
    invalid_secret: "This link was used already or has expired. Ask your library’s admin for a new one.",
    name_taken: "This name is taken in the library: choose another one.",
    device_limit: "Your account has as many devices as the hub allows: remove one to sign in here.",
    rate_limited: "Too many attempts. Wait a little and try again.",
    bad_request: answer.message || "The hub refused the request.",
  }[answer.error] || `The hub refused (${response.status}).`;
  const wait = response.headers.get("Retry-After");
  return new Refused(wait && answer.error === "rate_limited" ? `${text} (${wait} s)` : text, answer.error, answer.devices);
}

/**
 * Signs in to `address` with `password`; at the device limit throws a Refused with
 * `devices` and a `retry(removeDevice)` that signs in again without deriving the keys
 * a second time. Returns the account, shown from now on.
 */
export async function signIn(address, password) {
  const normalized = await normalizeAddress(address);
  if (!normalized) {
    throw new Refused("An address looks like grandma@smiths: your name, @, your library.", "bad_request");
  }
  const pre = await store.api("prelogin", store.json("POST", { address: normalized }));
  if (!pre.ok) {
    throw await refusal(pre);
  }
  const { salt, kdf } = await pre.json();
  const min = await rules();
  if (kdf.memory_kib < min.memory_kib || kdf.iterations < min.iterations) {
    throw new Refused("The hub asks for weaker password settings than this reader allows.", "bad_request");
  }
  const { login, wrapping } = await derive(password, c.unbase64url(salt), kdf);
  const attempt = async (removeDevice) => {
    const body = { address: normalized, login_key: c.base64url(login) };
    if (removeDevice !== undefined) {
      body.remove_device = removeDevice;
    }
    const response = await store.api("login", store.json("POST", body));
    if (!response.ok) {
      const e = await refusal(response);
      if (e.code === "device_limit") {
        e.retry = attempt;
      }
      throw e;
    }
    const issued = await response.json();
    const bundle = await store.api("bundle", {}, issued.token);
    if (!bundle.ok) {
      throw new Refused(`The hub did not give the account key (${bundle.status}).`, "bad_request");
    }
    const sealed = c.unbase64url((await bundle.json()).bundle);
    const raw = await c.openBundle(wrapping, sealed);
    let p = await store.register(issued, raw);
    p = await store.updateAccount(p, { devices_seen: null });
    return p;
  };
  return attempt(undefined);
}

/**
 * What an invite link `<secret>.<one-time key>` is for, without spending it:
 * `{ kind: "new" | "reset", handle, name, payload, secret, oneTime }`, the payload
 * opened (library, the name the admin gave, the keys).
 */
export async function previewInvite(fragment) {
  const dot = fragment.indexOf(".");
  const secret = fragment.slice(0, dot);
  let oneTime;
  try {
    oneTime = await c.aesKey(c.unbase64url(fragment.slice(dot + 1)));
  } catch (e) {
    oneTime = null;
  }
  if (dot < 1 || !oneTime || !/^[A-Za-z0-9_-]+$/.test(secret)) {
    throw new Refused("This invite link is incomplete. Open the whole link again.", "bad_request");
  }
  const response = await store.api("invites/preview", store.json("POST", { secret }));
  if (!response.ok) {
    throw await refusal(response);
  }
  const preview = await response.json();
  let payload;
  try {
    payload = await c.openInvite(oneTime, c.unbase64url(preview.payload));
  } catch (e) {
    throw new Refused("This invite link is damaged. Ask your library’s admin for a new one.", "bad_request");
  }
  return Object.assign({}, preview, { payload, secret });
}

/**
 * Spends an invite: a new account `name` with `password` (a reset keeps the
 * address), then this browser as its device, the library key of the invite, and for
 * a new account the person's display name and colour as the `profile` record.
 */
export async function redeemInvite(preview, { name, password, displayName, color, removeDevice }) {
  const accountRaw = c.unbase64url(preview.payload.account_key);
  const fresh = await newPassword(password, accountRaw);
  const body = { secret: preview.secret, password: fresh.body };
  if (preview.kind === "new") {
    body.name = name;
  }
  if (removeDevice !== undefined) {
    body.remove_device = removeDevice;
  }
  const response = await store.api("invites/redeem", store.json("POST", body));
  if (!response.ok) {
    throw await refusal(response);
  }
  const issued = await response.json();
  const keys = { [preview.payload.library_key.key_id]: c.unbase64url(preview.payload.library_key.key) };
  let p = await store.register(issued, accountRaw, keys);
  p = await store.updateAccount(p, { library_name: preview.payload.library_name });
  if (preview.kind === "new") {
    await store.write(p, "profile", "profile", {
      color,
      display_name: displayName,
      updated_at: Math.floor(Date.now() / 1000),
    });
    try {
      await store.push(p);
    } catch (e) {
      // Pushed with the next sync.
    }
  }
  return p;
}

/** The login key of account `p`'s current password (checked by the hub on use). */
async function currentLogin(p, password) {
  const response = await store.api("bundle", {}, p.token);
  if (!response.ok) {
    throw new Refused("This account has no password yet: set one in your library.", "bad_request");
  }
  const { salt, kdf } = await response.json();
  const { login } = await derive(password, c.unbase64url(salt), kdf);
  return c.base64url(login);
}

async function confirmed(p, path, method, password, extra = {}) {
  const loginKey = await currentLogin(p, password);
  const response = await store.api(path, store.json(method, Object.assign({ login_key: loginKey }, extra)), p.token);
  if (!response.ok) {
    throw await refusal(response);
  }
  return response;
}

/** Changes the password of account `p` (asks the current one). */
export async function changePassword(p, current, fresh) {
  const response = await store.api("bundle", {}, p.token);
  if (!response.ok) {
    throw new Refused("This account has no password yet.", "bad_request");
  }
  const { salt, kdf, bundle } = await response.json();
  const old = await derive(current, c.unbase64url(salt), kdf);
  let accountRaw;
  try {
    accountRaw = await c.openBundle(old.wrapping, c.unbase64url(bundle));
  } catch (e) {
    throw new Refused("The current password is wrong.", "invalid_login");
  }
  const next = await newPassword(fresh, accountRaw);
  accountRaw.fill(0);
  const changed = await store.api("password", store.json("POST", { login_key: c.base64url(old.login), new: next.body }), p.token);
  if (!changed.ok) {
    throw await refusal(changed);
  }
  // Not a change made elsewhere: no notice here.
  return store.updateAccount(p, { password_seen: Math.floor(Date.now() / 1000) });
}

/** Changes the name part of account `p`'s address (asks the password). */
export async function changeName(p, password, name) {
  const normalized = await normalizeName(name);
  if (!normalized) {
    throw new Refused("A name has 3 to 32 Latin letters, digits, dots, dashes or underscores.", "bad_request");
  }
  await confirmed(p, "name", "POST", password, { name: normalized });
  return store.updateAccount(p, { name: normalized });
}

/** The devices of account `p`, their names opened: `[{ id, name, created_at, last_seen_at, current }]`. */
export async function devices(p) {
  const response = await store.api("devices", {}, p.token);
  if (!response.ok) {
    throw new Error(`The hub did not list the devices (${response.status}).`);
  }
  const list = (await response.json()).devices;
  for (const d of list) {
    try {
      d.name = d.name ? await c.openDeviceName(p.aes, c.unbase64url(d.name)) : null;
    } catch (e) {
      d.name = null;
    }
  }
  return list;
}

/** Removes another device of account `p` (asks the password). */
export function removeDevice(p, password, id) {
  return confirmed(p, `devices/${id}`, "DELETE", password);
}

/** Leaves the library: deletes account `p` on the hub (asks the password), then here. */
export async function leave(p, password) {
  try {
    await store.push(p);
  } catch (e) {
    // The marks not pushed go with the account anyway.
  }
  await confirmed(p, "account", "DELETE", password);
  await store.forget(p);
}

/**
 * The devices of account `p` added since this device last looked, by id (the first
 * look only remembers them). Updates what was seen.
 */
export async function newDevices(p) {
  const list = await devices(p);
  const seen = p.devices_seen;
  const ids = list.map((d) => d.id);
  if (!seen) {
    await store.updateAccount(p, { devices_seen: ids });
    return [];
  }
  return list.filter((d) => !d.current && !seen.includes(d.id));
}

/** Dismisses the notice of new devices: they count as seen. */
export async function seeDevices(p) {
  const list = await devices(p);
  return store.updateAccount(p, { devices_seen: list.map((d) => d.id) });
}

/** A password change the hub told for account `p` that this device has not shown yet. */
export async function passwordNotice(p) {
  const changed = await store.passwordChanged(p);
  if (!changed || (p.password_seen && changed.at <= p.password_seen)) {
    return null;
  }
  if (changed.via === "device" && changed.device === p.device) {
    return null;
  }
  return changed;
}

/** Dismisses the notice of a password change. */
export async function seePasswordChange(p) {
  const changed = await store.passwordChanged(p);
  return store.updateAccount(p, { password_seen: changed ? changed.at : Math.floor(Date.now() / 1000) });
}
