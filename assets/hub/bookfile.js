// Opening book files (`.sgbook`, concept §11 "Book files") on a device signed in to
// the library they come from.
//
// The file is untrusted: the WASM module (the instance's container reader) checks its
// layout from the first bytes, the end record and the central directory only, and the
// entries are cut out of the File by offset, so a large file never sits in memory.
// Then, in this order and before anything is decrypted: sg.json's versions (a file of
// 0.19–0.20 is "made by an older shelfgauge"), the library (the account of that
// library here; another library's file is refused without trying a key) and the
// generation of its key; only then the book key from the manifest's header (sealed
// under the library key) and the manifest. The snapshot is merged only into the
// account that made the file. Every check works offline unless a newer library key
// must be pulled. The copy is kept sealed in IndexedDB
// (store.js "copies" and "files") and opened like a hub publication.
//
// The Rust reference of these checks is src/hub_client/book_file.rs (`open`).
import * as c from "./crypto.js";
import * as store from "./store.js";

const CONTAINER_VERSION = 2;
// The book files of 0.19–0.20 (their keys no longer exist).
const OLD_CONTAINER_VERSION = 1;
const MIME = "application/vnd.shelfgauge.book";
const END_LEN = 22;
// No blob is larger than this (the hub's own limit is 64 MiB by default).
const MAX_BLOB = 256 * 1024 * 1024;

/** The `accept` list of "Open file": the type and both extensions. */
export const ACCEPT = `.sgbook,.sgb,${MIME}`;

/** A file that does not open here; `message` is for the reader. */
export class BookFileError extends Error {}

let wasmModule = null;

async function wasm() {
  if (!wasmModule) {
    wasmModule = import("./pkg/shelfgauge.js").then(async (m) => {
      await m.default();
      return m;
    });
  }
  return wasmModule;
}

async function bytes(blob) {
  return new Uint8Array(await blob.arrayBuffer());
}

/** The layout of `file`: `{ entries: Map(name → {offset, size}), sg }`. */
async function layout(file) {
  const m = await wasm();
  const damaged = (e) => new BookFileError(String(e && e.message ? e.message : e));
  const head = m.bookFileHeadLength();
  if (file.size < head + END_LEN) {
    throw new BookFileError("This is not a shelfgauge book file.");
  }
  try {
    m.bookFileHead(await bytes(file.slice(0, head)));
  } catch (e) {
    throw new BookFileError("This is not a shelfgauge book file.");
  }
  let list;
  try {
    const tail = await bytes(file.slice(file.size - END_LEN));
    const dir = JSON.parse(m.bookFileDirectory(tail, file.size));
    const directory = await bytes(file.slice(dir.offset, dir.offset + dir.size));
    list = JSON.parse(m.bookFileEntries(tail, directory, file.size));
  } catch (e) {
    throw damaged(e);
  }
  const entries = new Map(list.map((e) => [e.name, e]));
  const sgEntry = entries.get("sg.json");
  let sg;
  try {
    sg = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(await bytes(file.slice(sgEntry.offset, sgEntry.offset + sgEntry.size))));
  } catch (e) {
    throw new BookFileError("The book file is damaged (its sg.json does not read).");
  }
  return { entries, sg };
}

function checkFields(sg) {
  const fine = sg && typeof sg === "object"
    && Number.isInteger(sg.format) && Number.isInteger(sg.format_version) && Number.isInteger(sg.key_id)
    && typeof sg.publication === "string" && /^[0-9a-f]{32}$/.test(sg.publication)
    && typeof sg.library === "string" && /^[0-9a-f]{32}$/.test(sg.library)
    && typeof sg.account === "string" && /^[0-9a-f]{32}$/.test(sg.account)
    && typeof sg.key_scope === "string" && typeof sg.mime === "string"
    && (sg.snapshot === undefined || Array.isArray(sg.snapshot));
  if (!fine) {
    throw new BookFileError("The book file is damaged (its sg.json is incomplete).");
  }
}

/**
 * Checks book file `file` (a File or Blob), finds the account of its library here and
 * keeps a copy for it on this device. Returns `{ publication, contentKey }`. Throws a
 * BookFileError with a message for the reader.
 */
export async function importFile(file) {
  const { entries, sg } = await layout(file);
  if (sg && sg.format === OLD_CONTAINER_VERSION) {
    throw new BookFileError("This book file was made by an older shelfgauge; get it again from your library.");
  }
  checkFields(sg);
  if (sg.format !== CONTAINER_VERSION || sg.format_version !== c.FORMAT_VERSION || sg.key_scope !== c.LIBRARY_SCOPE) {
    throw new BookFileError("This book file needs a newer version of the reader. Update shelfgauge on your hub, then open the file again.");
  }
  if (sg.mime !== MIME) {
    throw new BookFileError("The book file is damaged (another media type).");
  }
  let p = (await store.accounts()).find((a) => a.library === sg.library);
  if (!p) {
    throw new BookFileError("This book file is from a library this device is not signed in to. Sign in with the address your library's admin gave you.");
  }
  if (!store.libraryKey(p, sg.key_id)) {
    // A newer generation: it may have arrived on the hub already.
    try {
      await store.pull(p);
      p = await store.takeLibraryKeys(p);
    } catch (e) {
      // Offline: the check below says so.
    }
  }
  const libraryKey = store.libraryKey(p, sg.key_id);
  if (!libraryKey) {
    throw new BookFileError("This book file needs a newer library key: connect to the internet to update, then open it again.");
  }
  const id = sg.publication;
  const slice = (name) => {
    const e = entries.get(name);
    return e ? file.slice(e.offset, e.offset + e.size) : null;
  };
  const manifestBlob = slice("manifest");
  if (!manifestBlob) {
    throw new BookFileError("The book file is damaged (no manifest).");
  }
  const manifest = await bytes(manifestBlob);
  let key;
  try {
    const header = c.keyHeader(manifest);
    if (header.library !== sg.library || header.key_id !== sg.key_id || header.key_scope !== sg.key_scope) {
      throw new Error("another key");
    }
    key = await c.bookKeyFromHeader(header, libraryKey, id);
  } catch (e) {
    throw new BookFileError("The book file is damaged (its key does not open).");
  }
  let summary;
  try {
    const m = await wasm();
    const reader = new m.Reader(await c.openBlob(key, c.unhex(id), "manifest", manifest));
    summary = JSON.parse(reader.summary());
    reader.free();
  } catch (e) {
    throw new BookFileError("The book file is damaged (its contents do not open).");
  }
  const paths = ["manifest"];
  summary.parts.forEach((_, n) => paths.push(`p/${n}`));
  summary.images.forEach((_, n) => paths.push(`i/${n}`));
  const blobs = [];
  for (const path of paths) {
    const blob = slice(path);
    if (!blob || blob.size > MAX_BLOB) {
      throw new BookFileError("The book file is damaged (a part is missing).");
    }
    blobs.push({ path, blob });
  }
  const contentKey = summary.content_key;
  await store.saveCopy(p, {
    publication: id,
    content_key: contentKey,
    title: summary.title || "",
    authors: (summary.authors || []).join(", "),
    key_id: sg.key_id,
    imported_at: Date.now(),
  }, blobs);
  // One copy per book: an earlier file of the same book (still openable by opening
  // it again) gives way to this one.
  await store.dropCopies(p, (copy) => copy.content_key === contentKey && copy.publication !== id);
  await store.setCurrent(p.account);
  await store.navigatorPersist();

  // The marks at export time, then the book on the shelf if it is not there (opening
  // a book file is putting it on the shelf).
  const snapshot = [];
  for (const r of sg.snapshot || []) {
    if (r && typeof r.kind === "string" && typeof r.id === "string" && typeof r.data === "string") {
      try {
        snapshot.push({ kind: r.kind, id: r.id, data: c.unbase64url(r.data) });
      } catch (e) {
        // Left out.
      }
    }
  }
  // The marks of the person who made the file: only into their own account.
  const kinds = new Set(["shelf_item", "annotation", "position"]);
  const usable = sg.account === p.account ? snapshot.filter((r) => kinds.has(r.kind)) : [];
  await store.mergeSnapshot(p, usable, { revive: false });
  const shelved = (await store.shelf(p)).some((i) => i.content_key === contentKey);
  if (!shelved) {
    const old = usable.find((r) => r.kind === "shelf_item");
    let instance = "";
    if (old) {
      try {
        instance = JSON.parse(new TextDecoder().decode(await c.openRecord(p.aes, old.kind, old.id, old.data))).instance || "";
      } catch (e) {
        // Unknown library.
      }
    }
    const contentBytes = c.unbase64url(contentKey);
    await store.write(p, "shelf_item", await c.shelfId(p.mac, contentBytes), {
      authors: (summary.authors || []).join(", "),
      book_key: null,
      content_key: contentKey,
      format_version: c.FORMAT_VERSION,
      instance: typeof instance === "string" ? instance : "",
      kind: "file",
      publication: id,
      title: summary.title || "",
      updated_at: Math.floor(Date.now() / 1000),
    });
    // Marks deleted when the book left the shelf come back with it.
    await store.mergeSnapshot(p, usable.filter((r) => r.kind !== "shelf_item"), { revive: true });
  }
  return { publication: id, contentKey };
}

// ---- Writing book files on the device (Phase 31: the Showcase's Download book file) --

const encoder = new TextEncoder();

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) {
      c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    }
    table[n] = c >>> 0;
  }
  return table;
})();

function crc32(data) {
  let c = 0xffffffff;
  for (let i = 0; i < data.length; i++) {
    c = CRC_TABLE[(c ^ data[i]) & 0xff] ^ (c >>> 8);
  }
  return (c ^ 0xffffffff) >>> 0;
}

/**
 * A book file of `entries` (`[{ name, data: Uint8Array }]`: `mimetype`, `sg.json`,
 * then the blobs, the manifest first) as a Blob, in the layout of the instance's
 * writer (src/hub_client/book_file.rs `write`): stored, no extra fields, the time of
 * 1980-01-01, made on Unix with mode 0644. The same entries give the same bytes.
 */
export function writeBookFile(entries) {
  const parts = [];
  const central = [];
  let offset = 0;
  for (const { name, data } of entries) {
    const nameBytes = encoder.encode(name);
    const crc = crc32(data);
    const local = new DataView(new ArrayBuffer(30));
    local.setUint32(0, 0x04034b50, true);
    local.setUint16(4, 10, true);
    local.setUint16(6, 0, true);
    local.setUint16(8, 0, true);
    local.setUint16(10, 0, true);
    local.setUint16(12, 33, true);
    local.setUint32(14, crc, true);
    local.setUint32(18, data.length, true);
    local.setUint32(22, data.length, true);
    local.setUint16(26, nameBytes.length, true);
    local.setUint16(28, 0, true);
    parts.push(new Uint8Array(local.buffer), nameBytes, data);
    const header = new DataView(new ArrayBuffer(46));
    header.setUint32(0, 0x02014b50, true);
    header.setUint16(4, 0x030a, true);
    header.setUint16(6, 10, true);
    header.setUint16(8, 0, true);
    header.setUint16(10, 0, true);
    header.setUint16(12, 0, true);
    header.setUint16(14, 33, true);
    header.setUint32(16, crc, true);
    header.setUint32(20, data.length, true);
    header.setUint32(24, data.length, true);
    header.setUint16(28, nameBytes.length, true);
    header.setUint16(30, 0, true);
    header.setUint16(32, 0, true);
    header.setUint16(34, 0, true);
    header.setUint16(36, 0, true);
    header.setUint32(38, 0x81a40000, true);
    header.setUint32(42, offset, true);
    central.push(new Uint8Array(header.buffer), nameBytes);
    offset += 30 + nameBytes.length + data.length;
  }
  const size = central.reduce((n, part) => n + part.length, 0);
  const end = new DataView(new ArrayBuffer(22));
  end.setUint32(0, 0x06054b50, true);
  end.setUint16(8, entries.length, true);
  end.setUint16(10, entries.length, true);
  end.setUint32(12, size, true);
  end.setUint32(16, offset, true);
  return new Blob([...parts, ...central, new Uint8Array(end.buffer)], { type: MIME });
}

/** The file name of a book file, as the library names it (`Authors - Title.sgbook`). */
export function fileName(title, authors) {
  const clean = (s) => [...(s || "")].map((ch) => (/[\u0000-\u001f\u007f-\u009f/\\:*?"<>|]/.test(ch) ? "_" : ch)).join("")
    .split(/\s+/).filter(Boolean).join(" ");
  const t = clean(title);
  const a = clean(authors);
  let stem = !t ? "book" : a ? `${a} - ${t}` : t;
  while (encoder.encode(stem).length > 200) {
    stem = [...stem].slice(0, -1).join("");
  }
  stem = stem.replace(/[. ]+$/, "") || "book";
  return `${stem}.sgbook`;
}

/**
 * The book file of publication `id` of account `p`, made here from the hub's blobs
 * (through the worker's cache) with this person's records of the book as its
 * snapshot, as the library makes it: `{ blob, name }`. Throws a BookFileError with a
 * message for the reader.
 */
export async function exportPublication(p, id) {
  const m = await wasm();
  const sealed = await store.blob(p, id, "manifest");
  if (!sealed) {
    throw new BookFileError("This book is no longer on the hub.");
  }
  const header = c.keyHeader(sealed);
  const libraryKey = store.libraryKey(p, header.key_id);
  if (header.key_scope !== c.LIBRARY_SCOPE || !libraryKey) {
    throw new BookFileError("This book needs a newer library key: connect to the internet and try again.");
  }
  const bookKey = await c.bookKeyFromHeader(header, libraryKey, id);
  const reader = new m.Reader(await c.openBlob(bookKey, c.unhex(id), "manifest", sealed));
  const summary = JSON.parse(reader.summary());
  reader.free();
  const blobs = [{ name: "manifest", data: sealed }];
  const paths = [
    ...summary.parts.map((_, n) => `p/${n}`),
    ...summary.images.map((_, n) => `i/${n}`),
  ];
  for (const path of paths) {
    const data = await store.blob(p, id, path);
    if (!data) {
      throw new BookFileError("This book is no longer on the hub.");
    }
    blobs.push({ name: path, data });
  }
  const contentKey = c.unbase64url(summary.content_key);
  const snapshot = await store.bookRecords(p, contentKey);
  const sg = {
    format: CONTAINER_VERSION,
    mime: MIME,
    format_version: summary.format_version,
    publication: id,
    library: header.library,
    key_scope: header.key_scope,
    key_id: header.key_id,
    account: p.account,
    snapshot,
  };
  const blob = writeBookFile([
    { name: "mimetype", data: encoder.encode(MIME) },
    { name: "sg.json", data: encoder.encode(JSON.stringify(sg)) },
    ...blobs,
  ]);
  return { blob, name: fileName(summary.title, summary.authors.join(", ")) };
}
