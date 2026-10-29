// The hub's reader app: signing in (`/signin`, an address `name@library` and a
// password), redeeming an invite (`/invite#<secret>.<one-time key>`), pairing the
// owner's phone (`/pair#<secret>.<account key>`, or a book link with
// `#pair=<secret>.<key>`), the shelf of each library this browser is signed in to
// (`/`, with a switcher, All shelves and "Open file" for book files), My account
// (`/device`) and the reading view (`/r/<publication id>?part=<n>`) of a hub
// publication or of a book file's copy kept on this device (bookfile.js).
//
// Books arrive as encrypted blobs; crypto.js opens them, the WASM module
// (pkg/shelfgauge.js, the instance's own renderer) turns them into escaped HTML, and
// the page is then the instance's reading page: reader.js runs on it unchanged, with
// `window.shelfgaugeSource` giving it the pages of other parts. Text from records or
// the manifest goes into the page as text only (`textContent`), never as markup.
import * as account from "./account.js";
import * as bookfile from "./bookfile.js";
import * as c from "./crypto.js";
import * as store from "./store.js";

const SHARE_SCALE = 10000;
const base = store.base;
const route = location.pathname.slice(base.length) || "/";

/** An element with text. */
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (name === "class") {
      node.className = value;
    } else {
      node.setAttribute(name, value);
    }
  }
  node.append(...children);
  return node;
}

/** Whether `e` is only the hub being out of reach (offline, or the hub is down). */
function unreachable(e) {
  return !navigator.onLine || e instanceof TypeError || e instanceof store.Unreachable;
}

/** Where the reader learns how to sign this browser in. */
function signedOutHelp() {
  return [
    el("h1", {}, "This device is not signed in"),
    el("p", {}, "Books on this hub open only on the devices of their library's members."),
    el("p", {}, el("a", { class: "button primary", href: `${base}/signin` }, "Sign in"),
      " with the address your library's admin gave you (like grandma@smiths) and your password."),
    el("p", { class: "muted" }, "Invited? Open the invite link you got. The library's owner pairs their phone with the QR code on the library's computer."),
    el("p", { class: "muted" }, "Signed in before? The browser may have cleared this site's data (Safari does after a week without a visit, unless the app is on the home screen). Sign in again; your marks are kept in your library."),
  ];
}

// A message shown once on the next page (an account was dropped).
const FLASH = "shelfgauge-flash";

function setFlash(text) {
  try {
    sessionStorage.setItem(FLASH, text);
  } catch (e) {
    // Not shown then.
  }
}

function takeFlash() {
  try {
    const text = sessionStorage.getItem(FLASH);
    sessionStorage.removeItem(FLASH);
    return text;
  } catch (e) {
    return null;
  }
}

/** The library's name of account `p` for the person ("your library" if unknown). */
function libraryName(p) {
  return p.library_name || `@${p.handle}`;
}

/**
 * Forgets account `p` here because the hub no longer knows its token (signed out,
 * removed, left or expired: the hub does not say which), with the one message, and
 * reloads.
 */
async function dropAccount(p) {
  await store.forget(p).catch((e) => console.error(e));
  setFlash(`This device is no longer signed in to ${libraryName(p)}: it was signed out, or the account was removed or expired. Sign in again if you still belong to it.`);
  location.reload();
}

// Offline: the Service Worker keeps the app and the books read here. A new version
// of the app (after the hub was upgraded) is switched to on the shelf page only,
// which then reloads; an open book keeps the version it was opened with.
if ("serviceWorker" in navigator) {
  navigator.serviceWorker.register(`${base}/sw.js`, { scope: `${base}/` }).then((registration) => {
    if (route !== "/" || !navigator.serviceWorker.controller) {
      return;
    }
    navigator.serviceWorker.addEventListener("controllerchange", () => location.reload(), { once: true });
    // Asked again while one waits (a busy old version may not let go at once).
    const ask = () => {
      if (registration.waiting) {
        registration.waiting.postMessage("activate");
        setTimeout(ask, 2000);
      }
    };
    ask();
    registration.addEventListener("updatefound", () => {
      const worker = registration.installing;
      worker.addEventListener("statechange", () => {
        if (worker.state === "installed") {
          ask();
        }
      });
    });
  }).catch((e) => console.error(e));
}

function show(...nodes) {
  const app = document.getElementById("hub-app");
  app.replaceChildren(...nodes);
}

// ---- Pairing -------------------------------------------------------------------

async function pairPage() {
  // The fragment holds the account key: out of the address bar and the history at
  // once, before anything else can see it.
  const code = location.hash.slice(1);
  history.replaceState(null, "", location.pathname);
  if (!code) {
    const p = await store.current();
    show(...(p ? [el("h1", {}, "This device is signed in"), el("p", {}, el("a", { href: `${base}/` }, "My books"))] : signedOutHelp()));
    return;
  }
  show(el("h1", {}, "Pairing this device…"));
  try {
    await store.pair(code);
  } catch (e) {
    show(el("h1", {}, "This device was not paired"), el("p", { class: "error" }, e.message));
    return;
  }
  if (navigator.storage && navigator.storage.persist) {
    // Asks the browser to keep the books and keys when space runs low.
    navigator.storage.persist().catch(() => {});
  }
  location.replace(`${base}/`);
}

// ---- Shelf ---------------------------------------------------------------------

function bookItem(item) {
  // A book on the shelf as files only opens from a copy on this device.
  const target = item.copy ? item.copy.publication : (item.kind === "file" ? null : item.publication);
  const link = target
    ? el("a", { href: `${base}/r/${target}` }, item.title || "Untitled")
    : el("strong", {}, item.title || "Untitled");
  const meta = [];
  if (item.authors) {
    meta.push(item.authors);
  }
  if (item.instance) {
    meta.push(`from ${item.instance}`);
  }
  const li = el("li", { class: "hub-book" }, link);
  if (meta.length) {
    li.append(el("span", { class: "muted" }, meta.join(" · ")));
  }
  if (item.copy) {
    li.append(el("span", { class: "muted hub-offline", title: "Opened from a book file: kept on this device, opens without a connection" }, "file · offline"));
  } else if (item.offline) {
    li.append(el("span", { class: "muted hub-offline", title: "Saved on this device: opens without a connection" }, "offline"));
  } else if (!target) {
    li.append(el("span", { class: "muted hub-file-hint" }, "Open its book file on this device"));
  }
  if (item.position && Number.isFinite(item.position.progress)) {
    li.append(el("span", { class: "muted hub-progress" }, `${Math.min(Math.floor(item.position.progress / 10), 100)} %`));
  }
  return li;
}

/** A notice above the shelf, with a button that dismisses it if `dismiss` is given. */
function notice(text, { error = false, action = null, dismiss = null } = {}) {
  const node = el("div", { class: `notice hub-notice${error ? " error" : ""}`, role: "status" }, el("span", {}, text));
  if (action) {
    node.append(" ", action);
  }
  if (dismiss) {
    const button = el("button", { type: "button", class: "hub-notice-dismiss" }, "Dismiss");
    button.addEventListener("click", async () => {
      button.disabled = true;
      await dismiss();
      node.remove();
    });
    node.append(" ", button);
  }
  return node;
}

/** The library switcher (shown with two or more accounts) and All shelves. */
function switcher(all, p, showingAll) {
  if (all.length < 2) {
    return null;
  }
  const select = el("select", { class: "hub-library-switch", "aria-label": "Library" });
  for (const a of all) {
    const option = el("option", { value: a.account }, libraryName(a));
    if (!showingAll && p && a.account === p.account) {
      option.selected = true;
    }
    select.append(option);
  }
  const allOption = el("option", { value: "all" }, "All shelves");
  allOption.selected = showingAll;
  select.append(allOption);
  select.addEventListener("change", async () => {
    if (select.value === "all") {
      location.assign(`${base}/?all=1`);
    } else {
      await store.setCurrent(select.value);
      location.assign(`${base}/`);
    }
  });
  return el("div", { class: "hub-libraries" }, select);
}

function renderShelf(items, status, { head = [], showLibrary = false } = {}) {
  const reading = items
    .filter((i) => i.position && i.position.progress < 1000)
    .sort((a, b) => b.position.updated_at - a.position.updated_at);
  const rest = items
    .filter((i) => !reading.includes(i))
    .sort((a, b) => (a.title || "").localeCompare(b.title || ""));
  const nodes = [el("h1", {}, "My books"), ...head.filter(Boolean)];
  if (status) {
    nodes.push(el("p", { class: "muted hub-sync", role: "status" }, status));
  }
  nodes.push(openFileForm());
  if (!items.length) {
    nodes.push(el("p", { class: "hint" }, "No books yet. Open a book link your library sent you, or a book file with Open file. The library's admin sends books with Read on phone on a book's page."));
  }
  const item = (i) => {
    const li = bookItem(i);
    if (showLibrary && i.library) {
      li.append(el("span", { class: "muted hub-book-library" }, i.library));
    }
    return li;
  };
  if (reading.length) {
    nodes.push(el("h2", {}, "Currently reading"), el("ul", { class: "hub-books" }, ...reading.map(item)));
  }
  if (rest.length) {
    if (reading.length) {
      nodes.push(el("h2", {}, "On the shelf"));
    }
    nodes.push(el("ul", { class: "hub-books" }, ...rest.map(item)));
  }
  show(...nodes);
}

/** The shelf of account `p` with each book's offline state and its book file copy here. */
async function shelfItems(p) {
  const items = await store.shelf(p);
  const offline = await store.offline(p).catch(() => new Set());
  const copies = await store.copies(p).catch(() => []);
  for (const item of items) {
    item.offline = offline.has(item.publication);
    item.library = libraryName(p);
    // The newest copy of the book.
    item.copy = copies
      .filter((copy) => copy.content_key === item.content_key)
      .sort((a, b) => b.imported_at - a.imported_at)[0] || null;
  }
  return items;
}

/**
 * Brings account `p` up to date: pushes, pulls, takes new library keys, drops the
 * copies of books that left its shelf. The account as stored now.
 */
async function syncAccount(p) {
  await store.push(p);
  await store.pull(p);
  p = await store.takeLibraryKeys(p);
  const shelved = new Set((await store.shelf(p)).map((i) => i.content_key));
  await store.dropCopies(p, (copy) => !shelved.has(copy.content_key)).catch((e) => console.error(e));
  return p;
}

/** Drops the downloaded blobs of publications no account here has on its shelf. */
async function dropUnshelved(all) {
  const kept = new Set();
  for (const a of all) {
    for (const i of await store.shelf(a).catch(() => [])) {
      kept.add(i.publication);
    }
  }
  await store.dropBlobs((id) => !kept.has(id)).catch((e) => console.error(e));
}

/** The notices of account `p`: a new device, a password changed elsewhere. */
async function accountNotices(p) {
  const nodes = [];
  try {
    for (const d of await account.newDevices(p)) {
      nodes.push(notice(`New device added to your account at ${libraryName(p)}: ${d.name || "an unnamed device"} — not you? Remove it in My account.`, {
        error: true,
        action: el("a", { href: `${base}/device` }, "My account"),
        dismiss: () => account.seeDevices(p),
      }));
    }
  } catch (e) {
    if (e instanceof store.Unpaired) {
      throw e;
    }
  }
  const changed = await account.passwordNotice(p);
  if (changed) {
    const how = changed.via === "reset" ? `with a link from ${libraryName(p)}` : changed.via === "library" ? "on the library's computer" : "on another device";
    nodes.push(notice(`Your password at ${libraryName(p)} was changed ${how} — not you? Ask the library's admin to reset it and sign out all devices.`, {
      error: true,
      dismiss: () => account.seePasswordChange(p),
    }));
  }
  return nodes;
}

async function shelfPage() {
  acceptDrops();
  const flash = takeFlash();
  const old = await store.oldStateNotice();
  const all = await store.accounts();
  const showingAll = new URLSearchParams(location.search).has("all") && all.length > 1;
  const head = [];
  if (flash) {
    head.push(notice(flash, { error: true }));
  }
  if (old) {
    head.push(notice("shelfgauge 0.21 uses new accounts: this device forgot the old pairing and its books. Ask your library's admin for an invite.", {
      dismiss: () => store.dismissOldState(),
    }));
  }
  if (!all.length) {
    show(...head, ...signedOutHelp());
    return;
  }
  let p = await store.current();
  head.push(switcher(all, p, showingAll));
  const items = async () => {
    if (!showingAll) {
      return shelfItems(p);
    }
    const lists = await Promise.all(all.map((a) => shelfItems(a)));
    return lists.flat();
  };
  renderShelf(await items(), "Updating…", { head, showLibrary: showingAll });
  let status = "";
  for (const a of showingAll ? all : [p]) {
    try {
      const synced = await syncAccount(a);
      if (!showingAll) {
        p = synced;
        head.push(...await accountNotices(p));
      }
    } catch (e) {
      if (e instanceof store.Unpaired) {
        await dropAccount(a);
        return;
      }
      if (e instanceof store.HubFull) {
        status = "The hub is full: your marks wait on this device.";
        continue;
      }
      if (!unreachable(e)) {
        console.error(e);
      }
      status = "Offline: showing the books as of the last update. Books marked offline open.";
    }
  }
  await dropUnshelved(all);
  renderShelf(await items(), status, { head, showLibrary: showingAll });
}

// ---- Book files ----------------------------------------------------------------

/** The status line of opening a file (kept across shelf redraws). */
let fileStatus = null;

/** "Open file": a button for the file picker (and drag and drop on the page). */
function openFileForm() {
  const input = el("input", { type: "file", accept: bookfile.ACCEPT, hidden: "", class: "hub-file-input" });
  input.addEventListener("change", () => {
    if (input.files && input.files[0]) {
      openBookFile(input.files[0]);
    }
    input.value = "";
  });
  const button = el("button", { type: "button", class: "hub-open-file" }, "Open file");
  button.addEventListener("click", () => input.click());
  const status = el("p", { class: "hub-file-status", role: "status" });
  if (fileStatus) {
    status.textContent = fileStatus.text;
    status.classList.toggle("error", fileStatus.error);
  }
  return el("div", { class: "hub-open" }, button, input,
    el("span", { class: "muted" }, "a book file (.sgbook) downloaded from your library"), status);
}

function setFileStatus(text, error = false) {
  fileStatus = text ? { text, error } : null;
  const status = document.querySelector(".hub-file-status");
  if (status) {
    status.textContent = text;
    status.classList.toggle("error", error);
  }
}

/** Opens a book file: keeps a copy here and opens the book. */
async function openBookFile(file) {
  if (!(await store.accounts()).length) {
    show(...signedOutHelp());
    return;
  }
  setFileStatus(`Opening ${file.name}…`);
  try {
    const { publication } = await bookfile.importFile(file);
    setFileStatus("");
    location.assign(`${base}/r/${publication}`);
  } catch (e) {
    if (!(e instanceof bookfile.BookFileError)) {
      console.error(e);
    }
    setFileStatus(e instanceof bookfile.BookFileError ? e.message : "The book file could not be opened.", true);
  }
}

/** Drag and drop of a book file anywhere on the shelf page. */
function acceptDrops() {
  document.addEventListener("dragover", (event) => {
    if (event.dataTransfer && Array.from(event.dataTransfer.types).includes("Files")) {
      event.preventDefault();
      event.dataTransfer.dropEffect = "copy";
    }
  });
  document.addEventListener("drop", (event) => {
    const file = event.dataTransfer && event.dataTransfer.files[0];
    if (file) {
      event.preventDefault();
      openBookFile(file);
    }
  });
}

// ---- This device ---------------------------------------------------------------

/** `n` bytes for people: "1.4 MB". */
function formatBytes(n) {
  if (!Number.isFinite(n)) {
    return "unknown";
  }
  const units = ["bytes", "KB", "MB", "GB", "TB"];
  let unit = 0;
  while (n >= 1000 && unit < units.length - 1) {
    n /= 1000;
    unit += 1;
  }
  return unit === 0 ? `${n} bytes` : `${n.toFixed(n < 10 ? 1 : 0)} ${units[unit]}`;
}

/** How long ago `ms` was: "just now", "5 minutes ago", or the date. */
function ago(ms) {
  const minutes = Math.floor((Date.now() - ms) / 60000);
  if (minutes < 1) {
    return "just now";
  }
  if (minutes < 60) {
    return minutes === 1 ? "a minute ago" : `${minutes} minutes ago`;
  }
  const hours = Math.floor(minutes / 60);
  if (hours < 24) {
    return hours === 1 ? "an hour ago" : `${hours} hours ago`;
  }
  return new Date(ms).toLocaleDateString();
}

/** `n` of `max` ("3 of 5"; no limit when `max` is 0). */
function ofQuota(n, max, format = String) {
  return max ? `${format(n)} of ${format(max)}` : format(n);
}

/** A `dl` of [term, description] pairs; a description may be a node. */
function facts(pairs) {
  const dl = el("dl", { class: "facts settings-facts" });
  for (const [term, description] of pairs) {
    dl.append(el("dt", {}, term), el("dd", {}, description));
  }
  return dl;
}

/** A link for an operator contact (an e-mail address or an https URL), else text. */
function contactNode(contact) {
  if (/^https:\/\/\S+$/.test(contact)) {
    return el("a", { href: contact, rel: "noopener" }, contact);
  }
  if (/^[\w.+-]+@[\w-]+(\.[\w-]+)+$/.test(contact)) {
    return el("a", { href: `mailto:${contact}` }, contact);
  }
  return contact;
}

/** The hub's address, version, operator and terms (shown paired or not). */
function hubSection(info) {
  const operator = info && typeof info.operator === "object" && info.operator ? info.operator : {};
  const pairs = [
    ["Address", `${location.origin}${base}/`],
    ["Version", info && typeof info.version === "string" ? info.version : "unknown (offline)"],
  ];
  if (typeof operator.name === "string" && operator.name) {
    pairs.push(["Run by", operator.name]);
  }
  if (typeof operator.contact === "string" && operator.contact) {
    pairs.push(["Contact", contactNode(operator.contact)]);
  }
  if (operator.terms === true) {
    pairs.push(["Terms of use", el("a", { href: `${base}/terms` }, "Read the terms")]);
  }
  return el("section", { class: "hub-device-hub" }, el("h2", {}, "Hub"), facts(pairs));
}

/** A password field with its label. */
function passwordField(label, autocomplete = "current-password") {
  const input = el("input", { type: "password", autocomplete, required: "" });
  return { input, node: el("label", { class: "field" }, el("span", {}, label), input) };
}

/** A text field with its label. */
function textField(label, value = "", attrs = {}) {
  const input = el("input", Object.assign({ type: "text", autocomplete: "off", spellcheck: "false" }, attrs));
  input.value = value;
  return { input, node: el("label", { class: "field" }, el("span", {}, label), input) };
}

/**
 * A small form: `fields` (from passwordField/textField), a submit button and a status
 * line; `action()` returns a message for the person, or throws one.
 */
function smallForm(title, fields, button, action, { danger = false } = {}) {
  const status = el("p", { class: "hub-form-status", role: "status" });
  const submit = el("button", { type: "submit", class: danger ? "danger" : "" }, button);
  const form = el("form", { class: "hub-form" }, ...(title ? [el("h3", {}, title)] : []), ...fields.map((f) => f.node), submit, status);
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    submit.disabled = true;
    status.classList.remove("error");
    status.textContent = "Working…";
    // Argon2 blocks this page for a moment on a phone: paint first.
    await new Promise((r) => requestAnimationFrame(() => setTimeout(r, 0)));
    try {
      status.textContent = (await action()) || "Done.";
    } catch (e) {
      if (!(e instanceof account.Refused) && !unreachable(e)) {
        console.error(e);
      }
      status.textContent = unreachable(e) ? "The hub cannot be reached; try again with a connection." : e.message;
      status.classList.add("error");
    }
    submit.disabled = false;
  });
  return form;
}

/** My account for account `p`: the address, devices, name, password, leaving. */
async function accountSection(p) {
  const section = el("section", { class: "hub-account" },
    el("h2", {}, libraryName(p)),
    facts([["Your address", `${p.name}@${p.handle}`], ["This device", store.deviceName()]]));
  const list = el("ul", { class: "device-list hub-devices" }, el("li", { class: "muted" }, "Asking the hub…"));
  section.append(el("h3", {}, "Your devices"), list);
  const owner = p.role === "owner";
  const fill = async () => {
    try {
      const devices = await account.devices(p);
      list.replaceChildren(...devices.map((d) => {
        const li = el("li", {},
          el("strong", {}, d.name || "Unnamed device"),
          d.current ? el("span", { class: "muted" }, " (this device)") : "",
          el("span", { class: "muted" }, ` · added ${new Date(d.created_at * 1000).toLocaleDateString()} · last seen ${new Date(d.last_seen_at * 1000).toLocaleDateString()}`));
        if (!d.current && !owner) {
          const pw = passwordField("Your password");
          li.append(smallForm(null, [pw], "Remove", async () => {
            await account.removeDevice(p, pw.input.value, d.id);
            await fill();
            return "Removed.";
          }));
        }
        return li;
      }));
      await account.seeDevices(p);
    } catch (e) {
      if (e instanceof store.Unpaired) {
        await dropAccount(p);
        return;
      }
      list.replaceChildren(el("li", { class: "muted" }, "Offline: the hub is not reachable."));
    }
  };
  if (owner) {
    section.append(el("p", { class: "muted" }, "This is the library's own account: its devices, name and password are managed on the library's computer (My profile → Phone and reading through the hub)."));
  } else {
    const nameField = textField("New name", p.name, { maxlength: "32" });
    const namePw = passwordField("Your password");
    const oldPw = passwordField("Current password");
    const newPw = passwordField("New password", "new-password");
    const leavePw = passwordField("Your password");
    section.append(
      smallForm("Change your name", [nameField, namePw], "Change name", async () => {
        p = await account.changeName(p, namePw.input.value, nameField.input.value);
        return `Your address is now ${p.name}@${p.handle}.`;
      }),
      smallForm("Change your password", [oldPw, newPw], "Change password", async () => {
        const min = (await account.rules()).min_chars;
        if ([...newPw.input.value].length < min) {
          throw new account.Refused(`Use at least ${min} characters; a few words are easy to remember.`, "bad_request");
        }
        p = await account.changePassword(p, oldPw.input.value, newPw.input.value);
        return "Your password is changed; your other devices stay signed in and are told.";
      }),
      smallForm("Leave the library", [leavePw], "Leave library", async () => {
        if (!confirm(`Leave ${libraryName(p)}? Your account there is deleted with your marks on the hub, and this device forgets the library's books.`)) {
          return "Not left.";
        }
        await account.leave(p, leavePw.input.value);
        location.assign(`${base}/device`);
        return "Left.";
      }, { danger: true }),
    );
  }
  const signOut = el("button", { type: "button" }, "Sign out of this library");
  signOut.addEventListener("click", async () => {
    if (!confirm(`Sign this device out of ${libraryName(p)}? Its books and marks are removed from this device; your account stays. Marks made here are sent first.`)) {
      return;
    }
    try {
      await store.forgetDevice(p);
    } catch (e) {
      const n = await store.waiting(p);
      const lost = n ? ` ${n} ${n === 1 ? "change" : "changes"} made here will be lost.` : "";
      if (!confirm(`The hub is not reachable.${lost} Sign out anyway?`)) {
        return;
      }
      await store.forgetDevice(p, { offline: true });
    }
    location.assign(`${base}/device`);
  });
  section.append(el("div", { class: "hub-device-actions" }, signOut));
  fill();
  return section;
}

/** `/device`: My account per library, this device's storage, the hub. */
async function devicePage() {
  const info = await store.api("info").then((r) => (r.ok ? r.json() : null)).catch(() => null);
  const all = await store.accounts();
  const offlineBooks = el("span");
  const fileBooks = el("span");
  const used = el("span");
  const kept = el("span");
  const refreshLocal = async () => {
    let offline = 0;
    let files = 0;
    for (const a of all) {
      offline += (await store.offline(a).catch(() => new Set())).size;
      files += (await store.copies(a).catch(() => [])).length;
    }
    offlineBooks.textContent = String(offline);
    fileBooks.textContent = String(files);
    const storage = await store.storageInfo();
    used.textContent = storage.usage === null ? "unknown"
      : storage.quota ? `${formatBytes(storage.usage)} of ${formatBytes(storage.quota)} the browser allows` : formatBytes(storage.usage);
    kept.textContent = storage.persisted === null ? "unknown"
      : storage.persisted ? "yes, kept when space runs low"
      : "may be cleared when space runs low; on an iPhone or iPad, add the reader to the home screen";
  };
  const removeDownloads = el("button", { type: "button", class: "hub-device-remove" }, "Remove downloaded books");
  const localStatus = el("p", { class: "muted hub-device-local-status", role: "status" });
  removeDownloads.addEventListener("click", async () => {
    if (!confirm("Remove the books downloaded to this device? They download again when you open them (with a connection). Books opened from book files stay.")) {
      return;
    }
    await store.removeDownloads();
    localStatus.textContent = "Downloaded books removed.";
    await refreshLocal();
  });
  const sections = [];
  for (const a of all) {
    sections.push(await accountSection(a));
  }
  const signOutAll = el("button", { type: "button", class: "danger" }, "Sign out of all");
  signOutAll.addEventListener("click", async () => {
    if (!confirm("Sign this device out of every library? Hand it over only after this: one browser is one person.")) {
      return;
    }
    for (const a of all) {
      await store.forgetDevice(a).catch(() => store.forgetDevice(a, { offline: true }));
    }
    location.assign(`${base}/`);
  });
  show(
    el("h1", {}, "My account"),
    ...(all.length ? sections : signedOutHelp()),
    el("section", { class: "hub-add-library" },
      el("h2", {}, "Another library"),
      el("p", { class: "muted" }, "In another library too? Sign in with that library's address; this device then holds both. One browser is one person: sign out of all before handing it over."),
      el("div", { class: "hub-device-actions" },
        el("a", { class: "button", href: `${base}/signin` }, "Add a library"),
        ...(all.length ? [signOutAll] : []))),
    el("section", { class: "hub-device-local" },
      el("h2", {}, "On this device"),
      facts([
        ["Books readable offline", offlineBooks],
        ["Opened from book files", fileBooks],
        ["Space used", used],
        ["Kept by the browser", kept],
      ]),
      el("div", { class: "hub-device-actions" }, removeDownloads),
      localStatus),
    hubSection(info),
  );
  await refreshLocal();
}

/** Where the browser runs inside a messenger (its storage is not the browser's own). */
function inAppBrowser() {
  const ua = navigator.userAgent;
  return /FBAN|FBAV|Instagram|Line\/|Telegram|WhatsApp|; wv\)|MicroMessenger|VKClient/.test(ua);
}

function inAppNotice() {
  return inAppBrowser()
    ? notice("This page is open inside an app. Open the link in Chrome or Safari instead, so that your books stay in your browser: the menu (⋮ or ⋯) → Open in browser.", { error: true })
    : null;
}

/** `/signin`: an address and a password; at the device limit, a device to remove. */
function signinPage(after = null) {
  const address = textField("Your address", "", { autocomplete: "username", placeholder: "grandma@smiths", inputmode: "email" });
  const pw = passwordField("Password");
  const choice = el("div", { class: "hub-device-choice" });
  let retry = null;
  const form = smallForm(null, [address, pw], "Sign in", async () => {
    let p;
    try {
      if (retry) {
        const picked = choice.querySelector("input[name=remove]:checked");
        if (!picked) {
          throw new account.Refused("Choose a device to remove.", "bad_request");
        }
        p = await retry(Number(picked.value));
      } else {
        p = await account.signIn(address.input.value, pw.input.value);
      }
    } catch (e) {
      if (e instanceof account.Refused && e.code === "device_limit" && e.devices) {
        retry = e.retry;
        choice.replaceChildren(el("p", {}, "Choose a device to remove (an old phone, a browser you no longer use):"),
          ...e.devices.map((d, n) => el("label", { class: "check" },
            el("input", Object.assign({ type: "radio", name: "remove", value: String(d.id) }, n === 0 ? { checked: "" } : {})),
            ` Device added ${new Date(d.created_at * 1000).toLocaleDateString()}, last seen ${new Date(d.last_seen_at * 1000).toLocaleDateString()}`)));
      }
      throw e;
    }
    await syncAccount(p).catch(() => p);
    navigator.storage && navigator.storage.persist && navigator.storage.persist().catch(() => {});
    location.assign(after || `${base}/`);
    return "Signed in.";
  });
  form.insertBefore(choice, form.querySelector("button"));
  show(
    el("h1", {}, "Sign in"),
    ...[inAppNotice()].filter(Boolean),
    el("p", {}, "With the address your library's admin gave you, like grandma@smiths, and your password."),
    form,
    el("p", { class: "muted" }, "Forgot your password? Ask your library's admin for a new link (Reset password)."),
  );
}

/** `/invite#<secret>.<one-time key>`: create an account, or set a new password. */
async function invitePage() {
  const fragment = location.hash.slice(1);
  // The link is a key: out of the address bar and the history at once.
  history.replaceState(null, "", location.pathname);
  if (!fragment) {
    show(el("h1", {}, "Open the whole invite link"), el("p", {}, "The link you got ends in a long code after #. Open it again as it came."));
    return;
  }
  show(el("h1", {}, "Opening your invite…"));
  let preview;
  try {
    preview = await account.previewInvite(fragment);
  } catch (e) {
    show(el("h1", {}, "This invite does not open"), el("p", { class: "error" }, unreachable(e) ? "The hub cannot be reached; open the link again with a connection." : e.message));
    return;
  }
  const library = preview.payload.library_name || `@${preview.handle}`;
  const min = (await account.rules()).min_chars;
  const pw = passwordField(`Password (at least ${min} characters)`, "new-password");
  const repeat = passwordField("Repeat it", "new-password");
  const check = () => {
    if ([...pw.input.value].length < min) {
      throw new account.Refused(`Use at least ${min} characters; a few words are easy to remember.`, "bad_request");
    }
    if (pw.input.value !== repeat.input.value) {
      throw new account.Refused("The two passwords differ.", "bad_request");
    }
  };
  if (preview.kind === "reset") {
    show(
      el("h1", {}, `A new password for ${preview.name}@${preview.handle}`),
      ...[inAppNotice()].filter(Boolean),
      el("p", {}, `${library} sent you this link to set a new password. Your address, books and marks stay.`),
      smallForm(null, [pw, repeat], "Set password", async () => {
        check();
        const p = await account.redeemInvite(preview, { password: pw.input.value });
        await syncAccount(p).catch(() => p);
        location.assign(`${base}/`);
        return "Done.";
      }),
    );
    return;
  }
  const name = textField("Your name in the address", "", { maxlength: "32", placeholder: "grandma" });
  const display = textField("Your name in the library", preview.payload.profile_name, { maxlength: "32" });
  const color = el("select", {});
  ["Teal", "Orange", "Purple", "Green", "Red", "Blue", "Brown", "Pink"].forEach((label, n) => color.append(el("option", { value: String(n) }, label)));
  const colorField = { input: color, node: el("label", { class: "field" }, el("span", {}, "Colour"), color) };
  const address = el("p", { class: "muted" }, `Your address: …@${preview.handle}`);
  name.input.addEventListener("input", () => {
    address.textContent = `Your address: ${name.input.value.trim().toLowerCase() || "…"}@${preview.handle}`;
  });
  show(
    el("h1", {}, `Join ${library}`),
    ...[inAppNotice()].filter(Boolean),
    el("p", {}, `${library} invites you to read its books on this device and on any other you sign in on.`),
    el("ul", {},
      el("li", {}, "Every member of the library can open every book the library sends to the hub."),
      el("li", {}, "Your notes and reading places sync between your devices. The library's admin could technically read your notes in this library and can reset your password; nobody else can."),
      el("li", {}, "The hub stores everything encrypted and never sees your password.")),
    smallForm(null, [name, display, colorField, pw, repeat], "Create account", async () => {
      const normalized = await account.normalizeName(name.input.value);
      if (!normalized) {
        throw new account.Refused("Your name has 3 to 32 Latin letters, digits, dots, dashes or underscores, like grandma or olga.s.", "bad_request");
      }
      check();
      const p = await account.redeemInvite(preview, {
        name: normalized,
        password: pw.input.value,
        displayName: display.input.value.trim() || preview.payload.profile_name,
        color: Number(color.value),
      });
      await syncAccount(p).catch(() => p);
      location.assign(`${base}/`);
      return "Done.";
    }),
    address,
  );
}

// ---- Reading view --------------------------------------------------------------

/** A message in place of the text. */
function readerMessage(title, ...lines) {
  const text = document.querySelector("article.reader .text");
  text.replaceChildren(el("h1", {}, title), ...lines.map((line) => el("p", {}, line)));
  text.append(el("p", {}, el("a", { href: `${base}/` }, "My books")));
}

/** The opened publication: the WASM reader plus what it needs to fetch more. */
class Book {
  constructor(p, id, key, reader, copy = null) {
    this.p = p;
    this.id = id;
    // A book file's copy on this device: blobs from IndexedDB, never from the hub.
    this.copy = copy;
    this.idBytes = c.unhex(id);
    this.key = key;
    this.reader = reader;
    this.summary = JSON.parse(reader.summary());
    this.loaded = new Map();
    this.images = new Map();
  }

  async open(path) {
    const sealed = this.copy ? await store.copyBlob(this.p, this.id, path) : await store.blob(this.p, this.id, path);
    if (!sealed) {
      throw new Error("gone");
    }
    return c.openBlob(this.key, this.idBytes, path, sealed);
  }

  /** Makes part `n` (0-based) renderable. */
  part(n) {
    if (!this.loaded.has(n)) {
      const loading = this.open(`p/${n}`).then((bytes) => this.reader.addPart(n, bytes));
      loading.catch(() => this.loaded.delete(n));
      this.loaded.set(n, loading);
    }
    return this.loaded.get(n);
  }

  /**
   * Loads every part and builds the book's text in WASM (quotes, positions, lists and
   * search need the whole book). Once per page.
   */
  text() {
    if (!this.textReady) {
      this.textReady = (async () => {
        const count = this.summary.parts.length;
        // A few at a time, the open part first (it is in already).
        for (let at = 0; at < count; at += 4) {
          const batch = [];
          for (let n = at; n < Math.min(at + 4, count); n++) {
            batch.push(this.part(n));
          }
          await Promise.all(batch);
        }
        this.reader.loadText();
      })();
      this.textReady.then(() => this.saveOffline()).catch((e) => console.error(e));
      this.textReady.catch(() => {
        this.textReady = null;
      });
    }
    return this.textReady;
  }

  /**
   * Makes the whole book readable offline: every blob once more through the Service
   * Worker (which keeps them; fetches from before it took over this page were not
   * kept), then the shelf can say "offline".
   */
  async saveOffline() {
    if (this.copy || !("serviceWorker" in navigator)) {
      return;
    }
    await navigator.serviceWorker.ready;
    if (!navigator.serviceWorker.controller) {
      await new Promise((resolve) => navigator.serviceWorker.addEventListener("controllerchange", resolve, { once: true }));
    }
    const paths = ["manifest"];
    for (let n = 0; n < this.summary.parts.length; n++) {
      paths.push(`p/${n}`);
    }
    for (let n = 0; n < this.summary.images.length; n++) {
      paths.push(`i/${n}`);
    }
    for (const path of paths) {
      if (!(await store.blob(this.p, this.id, path))) {
        return;
      }
    }
    // A full storage keeps nothing (the worker serves anyway): only then "offline".
    for (const path of paths) {
      if (!(await store.cached(this.id, path))) {
        return;
      }
    }
    await store.markOffline(this.p, this.id);
  }

  /** A `blob:` URL of image `imageId`, or null. */
  image(imageId) {
    if (!this.images.has(imageId)) {
      const index = this.summary.images.findIndex((i) => i.id === imageId);
      this.images.set(imageId, index < 0 ? Promise.resolve(null) : this.open(`i/${index}`)
        .then((bytes) => URL.createObjectURL(new Blob([bytes], { type: this.summary.images[index].mime })))
        .catch((e) => {
          console.error(e);
          return null;
        }));
    }
    return this.images.get(imageId);
  }

  /** Fills in the images of `root` (rendered with ids instead of URLs). */
  fillImages(root) {
    for (const img of root.querySelectorAll("img[data-image]")) {
      const imageId = img.dataset.image;
      img.removeAttribute("data-image");
      this.image(imageId).then((url) => {
        if (url) {
          img.src = url;
        }
      });
    }
  }
}

/** The part a progress share falls in (`?progress=`), as the track asks for it. */
function partAtShare(parts, share) {
  let found = 0;
  parts.forEach((part, i) => {
    if (part.progress && part.progress.start <= share) {
      found = i;
    }
  });
  return found;
}

/** The main-text block at `share` within the rendered part (reader.js's rule). */
function blockAtShare(text, progress, share) {
  let total = 0;
  const leaves = [];
  const block = /^b[1-9][0-9]*$/;
  for (const leaf of text.querySelectorAll("[id]")) {
    const isLeaf = block.test(leaf.id) && !Array.from(leaf.querySelectorAll("[id]")).some((e) => block.test(e.id));
    if (!isLeaf || leaf.closest(".notes")) {
      continue;
    }
    leaves.push([leaf, total]);
    total += leaf.textContent.length;
  }
  const target = progress.end > progress.start ? ((share - progress.start) / (progress.end - progress.start)) * total : 0;
  let found = leaves.length ? leaves[0][0] : null;
  for (const [leaf, chars] of leaves) {
    if (chars > target) {
      break;
    }
    found = leaf;
  }
  return found;
}

function partNav(summary, index) {
  const count = summary.parts.length;
  const link = (i, rel, label) => el("a", { href: `?part=${i + 1}`, rel, title: `${summary.parts[i].title} (${rel === "prev" ? "←" : "→"})` }, label);
  return [
    index > 0 ? link(index - 1, "prev", `‹ ${summary.parts[index - 1].title}`) : el("span"),
    el("span", { class: "muted", lang: "en" }, `Part ${index + 1} of ${count}`),
    index + 1 < count ? link(index + 1, "next", `${summary.parts[index + 1].title} ›`) : el("span"),
  ];
}

/** Fills the reading view with part `index` of `book`. */
function fillPage(book, index, html) {
  const s = book.summary;
  const part = s.parts[index];
  const root = document.documentElement;
  if (s.lang) {
    root.lang = s.lang;
  }
  if (root.lang !== "en") {
    document.querySelector(".topbar").lang = "en";
  }
  const partTitle = part ? part.title : "";
  document.title = `${s.parts.length > 1 ? `${s.title} — ${partTitle}` : s.title} · shelfgauge`;
  const article = document.querySelector("article.reader");
  const head = article.querySelector(".reader-head");
  head.querySelector(".book-link").textContent = s.title;
  head.querySelector(".part-title").textContent = partTitle ? ` · ${partTitle}` : "";
  document.querySelector(".toc-book .book-title").textContent = s.title;
  for (const nav of article.querySelectorAll(".reader-nav")) {
    nav.replaceChildren(...partNav(s, index));
  }
  const text = article.querySelector(".text");
  // Escaped HTML from the instance's renderer (WASM), never markup from the hub.
  text.innerHTML = html;
  book.fillImages(text);

  const toc = book.reader.renderToc(index);
  const tocTab = document.querySelector('.reader-tabs a[data-tab="toc"]');
  const tocPanel = document.getElementById("tab-toc");
  if (toc) {
    tocPanel.innerHTML = toc;
    tocTab.hidden = false;
  } else {
    tocTab.remove();
    tocPanel.remove();
  }

  const authors = s.authors.join(", ");
  document.querySelector(".running-title").textContent =
    (part && part.head) || `${authors ? `${authors} · ` : ""}${s.title}`;
  const progress = part && part.progress;
  if (progress) {
    article.dataset.progressStart = progress.start;
    article.dataset.progressEnd = progress.end;
  }
  document.querySelector(".status-progress").textContent =
    progress ? `${Math.floor((progress.start * 100) / SHARE_SCALE)} %` : "100 %";
  const track = document.querySelector(".status-line .track");
  track.dataset.href = location.pathname;
  track.replaceChildren(...s.chapters.map((chapter) => el("i", { class: "tick chapter", "data-at": chapter.at, "data-title": chapter.title })));
}

/** Unix seconds, as the instance's records use. */
function now() {
  return Math.floor(Date.now() / 1000);
}

/** Code points (the limits count them, as the instance does). */
function cpLength(text) {
  let n = 0;
  for (const _ of text) {
    n += 1;
  }
  return n;
}

/** The ordinal of a block id ("b12" → 12). */
function ordinal(block) {
  return Number(block.slice(1));
}

/**
 * Sync of this page: pushes changes made here (soon after a change; a position 30 s
 * after the last one, as on the instance), pulls on opening, when the device comes
 * online and every few minutes while the page is visible; `onChange` runs when the
 * pull changed records.
 */
class Sync {
  constructor(p, onChange) {
    this.p = p;
    this.onChange = onChange;
    this.timer = null;
    this.running = null;
    addEventListener("online", () => this.now());
    setInterval(() => {
      if (document.visibilityState === "visible") {
        this.now();
      }
    }, 5 * 60 * 1000);
    // Changes waiting for their push (a mark made just before) go out now: a hidden
    // page may be frozen before a timer fires.
    document.addEventListener("visibilitychange", () => {
      if (document.visibilityState === "hidden") {
        this.push({ keepalive: true });
      }
    });
  }

  /** A push in `delay` ms (a later call moves it). */
  later(delay) {
    clearTimeout(this.timer);
    this.timer = setTimeout(() => this.push(), delay);
  }

  async push(options) {
    clearTimeout(this.timer);
    try {
      await store.push(this.p, options);
    } catch (e) {
      this.failed(e);
    }
  }

  /** Push, then pull; one at a time. */
  now() {
    if (!this.running) {
      this.running = (async () => {
        try {
          await store.push(this.p);
          if ((await store.pull(this.p)) > 0) {
            this.onChange();
          }
        } catch (e) {
          this.failed(e);
        } finally {
          this.running = null;
        }
      })();
    }
    return this.running;
  }

  failed(e) {
    if (e instanceof store.Unpaired) {
      dropAccount(this.p);
    } else if (!unreachable(e) && !(e instanceof store.HubFull)) {
      console.error(e);
    }
  }
}

/**
 * The reader's data on this device (reader.js's `window.shelfgaugeSource`): pages of
 * other parts, lists, search results and new annotations come from the WASM module
 * and the records in IndexedDB.
 */
function deviceSource(book, item, p, sync) {
  const contentKey = book.summary.content_key;
  const article = document.querySelector("article.reader");
  const maxText = Number(article.dataset.maxText) || Infinity;
  const maxNote = Number(article.dataset.maxNote) || Infinity;
  const annotations = async () => (await store.records(p, "annotation"))
    .map((r) => r.value)
    .filter((a) => a.id && a.content_key === contentKey && a.deleted_at === null);
  const snapshot = { book_title: item.title || "", book_authors: item.authors || "" };
  return {
    async page(url) {
      const part = Number(new URL(url, location.href).searchParams.get("part")) || 1;
      const n = Math.min(Math.max(part, 1), book.summary.parts.length) - 1;
      await book.part(n);
      const doc = document.implementation.createHTMLDocument("");
      const text = doc.createElement("div");
      text.className = "text";
      text.innerHTML = book.reader.renderPart(n);
      doc.body.append(text);
      return doc;
    },
    async lists() {
      await book.text();
      return book.reader.lists(JSON.stringify(await annotations()));
    },
    async create(body) {
      await book.text();
      let anchor;
      let quoted;
      if (body.block) {
        quoted = JSON.parse(book.reader.bookmark(body.block));
        anchor = { start_block: ordinal(body.block), start_offset: 0, end_block: ordinal(body.block), end_offset: 0 };
      } else {
        quoted = JSON.parse(book.reader.quote(JSON.stringify({
          start_block: body.start_block,
          start_offset: body.start_offset,
          end_block: body.end_block,
          end_offset: body.end_offset,
        })));
        const length = cpLength(quoted.text);
        if (length > maxText) {
          throw new Error(`the selection is ${length} characters long; the limit is ${maxText}`);
        }
        anchor = {
          start_block: ordinal(body.start_block),
          start_offset: body.start_offset,
          end_block: ordinal(body.end_block),
          end_offset: body.end_offset,
        };
      }
      const note = typeof body.note === "string" && body.note.trim() ? body.note : null;
      if (note && cpLength(note) > maxNote) {
        throw new Error(`the note is too long; the limit is ${maxNote} characters`);
      }
      const t = now();
      const id = crypto.randomUUID();
      await store.write(p, "annotation", id, Object.assign({
        id,
        content_key: contentKey,
        text: quoted.text,
        prefix: quoted.prefix,
        suffix: quoted.suffix,
        section_title: quoted.section_title,
        note,
        created_at: t,
        updated_at: t,
        deleted_at: null,
      }, anchor, snapshot));
      sync.later(1000);
      return { id };
    },
    async remove(form) {
      const id = new URL(form.action, location.href).searchParams.get("delete");
      const record = id && await store.get(p, "annotation", id);
      if (!record || record.content_key !== contentKey) {
        throw new Error("not found");
      }
      const t = now();
      record.deleted_at = t;
      record.updated_at = t;
      await store.write(p, "annotation", id, record);
      sync.later(1000);
    },
    async savePosition(body, keepalive) {
      await book.text();
      const position = JSON.parse(book.reader.position(body.block));
      if (!position) {
        return; // In the notes: not a reading position.
      }
      const id = await c.positionId(p.mac, c.unbase64url(contentKey));
      await store.write(p, "position", id, Object.assign({
        content_key: contentKey,
        block: ordinal(body.block),
        offset: 0,
        suffix: position.suffix,
        progress: position.progress,
        updated_at: now(),
      }, snapshot));
      if (keepalive) {
        // Saved as the page is hidden or left (reader.js): pushed at once, as the
        // reader may go on reading on another device.
        await sync.push({ keepalive: true });
      } else {
        sync.later(30000);
      }
    },
    async find(raw) {
      await book.text();
      return book.reader.find(raw);
    },
  };
}

/** Shows the results of `?find=` before reader.js starts (as the instance's page does). */
async function showFind(book, raw, hit) {
  await book.text();
  const panel = document.getElementById("tab-find");
  const template = document.createElement("template");
  // Escaped HTML from the instance's template (WASM).
  template.innerHTML = book.reader.find(raw, Number.isInteger(hit) && hit > 0 ? hit : undefined).trim();
  const results = template.content.firstElementChild;
  panel.querySelector(".find-results").replaceWith(results);
  panel.hidden = false;
  const tab = document.querySelector('.reader-tabs a[data-tab="find"]');
  tab.hidden = false;
  tab.querySelector(".tab-count").textContent = results.dataset.tabCount || "";
  tab.title = results.dataset.tabTitle || "Search results";
  tab.setAttribute("aria-label", tab.title);
  document.querySelector(".find-input").value = raw;
}

function startReader(book, item, p) {
  const sync = new Sync(p, () => document.dispatchEvent(new Event("shelfgauge:lists")));
  window.shelfgaugeSource = deviceSource(book, item, p, sync);
  // Images in note panels and anything else added later.
  new MutationObserver((changes) => {
    for (const change of changes) {
      for (const node of change.addedNodes) {
        if (node.nodeType === Node.ELEMENT_NODE) {
          book.fillImages(node.matches("img[data-image]") ? node.parentNode : node);
        }
      }
    }
  }).observe(document.body, { childList: true, subtree: true });
  const script = document.createElement("script");
  script.src = `${base}/assets/reader.js`;
  script.addEventListener("load", () => {
    // The lists need the whole book: they appear once it is in.
    book.text().then(() => document.dispatchEvent(new Event("shelfgauge:lists")), (e) => console.error(e));
    sync.now();
  });
  document.head.append(script);
}

/** The account here whose book file copy, shelf item or library has publication `id`. */
async function findAccount(id) {
  const all = await store.accounts();
  for (const p of all) {
    const copy = await store.copy(p, id).catch(() => null);
    if (copy) {
      return { p, copy, sealed: await store.copyBlob(p, id, "manifest") };
    }
  }
  for (const p of all) {
    if ((await store.shelf(p).catch(() => [])).some((i) => i.publication === id)) {
      return { p, copy: null, sealed: await store.blob(p, id, "manifest") };
    }
  }
  // A link of a book not on the shelf yet: the hub answers only the library's members.
  for (const p of all) {
    try {
      const sealed = await store.blob(p, id, "manifest");
      if (sealed) {
        return { p, copy: null, sealed };
      }
    } catch (e) {
      if (e instanceof store.Unpaired) {
        await store.forget(p);
        continue;
      }
      throw e;
    }
  }
  return null;
}

/** The book key of a manifest of account `p` from its header, syncing once for a newer key. */
async function bookKey(p, id, sealed) {
  let header;
  try {
    header = c.keyHeader(sealed);
  } catch (e) {
    if (/older shelfgauge/.test(e.message)) {
      return { message: ["This book was made by an older shelfgauge", "Ask your library to send it again."] };
    }
    throw e;
  }
  if (header.key_scope !== c.LIBRARY_SCOPE) {
    return { message: ["This book needs a newer shelfgauge", "Your hub's reader app is older than the library that sent this book."] };
  }
  let key = store.libraryKey(p, header.key_id);
  if (!key) {
    try {
      p = await syncAccount(p);
    } catch (e) {
      if (e instanceof store.Unpaired) {
        throw e;
      }
    }
    key = store.libraryKey(p, header.key_id);
  }
  if (!key) {
    return { message: ["This book needs a newer library key", "Connect to the internet to update, then open it again."] };
  }
  return { p, key: await c.bookKeyFromHeader(header, key, id) };
}

async function readerPage(id) {
  // `#pair=<secret>.<key>`: the owner's QR code that also pairs a new phone.
  const pairing = location.hash.startsWith("#pair=") ? location.hash.slice(6) : null;
  if (pairing) {
    history.replaceState(null, "", location.pathname + location.search);
  }
  try {
    let found = await findAccount(id);
    if (!found && pairing) {
      readerMessage("Pairing this device…");
      try {
        await store.pair(pairing);
      } catch (e) {
        readerMessage("This device was not paired", e.message);
        return;
      }
      const p = await store.current();
      await syncAccount(p).catch(() => p);
      found = await findAccount(id);
    }
    if (!found) {
      if (!(await store.accounts()).length) {
        readerMessage(...signedOutHelp().map((node) => node.textContent));
      } else {
        // The hub answers alike for a deleted book and another library's (it tells
        // nothing about publications a device may not read).
        readerMessage("This book does not open here",
          "Its library may have deleted it, or it is from a library this device is not signed in to. In that case sign in with the address your library's admin gave you: My account → Add a library.");
      }
      return;
    }
    if (!found.sealed) {
      const fileItem = (await store.shelf(found.p)).find((i) => i.publication === id && i.kind === "file");
      if (fileItem) {
        readerMessage(`Open the book file of “${fileItem.title}”`, "This book is on your shelf as a book file: the hub has no copy. Open the file on this device with Open file on My books.");
        return;
      }
      readerMessage("This book is no longer on the hub", "It was not opened for a long time, or the library deleted it. Ask the library to send it again.");
      return;
    }
    const opened = await bookKey(found.p, id, found.sealed);
    if (opened.message) {
      readerMessage(...opened.message);
      return;
    }
    const p = opened.p;
    const key = opened.key;
    const sealed = found.sealed;
    const copy = found.copy;
    let item = (await store.shelf(p)).find((i) => (copy ? i.content_key === copy.content_key : i.publication === id));
    if (!copy && item) {
      // A book file's copy of the same book on this device opens without the hub.
      const other = (await store.copies(p)).find((x) => x.publication !== id && x.content_key === item.content_key);
      if (other) {
        location.replace(`${base}/r/${other.publication}${location.search}${location.hash}`);
        return;
      }
    }
    const wasm = await import("./pkg/shelfgauge.js");
    await wasm.default();
    const reader = new wasm.Reader(await c.openBlob(key, c.unhex(id), "manifest", sealed));
    const book = new Book(p, id, key, reader, copy);
    if (!item) {
      // A member opening a link: the book goes on their shelf (the library's
      // publication; the book key stays in its header).
      item = {
        authors: (book.summary.authors || []).join(", "),
        book_key: null,
        content_key: book.summary.content_key,
        format_version: c.FORMAT_VERSION,
        instance: p.library_name || p.handle,
        kind: copy ? "file" : "upload",
        publication: id,
        title: book.summary.title || "",
        updated_at: Math.floor(Date.now() / 1000),
      };
      const contentKey = c.unbase64url(item.content_key);
      await store.write(p, "shelf_item", await c.shelfId(p.mac, contentKey), item);
      store.push(p).catch(() => {});
    }
    if (book.summary.content_key !== item.content_key) {
      // Marks would be filed under another book.
      readerMessage("This book could not be opened", "Its shelf entry does not match the published book. Publish it again from your library.");
      return;
    }
    const params = new URLSearchParams(location.search);
    const count = book.summary.parts.length;
    const share = params.has("progress") ? Number(params.get("progress")) : NaN;
    let index;
    let resumeAt = null;
    if (Number.isFinite(share) && share >= 0 && share <= SHARE_SCALE) {
      index = partAtShare(book.summary.parts, share);
    } else if (!params.has("part") && !params.has("find") && !location.hash) {
      // Opened from the shelf: where the reader left off, here or elsewhere (a pull
      // first, unless the hub is slow to answer).
      await Promise.race([
        store.pull(p).catch((e) => {
          if (e instanceof store.Unpaired) {
            throw e;
          }
        }),
        new Promise((resolve) => setTimeout(resolve, 3000)),
      ]);
      const position = await store.get(p, "position", await c.positionId(p.mac, c.unbase64url(item.content_key)));
      const part = position && Number.isInteger(position.block) ? reader.partOf(`b${position.block}`) : undefined;
      index = part === undefined ? 0 : part;
      resumeAt = part === undefined ? null : `b${position.block}`;
    } else {
      index = Math.min(Math.max(Number(params.get("part")) || 1, 1), Math.max(count, 1)) - 1;
    }
    let html = "<p class=\"empty\">This book has no text.</p>";
    if (count) {
      await book.part(index);
      html = book.reader.renderPart(index);
    }
    fillPage(book, index, html);
    document.querySelector(".find-form").dataset.part = index + 1;
    if (Number.isFinite(share) && count) {
      // The track's jump into another part: the block at that share, as a fragment.
      const block = blockAtShare(document.querySelector("article.reader .text"), book.summary.parts[index].progress || { start: 0, end: 0 }, share);
      history.replaceState(null, "", `?part=${index + 1}${block ? `#${block.id}` : ""}`);
    } else if (resumeAt) {
      history.replaceState(null, "", `?part=${index + 1}#${resumeAt}`);
    }
    const raw = params.get("find");
    if (raw && raw.trim() && count) {
      await showFind(book, raw, Number(params.get("hit")));
    }
    startReader(book, item, p);
    if (location.hash) {
      const target = document.getElementById(decodeURIComponent(location.hash.slice(1)));
      if (target) {
        target.scrollIntoView();
      }
    }
  } catch (e) {
    if (e instanceof store.Unpaired) {
      const p = await store.current();
      if (p) {
        await dropAccount(p);
      }
      return;
    }
    console.error(e);
    readerMessage("The book could not be opened", unreachable(e) ? "The hub cannot be reached, and this book is not saved on this device yet." : String(e.message || e));
  }
}

/**
 * `/open`: a book file handed to the installed app — by the system (`file_handlers`,
 * through `launchQueue`) or shared to it (`share_target`: the Service Worker kept it,
 * `?shared=1`). Shows the shelf and opens the file. Also `/?shared=lost` (a share
 * that no Service Worker took).
 */
function openPage() {
  const shared = new URLSearchParams(location.search).get("shared");
  history.replaceState(null, "", `${base}/`);
  shelfPage();
  if ("launchQueue" in window) {
    window.launchQueue.setConsumer(async (launch) => {
      if (launch.files && launch.files.length) {
        openBookFile(await launch.files[0].getFile());
      }
    });
  }
  if (shared === "1") {
    takeShared().catch((e) => {
      console.error(e);
      setFileStatus("The shared file could not be opened. Use Open file instead.", true);
    });
  } else if (shared) {
    setFileStatus("The shared file did not arrive. Open shelfgauge once while connected, then share the file again (or use Open file).", true);
  }
}

/** Opens the book file the Service Worker kept from a share, and forgets it. */
async function takeShared() {
  const cache = await caches.open("shelfgauge-shared");
  const key = `${base}/shared-file`;
  const response = await cache.match(key);
  await cache.delete(key);
  if (!response) {
    return;
  }
  const name = decodeURIComponent(response.headers.get("X-File-Name") || "book.sgbook");
  openBookFile(new File([await response.blob()], name));
}

// The state of 0.18–0.20 goes before anything else reads the accounts.
await store.dropOldState().catch((e) => console.error(e));
if (route === "/pair") {
  pairPage();
} else if (route === "/signin") {
  signinPage();
} else if (route === "/invite") {
  invitePage();
} else if (route.startsWith("/r/")) {
  readerPage(route.slice(3));
} else if (route === "/device") {
  devicePage();
} else if (route === "/open" || new URLSearchParams(location.search).has("shared")) {
  openPage();
} else {
  shelfPage();
}
