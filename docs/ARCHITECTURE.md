# shelfgauge — Architecture

> Status: v1 implemented (scanner, storage, web server, library and reading mode,
> hardening), annotations (0.6.0), reader settings (0.7.0), EPUB (0.8.0), plain text
> and `.fbz` (0.9.0), HTML (0.10.0, opt-in). The V2 hub below is designed, not built.

`shelfgauge` is a single-binary Rust app that indexes a directory tree of ebook files (FB2
(also `.fbz`), EPUB, TXT and opt-in HTML, plain files and `.zip` archives with any number of books inside) into a local SQLite DB
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
  skipped; an `.epub` or `.fbz` inside a `.zip` is a book (a book file whose zip
  structure its parser reads itself, not an archive container; only `.zip` is an
  archive, so `.fb2.zip` stays an ordinary archive). Each archive records the
  formats it was listed for (`containers.listed_formats`, schema v3): when a format is
  added, unchanged archives are listed again (central directory only), so their books of
  the new format are found while known entries are not parsed again. Removed books get status `removed` instead of being deleted.
  An archive's entries are parsed in parallel jobs, at most `threads × 4` per archive:
  every job opens the archive and parses its central directory, so many small jobs on a
  huge archive would be quadratic.
- **Enabled formats**: `[scan] formats` (default `fb2`, `epub`, `txt`; `html` is known
  but opt-in) builds the
  scanner's registry (`Registry::with_formats`; unknown names are a config error).
  Taking a format off the list makes its files invisible to the walk, so their books
  are marked removed (annotations survive by the usual retention); putting it back finds
  them again (moved-book matching keeps their ids), and archives are listed again thanks
  to `listed_formats`.
- **"Not a book" is an outcome, not an error**: a parser returns
  `ParseErrorKind::NotABook` for a file of its format that is not a book (a short text,
  `readme.txt`). The scanner records nothing for it (`ScanSummary::not_books` counts
  it); a plain file's container row is kept without a book, so an unchanged file is not
  read again. A book that later turns out not to be a book is marked removed. Parsers
  get the file name for this (`BookParser::parse_metadata_named`).
- **Parser upgrades reach failed books**: each book row records the `PARSER_VERSION`
  that wrote it (`books.parser_version`, schema v2). A book that failed with an older
  parser is parsed again by the next scan even if its file is unchanged; books that
  parsed are left alone. DRM-protected books (status `protected`, schema v3) are not
  failures and are never parsed again.
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
  append-only migrations (`PRAGMA user_version`; local indexes already exist). Migrations
  run with foreign keys off (a table rebuild would otherwise cascade-delete the rows
  referencing it) and must pass `foreign_key_check` before their commit.
- **Parser threads need a large stack**: `roxmltree` recurses once per XML nesting level
  and a stack overflow aborts the whole process, so the XML-based parsers (FB2, EPUB
  documents) reject nesting deeper than 256 levels, and every thread that runs a parser (rayon pool, blocking workers) is
  created with `format::PARSER_STACK_SIZE` (16 MB) of stack.
- **Encoding detection** (`format::encoding`): BOM → (mostly) valid UTF-8 →
  `chardetng` (windows-1251, CP866 and KOI8 recognized; KOI8-R is reported as KOI8-U,
  which decodes Russian identically). FB2 inserts its own steps: BOM → UTF-16 pattern →
  XML declaration (`encoding_rs`) → fallback (valid UTF-8, else `chardetng`). The declaration is not
  trusted blindly: it is overridden when the bytes contradict it. `roxmltree` ignores the
  declared encoding, so the decoded text is parsed as is.
- **Unbalanced tags are repaired**: when strict parsing fails only on the tag structure
  (`UnexpectedCloseTag`, `UnclosedRootNode`, `UnexpectedEndOfStream`), the tags are
  rebalanced (stray close tags dropped, unclosed elements closed; comments, CDATA and
  attribute values untouched) and the text is parsed again under the same limits.
- **Robustness tests**: `tests/mutation.rs` is an in-tree mutation fuzzer (seeded, no
  extra dependencies) that runs corrupted FB2 files (plain and as `.fbz`), EPUB books
  (mutated documents re-zipped, or a corrupted container) and plain text (mixed
  encodings, control characters, huge lines) through parse → split → render and corrupted zip
  archives through listing and reading; long runs via
  `SHELFGAUGE_FUZZ_ITERATIONS`.
- **Format abstraction is two layers, and must stay that way as formats are added**:
  - *Container layer*: resolves raw bytes to parse (bare file vs. one entry of a `.zip`),
    independent of ebook format.
  - *Format layer*: a `BookParser` trait (one impl per format: FB2, EPUB, TXT, HTML) producing a
    common `BookMetadata` / `BookContent` representation. The viewer must only depend on
    `BookContent`/`BookMetadata`, never on format-specific details.
  - Shared pieces of the format layer: `format::zipbook` (the limited in-memory reader
    of zipped books: entry count, per-entry size and ratio, total size), used by EPUB
    and `.fbz`; `format::encoding` (BOM, in-band declaration checked against the
    bytes, UTF-8, detector); `format::sections` (a flat list of items and cuts → the
    section tree with the rules for titles, used by EPUB, TXT and HTML);
    `format::xhtml` (the XHTML → items converter, notes, anchors and links, the XML
    pre-pass, path resolution; images through a `Resources` trait), used by EPUB and
    HTML.
  - The format set is final: FB2 (+ `.fbz`), EPUB, TXT and (0.10.0, opt-in) HTML.
    Fixed-page-layout formats (PDF/DjVu), MOBI/AZW3, FB3, DOCX/RTF/ODT and comics are
    out of scope.
- **FB2 zipped** (`.fbz`, or a `.fb2` whose bytes start with a zip signature): the FB2
  parser unzips the first `.fb2` entry (else the first entry) with `format::zipbook`.
- **EPUB** (`src/format/epub/`, EPUB 2.0.1 and 3.x): the parser opens the OCF zip from
  the bytes it is given, with its own limits (entries, per-entry size and compression
  ratio, total unpacked size), follows `META-INF/container.xml` to the package document
  and reads metadata (`dc:*`, EPUB 3 refinements and EPUB 2 `opf:` attributes, series
  from `belongs-to-collection` of type `series` or Calibre's meta, cover from
  `cover-image` / `meta name="cover"`; scanning never reads the spine). XHTML goes
  through a pre-pass (DOCTYPE removed, XHTML 1.x named entities made numeric, invalid
  characters dropped, undeclared prefixes declared) before `roxmltree`. The linear
  spine documents become one main body: wrapper elements are not blocks, a run of inline
  content is one paragraph, sections are cut at the navigation document's entries (NCX
  for EPUB 2; headings or spine documents as fallbacks), headings at a cut become the
  section title, else the entry's label is the section's navigation-only title.
  Footnotes (`aside`/elements typed as notes), note collections and non-linear documents
  form a notes body. Every element id is an anchor key `path#id` bound to the first
  block completed after it; links are rewritten to those anchors (unresolvable ones
  become text). CSS, scripts, fonts and remote resources are ignored; only known raster
  images are kept (SVG never). The model gained lists, list items (containers),
  preformatted text, rules and line breaks; FB2 never produces them, so its numbering
  is unchanged.
- **TXT** (`src/format/txt/`): decoded with `format::encoding` (no declaration step),
  line ends normalized, controls dropped (a form feed is a paragraph break). One
  paragraph *style* per file from statistics: *indented* (lib.ru: a paragraph at each
  indented line), *blocks* (Gutenberg: blank-line separated, hard-wrapped lines joined)
  or *lines* (one paragraph per line); runs of short non-prose lines become verse (one
  paragraph with line breaks), `* * *`-like lines rules, paragraphs over 100 000
  characters are split at sentence ends. Heading-like lines (`Часть|Книга|Том|Part|Book|
  Volume` + number: rank 1; `Глава|Chapter` + number, prologues and the like, lone
  numerals when there are 3+: rank 2; set apart by blank lines; a following short line
  is the chapter's name) cut sections through `format::sections`; candidates with no
  text before the next heading of the same or a higher rank are a table of contents and
  stay text. Metadata only from the file name (`Author - Title`); less than 2 000
  characters of text or a name like `readme`/`license` → not a book. Only existing
  block kinds are produced, so FB2 and EPUB numbering are untouched; a golden test pins
  the TXT mapping.
- **HTML** (`src/format/html/`, opt-in): bytes → text (BOM, `<meta charset>` /
  `http-equiv` prescan of the first 1 024 bytes checked against the bytes, UTF-8,
  detector) → `html5ever` into our own arena DOM (`dom.rs`: index-linked nodes, O(1)
  tree operations, no recursion; not `scraper` — selectors and CSS parsing we do not
  need — and not `markup5ever_rcdom`, which is test-only). The text is fed in 64 KB
  chunks and parsing stops with an error when the tree gets deeper than 512 (the HTML
  tree builder scans the open-element stack, so deep nesting is quadratic) or has more
  than 1 M nodes (memory). In `.xhtml` files (XML declaration or XHTML namespace)
  self-closed raw-text elements (`<title/>`, `<script .../>`) are expanded first, and
  the XML declaration's encoding is used when there is no `<meta charset>`. The body
  is serialized as well-formed XHTML (`serialize.rs`: names validated, Word's `o:p`
  unwrapped, only `epub:`/`xlink:` attribute prefixes, `<a name>` → `id`; scripts,
  forms, SVG/MathML, `pagenum` spans and link-dominated `nav`/`header`/`footer` left
  out; nesting below 128 flattened to text) and converted by `format::xhtml` as one
  document without images. A `<pre>` with more than 50 lines or most of the text goes
  through the TXT paragraph and heading rules, its `<h*>` elements as forced headings.
  Section depth = rank of (heading level, part-like keyword) — Gutenberg uses `h2` for
  both books and chapters; a heading right after a higher-ranked one continues its
  title unless it is itself a heading line; a single top-ranked heading that is not
  part-like (the book's title in `<h1>`) starts no section. Metadata from Dublin Core metas,
  `author`/`description` metas, `<html lang>`, else `<title>` patterns. Less than
  5 000 characters of text or more than half link text → not a book. html5ever panics
  are caught in the parser.
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
    longer than `max_part_size` characters is cut into equal pieces ("Title (1/3)"):
    as few as the limit allows, each cut at the block boundary nearest to an equal
    share, so no short remainder is left; in `size` mode a part shorter than a quarter
    of `part_size` joins its neighbour.
    `?at=b<n>` redirects to the part URL with `#b<n>`.
  - Content comes from `.shelfgauge/content/` (keyed by the indexed content identity:
    a plain file's hash, a zip entry's size + CRC32), else the book is read, verified
    against that identity (a mismatch asks for a rescan) and parsed; loading, splitting
    and rendering run on a thread with the parser stack. A few parsed books are kept in
    memory for part changes and images. Source-format anchors are resolved to block
    IDs and never emitted as HTML ids.
  - **Block IDs** are the block's ordinal in document order (`b1234`), counting every
    block (the exact rule is documented in `src/format/model.rs` and pinned by a golden
    test, with a second golden test for the EPUB mapping). Each FB2 `<body>` is a root
    section (main body first, then notes bodies); an EPUB has one main body and at most
    one notes body. They must be deterministic and **must not depend on the split setting**; deep
    links address a block (`?at=<block-id>`) and the server resolves the part. Cached
    `BookContent` carries a `parser_version`; bumping it invalidates the cache.

## Annotations (0.6.0)

Code: `src/annotations/` (`text`, `anchor`, `store`, `export`, `attach`),
`src/web/routes/annotations.rs`, `assets/reader.js`/`reader.css`.

- **One entity**, the annotation (UI: "Quote"; "mark" in the code), replaces bookmarks,
  notes and quotes: the **selected text is immutable** (it is the quote and the anchor's
  fingerprint), a separate **note** is editable and optional (bookmark name / note
  body). An annotation with no selection is a **bookmark** on the topmost visible block
  (note optional; the automatic label is the section title and the start of the block).
  No `kind` field: bookmarks (empty text) and quotes (non-empty text) are one model
  shown as two lists.
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
- Ids are UUIDs and deletion is soft (`deleted_at`), so the V2 hub (below) can sync
  into this table later.
- **UI**: `reader.js`/`reader.css` load only on read pages; selecting text offers "Quote";
  saved annotations are highlighted on the client; a bookmark is one action (button or
  `B`). The reader sidebar has tabs Contents | Bookmarks | Quotes; the book card lists
  bookmarks and marks in two `<details>` blocks with plain-form edit/delete.
  Lists are server-rendered and work without JS. Writes are a separate route group
  behind the Origin check.
- **Export**: one Markdown file per book in `.shelfgauge/annotations/`, regenerated
  atomically from the DB after every change, **one way only** (edits to the file are not
  read back): bookmarks first, then quotes grouped by section, notes, and `?at=b<n>`
  links.
  `shelfgauge annotations export` rewrites all files; generated files carry a marker
  comment and only marked files are ever removed.
- **Reading position** (separate from bookmarks, never in the lists): one server-side
  row per book in `userdata.sqlite` (topmost visible block + fingerprint, progress),
  same re-attaching and cleanup rules. Saved by `reader.js` after scrolling stops, on
  part change and on leaving the page, never inside notes bodies and only after a dwell
  time (stray jumps do not overwrite it); a page view opened from a bookmark or quote
  link (`from=mark`, `from=quote`) never saves it until the reader moves on with
  prev/next; last write wins. A deep link highlights its target block, except for a
  quote (the quote itself is highlighted) and a place picked on the status line's track
  (`from=track`, only a rough place). The book card offers
  "Continue reading" / "Read from the start" and shows progress; an explicit `?at=`
  always wins. `[reader] remember_position` turns it off.
- **Currently reading** (library page): the unfinished books with a position, most
  recently read first, up to `library.reading_list_size`, only books whose content key
  is live in the index. "Mark as finished" sets the progress to 100 %; "Clear list"
  sets `reading_positions.dismissed_at` (userdata schema v2) and keeps the positions;
  the next saved position (reading again, not re-attaching) clears it.
- **Implementation notes.**
  - Offsets count code points of a block's *rendered text*, which must equal the
    browser's `textContent` of `id="b<n>"`: containers concatenate their children with
    no separator, tables their cells, an image contributes only a non-blank caption
    (`annotations::text::rendered_text`, pinned by a test that renders books and
    compares). Selections are anchored in leaf blocks; quotes spanning blocks join them
    with `\n`. The client only sends block ids and offsets (converting UTF-16 to code
    points); the server derives `text`, `prefix`/`suffix` (32 code points) and the
    section title, and checks the limits and that the selection lies within one part.
  - The fingerprint search looks for `prefix + text + suffix` (or, for quotes of at
    least 16 code points, the text alone) and takes the match nearest the old block; a
    bookmark is found as a leaf block starting with its `suffix` (64 code points).
  - `userdata.sqlite` is one connection behind a mutex, used on blocking threads; the
    web handlers combine it with a pooled index connection. Removed-book cleanup runs
    as a hook of `BackgroundScan` (and after `shelfgauge scan`) only after complete,
    uncancelled scans; it also refreshes export files (renamed books, new orphans).
    Export files are named `<Authors> — <Title> (<8 hex of xxh3(key)>).md` and are not
    rewritten when unchanged.
  - Verified: the CSS Custom Highlight API is Baseline 2025 (Chrome, Firefox, Safari);
    without it, `reader.js` wraps text nodes in `<mark>` (offsets are unaffected). A
    same-origin `fetch` with `keepalive` on `pagehide` is a normal POST and carries
    `Origin` (Fetch standard, "append a request `Origin` header"; our
    `Referrer-Policy: same-origin` does not null it), so it passes the Origin check.
  - Write routes: `POST /book/{id}/annotations` (JSON), `/annotation/{id}/note` and
    `/annotation/{id}/delete` (JSON for the reader, forms with a local `back` URL and
    303 for the card), `/book/{id}/position` (JSON); request bodies are limited by
    `web::write_body_limit` (derived from `max_note_length`).
- Reader settings (font family from system stacks, size, line height, column width,
  alignment; the background is the color theme) and a focus mode (no sidebar, top bar
  shown on scroll up, a running head and a status line with the chapter and the
  progress, and a track over the whole book with chapter and bookmark ticks that jumps
  on click) are purely client-side; the server only adds positions as shares of the
  main text in ten-thousandths (`BookText::share_before`: the part's bounds, chapter
  ticks, `data-progress` on bookmark list items) and resolves `?progress=` back to a
  block (`BookText::block_at_share`) with the `?at=` redirect. Settings are
  whitelisted values in `localStorage`, applied by `theme.js` before the first paint as
  `data-` attributes on `<html>` that `reader.css` maps to CSS custom properties. They affect only the book text on read
  pages and keep the block at the top of the screen in place. The reader is not a
  separate crate.

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
- Routes are split into a *read* group, an *admin* group (scan, settings) and a *user
  writes* group (annotations, reading position), with an auth middleware slot; the admin
  group may stay localhost-only, user writes need authentication for remote access.
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

## V2: hub — books on mobile devices (designed, not built)

Agreed 2026-10-14. A user opens a book on a phone (or any other device) in a browser and
reads it even when the computer with the library is off, with annotations and the
reading position shared with the local instance. It replaces the earlier plan of a
local offline reader with `/reader/<token>` links. Remote access to the instance itself
(Tailscale / reverse proxy + built-in login) and an OPDS catalog are separate,
independent features.

- **Zero-knowledge hub**: `shelfgauge hub --data <dir>`, a subcommand of the same binary
  (axum + SQLite + a blob directory, HTTPS through a reverse proxy). It stores only
  ciphertext and never gets a key. Self-hosted first; the protocol is ready for a public
  hub, which is decided later.
- **Roles**: the *instance* (local shelfgauge, owns the library and `userdata.sqlite`),
  the *hub*, and *devices* (browsers running the hub's reader app). Several instances may
  share one account; books are matched by content key and the shelf shows each book's
  library (`[hub] instance_name`).
- **Accounts are anonymous** (no email). Authentication and encryption are separate:
  a revocable *device token* authorizes API calls and binds them to the account; the
  *account key* (32 bytes, only on clients: `userdata.sqlite`, never `config.toml`;
  IndexedDB on devices) encrypts the shelf and user-data records; a *book key* per
  publication encrypts its blobs. A *recovery code* (account id + account key + a
  recovery secret, whose hash is all the hub keeps) restores access; without it and
  without any paired client the hub data is lost (the books are not).
- **Pairing**: the instance shows a QR code `https://<hub>/pair#<pairing secret>.<account
  key>`; the fragment never reaches the server, the reader app sends only the one-time
  secret and gets a device token. Device list with revocation in the instance's UI.
- **Personal links**: "Open on phone" on the book card publishes the book and shows a QR
  code for `https://<hub>/r/<publication-id>`. The link carries no key and opens only on
  devices paired with the account (the book key comes from the shelf record). This keeps
  the hub from becoming a file-sharing service and keeps purchased books with their
  watermarks on the buyer's devices. DRM-protected books are never published (they
  cannot be read at all, see below).
- **Publication = structured data, never HTML**: an encrypted manifest (format version,
  metadata, content key, TOC, part ranges, shares for the progress track, image list),
  one blob per part (the part's `BookContent` blocks) and one per image (whitelisted
  types, shown through `blob:` URLs). Each blob is compressed, padded to the next size of
  a geometric sequence with ratio 9/8 (hides the size fingerprint of well-known books)
  and encrypted with AES-256-GCM (random nonce; associated data = publication id + blob
  path + format version). Block ids do not depend on the split, so annotations made on a
  device anchor exactly like local ones; a changed book is a new publication.
- **Rendering on the device runs the same Rust code in WASM** (`render` and
  `annotations::text`): blob data is untrusted, so trusted code turns it into escaped
  HTML exactly as the server does, under a strict CSP (`script-src 'self'
  'wasm-unsafe-eval'`, no inline code). This requires a workspace with a core crate
  (format model, split, render, annotation text; no filesystem, SQLite or Tokio) that
  builds for `wasm32` (`wasm-bindgen --target web`, no Node). The reader app is plain
  JS + WASM, reusing `reader.css`/`theme.js`.
- **Sync**: the hub keeps per-account records `(id, kind, seq, ciphertext)` of kinds
  `shelf_item`, `annotation` (id = annotation UUID) and `position` (id = HMAC(account
  key, content key)); clients pull since a `seq` cursor and merge: annotations
  accumulate, the newest `updated_at` wins per id, deletions are tombstones; positions:
  last write wins. Only books on the shelf are synced: publishing pushes their existing
  data, unpublishing deletes the publication and its records on the hub (local data
  stays). Records for books an instance does not have are kept as orphans (annotation
  rules above). Devices work offline (Service Worker + Cache Storage, changes queued in
  IndexedDB).
- **Book files (`.sgbook`)**, agreed 2026-10-15: a second delivery channel for the same
  publication (the hub's publication storage stays). A zip without compression: first
  entry `mimetype` (`application/vnd.shelfgauge.book`, so files are recognized by
  content, not name), then `sg.json` (format version, publication id, account id and
  key id, the book key encrypted with the account key, optionally an encrypted snapshot
  of the book's annotations) and the blobs at the hub's paths. Passed on by any means
  and opened only in the hub's reader app, which copies it into browser storage (OPFS /
  IndexedDB); it opens only where the account key is, also offline — a forwarded file
  is useless to others. Opening: File Handling API on desktop Chromium, Web Share Target
  on Chrome for Android, an "Open file" button everywhere (the only way on iOS / Safari
  and Firefox). Exporting or opening a file puts the book on the shelf; files do not
  expire and cannot be revoked; clients keep old account keys after a rotation (selected
  by the key id).
- **Lifetime and quotas** are hub settings: `publication_expiry_days` (days without
  opens, `0` = never, the self-hosted default; the shelf item stays and the book can be
  republished) and per-account limits on bytes, publications and devices (unlimited by
  default on a self-hosted hub).
- **Abuse and compatibility**: uploads only from registered accounts, rate limits,
  takedown by publication id (`shelfgauge hub takedown <id>`; the operator cannot see
  content but can delete it). The manifest format is versioned; `/api/info` advertises
  the supported range and an instance refuses to publish a format the hub cannot read.
- **Known limits**: web end-to-end encryption protects against a leaked hub database,
  not against a malicious operator serving a key-stealing reader (mitigation: open
  source, self-hosting); a revoked device keeps the account key until the key is rotated
  (re-encrypting all records on a trusted client); copies already cached on a device
  cannot be retracted, nor can exported `.sgbook` files.

**DRM** (implemented for EPUB in 0.8.0): DRM-free books,
including purchased ones with a watermark, are ordinary books and watermarks are never
removed; EPUB font obfuscation is not DRM (embedded fonts are not used anyway).
DRM-encrypted books (Adobe ADEPT, Readium LCP, FairPlay, Kindle) are never decrypted:
the parser detects them (EPUB: `META-INF/encryption.xml` with algorithms other than font
obfuscation — `rights.xml` then names Adobe ADEPT —, `license.lcpl`, `sinf.xml`), indexes
the readable metadata and gives them a separate "Protected by DRM" status (`protected`,
with the scheme in `books.drm`) — not a parse error, never parsed again, never opened
in the reader (content loading refuses them before reading the file), never published.

**Order of new formats**: EPUB (0.8.0, done) comes before the hub. The hub turns `BookContent`
into a versioned wire format, so the model must be extended for EPUB (lists, `pre`,
rules, sections from the navigation document and headings, inline EPUB3 footnotes)
while a change still costs only a `PARSER_VERSION` bump. FB2 block numbering stays
unchanged (its golden test must not change). TXT and `.fbz` followed in 0.9.0, HTML
(opt-in) followed in 0.10.0; no other formats (decided 2026-10-15). Images of HTML books
(sibling files) need a container-layer extension and come later.

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
| XML parsing (FB2, EPUB) | roxmltree (read-only DOM) |
| HTML parsing | html5ever + own arena DOM, serialized to XHTML for roxmltree |
| Encoding detection | encoding_rs (+ chardetng fallback; FB2, TXT) |
| Directory walking | walkdir + ignore |
| Archives | `zip` crate (`.zip` libraries, EPUB containers, `.fbz`) |
| Parallel parsing | rayon |

## Out of scope for v1

The V2 hub (above), deduplication across the library, RAR/other archive formats, nested
archives, multi-root libraries.
