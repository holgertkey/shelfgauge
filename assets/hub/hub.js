// The hub's reader app: pairing this browser (`/pair#<secret>.<account key>`), the
// shelf (`/`) and the reading view (`/r/<publication id>?part=<n>`).
//
// Books arrive as encrypted blobs; crypto.js opens them, the WASM module
// (pkg/shelfgauge.js, the instance's own renderer) turns them into escaped HTML, and
// the page is then the instance's reading page: reader.js runs on it unchanged, with
// `window.shelfgaugeSource` giving it the pages of other parts. Text from records or
// the manifest goes into the page as text only (`textContent`), never as markup.
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

/** Where the reader learns how to pair this browser. */
function unpairedHelp() {
  return [
    el("h1", {}, "This device is not paired"),
    el("p", {}, "Books on this hub open only on devices paired with their library's account."),
    el("p", {}, "On the computer with your library, open My profile → Phone and other devices, choose Pair a device, and scan the QR code with this device (or open its link here)."),
  ];
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
  const link = el("a", { href: `${base}/r/${item.publication}` }, item.title || "Untitled");
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
  if (!items.length) {
    nodes.push(el("p", { class: "hint" }, "No books yet. In your library, choose Open on phone on a book's page to put it here."));
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

async function shelfPage() {
  const p = await store.pairing();
  if (!p) {
    show(...unpairedHelp());
    return;
  }
  renderShelf(await store.shelf(p), "Updating…");
  try {
    await store.pull(p);
    renderShelf(await store.shelf(p), "");
  } catch (e) {
    if (e instanceof store.Unpaired) {
      await store.forget();
      show(...unpairedHelp());
      return;
    }
    console.error(e);
    renderShelf(await store.shelf(p), "Offline: showing the books as of the last update.");
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
  constructor(p, id, key, reader) {
    this.p = p;
    this.id = id;
    this.idBytes = c.unhex(id);
    this.key = key;
    this.reader = reader;
    this.summary = JSON.parse(reader.summary());
    this.loaded = new Map();
    this.images = new Map();
  }

  async open(path) {
    const sealed = await store.blob(this.p, this.id, path);
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

function startReader(book) {
  window.shelfgaugeSource = {
    // A page of another part, rendered here (notes open in a panel from it).
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
  };
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
  document.head.append(script);
}

async function readerPage(id) {
  const p = await store.pairing();
  if (!p) {
    readerMessage(...unpairedHelp().map((node) => node.textContent));
    return;
  }
  try {
    let item = (await store.shelf(p)).find((i) => i.publication === id);
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
    const key = await c.aesKey(c.unbase64url(item.book_key));
    const sealed = await store.blob(p, id, "manifest");
    if (!sealed) {
      readerMessage(`“${item.title}” is no longer on the hub`, "It was not opened for a long time, or it was removed. Choose Open on phone on the book's page in your library to publish it again.");
      return;
    }
    const wasm = await import("./pkg/shelfgauge.js");
    await wasm.default();
    const reader = new wasm.Reader(await c.openBlob(key, c.unhex(id), "manifest", sealed));
    const book = new Book(p, id, key, reader);
    const params = new URLSearchParams(location.search);
    const count = book.summary.parts.length;
    const share = params.has("progress") ? Number(params.get("progress")) : NaN;
    let index;
    if (Number.isFinite(share) && share >= 0 && share <= SHARE_SCALE) {
      index = partAtShare(book.summary.parts, share);
    } else {
      index = Math.min(Math.max(Number(params.get("part")) || 1, 1), Math.max(count, 1)) - 1;
    }
    let html = "<p class=\"empty\">This book has no text.</p>";
    if (count) {
      await book.part(index);
      html = book.reader.renderPart(index);
    }
    fillPage(book, index, html);
    if (Number.isFinite(share) && count) {
      // The track's jump into another part: the block at that share, as a fragment.
      const block = blockAtShare(document.querySelector("article.reader .text"), book.summary.parts[index].progress || { start: 0, end: 0 }, share);
      history.replaceState(null, "", `?part=${index + 1}${block ? `#${block.id}` : ""}`);
    }
    startReader(book);
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
    readerMessage("The book could not be opened", navigator.onLine ? String(e.message || e) : "This device is offline.");
  }
}

if (route === "/pair") {
  pairPage();
} else if (route.startsWith("/r/")) {
  readerPage(route.slice(3));
} else {
  shelfPage();
}
