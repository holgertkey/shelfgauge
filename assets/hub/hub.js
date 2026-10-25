// The hub's reader app: pairing this browser (`/pair#<secret>.<account key>`), the
// shelf (`/`, with "Open file" for book files) and the reading view
// (`/r/<publication id>?part=<n>`), of a hub publication or of a book file's copy
// kept on this device (bookfile.js).
//
// Books arrive as encrypted blobs; crypto.js opens them, the WASM module
// (pkg/shelfgauge.js, the instance's own renderer) turns them into escaped HTML, and
// the page is then the instance's reading page: reader.js runs on it unchanged, with
// `window.shelfgaugeSource` giving it the pages of other parts. Text from records or
// the manifest goes into the page as text only (`textContent`), never as markup.
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

/** Where the reader learns how to pair this browser. */
function unpairedHelp() {
  return [
    el("h1", {}, "This device is not paired"),
    el("p", {}, "Books on this hub open only on devices paired with their library's account."),
    el("p", {}, "On the computer with your library, open My profile → Phone and other devices, choose Pair a device, and scan the QR code with this device (or open its link here)."),
    el("p", { class: "muted" }, "Paired before? The browser may have cleared this site's data (Safari does after a week without a visit, unless the app is on the home screen). Pair again; your marks are kept in your library."),
  ];
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
    const p = await store.pairing();
    show(...(p ? [el("h1", {}, "This device is paired"), el("p", {}, el("a", { href: `${base}/` }, "My books"))] : unpairedHelp()));
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

function renderShelf(items, status) {
  const reading = items
    .filter((i) => i.position && i.position.progress < 1000)
    .sort((a, b) => b.position.updated_at - a.position.updated_at);
  const rest = items
    .filter((i) => !reading.includes(i))
    .sort((a, b) => (a.title || "").localeCompare(b.title || ""));
  const nodes = [el("h1", {}, "My books")];
  if (status) {
    nodes.push(el("p", { class: "muted hub-sync", role: "status" }, status));
  }
  nodes.push(openFileForm());
  if (!items.length) {
    nodes.push(el("p", { class: "hint" }, "No books yet. In your library, choose Open on phone on a book's page to put it here, or Download book file and open the file here."));
  }
  if (reading.length) {
    nodes.push(el("h2", {}, "Currently reading"), el("ul", { class: "hub-books" }, ...reading.map(bookItem)));
  }
  if (rest.length) {
    if (reading.length) {
      nodes.push(el("h2", {}, "On the shelf"));
    }
    nodes.push(el("ul", { class: "hub-books" }, ...rest.map(bookItem)));
  }
  show(...nodes);
}

/** The shelf with each book's offline state and its book file copy here, if any. */
async function shelfItems(p) {
  const items = await store.shelf(p);
  const offline = await store.offline().catch(() => new Set());
  const copies = await store.copies().catch(() => []);
  for (const item of items) {
    item.offline = offline.has(item.publication);
    // The newest copy of the book.
    item.copy = copies
      .filter((copy) => copy.content_key === item.content_key)
      .sort((a, b) => b.imported_at - a.imported_at)[0] || null;
  }
  return items;
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
  const p = await store.pairing();
  if (!p) {
    show(...unpairedHelp());
    return;
  }
  setFileStatus(`Opening ${file.name}…`);
  try {
    const { publication } = await bookfile.importFile(p, file);
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

async function shelfPage() {
  acceptDrops();
  const p = await store.pairing();
  if (!p) {
    show(...unpairedHelp());
    return;
  }
  renderShelf(await shelfItems(p), "Updating…");
  try {
    // Marks made offline go out from here too.
    await store.push(p);
    await store.pull(p);
    // Copies of books that left the shelf (or were republished) go; so do book
    // file copies of books no longer on the shelf.
    const items = await store.shelf(p);
    const kept = new Set(items.map((i) => i.publication));
    await store.dropBlobs((id) => !kept.has(id)).catch((e) => console.error(e));
    const shelved = new Set(items.map((i) => i.content_key));
    await store.dropCopies((copy) => !shelved.has(copy.content_key)).catch((e) => console.error(e));
    renderShelf(await shelfItems(p), "");
  } catch (e) {
    if (e instanceof store.Unpaired) {
      await store.forget();
      show(...unpairedHelp());
      return;
    }
    if (!unreachable(e)) {
      console.error(e);
    }
    renderShelf(await shelfItems(p), "Offline: showing the books as of the last update. Books marked offline open.");
  }
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

/** `/device`: what this device is paired with, its sync, what it keeps, unpairing. */
async function devicePage() {
  const info = await store.api("info").then((r) => (r.ok ? r.json() : null)).catch(() => null);
  const p = await store.pairing();
  if (!p) {
    show(...unpairedHelp(), hubSection(info));
    return;
  }
  const libraries = [...new Set((await store.shelf(p)).map((i) => i.instance).filter(Boolean))];
  const name = el("span", {}, store.deviceName());
  const since = el("span", {}, "unknown (offline)");
  const synced = el("span");
  const waiting = el("span");
  const syncStatus = el("p", { class: "muted hub-device-status", role: "status" });
  const offlineBooks = el("span");
  const fileBooks = el("span");
  const used = el("span");
  const kept = el("span");
  const account = el("div", {}, el("p", { class: "muted" }, "Asking the hub…"));

  const refreshSync = async () => {
    const at = await store.syncedAt();
    synced.textContent = at ? ago(at) : "not yet";
    const n = await store.waiting();
    waiting.textContent = n ? `${n} ${n === 1 ? "change" : "changes"} made here, not sent yet` : "none";
  };
  const refreshLocal = async () => {
    offlineBooks.textContent = String((await store.offline().catch(() => new Set())).size);
    fileBooks.textContent = String((await store.copies().catch(() => [])).length);
    const storage = await store.storageInfo();
    used.textContent = storage.usage === null ? "unknown"
      : storage.quota ? `${formatBytes(storage.usage)} of ${formatBytes(storage.quota)} the browser allows` : formatBytes(storage.usage);
    kept.textContent = storage.persisted === null ? "unknown"
      : storage.persisted ? "yes, kept when space runs low"
      : "may be cleared when space runs low; on an iPhone or iPad, add the reader to the home screen";
  };

  const update = el("button", { type: "button", class: "hub-device-update" }, "Update now");
  update.addEventListener("click", async () => {
    update.disabled = true;
    syncStatus.textContent = "Updating…";
    try {
      await store.push(p);
      await store.pull(p);
      syncStatus.textContent = "Up to date.";
    } catch (e) {
      if (e instanceof store.Unpaired) {
        await store.forget();
        location.reload();
        return;
      }
      if (!unreachable(e)) {
        console.error(e);
      }
      syncStatus.textContent = "Offline: the hub is not reachable. Changes made here are sent when it is.";
    }
    await refreshSync();
    update.disabled = false;
  });

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

  const forgetButton = el("button", { type: "button", class: "danger hub-device-forget" }, "Forget this device");
  const forgetStatus = el("p", { class: "hub-device-forget-status", role: "status" });
  forgetButton.addEventListener("click", async () => {
    if (!confirm("Forget this device? Your books, marks and keys are removed from it, and the hub stops accepting it. Pair it again in your library to read here.")) {
      return;
    }
    forgetButton.disabled = true;
    forgetStatus.textContent = "Forgetting…";
    try {
      await store.forgetDevice(p);
    } catch (e) {
      if (!unreachable(e)) {
        console.error(e);
        forgetStatus.textContent = e.message;
        forgetStatus.classList.add("error");
        forgetButton.disabled = false;
        return;
      }
      const n = await store.waiting();
      const lost = n ? ` ${n} ${n === 1 ? "change" : "changes"} made on this device (marks, the reading position) have not reached your library and will be lost.` : "";
      if (!confirm(`The hub is not reachable.${lost} Forget this device anyway? Your library will list it until you remove it there (My profile → Phone and other devices).`)) {
        forgetStatus.textContent = "";
        forgetButton.disabled = false;
        return;
      }
      await store.forgetDevice(p, { offline: true });
    }
    show(el("h1", {}, "This device was forgotten"),
      el("p", {}, "Its books, marks and keys are gone from it. To read here again, pair it in your library: My profile → Phone and other devices → Pair a device."));
  });

  show(
    el("h1", {}, "This device"),
    el("section", { class: "hub-device-pairing" },
      el("p", { class: "hub-device-title" },
        document.querySelector(".device-link svg").cloneNode(true),
        libraries.length
          ? `Paired with your library ${libraries.map((l) => `“${l}”`).join(", ")}.`
          : "Paired with your library's account."),
      facts([["This device", name], ["Paired since", since]])),
    el("section", { class: "hub-device-sync" },
      el("h2", {}, "Sync"),
      facts([["Last updated", synced], ["Waiting", waiting]]),
      el("div", { class: "hub-device-actions" }, update),
      syncStatus),
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
    el("section", { class: "hub-device-account" },
      el("h2", {}, "Your account on the hub"),
      account,
      el("p", { class: "muted" }, "Your devices are managed in your library: My profile → Phone and other devices.")),
    hubSection(info),
    el("section", { class: "hub-device-danger" },
      el("h2", {}, "Forget this device"),
      el("p", { class: "muted" }, "Removes your books, marks and keys from this device and unpairs it. Marks made here are sent to your library first."),
      el("div", { class: "hub-device-actions" }, forgetButton),
      forgetStatus),
  );
  await Promise.all([refreshSync(), refreshLocal()]);

  try {
    const [mine, list] = await Promise.all([
      store.api("account", {}, p.token).then((r) => (r.ok ? r.json() : Promise.reject(new Error(`account: ${r.status}`)))),
      store.api("devices", {}, p.token).then((r) => (r.ok ? r.json() : Promise.reject(new Error(`devices: ${r.status}`)))),
    ]);
    const current = (list.devices || []).find((d) => d.current);
    if (current) {
      if (typeof current.name === "string") {
        name.textContent = await c.openDeviceName(p.aes, c.unbase64url(current.name)).catch(() => store.deviceName());
      }
      if (Number.isFinite(current.created_at)) {
        since.textContent = new Date(current.created_at * 1000).toLocaleDateString();
      }
    }
    const usage = mine.usage || {};
    const quotas = mine.quotas || {};
    account.replaceChildren(facts([
      ["Space used", ofQuota(usage.bytes, quotas.max_bytes, formatBytes)],
      ["Books on the hub", ofQuota(usage.publications, quotas.max_publications)],
      ["Devices, your library included", ofQuota(usage.devices, quotas.max_devices)],
    ]));
  } catch (e) {
    if (e instanceof store.Unpaired) {
      await store.forget();
      show(...unpairedHelp(), hubSection(info));
      return;
    }
    if (!unreachable(e)) {
      console.error(e);
    }
    account.replaceChildren(el("p", { class: "muted" }, "Offline: the hub is not reachable."));
  }
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
    const sealed = this.copy ? await store.copyBlob(this.id, path) : await store.blob(this.p, this.id, path);
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
    await store.markOffline(this.id);
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
      store.forget().then(() => location.reload());
    } else if (!unreachable(e)) {
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

async function readerPage(id) {
  const p = await store.pairing();
  if (!p) {
    readerMessage(...unpairedHelp().map((node) => node.textContent));
    return;
  }
  try {
    // A book file's copy on this device first: it needs no hub.
    const copy = await store.copy(id);
    let item;
    let key;
    let sealed;
    if (copy) {
      item = (await store.shelf(p)).find((i) => i.content_key === copy.content_key)
        || { title: copy.title, authors: copy.authors, content_key: copy.content_key, publication: id };
      const raw = await c.openRecord(p.aes, "file_key", id, c.unbase64url(copy.book_key));
      key = await c.aesKey(raw);
      raw.fill(0);
      sealed = await store.copyBlob(id, "manifest");
    } else {
      item = (await store.shelf(p)).find((i) => i.publication === id);
      if (!item) {
        await store.pull(p).catch((e) => {
          if (e instanceof store.Unpaired) {
            throw e;
          }
        });
        item = (await store.shelf(p)).find((i) => i.publication === id);
      }
      if (!item) {
        readerMessage("This book is not on your shelf", "It may have been removed from the shelf in your library.");
        return;
      }
      const other = (await store.copies()).find((x) => x.content_key === item.content_key);
      if (other) {
        location.replace(`${base}/r/${other.publication}${location.search}${location.hash}`);
        return;
      }
      if (item.kind === "file") {
        readerMessage(`Open the book file of “${item.title}”`, "This book is on your shelf as a book file: the hub has no copy. Open the file on this device with Open file on My books (download it from the book's page in your library if it is not here).");
        return;
      }
      key = await c.aesKey(c.unbase64url(item.book_key));
      sealed = await store.blob(p, id, "manifest");
      if (!sealed) {
        readerMessage(`“${item.title}” is no longer on the hub`, "It was not opened for a long time, or it was removed. Choose Open on phone on the book's page in your library to publish it again.");
        return;
      }
    }
    const wasm = await import("./pkg/shelfgauge.js");
    await wasm.default();
    const reader = new wasm.Reader(await c.openBlob(key, c.unhex(id), "manifest", sealed));
    const book = new Book(p, id, key, reader, copy);
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
      await store.forget();
      readerMessage(...unpairedHelp().map((node) => node.textContent));
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

if (route === "/pair") {
  pairPage();
} else if (route.startsWith("/r/")) {
  readerPage(route.slice(3));
} else if (route === "/device") {
  devicePage();
} else if (route === "/open" || new URLSearchParams(location.search).has("shared")) {
  openPage();
} else {
  shelfPage();
}
