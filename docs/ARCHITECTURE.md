# shelfgauge — Architecture

> Status: design, not yet implemented. This document describes the target architecture
> for v1; details may change as implementation proceeds.

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
  `--no-scan`, and a `shelfgauge scan` subcommand (index and exit).
- **Scanner is incremental and crash-resistant**: check `(size, mtime)` before hashing;
  for `.zip` archives compare per-entry `(size, CRC32)` from the central directory so only
  changed entries are re-parsed; catch per-book parse failures so one broken book never
  aborts a scan (it's recorded with an error status). Zip entries are identified by raw
  name bytes (old archives use CP866 names without the UTF-8 flag). Nested archives are
  skipped. Removed books get status `removed` instead of being deleted.
- **Moved/renamed files** are re-matched during a scan: plain files by `content_hash`, zip
  entries by `(size, CRC32)`; a unique match keeps the book id and updates its location,
  ambiguous matches create new rows. Opening a book whose file is gone shows a "rescan"
  message, not a server error.
- **SQLite access**: WAL mode; the scanner parses in parallel (`rayon`) and sends results
  to a single writer thread that writes in batched transactions (plain threads, not
  async); web handlers use a small read-connection pool and `spawn_blocking`. Search uses
  FTS5 (`unicode61`, prefix queries; no stemming in v1).
- **Parser threads need a large stack**: `roxmltree` recurses once per XML nesting level
  and a stack overflow aborts the whole process, so the FB2 parser rejects nesting deeper
  than 256 levels, and every thread that runs a parser (rayon pool, blocking workers) is
  created with `format::PARSER_STACK_SIZE` (16 MB) of stack.
- **FB2 encoding detection** before XML parsing: BOM → UTF-16 pattern → XML declaration
  (`encoding_rs`) → fallback (valid UTF-8, else `chardetng`). The declaration is not
  trusted blindly: it is overridden when the bytes contradict it. `roxmltree` ignores the
  declared encoding, so the decoded text is parsed as is.
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
    virtual folder). Sidebar is paginated/lazy, never rendered in full. Main area shows
    the book card.
  - *Reading mode*: sidebar shows the book's TOC; main area shows one part of the book as
    HTML, rendered from `BookContent` (never directly from FB2). Splitting into parts is
    configured in `[reader]` (`none` / `section` / `size`, cut at block boundaries).
  - **Block IDs** are the block's ordinal in document order (`b1234`), counting every
    block (the exact rule is documented in `src/format/model.rs` and pinned by a golden
    test). Each FB2 `<body>` is a root section (main body first, then notes bodies). They must be deterministic and **must not depend on the split setting**; deep
    links address a block (`?at=<block-id>`) and the server resolves the part. Cached
    `BookContent` carries a `parser_version`; bumping it invalidates the cache.

## Security

v1 binds to `127.0.0.1` only, but is built to allow remote access later:

- **Host header check** against `127.0.0.1:<port>`, `localhost:<port>` and
  `allowed_hosts` (protection against DNS rebinding); state-changing requests also require
  a matching `Origin` (CSRF).
- If `bind` is not a loopback address and no authentication is configured, the program
  refuses to start.
- Routes are split into a *read* group and an *admin* group (scan, settings), with an
  auth middleware slot; the admin group may stay localhost-only.
- All URLs are built with a configurable `base_path` (reverse proxy on a subpath).
- Files are only ever served by id, never by a client-supplied path.
- No built-in TLS: remote access goes through a reverse proxy with HTTPS or a private
  network (Tailscale / WireGuard / SSH tunnel).

## V2 (deferred — do not build in v1)

Offline-first reader: `/reader/<token>` reading links (with revocation), Service Worker +
Cache Storage so a tab keeps working without the server, reading position / bookmarks /
quotes in IndexedDB, and one-directional additive backup sync client → server (server
shows them read-only). Depends on the fixed port, since browser storage is per origin, and
on a secure context (localhost or HTTPS). Anchors combine the block ID with a short text
fingerprint so they can be re-resolved when a book file is replaced. Sync details are
decided in V2.

## Planned tech stack

| Concern | Choice |
|---|---|
| Language / runtime | Rust (edition 2024), Tokio |
| Web framework | axum |
| Templates | askama |
| Static assets | embedded in the binary (`rust-embed` or similar) |
| Database | rusqlite (SQLite, WAL) + FTS5 |
| DB access from async | read pool (`deadpool-sqlite` or `r2d2_sqlite`) + `spawn_blocking` |
| Config | serde + toml |
| FB2 XML parsing | roxmltree (read-only DOM) |
| Encoding detection | encoding_rs (+ chardetng fallback) |
| Directory walking | walkdir + ignore |
| Archives | `zip` crate (`.zip` only) |
| Parallel parsing | rayon |

## Out of scope for v1

The V2 reader (above), deduplication across the library, RAR/other archive formats, nested
archives, multi-root libraries.
