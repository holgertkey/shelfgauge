// shelfgauge hub: the reader app's Service Worker (served at {{ base }}/sw.js with
// this hub's base path and asset version filled in; src/hub/shell.rs).
//
// Offline reading:
// - the app's pages (`/`, `/pair`, `/showcase`, `/r/<id>`): static per version, served from the
//   cache (every /r/<id> is the same page), so pages and files are of one version; a
//   new version takes over only when the shelf page asks (no open book switches);
// - the app's files: precached per version, served from the cache;
// - publication blobs: parts and images never change (a changed book is a new
//   publication), so they are served from the cache once fetched; the manifest comes
//   from the network when it answers in time (the hub counts its fetches as openings),
//   else from the cache — also when a reverse proxy answers 5xx for a hub that is
//   down, and when the hub no longer has it (`404`: expired or deleted there): a copy
//   here stays as long as the book is on the shelf (the app drops the copies of books
//   that left it); only a takedown (`410`) deletes it at once;
// - the rest of the API: always the network;
// - a book file shared to the installed app (`share_target`, `POST /share`): kept in
//   the cache SHARED for the app to open, answered with `303` to `/open?shared=1`.
// A full storage never breaks reading online: what cannot be cached is still served.
// Only ciphertext is cached; the app deletes the copies of books that left the shelf
// (and of every book on Remove downloaded books).
"use strict";

const BASE = "{{ base }}";
const VERSION = "{{ version }}";
const SHELL = `shelfgauge-shell-${VERSION}`;
const BLOBS = "shelfgauge-blobs";
// A book file shared to the app, until the page takes it (hub.js).
const SHARED = "shelfgauge-shared";
// The reading view is the same page for every publication: cached once, under this.
const READER = `${BASE}/r/00000000000000000000000000000000`;
const HOME = `${BASE}/`;
const FILES = [
{%- for file in files %}
  `${BASE}/assets/{{ file }}`,
{%- endfor %}
];

self.addEventListener("install", (event) => {
  event.waitUntil((async () => {
    const cache = await caches.open(SHELL);
    await cache.addAll([HOME, READER, ...FILES]);
    // A new version waits until the shelf page asks for it (hub.js): an open book
    // keeps the version it was opened with (its later imports come from that cache).
  })());
});

self.addEventListener("message", (event) => {
  if (event.data === "activate") {
    self.skipWaiting();
  }
});

self.addEventListener("activate", (event) => {
  event.waitUntil((async () => {
    for (const name of await caches.keys()) {
      if (name.startsWith("shelfgauge-shell-") && name !== SHELL) {
        await caches.delete(name);
      }
    }
    await self.clients.claim();
  })());
});

const BLOB = /^\/api\/pub\/[0-9a-f]{32}\/(manifest|p\/[0-9]+|i\/[0-9]+)$/;
const PAGE = /^\/(pair|open|device|signin|invite|showcase|r\/[0-9a-f]{32})?$/;
// How long the manifest may take before the cached copy is used.
const MANIFEST_WAIT_MS = 4000;

self.addEventListener("fetch", (event) => {
  const request = event.request;
  const url = new URL(request.url);
  const ours = url.pathname.startsWith(`${BASE}/`) || url.pathname === BASE;
  if (request.method === "POST" && url.origin === self.location.origin && url.pathname === `${BASE}/share`) {
    event.respondWith(share(request));
    return;
  }
  if (request.method !== "GET" || url.origin !== self.location.origin || !ours) {
    return;
  }
  const path = url.pathname.slice(BASE.length) || "/";
  if (request.mode === "navigate") {
    if (PAGE.test(path) || url.pathname === BASE) {
      const shell = path.startsWith("/r/") ? READER : HOME;
      event.respondWith((async () => (await caches.match(shell, { cacheName: SHELL })) || fetch(request))());
    }
  } else if (path.startsWith("/assets/")) {
    event.respondWith((async () => (await caches.match(url.pathname, { cacheName: SHELL })) || fetch(request))());
  } else if (BLOB.test(path)) {
    event.respondWith(blob(request, url, path.endsWith("/manifest")));
  }
});

/** Stores a copy; a full storage only means no offline copy. */
async function keep(cache, key, response) {
  try {
    await cache.put(key, response);
  } catch (e) {
    // Not kept.
  }
}

/** The network's answer, or null after `ms` or on a network error. */
function within(request, ms) {
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(null), ms);
    fetch(request).then((response) => {
      clearTimeout(timer);
      resolve(response);
    }, () => {
      clearTimeout(timer);
      resolve(null);
    });
  });
}

/**
 * Whether `response` is a blob of the hub (ciphertext). A proxy in between may answer
 * `200` with its own HTML page (ngrok's free-plan warning, a Wi-Fi sign-in page):
 * never kept, and a copy kept by an older worker is not served.
 */
function isBlob(response) {
  return response.ok && response.headers.get("Content-Type") === "application/octet-stream";
}

async function blob(request, url, manifest) {
  const cache = await caches.open(BLOBS);
  const key = url.origin + url.pathname;
  let cached = await cache.match(key);
  if (cached && !isBlob(cached)) {
    await cache.delete(key);
    cached = undefined;
  }
  if (!manifest) {
    if (cached) {
      return cached;
    }
    const response = await fetch(request);
    if (isBlob(response)) {
      await keep(cache, key, response.clone());
    }
    return response;
  }
  const response = await within(request, cached ? MANIFEST_WAIT_MS : 60000);
  if (response && isBlob(response)) {
    await keep(cache, key, response.clone());
    return response;
  }
  if (response && response.status === 410) {
    // Taken down by the hub's operator: so are the copies here.
    const prefix = key.slice(0, key.lastIndexOf("/"));
    for (const old of await cache.keys()) {
      if (old.url.startsWith(`${prefix}/`)) {
        await cache.delete(old);
      }
    }
    return response;
  }
  // Out of reach, slow, the proxy says the hub is down (5xx) or answers with its own
  // page, or the hub's copy is gone (`404`: expired, or deleted by the library, whose
  // removal from the shelf drops this copy at the next sync): the copy, if any.
  if (cached && (!response || response.status >= 500 || response.status === 404 || response.ok)) {
    return cached;
  }
  return response || fetch(request);
}

/** Keeps a shared book file for the page (the first file of the form) and sends the
 * browser to `/open`, which opens it. */
async function share(request) {
  let shared = "none";
  try {
    const form = await request.formData();
    const file = form.getAll("book").find((f) => f && typeof f.arrayBuffer === "function");
    if (file) {
      const cache = await caches.open(SHARED);
      await cache.put(`${BASE}/shared-file`, new Response(file, {
        headers: { "Content-Type": "application/octet-stream", "X-File-Name": encodeURIComponent(file.name || "book.sgbook") },
      }));
      shared = "1";
    }
  } catch (e) {
    shared = "lost";
  }
  return Response.redirect(`${self.location.origin}${BASE}/open?shared=${shared}`, 303);
}
