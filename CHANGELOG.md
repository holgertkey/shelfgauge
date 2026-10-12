# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Build numbers (`+NNN`) do not get their own sections.

## [Unreleased]

## [0.4.0] - 2026-10-12

### Added

- Reading mode: `/book/<id>/read` shows one part of a book as semantic HTML (sections,
  titles, epigraphs, poems, cites, tables, images, notes), with prev/next links, `←`/`→`
  keys, reading typography and the book's table of contents in the sidebar (entries of
  the current part highlighted). The book card has a Read button.
- Splitting into parts (`render::split`) per `[reader]`: `none`, `section` (sections up
  to `section_depth`; parts of only titles and small untitled parts such as a
  copyright notice or an epigraph join the next part) or `size` (about `part_size`
  characters). Parts
  are cut only between whole blocks, every block is in exactly one part, and a notes
  body is never split per note.
- Deep links: `?at=b<n>` redirects to the part with that block and scrolls to it;
  every rendered block has `id="b<n>"` independent of the split setting.
- HTML rendering (`render::html`): all text escaped; internal links resolved to block
  IDs (source anchors never become HTML ids); external links only for `http`, `https`
  and `mailto`; table alignment by classes.
- Content loading (`render::load`): parsed content is cached in
  `.shelfgauge/content/` keyed by the indexed content identity; a file changed or gone
  since the last scan shows the "rescan" page, an unparsable book a "cannot read" page
  with the reason. The last few books are also kept in memory.
- Book images at `/book/<id>/image/<image-id>`, only for known raster types, with
  immutable caching per book version.

## [0.3.0] - 2026-10-12

### Added

- Library mode UI: an mdBook-like layout with a top bar (search, theme, rescan), a
  collapsible, responsive sidebar and the main area; hand-written CSS with a system
  font stack, own inline SVG icons and five themes (auto, light, dark, sepia, navy)
  remembered per browser.
- Sidebar by author (alphabetical, paged by `library.page_size`, filtered by name
  prefix; an author expands to their books grouped by series) or by folder (lazy
  directory tree, zip archives as folders), switched with a tab or `m` and remembered
  in a cookie. Levels are loaded as server-rendered fragments; the current author,
  folder or book is revealed and highlighted on every page.
- Pages: library overview, author index, author, folder (with breadcrumbs), book card
  (cover, authors, series, genres, language, location, annotation; error, removed and
  missing-file notices with a rescan button) and full-text search with paging. Every
  page works without JavaScript.
- Covers served by book id from the cache, only for known image types, with immutable
  caching per book version.
- Scan progress banner: polled from `GET /scan/status` (JSON) while a scan runs;
  Rescan starts a scan in place.
- Keyboard shortcuts: `/` or `s` search, `t` next theme, `m` sidebar mode, `Esc`.
- Storage: `count_books`, `author_by_id`, `author_index`, `folder_book_index`; folder
  listings page their books.
- Web server (`web`): axum on a tokio runtime; `shelfgauge [PATH]` binds the fixed port
  (a busy port is a clear error), serves the index through the read pool and scans in
  the background (`--no-scan` to skip); Ctrl+C shuts down gracefully, letting the scan
  commit its current batch (a second Ctrl+C exits at once).
- Security: `Host` allowlist (`localhost`, `127.0.0.1`, `[::1]` with the port, the bind
  address, `server.allowed_hosts`) against DNS rebinding; an allowed `Origin` is
  required for every state-changing request; a non-loopback `bind` is refused while
  there is no authentication; read/admin route groups with an authentication slot;
  CSP, `nosniff`, `X-Frame-Options` and `Referrer-Policy` headers.
- `server.base_path` applied to all routes and generated URLs through one URL helper.
- Embedded assets (`rust-embed`) with content-hash URLs, `ETag`/304 and immutable
  caching; askama base layout (top bar, sidebar, main area) and error pages (404, 500,
  "file not found — rescan").
- `POST /scan` (admin) starts a rescan; `scanner::BackgroundScan` runs one scan at a time
  on a background thread.

### Changed

- `server.base_path` accepts only ASCII letters, digits, `-`, `.`, `_`, `~` and `/`.
- Authors with the same sort key are ordered by id, so author pages are stable.

## [0.2.0] - 2026-10-11

### Changed

- A new library (`.shelfgauge/`) is created only when PATH is given explicitly
  (`shelfgauge .`, `shelfgauge scan .`); without PATH, the current directory must
  already be a library, so `shelfgauge -v` in an arbitrary directory no longer creates
  `.shelfgauge/config.toml` there.

### Added

- Format-independent book model (`format::model`): `BookMetadata` (title, authors,
  series list with a verbatim index, genres, annotation, language, cover) and
  `BookContent` (bodies → section tree → blocks, images by id), with inline markup and
  links. Every block gets a deterministic `BlockId` (`b<n>`) in document order; the
  numbering rule is documented and pinned by a golden test. `PARSER_VERSION` = 1.
- `BookParser` trait and an extension → parser `Registry` (case-insensitive).
- FB2 encoding detection (`format::fb2::encoding`): BOM → UTF-16 pattern → XML
  declaration → valid UTF-8 → `chardetng` guess. Lying declarations are handled
  (declared UTF-8 but invalid → guessed; declared legacy but valid non-ASCII UTF-8 →
  UTF-8); mostly-valid UTF-8 with a few stray bytes stays UTF-8 (with replacement
  characters) instead of being misread as a legacy encoding; malformed input is reported
  instead of failing.
- FB2 parser (`format::fb2::Fb2Parser`, registered by `Registry::with_builtin_formats`):
  `<title-info>` metadata (title, authors, nested sequences, genres, annotation, language,
  cover) and full content (all bodies and FB2 block/inline elements, binaries with
  lenient base64 and MIME sniffing). Elements are matched by local name; unknown
  elements keep their text; trailing content after subsections is wrapped into an
  untitled section; a `<body>` image before the title is allowed. Never panics: non-XML/non-FB2 input, DTDs and nesting deeper than
  256 levels are errors; nesting beyond 64 levels is flattened to text.
- `format::PARSER_STACK_SIZE` (16 MB) and `run_with_parser_stack`: threads running
  parsers need this much stack.
- Container layer (`container`): `BookLocation` (path relative to the library root +
  optional zip entry identified by raw name bytes, index, size, CRC32) and `read_book`.
  Zip archives are listed from the central directory without decompression; entry
  display names are decoded as UTF-8, CP866, windows-1251 or CP437; directories,
  encrypted entries and nested archives are skipped. Reads check the recorded size and
  CRC32 (changed/missing entries report `needs_rescan`), stop at the declared size and
  refuse books over `ReadLimits::max_book_size` (256 MiB by default). Supported
  compression: stored, deflate, deflate64, bzip2, LZMA (all pure Rust).
- SQLite storage (`storage`, bundled SQLite via `rusqlite`): WAL mode (falls back to
  the rollback journal with a warning where WAL is unavailable), foreign keys, busy
  timeout; schema migrations via `PRAGMA user_version` (newer schemas are refused);
  read-only connection pool (`r2d2_sqlite`) for the web layer. Schema v1: containers
  (path, kind, size, mtime, content hash), books (location, metadata, cover MIME,
  status `ok`/`error`/`removed`), authors (de-duplicated by a normalized name key),
  series, genres and an FTS5 index (`unicode61`, prefix indexes, `ё` folded to `е`).
  Queries: idempotent book upsert, removal and relocation, matcher lookups by content
  hash and by entry size + CRC32, paged author list with prefix filter, books by
  author grouped by series, folder tree with zip archives as virtual folders, book
  details and full-text search with safe query building.
- Scanner (`scanner`) and `shelfgauge scan`: walks the library (symlinks not followed,
  `.shelfgauge/` and `[scan].exclude` gitignore patterns skipped, non-UTF-8 paths skipped
  with a warning), skips unchanged files and archives by `(size, mtime)`, lists changed
  archives from their central directory and re-parses only changed entries, parses in
  parallel (rayon, `PARSER_STACK_SIZE` stacks, panics caught per book) and writes
  through a single writer thread in batched transactions. Moved/renamed books keep
  their id: zip entries are matched by `(size, CRC32)` before decompression, plain
  files by an xxh3-128 content hash; ambiguous matches create new books. Crash- and
  error-resistant: archives are finalized only after all their books are written,
  unreadable directories/archives and transient read errors never remove books, a
  cancelled scan removes nothing. Progress counters and a summary (added, updated,
  moved, removed, unchanged, parsed, errors).
- Cover cache (`.shelfgauge/covers/<id>.<ext>`, atomic writes) and a parsed-content
  cache API (`.shelfgauge/content/`, postcard, keyed by content identity and
  `PARSER_VERSION`).

## [0.1.0] - 2026-10-11

### Added

- Library + binary crate layout (`src/lib.rs` engine with `#![warn(missing_docs)]`,
  thin `src/main.rs`).
- Base dependencies: `anyhow`, `thiserror`, `tracing`, `tracing-subscriber`, `clap`.
- Minimal CLI with `--version` and `--help`.
- `build.rs` version sync: the version from `Cargo.toml` is written into `README.md`
  and a missing `CHANGELOG.md` section header is added. The `+BUILD` counter is a local
  development aid: only debug builds show it in `--version`.
- `README.md` and `CHANGELOG.md`.
- GitHub Actions CI (fmt, clippy, test, doc) on Linux, Windows and macOS.
- Library root: positional `PATH` argument (default: the current directory); working
  data lives in `.shelfgauge/` inside it.
- Config file `.shelfgauge/config.toml` with `[server]`, `[scan]`, `[library]` and
  `[reader]` sections, created with commented defaults on first run. Missing keys use
  defaults, unknown keys are logged as warnings, invalid values fail with an error
  naming the key; `server.base_path` is normalized.
- CLI: `shelfgauge [PATH]` (serve), `shelfgauge scan [PATH]`, `--port`, `--bind`,
  `--no-scan` and `-v`/`-vv`; CLI values override the config file. Serve and scan are
  stubs that print the effective configuration.
- Logging to stderr via `tracing`; level from `-v` flags, else `RUST_LOG`.
