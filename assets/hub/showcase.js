// The library's Showcase in the hub reader (`/showcase`, Phase 31): the catalog of the
// books in the folders the library's admin opened, kept on this device and searched
// offline, with "Get this book" for any of them.
//
// The catalog comes from the hub sealed (crypto.js; the Rust side is
// crates/shelfgauge-core/src/catalog/ and src/hub_client/catalog_sync.rs): the index
// first (its header holds the catalog key sealed under the library key), then the
// chunks whose hash changed. The WASM module (LibraryCatalog) checks every chunk
// against the index, refuses an index older than the one kept here, and searches and
// lists with the library's own folding. A chunk that does not match means the library
// is uploading a new catalog: what is kept here stays until the next try.
//
// "Get this book" writes a `book_request` record into the person's account; the
// library publishes the book onto their shelf at its next check (every 30 s while its
// computer is on), or refuses it (`refused` with a reason). The reading view asks the
// same way for a shelf book whose copy on the hub is gone (Phase 31.6, hub.js). A book card shows the
// book's thumbnail: kept here, else fetched from the hub, else asked for with a
// `cover_request` record (the library makes it at its next check). "Download book
// file" makes a book file here from a published book's blobs (bookfile.js). Text from
// the catalog goes into the page as text only (`textContent`), never as markup.
import * as bookfile from "./bookfile.js";
import * as c from "./crypto.js";
import * as store from "./store.js";

const base = store.base;
const PAGE_SIZE = 50;
// How often the page asks for the answers to its requests while some are waiting.
const POLL_MS = 15000;
// How often, and how many times, a thumbnail asked for is looked for again.
const COVER_RETRY_MS = 15000;
const COVER_TRIES = 6;
// The largest thumbnail opened.
const MAX_THUMBNAIL = 1024 * 1024;

let ui;
let wasm = null;
// { cat, generation, entries, key } of the catalog shown.
let shown = null;
let p = null;
let status = "";
// Book ids (hex) on the person's shelf → publication id.
let shelved = new Map();
// Book ids (hex) → the request record's value.
let requests = new Map();
let pollTimer = null;
// When the library's computer last checked in with the hub (Unix seconds), or null
// (it does not let its members know, or the hub restarted since).
let onlineAt = null;
// A check older than this means the computer is not on now (it checks every 30 s;
// the hub keeps the minute).
const ONLINE_WITHIN_S = 150;

const encoder = new TextEncoder();

/** The catalog book id of a content key: SHA-256("sg-catalog book" ‖ 0 ‖ key), 16 bytes, hex. */
export async function bookId(contentKey) {
  const data = new Uint8Array(16 + contentKey.length);
  data.set(encoder.encode("sg-catalog book\0"));
  data.set(contentKey, 16);
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", data));
  return c.hex(digest.subarray(0, 16));
}

async function loadWasm() {
  if (!wasm) {
    wasm = await import("./pkg/shelfgauge.js");
    await wasm.default();
  }
  return wasm;
}

/** The catalog this device keeps, or null (none, or broken). */
async function localCatalog(account) {
  const index = await store.catalogGet(account, "index");
  if (!index) {
    return null;
  }
  const cat = new wasm.LibraryCatalog();
  try {
    const summary = JSON.parse(cat.setIndex(index.plain, 0));
    for (let n = 0; n < summary.chunks.length; n++) {
      const chunk = await store.catalogGet(account, `c/${n}`);
      if (!chunk) {
        throw new Error(`chunk ${n} is missing`);
      }
      // Checked by its bytes when it was fetched: by its hash now.
      cat.setKeptChunk(n, chunk.plain, chunk.hash);
    }
    return { cat, generation: index.generation, entries: summary.entries, key: null };
  } catch (e) {
    console.error(e);
    cat.free();
    return null;
  }
}

/**
 * The hub's catalog of account `account`, newer than generation `lowest` (or as new):
 * `{ catalog }`, `{ none }` (the library never sent one, or the hub keeps none),
 * `{ older }` (refused: older than the one kept here), `{ changing }` (a chunk does
 * not match its index: the library is uploading) or `{ message }`.
 */
async function hubCatalog(account, lowest) {
  const response = await store.api("catalog/index", {}, account.token);
  if (response.status === 404) {
    return { none: true };
  }
  if (!response.ok) {
    throw new Error(`The hub answered ${response.status}.`);
  }
  const blob = new Uint8Array(await response.arrayBuffer());
  let header;
  try {
    header = c.catalogHeader(blob);
  } catch (e) {
    return { message: /newer shelfgauge/.test(e.message) ? "This Showcase needs a newer shelfgauge reader." : "The hub sent a broken Showcase." };
  }
  if (header.library !== account.library) {
    return { message: "The hub sent another library's Showcase." };
  }
  if (header.key_scope !== c.LIBRARY_SCOPE) {
    return { message: "This Showcase needs a newer shelfgauge reader." };
  }
  const libraryKey = store.libraryKey(account, header.key_id);
  if (!libraryKey) {
    return { message: "This Showcase needs a newer library key: connect to the internet and open it again." };
  }
  const key = await c.catalogKeyFromHeader(header, libraryKey);
  const libraryId = c.unhex(account.library);
  const plain = await c.openCatalogBlob(key, libraryId, "index", blob, c.MAX_INDEX_LEN);
  const cat = new wasm.LibraryCatalog();
  let summary;
  try {
    summary = JSON.parse(cat.setIndex(plain, lowest));
  } catch (e) {
    cat.free();
    return { older: true };
  }
  const put = [["index", { generation: summary.generation, plain }]];
  for (let n = 0; n < summary.chunks.length; n++) {
    const hash = summary.chunks[n];
    const local = await store.catalogGet(account, `c/${n}`);
    if (local && local.hash === hash) {
      try {
        cat.setKeptChunk(n, local.plain, hash);
        continue;
      } catch (e) {
        // Fetched again below.
      }
    }
    const answer = await store.api(`catalog/c/${n}`, {}, account.token);
    if (!answer.ok) {
      cat.free();
      return { changing: true };
    }
    const chunk = await c.openCatalogBlob(key, libraryId, `c/${n}`, new Uint8Array(await answer.arrayBuffer()), c.MAX_CHUNK_LEN);
    put.push([`c/${n}`, { hash, plain: chunk }]);
    try {
      cat.setChunk(n, chunk);
    } catch (e) {
      cat.free();
      return { changing: true };
    }
  }
  const drop = (await store.catalogKeys(account))
    .filter((k) => k.startsWith("c/") && Number(k.slice(2)) >= summary.chunks.length);
  await store.catalogWrite(account, put, drop);
  return { catalog: { cat, generation: summary.generation, entries: summary.entries, key } };
}

/** Brings the catalog shown up to date with the hub; sets `status`. */
async function refreshCatalog() {
  try {
    // The floor is the generation kept here, even when its chunks did not load.
    const kept = await store.catalogGet(p, "index").catch(() => null);
    const got = await hubCatalog(p, kept ? kept.generation : 0);
    if (got.catalog) {
      if (shown) {
        shown.cat.free();
      }
      shown = got.catalog;
      status = "";
    } else if (got.none) {
      status = shown ? "" : "none";
    } else if (got.older) {
      status = "The hub sent an older Showcase than this device has: showing the one kept here.";
    } else if (got.changing) {
      status = `${ui.libraryName(p)} is updating its Showcase: showing the one kept here.`;
    } else {
      status = got.message;
    }
  } catch (e) {
    if (e instanceof store.Unpaired) {
      throw e;
    }
    if (!ui.unreachable(e)) {
      console.error(e);
    }
    status = shown ? "Offline: showing the Showcase as of the last update." : "offline";
  }
}

/** When the library's computer last checked in (from `GET /api/account`). */
async function refreshOnline() {
  onlineAt = await libraryOnlineAt(p, onlineAt);
}

/**
 * When the library of account `account` last checked in with the hub (Unix seconds),
 * or null (it does not let its members know); `known` when the hub cannot be asked.
 */
export async function libraryOnlineAt(account, known = null) {
  try {
    const response = await store.api("account", {}, account.token);
    if (response.ok) {
      const at = (await response.json()).library_online_at;
      return Number.isFinite(at) ? at : null;
    }
  } catch (e) {
    // Offline: as known.
  }
  return known;
}

/** "3 hours ago" for a Unix time in seconds. */
function ago(seconds) {
  const s = Math.max(0, Date.now() / 1000 - seconds);
  const unit = (n, one) => `${n} ${one}${n === 1 ? "" : "s"} ago`;
  if (s < 3600) {
    return unit(Math.max(1, Math.round(s / 60)), "minute");
  }
  if (s < 86400) {
    return unit(Math.round(s / 3600), "hour");
  }
  return unit(Math.round(s / 86400), "day");
}

/** A line on the library's computer: online, last online, or nothing when unknown. */
function onlineLine() {
  return onlineNode(ui.el, ui.libraryName(p), onlineAt);
}

/** Whether a library whose computer last checked in at `at` (or null) is online now. */
function isOnlineAt(at) {
  return at !== null && Date.now() / 1000 - at < ONLINE_WITHIN_S;
}

/** What a pending request to library `name` waits for, from its last check-in `at`. */
export function waitingText(name, at) {
  return isOnlineAt(at)
    ? `Asked for it. ${name} is online: the book comes to your shelf within a minute.`
    : at !== null
      ? `Asked for it. Waiting for ${name} to come online (last online ${ago(at)}): it sends the book to your shelf when its computer is on.`
      : `Asked for it. Waiting for ${name} to come online: it sends the book to your shelf when its computer is on.`;
}

/**
 * The line saying whether library `name` is online, from its last check-in `at`
 * (`libraryOnlineAt`), made with `el`; null when the library does not tell.
 */
export function onlineNode(el, name, at) {
  if (at === null) {
    return null;
  }
  return isOnlineAt(at)
    ? el("p", { class: "muted hub-online", role: "status" }, el("span", { class: "hub-online-dot", "aria-hidden": "true" }), ` ${name} is online.`)
    : el("p", { class: "muted hub-online", role: "status" }, `${name} was last online ${ago(at)}.`);
}

/** The person's shelf and requests, by catalog book id. */
async function readAccount() {
  shelved = new Map();
  for (const item of await store.shelf(p).catch(() => [])) {
    try {
      shelved.set(await bookId(c.unbase64url(item.content_key)), item.publication);
    } catch (e) {
      // A broken item: left out.
    }
  }
  requests = new Map((await store.records(p, "book_request").catch(() => [])).map((r) => [r.id, r.value]));
}

// ---- Navigation ----------------------------------------------------------------

function params() {
  return new URLSearchParams(location.search);
}

/** The Showcase URL with `fields` (null values left out). */
function url(fields = {}) {
  const q = new URLSearchParams();
  for (const [name, value] of Object.entries(fields)) {
    if (value !== null && value !== undefined && value !== "") {
      q.set(name, value);
    }
  }
  const query = q.toString();
  return `${base}/showcase${query ? `?${query}` : ""}`;
}

/** A link within the Showcase (opened without reloading the page). */
function link(text, fields, attrs = {}) {
  return ui.el("a", Object.assign({ href: url(fields), "data-showcase": "" }, attrs), text);
}

function go(href) {
  history.pushState(null, "", href);
  render();
  window.scrollTo(0, 0);
}

// ---- Views ---------------------------------------------------------------------

/** What a book's line or card says about getting it. */
function bookState(entry) {
  const own = shelved.get(entry.id);
  if (own) {
    return { state: "shelf", publication: own };
  }
  const request = requests.get(entry.id);
  if (request && request.state === "pending") {
    return { state: "pending" };
  }
  if (request && request.state === "refused") {
    return { state: "refused", reason: request.reason };
  }
  return { state: entry.publication ? "published" : "none" };
}

function refusal(reason) {
  return refusalText(ui.libraryName(p), reason);
}

/** Why library `name` refused a book request, from its `reason`. */
export function refusalText(name, reason) {
  switch (reason) {
    case "closed":
      return "This book is not on the Showcase any more.";
    case "drm":
      return "This book is protected by DRM and never leaves the library.";
    case "missing":
      return `${name} no longer has this book.`;
    case "quota":
      return `${name} could not send it: its space on the hub is full.`;
    case "too-large":
      return `${name} could not send it: the book is larger than the hub takes.`;
    default:
      return `${name} could not send this book. Try again later.`;
  }
}

function badge(entry) {
  const { state } = bookState(entry);
  const text = { shelf: "on your shelf", pending: "asked", refused: "refused" }[state];
  return text ? ui.el("span", { class: `badge hub-badge-${state}` }, text) : null;
}

function bookList(page) {
  if (!page.entries.length) {
    return [ui.el("p", { class: "empty" }, "No books.")];
  }
  const list = ui.el("ul", { class: "books" });
  for (const entry of page.entries) {
    const li = ui.el("li", {}, link(entry.title || "Untitled", { book: entry.id }));
    if (entry.author.length) {
      li.append(" ", ui.el("span", { class: "authors" }, entry.author.join(", ")));
    }
    if (entry.series.length) {
      const s = entry.series[0];
      li.append(" ", ui.el("span", { class: "series" }, s.position ? `${s.name} #${s.position}` : s.name));
    }
    const b = badge(entry);
    if (b) {
      li.append(" ", b);
    }
    list.append(li);
  }
  return [list];
}

function pager(fields, page, total) {
  const pages = Math.ceil(total / PAGE_SIZE);
  if (pages < 2) {
    return null;
  }
  const nav = ui.el("nav", { class: "pager", "aria-label": "Pages" });
  if (page > 1) {
    nav.append(link("‹ Prev", Object.assign({}, fields, { page: page - 1 }), { rel: "prev" }));
  }
  nav.append(ui.el("span", {}, `${page} of ${pages}`));
  if (page < pages) {
    nav.append(link("Next ›", Object.assign({}, fields, { page: page + 1 }), { rel: "next" }));
  }
  return nav;
}

/** A page of a book list of the catalog. */
function books(list, arg, fields, title) {
  const pageNumber = Math.max(1, Number(params().get("page")) || 1);
  const page = JSON.parse(shown.cat.list(list, arg, (pageNumber - 1) * PAGE_SIZE, PAGE_SIZE));
  const nodes = [];
  if (title) {
    nodes.push(ui.el("h2", {}, title), ui.el("p", { class: "muted" }, `${page.total} ${page.total === 1 ? "book" : "books"}`));
  }
  nodes.push(...bookList(page), pager(fields, pageNumber, page.total));
  return nodes;
}

function names(which, param) {
  const all = JSON.parse(shown.cat.names(which));
  if (!all.length) {
    return [ui.el("p", { class: "empty" }, "None.")];
  }
  const list = ui.el("ul", { class: "authors" });
  for (const n of all) {
    list.append(ui.el("li", {}, link(n.name, { [param]: n.key || n.code }), " ", ui.el("span", { class: "count" }, String(n.count))));
  }
  return [list];
}

function formatBytes(n) {
  if (n < 1024 * 1024) {
    return `${Math.max(1, Math.round(n / 1024))} KB`;
  }
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

/** "Download book file": the book file made here from the publication's blobs. */
function downloadButton(publication) {
  const button = ui.el("button", { type: "button" }, "Download book file");
  const note = ui.el("span", { class: "muted hub-download-status", role: "status" });
  button.addEventListener("click", async () => {
    button.disabled = true;
    note.textContent = "Making the book file…";
    try {
      const { blob, name } = await bookfile.exportPublication(p, publication);
      const href = URL.createObjectURL(blob);
      const a = ui.el("a", { href, download: name, hidden: "" });
      document.body.append(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(href), 60000);
      note.textContent = "";
    } catch (e) {
      note.textContent = e instanceof bookfile.BookFileError ? e.message
        : ui.unreachable(e) ? "Offline: the book file needs a connection (or the whole book read here once)." : "The book file could not be made.";
      if (!(e instanceof bookfile.BookFileError) && !ui.unreachable(e)) {
        console.error(e);
      }
    } finally {
      button.disabled = false;
    }
  });
  return [button, " ", note];
}

/** The buttons and notes of a book card. */
function actions(entry) {
  const { state, publication, reason } = bookState(entry);
  const name = ui.libraryName(p);
  const nodes = [];
  const row = ui.el("p", { class: "actions" });
  if (state === "shelf") {
    row.append(ui.el("a", { class: "button primary", href: `${base}/r/${publication}` }, "Open"), " ", ...downloadButton(publication));
    nodes.push(row, ui.el("p", { class: "muted" }, "On your shelf."));
    return nodes;
  }
  if (state === "published") {
    row.append(ui.el("a", { class: "button primary", href: `${base}/r/${entry.publication}` }, "Open"), " ");
  }
  if (state === "pending") {
    nodes.push(ui.el("p", { class: "notice hub-waiting", role: "status" }, waitingText(name, onlineAt)));
    return nodes;
  }
  const get = ui.el("button", { type: "button", class: state === "published" ? "" : "primary" }, state === "refused" ? "Ask again" : "Get this book");
  get.addEventListener("click", async () => {
    get.disabled = true;
    await ask(entry);
  });
  if (!(state === "refused" && reason === "closed")) {
    row.append(get);
  }
  if (state === "published") {
    row.append(" ", ...downloadButton(entry.publication));
  }
  nodes.push(row);
  if (state === "refused") {
    nodes.push(ui.el("p", { class: "notice error", role: "status" }, refusal(reason)));
  } else if (state === "published") {
    nodes.push(ui.el("p", { class: "muted" }, "Already on the hub: it opens now. Get this book puts it on your shelf too."));
  }
  return nodes;
}

function bookCard(id) {
  const json = shown.cat.get(id);
  if (!json) {
    return [ui.el("p", { class: "empty" }, "This book is not on the Showcase (any more)."), ui.el("p", {}, link("Back to the Showcase", {}))];
  }
  const entry = JSON.parse(json);
  const info = ui.el("div", { class: "info" }, ui.el("h1", {}, entry.title || "Untitled"));
  if (entry.author.length) {
    const by = ui.el("p", { class: "by" });
    entry.author.forEach((a, i) => {
      if (i) {
        by.append(", ");
      }
      by.append(link(a, { author: authorKey(a) }));
    });
    info.append(by);
  }
  const facts = ui.el("dl", { class: "facts" });
  const fact = (term, ...value) => facts.append(ui.el("dt", {}, term), ui.el("dd", {}, ...value));
  if (entry.series.length) {
    const nodes = [];
    entry.series.forEach((s, i) => {
      if (i) {
        nodes.push("; ");
      }
      nodes.push(link(s.position ? `${s.name} #${s.position}` : s.name, { series: authorKey(s.name) }));
    });
    fact("Series", ...nodes);
  }
  if (entry.subject.length) {
    const nodes = [];
    entry.subject.forEach((g, i) => {
      if (i) {
        nodes.push(", ");
      }
      nodes.push(link(g.name, { genre: g.code }));
    });
    fact("Genres", ...nodes);
  }
  if (entry.language) {
    fact("Language", entry.language);
  }
  fact("Format", `${entry.format || "book"}, ${formatBytes(entry.size)}`);
  if (entry.added) {
    fact("Added", new Date(entry.added * 1000).toLocaleDateString());
  }
  info.append(facts, ...actions(entry));
  const placeholder = ui.el("div", { class: "cover placeholder", "aria-hidden": "true" }, entry.title || "");
  const card = ui.el("article", { class: "book-card" }, placeholder, info);
  if (entry.cover) {
    showCover(entry, placeholder);
  }
  const nodes = [card];
  if (entry.description) {
    nodes.push(ui.el("section", { class: "annotation" }, ui.el("h2", {}, "Annotation"), ui.el("p", {}, entry.description)));
  }
  return nodes;
}

// ---- Covers --------------------------------------------------------------------

// Thumbnails asked for in this page's life (not asked again).
const coversAsked = new Set();

/** The thumbnail of `entry` in place of `placeholder`: kept here, else fetched, else asked for. */
async function showCover(entry, placeholder, tries = 0) {
  try {
    let image = await store.catalogGet(p, `t/${entry.cover}`);
    if (!image && shown.key) {
      const response = await store.api(`catalog/t/${entry.cover}`, {}, p.token);
      if (response.ok) {
        const sealed = new Uint8Array(await response.arrayBuffer());
        const plain = await c.openCatalogBlob(shown.key, c.unhex(p.library), `t/${entry.cover}`, sealed, MAX_THUMBNAIL);
        image = new Blob([plain], { type: "image/jpeg" });
        await store.catalogWrite(p, [[`t/${entry.cover}`, image]]);
      } else if (response.status === 404) {
        await askCover(entry.cover);
        if (tries < COVER_TRIES && placeholder.isConnected) {
          setTimeout(() => showCover(entry, placeholder, tries + 1), COVER_RETRY_MS);
        }
      }
    }
    if (image && placeholder.isConnected) {
      const img = ui.el("img", { class: "cover", alt: `Cover of ${entry.title || "the book"}` });
      img.src = URL.createObjectURL(image);
      img.addEventListener("load", () => URL.revokeObjectURL(img.src), { once: true });
      placeholder.replaceWith(img);
    }
  } catch (e) {
    if (!ui.unreachable(e)) {
      console.error(e);
    }
  }
}

/** Asks the library for a thumbnail (a `cover_request` record of this device). */
async function askCover(cover) {
  if (coversAsked.has(cover)) {
    return;
  }
  coversAsked.add(cover);
  await store.write(p, "cover_request", `covers-${p.device}`, {
    covers: [...coversAsked].slice(-200),
    updated_at: Math.floor(Date.now() / 1000),
  });
  await store.push(p).catch(() => {});
}

/** The key the lists of authors and series take (the core's `normalize_key`). */
function authorKey(name) {
  return wasm.normalizeKey(name);
}

function emptyShowcase() {
  const name = ui.libraryName(p);
  const nodes = [ui.el("p", { class: "empty" }, `${name} hasn't put any books on its Showcase yet.`)];
  nodes.push(ui.el("p", { class: "hint" }, p.role === "owner"
    ? "On the library's computer, open folders for the Showcase with the checkbox next to each folder in the sidebar (Folders)."
    : `Ask ${name}'s admin to open folders for the Showcase.`));
  return nodes;
}

/** The page tabs, Showcase active; its tab goes to the Showcase's start in place. */
function showcaseTabs(title) {
  const nodes = ui.pageTabs("showcase", title === undefined ? {} : { title });
  const tab = nodes[nodes.length - 1].querySelector(".active");
  tab.setAttribute("href", url({}));
  tab.dataset.showcase = "";
  return nodes;
}

function heading() {
  const name = ui.libraryName(p);
  const day = shown && shown.generation
    ? new Date(shown.generation).toLocaleDateString(undefined, { day: "numeric", month: "long", year: "numeric" })
    : "";
  // Some locales end the date with a dot of their own ("1 ноября 2026 г."): no second one.
  const updated = day ? ` Updated ${day}${day.endsWith(".") ? "" : "."}` : "";
  const nodes = [...showcaseTabs(), ui.el("p", { class: "muted" }, `The books of ${name}: get any of them onto your shelf.${updated}`)];
  const online = onlineLine();
  if (online) {
    nodes.push(online);
  }
  if (status && status !== "none" && status !== "offline") {
    nodes.push(ui.el("p", { class: "muted hub-sync", role: "status" }, status));
  }
  return nodes;
}

function searchForm(query) {
  const input = ui.el("input", { type: "search", name: "q", placeholder: "Title, author, series, genre", "aria-label": "Search the Showcase", autocomplete: "off" });
  input.value = query;
  const form = ui.el("form", { class: "inline-form hub-showcase-search", role: "search" }, input, ui.el("button", { type: "submit" }, "Search"));
  form.addEventListener("submit", (event) => {
    event.preventDefault();
    go(url({ q: input.value.trim() }));
  });
  return form;
}

function tabs(current) {
  const nav = ui.el("nav", { class: "hub-showcase-tabs", "aria-label": "Showcase" });
  for (const [view, text] of [["recent", "Recent"], ["authors", "Authors"], ["series", "Series"], ["genres", "Genres"], ["all", "All books"]]) {
    const a = link(text, view === "recent" ? {} : { view });
    if (view === current) {
      a.setAttribute("aria-current", "page");
      a.className = "active";
    }
    nav.append(a);
  }
  return nav;
}

/** Renders the Showcase for the current URL. */
function render() {
  const q = params();
  if (!shown) {
    const nodes = heading();
    if (status === "none") {
      nodes.push(ui.el("p", { class: "empty" }, `${ui.libraryName(p)} has not sent its Showcase to the hub yet. It does when its computer is on.`));
    } else if (status === "offline") {
      nodes.push(ui.el("p", { class: "empty" }, "Offline: the Showcase opens once this device has downloaded it with a connection."));
    } else if (status) {
      nodes.push(ui.el("p", { class: "error" }, status));
    }
    ui.show(...nodes);
    return;
  }
  if (q.has("book")) {
    // The card has its own title (the book's).
    const back = ui.el("p", { class: "hub-back" }, link("‹ Showcase", {}));
    ui.show(...showcaseTabs(null), back, ...bookCard(q.get("book")));
    return;
  }
  const nodes = heading();
  if (!shown.entries) {
    nodes.push(...emptyShowcase());
    ui.show(...nodes);
    return;
  }
  const query = q.get("q") || "";
  nodes.push(searchForm(query));
  if (query) {
    nodes.push(...books("search", query, { q: query }, `Found for “${query}”`));
  } else if (q.has("author")) {
    nodes.push(tabs("authors"), ...books("author", q.get("author"), { author: q.get("author") }, authorTitle("authors", q.get("author"))));
  } else if (q.has("series")) {
    nodes.push(tabs("series"), ...books("series", q.get("series"), { series: q.get("series") }, authorTitle("series", q.get("series"))));
  } else if (q.has("genre")) {
    nodes.push(tabs("genres"), ...books("genre", q.get("genre"), { genre: q.get("genre") }, genreTitle(q.get("genre"))));
  } else {
    const view = q.get("view") || "recent";
    nodes.push(tabs(view));
    if (view === "authors" || view === "series") {
      nodes.push(...names(view, view === "authors" ? "author" : "series"));
    } else if (view === "genres") {
      nodes.push(...names("genres", "genre"));
    } else if (view === "all") {
      nodes.push(...books("all", "", { view: "all" }, null));
    } else {
      nodes.push(...books("recent", "", {}, null));
    }
  }
  ui.show(...nodes.filter(Boolean));
}

function authorTitle(which, key) {
  const found = JSON.parse(shown.cat.names(which)).find((n) => n.key === key);
  return found ? found.name : key;
}

function genreTitle(code) {
  const found = JSON.parse(shown.cat.names("genres")).find((n) => n.code === code);
  return found ? found.name : code;
}

// ---- Requests ------------------------------------------------------------------

/** "Get this book": a request record, pushed at once; the answer comes by polling. */
async function ask(entry) {
  const value = await requestBook(p, entry.id);
  requests.set(entry.id, value);
  render();
  try {
    await store.push(p);
  } catch (e) {
    if (!ui.unreachable(e)) {
      console.error(e);
    }
  }
  poll();
}

/**
 * Writes a pending request of account `account` for catalog book `id` (hex) and
 * leaves it for the next push; the request's value. Later than an earlier answer kept
 * here, so it wins the merge even when the library's clock is ahead of this device's.
 */
export async function requestBook(account, id) {
  const earlier = await store.get(account, "book_request", id).catch(() => null);
  const now = Math.floor(Date.now() / 1000);
  const updatedAt = earlier && Number.isInteger(earlier.updated_at) ? Math.max(now, earlier.updated_at + 1) : now;
  const value = { book: id, reason: null, state: "pending", updated_at: updatedAt };
  await store.write(account, "book_request", id, value);
  return value;
}

/** While a request waits, asks for the answers every POLL_MS. */
function poll() {
  clearTimeout(pollTimer);
  const waiting = [...requests.entries()].some(([id, r]) => r.state === "pending" && !shelved.has(id));
  if (!waiting) {
    return;
  }
  pollTimer = setTimeout(async () => {
    try {
      p = await ui.syncAccount(p);
      await readAccount();
      await refreshOnline();
      render();
    } catch (e) {
      if (!ui.unreachable(e)) {
        console.error(e);
      }
    }
    poll();
  }, POLL_MS);
}

/**
 * The Showcase page; `helpers` are the shelf page's (`el`, `show`, `notice`,
 * `syncAccount`, `libraryName`, `unreachable`, `signedOut`).
 */
export async function showcasePage(helpers) {
  ui = helpers;
  p = await store.current();
  if (!p) {
    ui.show(...ui.signedOut());
    return;
  }
  ui.show(...showcaseTabs(), ui.el("p", { class: "muted", role: "status" }, "Opening…"));
  await loadWasm();
  shown = await localCatalog(p);
  await readAccount();
  render();
  try {
    p = await ui.syncAccount(p);
  } catch (e) {
    if (e instanceof store.Unpaired) {
      ui.show(...ui.signedOut());
      return;
    }
  }
  await refreshCatalog();
  await readAccount();
  await refreshOnline();
  render();
  poll();
  window.addEventListener("popstate", render);
  document.getElementById("hub-app").addEventListener("click", (event) => {
    const a = event.target.closest("a[data-showcase]");
    if (a && !event.ctrlKey && !event.metaKey && !event.shiftKey && event.button === 0) {
      event.preventDefault();
      go(a.getAttribute("href"));
    }
  });
}
