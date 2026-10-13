# shelfgauge — Architecture

> Status: v1 implemented (scanner, storage, web server, library and reading mode,
> hardening). Annotations are designed (planned for 0.6.0), not built yet. The V2
> offline reader below is not.

`shelfgauge` is a single-binary Rust app that indexes a directory tree of ebook files (FB2 in
v1, plain files and `.zip` archives with any number of books inside) into a local SQLite DB
and serves a browsable, server-rendered web library on a fixed `localhost` port. License: MIT.
Key design points to keep in mind when implementing:

- **Everything lives under `.shelfgauge/`** inside the scanned directory (config, SQLite DB,
  cover cache, parsed-content cache) — the library must stay self-contained and portable
  with the books it indexes.
- **Config** is TOML at `.shelfgauge/config.toml`, created with defaults on first run.
  CLI flags override it. The port is fixed (default `8742`): if it is busy, exit with a
  clear error — never silently fall back to another port (a changing port changes the
  browser origin).
- **Scanning runs on every start, in the background**: the server starts immediately and
  serves the existing index while the UI shows progress. Also: a "Rescan" button,
  `--no-scan`, and a `shelfgauge scan` subcommand (index and exit). One scan runs at a
  time on its own thread (`scanner::BackgroundScan`). Ctrl+C stops accepting requests,
  cancels the scan and waits until the writer has committed what it received; a second
  Ctrl+C exits at once.
- **Scanner is incremental and crash-resistant**: check `(size, mtime)` before hashing;
  for `.zip` archives compare per-entry `(size, CRC32)` from the central directory so only
  changed entries are re-parsed; catch per-book parse failures so one broken book never
  aborts a scan (it's recorded with an error status). Zip entries are identified by raw
  name bytes (old archives use CP866 names without the UTF-8 flag; display names are
  decoded UTF-8 → CP866 → windows-1251 → CP437). Nested archives are
  skipped. Removed books get status `removed` instead of being deleted.
  An archive's entries are parsed in parallel jobs, at most `threads × 4` per archive:
  every job opens the archive and parses its central directory, so many small jobs on a
  huge archive would be quadratic.
- **Parser upgrades reach failed books**: each book row records the `PARSER_VERSION`
  that wrote it (`books.parser_version`, schema v2). A book that failed with an older
  parser is parsed again by the next scan even if its file is unchanged; books that
  parsed are left alone.
- **Scan safety**: an archive's real `(size, mtime)` is committed only after all of its
  books (a sentinel mtime marks it unfinished), a plain file's container and book are
  committed together, so an interrupted scan is completed by the next one. Books are
  marked `removed` only after a complete, uncancelled walk, never below an unreadable
  directory, for an archive that cannot be listed, or after a transient read error.
  Symlinks are not followed; non-UTF-8 paths are skipped with a warning.
- **Moved/renamed files** are re-matched during a scan: plain files by `content_hash`, zip
  entries by `(size, CRC32)` (before any decompression); a unique match keeps the book id
  and updates its location, ambiguous matches create new rows. The content hash is
  xxh3-128 and is stored on the book row, so it stays valid when a path is reused. Moves
  between a plain file and an archive are not matched (new id). Opening a book whose file
  is gone shows a "rescan" message, not a server error.
- **SQLite access**: WAL mode; the scanner parses in parallel (`rayon`) and sends results
  to a single writer thread that writes in batched transactions (plain threads, not
  async); web handlers use a small read-connection pool (`r2d2_sqlite`) and
  `spawn_blocking`. Search uses FTS5 (`unicode61`, prefix queries; `unicode61` does not
  fold `ё`, so indexed text and queries map `ё` → `е`; no stemming in v1). Paths are
  stored relative to the library root as `/`-separated TEXT; schema changes are
  append-only migrations (`PRAGMA user_version`; local indexes already exist).
- **Parser threads need a large stack**: `roxmltree` recurses once per XML nesting level
  and a stack overflow aborts the whole process, so the FB2 parser rejects nesting deeper
  than 256 levels, and every thread that runs a parser (rayon pool, blocking workers) is
  created with `format::PARSER_STACK_SIZE` (16 MB) of stack.
- **FB2 encoding detection** before XML parsing: BOM → UTF-16 pattern → XML declaration
  (`encoding_rs`) → fallback (valid UTF-8, else `chardetng`). The declaration is not
  trusted blindly: it is overridden when the bytes contradict it. `roxmltree` ignores the
  declared encoding, so the decoded text is parsed as is.
- **Unbalanced tags are repaired**: when strict parsing fails only on the tag structure
  (`UnexpectedCloseTag`, `UnclosedRootNode`, `UnexpectedEndOfStream`), the tags are
  rebalanced (stray close tags dropped, unclosed elements closed; comments, CDATA and
  attribute values untouched) and the text is parsed again under the same limits.
- **Robustness tests**: `tests/mutation.rs` is an in-tree mutation fuzzer (seeded, no
  extra dependencies) that runs corrupted FB2 files through parse → split → render and
  corrupted zip archives through listing and reading; long runs via
  `SHELFGAUGE_FUZZ_ITERATIONS`.
- **Format abstraction is two layers, and must stay that way as formats are added**:
  - *Container layer*: resolves raw bytes to parse (bare file vs. one entry of a `.zip`),
    independent of ebook format.
  - *Format layer*: a `BookParser` trait (one impl per format — only FB2 in v1) producing a
    common `BookMetadata` / `BookContent` representation. The viewer must only depend on
    `BookContent`/`BookMetadata`, never on format-specific details.
  - Fixed-page-layout formats (PDF/DjVu) are explicitly out of scope.
- **UI is server-rendered** (askama templates, hand-written CSS/JS, assets embedded in the
  binary; no SPA, no Node). Layout is inspired by mdBook: collapsible left sidebar + main
  area + top bar + color themes. **Nothing is copied from mdBook** (MPL-2.0) — only the
  idea; no bundled third-party fonts/icon fonts.
  - *Library mode*: sidebar groups books by author or by folder (switchable; a `.zip` is a
    virtual folder). Sidebar is paginated/lazy, never rendered in full: each level is a
    server-rendered HTML fragment (`/sidebar/...`) fetched when a node is expanded, and
    every page renders the path to its author/folder/book already expanded (askama has
    no recursion, so levels are rendered bottom-up into strings). Main area shows the
    book card; every page also works without JavaScript.
  - The sidebar mode is a cookie (the server must know it to render the sidebar;
    cookies ignore the port, so libraries on one host share it); the theme is in
    `localStorage`, applied by a small blocking script in `<head>` (the CSP allows no
    inline scripts or styles, and all URLs reach the scripts through `data-` attributes
    so `base_path` keeps working).
  - Scan progress is polled (`/scan/status`, JSON), not streamed: an open SSE
    connection would hold up the graceful shutdown.
  - *Reading mode*: sidebar shows the book's TOC; main area shows one part of the book as
    HTML, rendered from `BookContent` (never directly from FB2). Splitting into parts is
    configured in `[reader]` (`none` / `section` / `size`, cut at block boundaries).
    A part is a contiguous range of block IDs, cut only between whole top-level blocks
    of a section (a poem or cite is never split); `section` cuts at sections up to
    `section_depth`, merges title-only parts and small untitled parts (front matter)
    into the next one and never splits a notes body per note. In every mode a part
    longer than `max_part_size` characters is cut into equal pieces ("Title (1/3)").
    `?at=b<n>` redirects to the part URL with `#b<n>`.
  - Content comes from `.shelfgauge/content/` (keyed by the indexed content identity:
    a plain file's hash, a zip entry's size + CRC32), else the book is read, verified
    against that identity (a mismatch asks for a rescan) and parsed; loading, splitting
    and rendering run on a thread with the parser stack. A few parsed books are kept in
    memory for part changes and images. Source-format anchors are resolved to block
    IDs and never emitted as HTML ids.
  - **Block IDs** are the block's ordinal in document order (`b1234`), counting every
    block (the exact rule is documented in `src/format/model.rs` and pinned by a golden
    test). Each FB2 `<body>` is a root section (main body first, then notes bodies). They must be deterministic and **must not depend on the split setting**; deep
    links address a block (`?at=<block-id>`) and the server resolves the part. Cached
    `BookContent` carries a `parser_version`; bumping it invalidates the cache.

## Annotations (planned for 0.6.0)

- **One entity**, the annotation (UI: "Mark"), replaces bookmarks, notes and quotes: the
  **selected text is immutable** (it is the quote and the anchor's fingerprint), a
  separate **note** is editable and optional (bookmark name / note body). An annotation
  with no selection is a **bookmark** on the topmost visible block (note optional; the
  automatic label is the section title and the start of the block). No `kind` field:
  bookmarks (empty text) and marks (non-empty text) are one model shown as two lists.
- **The server owns annotations.** They live in a separate SQLite file,
  `.shelfgauge/userdata.sqlite` (own migrations), not in the index DB: the index can be
  rebuilt by rescanning, user data cannot, and the scanner's single writer stays alone.
- **Anchors**: book content key (the same identity as the content cache: plain file hash,
  zip entry size + CRC32), block range with code point offsets into each block's
  rendered text, plus the selected text with a short prefix/suffix as fingerprint. A book
  is matched by content key, so annotations survive rescans and moves that keep the
  content; when the content changes they are kept as orphans, never deleted, and are
  re-attached lazily when a matching book (same `book_id` hint, or same title and
  authors) is opened and at least 80 % of their texts are found in it; the rest are
  marked "place not found". Annotations of a book removed from the library are
  soft-deleted by a cleanup step after every complete, uncancelled scan (unless a book
  with the same title and authors exists), restored if the book comes back, and purged
  after `removed_book_retention_days` (default 30). There is no orphan management UI.
  The stored `book_id` is only a hint. A selection lies within one part; text and note lengths are
  limited by `[annotations]` in the config (`max_text_length`, `max_note_length`,
  10000 characters by default).
- Ids are UUIDs and deletion is soft (`deleted_at`), so the V2 offline reader can sync
  into this table later.
- **UI**: `reader.js`/`reader.css` load only on read pages; selecting text offers "Mark";
  saved annotations are highlighted on the client; a bookmark is one action (button or
  `B`). The reader sidebar has tabs Contents | Bookmarks | Marks; the book card lists
  bookmarks and marks in two `<details>` blocks with plain-form edit/delete.
  Lists are server-rendered and work without JS. Writes are a separate route group
  behind the Origin check.
- **Export**: one Markdown file per book in `.shelfgauge/annotations/`, regenerated
  atomically from the DB after every change, **one way only** (edits to the file are not
  read back): bookmarks first, then quotes grouped by section, notes, and `?at=b<n>`
  links.
  `shelfgauge annotations export` rewrites all files; generated files carry a marker
  comment and only marked files are ever removed.
- Reader settings (font from system stacks, size, background, width, line height,
  alignment) are purely client-side (CSS custom properties + `localStorage`). The reader
  is not a separate crate.

## Security

v1 binds to `127.0.0.1` only, but is built to allow remote access later:

- **Host header check** against `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`,
  the bind address and `allowed_hosts` (protection against DNS rebinding); it runs as the
  outermost layer, so it also covers assets and unknown paths. An `allowed_hosts` entry
  without a port matches any port (reverse proxy on 443), one with a port matches
  exactly.
- **Origin check** (CSRF): every request with a method other than `GET`/`HEAD`/
  `OPTIONS`/`TRACE` needs an `Origin` whose authority is on the same allowlist (not
  compared with the request's `Host`, which a reverse proxy may rewrite); a missing or
  `null` Origin is rejected.
- If `bind` is not a loopback address and no authentication is configured, the program
  refuses to start.
- Routes are split into a *read* group and an *admin* group (scan, settings), with an
  auth middleware slot; the admin group may stay localhost-only.
- All routes and URLs are built with a configurable `base_path` (reverse proxy on a
  subpath) through one helper (`web::Urls`); `base_path` is limited to unreserved URL
  characters so it is safe in router paths and links.
- Every response carries a strict `Content-Security-Policy` (own scripts/styles only,
  no inline code), `nosniff`, `X-Frame-Options: DENY` and `Referrer-Policy`.
- Files are only ever served by id, never by a client-supplied path. Error messages
  shown in the UI use library-relative paths, never absolute server paths. Covers are served
  only for known image types with a canonical `Content-Type` (the type stored from the
  book file is never echoed, so an SVG "cover" with a script is not served).
- No built-in TLS: remote access goes through a reverse proxy with HTTPS or a private
  network (Tailscale / WireGuard / SSH tunnel).

## V2 (deferred — do not build in v1)

Offline-first reader: `/reader/<token>` reading links (with revocation), Service Worker +
Cache Storage so a tab keeps working without the server, reading position and offline
annotations in IndexedDB (same anchor model as above), synced into the server-owned
annotations when the server is reachable. Depends on the fixed port, since browser
storage is per origin, and on a secure context (localhost or HTTPS). Sync conflict rules
and per-user data are decided in V2.

## Planned tech stack

| Concern | Choice |
|---|---|
| Language / runtime | Rust (edition 2024), Tokio |
| Web framework | axum |
| Templates | askama |
| Static assets | embedded in the binary (`rust-embed`, also in debug builds) |
| Database | rusqlite (SQLite, WAL) + FTS5 |
| DB access from async | read pool (`r2d2_sqlite`) + `spawn_blocking` |
| Config | serde + toml |
| FB2 XML parsing | roxmltree (read-only DOM) |
| Encoding detection | encoding_rs (+ chardetng fallback) |
| Directory walking | walkdir + ignore |
| Archives | `zip` crate (`.zip` only) |
| Parallel parsing | rayon |

## Out of scope for v1

The V2 reader (above), deduplication across the library, RAR/other archive formats, nested
archives, multi-root libraries.
