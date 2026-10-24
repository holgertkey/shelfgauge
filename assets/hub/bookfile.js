// Opening book files (`.sgbook`, concept §11 "Book files") on a paired device.
//
// The file is untrusted: the WASM module (the instance's container reader) checks its
// layout from the first bytes, the end record and the central directory only, and the
// entries are cut out of the File by offset, so a large file never sits in memory.
// Then, in this order and before anything is decrypted: sg.json's versions, the
// account (another account's file is refused without trying its key) and the key id;
// only then the book key (sealed under the account key) and the manifest. Every check
// works offline: nothing here calls the hub. The copy is kept sealed in IndexedDB
// (store.js "copies" and "files") and opened like a hub publication.
//
// The Rust reference of these checks is src/hub_client/book_file.rs (`open`).
import * as c from "./crypto.js";
import * as store from "./store.js";

const CONTAINER_VERSION = 1;
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
    && typeof sg.account === "string" && /^[0-9a-f]{32}$/.test(sg.account)
    && typeof sg.book_key === "string" && typeof sg.mime === "string"
    && (sg.snapshot === undefined || Array.isArray(sg.snapshot));
  if (!fine) {
    throw new BookFileError("The book file is damaged (its sg.json is incomplete).");
  }
}

/**
 * Checks book file `file` (a File or Blob) for pairing `p` and keeps a copy on this
 * device. Returns `{ publication, contentKey }`. Throws a BookFileError with a
 * message for the reader.
 */
export async function importFile(p, file) {
  const { entries, sg } = await layout(file);
  checkFields(sg);
  if (sg.format !== CONTAINER_VERSION || sg.format_version !== c.FORMAT_VERSION) {
    throw new BookFileError("This book file needs a newer version of the reader. Update shelfgauge on your hub, then open the file again.");
  }
  if (sg.mime !== MIME) {
    throw new BookFileError("The book file is damaged (another media type).");
  }
  if (sg.account !== p.account) {
    throw new BookFileError("This book file belongs to another account. Books open only on devices paired with the library they come from.");
  }
  if (sg.key_id !== (p.keyId || 1)) {
    throw new BookFileError("This book file was made with another key of your account. Pair this device again in your library, then open the file again.");
  }
  // From here on the file is the account's own (or forged without its key, which
  // fails to open below).
  const id = sg.publication;
  let raw;
  try {
    raw = await c.openRecord(p.aes, "file_key", id, c.unbase64url(sg.book_key));
  } catch (e) {
    raw = null;
  }
  if (!raw || raw.length !== 32) {
    throw new BookFileError("The book file is damaged (its key does not open).");
  }
  const key = await c.aesKey(raw);
  const slice = (name) => {
    const e = entries.get(name);
    return e ? file.slice(e.offset, e.offset + e.size) : null;
  };
  const manifestBlob = slice("manifest");
  if (!manifestBlob) {
    throw new BookFileError("The book file is damaged (no manifest).");
  }
  let summary;
  try {
    const m = await wasm();
    const reader = new m.Reader(await c.openBlob(key, c.unhex(id), "manifest", await bytes(manifestBlob)));
    summary = JSON.parse(reader.summary());
    reader.free();
  } catch (e) {
    raw.fill(0);
    throw new BookFileError("The book file is damaged (its contents do not open).");
  }
  const paths = ["manifest"];
  summary.parts.forEach((_, n) => paths.push(`p/${n}`));
  summary.images.forEach((_, n) => paths.push(`i/${n}`));
  const blobs = [];
  for (const path of paths) {
    const blob = slice(path);
    if (!blob || blob.size > MAX_BLOB) {
      raw.fill(0);
      throw new BookFileError("The book file is damaged (a part is missing).");
    }
    blobs.push({ path, blob });
  }
  const contentKey = summary.content_key;
  await store.saveCopy({
    publication: id,
    content_key: contentKey,
    title: summary.title || "",
    authors: (summary.authors || []).join(", "),
    book_key: sg.book_key,
    imported_at: Date.now(),
  }, blobs);
  // One copy per book: an earlier file of the same book (still openable by opening
  // it again) gives way to this one.
  await store.dropCopies((copy) => copy.content_key === contentKey && copy.publication !== id);
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
  const kinds = new Set(["shelf_item", "annotation", "position"]);
  const usable = snapshot.filter((r) => kinds.has(r.kind));
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
      book_key: c.base64url(raw),
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
  raw.fill(0);
  return { publication: id, contentKey };
}
