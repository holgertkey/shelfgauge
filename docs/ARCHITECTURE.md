# shelfgauge — Architecture

> Status: v1 implemented (scanner, storage, web server, library and reading mode,
> hardening), annotations (0.6.0), reader settings (0.7.0), EPUB (0.8.0), plain text
> and `.fbz` (0.9.0), HTML (0.10.0, opt-in), search within the book (0.11.0), library
> statistics (0.12.0), the settings page (0.13.0). Of the V2 hub below, the core crate,
> the publication format (0.17.0) and the hub server (for 0.18.0) are built; the
> instance side and the reader app are not yet (see "Crates", "Publication format" and
> "Hub server").

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
- **Live configuration**: the server shares one `config::LiveConfig`
  (`Arc<RwLock<Arc<Config>>>`) between the request handlers (`AppState::config()`, one
  snapshot per request), the background scan (`exclude` and the registry are read at
  each `start()`) and the post-scan cleanup (the retention is read when it runs). The
  settings page swaps it as a whole after a save. `[server]` stays as started (the
  `Host` allowlist and `Urls` are built once): `AppState::server()` keeps the running
  values, and the live config always carries them. The body limit of user writes is a
  middleware reading the current `max_note_length` (wrapping the body in
  `http_body_util::Limited`, 413 when exceeded), not a `DefaultBodyLimit` fixed when
  the router is built.
- **Config write path** (`config::edit`): the settings form covers the editable keys
  (`edit::FIELDS`: label, kind and bounds; hints come from the comments of
  `DEFAULT_CONFIG_TOML`). A save parses the form into a `Config` with per-field errors
  (exclude patterns compiled as the scanner does), edits the file with `toml_edit` —
  only keys whose value differs from what the file gives now, keeping comments (also
  after a value), order and unknown keys — validates the new text with
  `Config::from_toml`, and writes it atomically (temporary file with the old
  permissions, renamed; a read-only file is refused). The form carries an xxh3 hash of
  the file it was rendered from; a different file on disk is a conflict (409). One
  save at a time (a lock in `AppState`). Errors shown in the browser carry no paths.

  | Section          | Takes effect                                        |
  | ---------------- | --------------------------------------------------- |
  | `[library]`      | at once (next request)                              |
  | `[reader]`       | at once (the split runs per request; block ids and deep links do not depend on it) |
  | `[annotations]`  | at once (limits on the next write, retention at the next cleanup) |
  | `[scan]`         | at the next scan; the page counts the books that would leave the index (`storage::stats::leaving_books`) |
  | `[server]`       | read-only on the page; `config.toml` and a restart  |
- **Scanning runs on every start, in the background**: the server starts immediately and
  serves the existing index while the UI shows progress. Also: a "Rescan" button, a
  re-read (`Scanner::reread`, from the settings page: every file and archive counts as
  changed, so all books are parsed again),
  `--no-scan`, a `shelfgauge scan` subcommand (index and exit) and `shelfgauge errors`
  (list the books that could not be read, with the reason). One scan runs at a
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
- **Content identity across kinds** (schema v4): plain files also record a CRC32
  (`containers.crc32`), computed while the file is read for its hash, so the size and
  CRC32 of a book's bytes compare plain files with zip entries (identical copies, see
  *Library statistics*). A plain file recorded before has a hash but no CRC32 and is
  read once more by the next scan (hash unchanged, not parsed); a file that could not
  be read has neither and is not retried. Every scan, finished or cancelled, is
  recorded in `scans` (times and the `ScanSummary` counts).
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
    cookies ignore the port, so libraries on one host share it); the theme is stored in
    the profile (0.15.0, see Profiles) and rendered on `<html>`; a small blocking script
    in `<head>` applies the browser's own layout state (the CSP allows no inline scripts
    or styles, and all URLs reach the scripts through `data-` attributes so `base_path`
    keeps working).
  - Scan progress is polled (`/scan/status`, JSON), not streamed: an open SSE
    connection would hold up the graceful shutdown.
  - *Library page* (0.12.0): "Currently reading" as cards (cover, progress bar),
    the 8 books added last (status `ok`, by id: rows are never deleted and
    `added_at` is set only on insert, so ids follow it without sorting the table), one
    line of figures (books, authors, size on disk) and a notice only when something
    needs attention (failures and DRM link to `/errors`, identical copies to
    `/duplicates`). Only cheap queries; the identical copies come from the statistics
    cache.
  - *Library statistics* (0.12.0, `/stats`, `src/storage/stats.rs`): counts, sizes,
    bars for formats, languages, genres, top series and authors, index health
    (last scan, failures, DRM, duplicates, no cover, no author) and reading and quotes
    (from `userdata.sqlite`, only books whose content key is live). The composition
    counts readable books (status `ok`); failed and protected books count in the index
    health. Each figure links to its list (`/books?format=|language=|genre=|missing=`,
    title order, the same condition, so a list's length is the figure). The whole
    result is computed in a few hundred milliseconds to about a second on 100 000
    books, so it is cached in `AppState` and dropped when a scan is recorded (keyed by
    the last `scans` id, which also notices a `shelfgauge scan` run beside the
    server). Bars are CSS: the width is one of the classes `w0`…`w100` (steps of 5),
    since the CSP allows no inline styles; the reading progress bars use them too.
    FB2 genre codes get names from the schema's list (`src/format/fb2/genres.rs`);
    unknown codes and EPUB subjects are shown as they are.
  - *Duplicates are reported, never merged or hidden* (`/duplicates`): identical copies
    share size + CRC32 (across plain files and archives; copies with one content key
    already share annotations and the reading position); probable duplicates share the
    normalized title (`sort_title`) and a non-empty author set with at least two
    contents (only titles that occur more than once are examined, via the
    `books_sort_title` index). Books without an author are never matched by title.
  - *Reading mode*: sidebar shows the book's TOC (one entry marked: the section at the
    top of the screen, followed while scrolling); main area shows one part of the book as
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
  - *Search within the book* (0.11.0): in reading mode the top bar's search field
    searches the open book; the results fill a fourth sidebar tab (a magnifier and
    the number of matches), which exists only while there is a query. No index: the
    query and the rendered text of every leaf block are normalized (lowercase one code
    point at a time, `ё` → `е`, `ς` → `σ`, `’ʼ` → `'`, soft hyphens and zero-width
    characters dropped, whitespace runs → one space; `annotations::find` in the core
    crate) and
    matched as plain substrings, never across two blocks; a map leads back to the
    original code points. The normalized text of the whole book is built on its first
    search and kept in the book's `BookText`, which stays in memory with the parsed
    book. At most 500 results are listed (a note gives the total), grouped by section (notes
    as "Notes"), each a snippet linking to `read?part=<n>&find=<query>&hit=<k>#<block>`.
    `GET /book/<id>/find?q=` returns the results as a fragment for the live search;
    without scripts the form submits to the read page, which renders them. The reader
    highlights the part's matches with the same rules (`assets/reader.js`; both sides
    are tested with `tests/fixtures/find/normalize.json`), and a page reached from a
    result does not move the reading position. Queries are not logged. Full-text
    search across the library is out of scope: indexing every book's text would cost
    far more than it gives.
  - Content comes from `.shelfgauge/content/` (keyed by the indexed content identity:
    a plain file's hash, a zip entry's size + CRC32), else the book is read, verified
    against that identity (a mismatch asks for a rescan) and parsed; loading, splitting
    and rendering run on a thread with the parser stack. A few parsed books are kept in
    memory for part changes and images, each with its rendered text (`BookText`,
    built when first needed). Source-format anchors are resolved to block
    IDs and never emitted as HTML ids.
  - **Block IDs** are the block's ordinal in document order (`b1234`), counting every
    block (the exact rule is documented in `format::model` in the core crate and pinned by a golden
    test, with a second golden test for the EPUB mapping). Each FB2 `<body>` is a root
    section (main body first, then notes bodies); an EPUB has one main body and at most
    one notes body. They must be deterministic and **must not depend on the split setting**; deep
    links address a block (`?at=<block-id>`) and the server resolves the part. Cached
    `BookContent` carries a `parser_version`; bumping it invalidates the cache.

## Annotations (0.6.0)

Code: `src/annotations/` (`store`, `export`, `attach`; `text`, `anchor` and `find` are
in the core crate), `src/web/routes/annotations.rs`, `assets/reader.js`/`reader.css`.

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
  `B`). The reader sidebar has tabs Contents | Bookmarks | Quotes (no Contents
  tab when the book has no titled sections or notes); the book card lists
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
  link (`from=mark`) never saves it until the reader moves on with prev/next; last
  write wins. A deep link's target block flashes (the highlight fades). The book card
  offers
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
  shown on scroll up, a running head with the current chapter, and a status line of
  constant length with the progress and a track over the whole book with chapter and
  bookmark ticks that jumps on click) are purely client-side; the server only adds the
  title in effect at the part start (the running head before the reader updates it)
  and positions as shares of the main text in ten-thousandths (`BookText::share_before`: the part's bounds, chapter
  ticks, `data-progress` on bookmark list items) and resolves `?progress=` back to a
  block (`BookText::block_at_share`) with the `?at=` redirect. Settings are
  whitelisted values stored in the profile (0.15.0; `localStorage` before), rendered
  by the server as `data-` attributes on `<html>` that `reader.css` maps to CSS custom
  properties; `theme.js` applies changes at once and saves them to the profile. They affect only the book text on read
  pages and keep the block at the top of the screen in place. The reader is not a
  separate crate.

## Phones (0.16.0)

The same pages serve phones; there is no separate mobile UI and no Node toolchain.
Below `48rem` the sidebar becomes an overlay over a dimmed page (a tap on the dimmed
page closes it and does nothing else, `app.js`), and controls that exist elsewhere leave
the top bar (the name, replaced by a house icon to the library page; the theme menu; Settings and Rescan move into the avatar menu; in the
reader focus mode). Below `48rem` the reader is always in focus mode, so a phone shows
the status line with the position and the marks: `theme.js` follows the media query
(also after the load, since a browser may apply the viewport meta tag late), the
button, the Aa checkbox, `f` and Esc do not leave it, and the sidebar button opens the
sidebar over the text with the top bar kept; a jump from a sidebar link closes it.
Wider screens keep the browser's choice in `localStorage` (`theme.js`). In focus mode a page
that fits the window (`body` height plus the top bar) keeps the top bar
(`.page-fits`, re-measured on resize and by a `ResizeObserver`): it could not be
scrolled up to bring it back. On touch screens (`pointer: coarse`) every
control, menu entry and list of links is at least 44 by 44 CSS px; links in running text
keep their size. Pages use `viewport-fit=cover` with `env(safe-area-inset-*)` padding.
Notes (`reader.js`, every device): a note link opens the note in a panel at the
bottom (under the text column on a wide screen, above the status line in focus mode)
instead of navigating; a note in another part comes from that part's page (fetched once
per page, parsed with `DOMParser`; the note is the `.notes section` around the target,
without its title and ids), so there is no extra route. "Open in notes" and any other
link in the text to a place in the book remember the reading place in
`sessionStorage`; the page they lead to shows "Back to text" in the status line,
before the track, and in the reader head (the one for screen readers), which returns
to that block. Modified clicks stay plain links.
Page turns (`reader.js`, touch screens only): a tap on the outer 30 % of the text on
either side or a sideways swipe (at least 60 px, mostly horizontal, within 600 ms, not
from the screen edges of 24 px) turns a page; a tap in the middle shows or hides the top
bar in focus mode, and a turn hides it. A page ends at the start of the text line at the
bottom edge (found with `caretPositionFromPoint`, else a page less one line), so that
line opens the next page uncut; it is lit up briefly (`.turn-line`). In focus mode (all
pointers) a page at rest, after a turn or once scrolling has stopped (`scrollend`, else
150 ms without scrolling), covers the text line cut by the bottom edge (`.page-cut`, in
the document down to the screen bottom, re-placed on resize and `visualViewport`
resize), so only whole lines show. The bottom edge is the top of the status line, but
at most the visible height less its height: while a browser toolbar slides, the status
line can still be laid out below the screen. The text slides
out and back in (Web Animations, 80 + 120 ms, not with `prefers-reduced-motion`). At a
part's end the next part opens, at its start the previous one with `#end` (scrolled to
its end, the hash dropped). The reader article has `touch-action: pan-y pinch-zoom` and
the page `overscroll-behavior-x: none`, so the browser neither zooms on a double tap
nor goes back in history on a swipe; a turning tap cancels its click, and a selection
the browser makes of quick taps is dropped. A tap within 24 px of a link in the text
(a note reference is a tiny target) opens that link instead. Links, controls, a selection, an open quote
popup, note or panel (the tap closes it) and sideways-scrolling tables keep their
touches.
Quoting works with the touch selection (`selectionchange`); the Quote button keeps
32 px below the selection handles, goes above the selection when there is no room, and
the note form stays in view when the keyboard opens (`visualViewport`). Browser check:
`.debug/TESTS/mobile_smoke.mjs` (390×844, touch, through an `allowed_hosts` name).

## Security

Local use on `127.0.0.1` needs no login; other devices log into a profile with its
password (`[server] auth = "password"`, 0.16.0):

- **Host header check** against `127.0.0.1:<port>`, `localhost:<port>`, `[::1]:<port>`,
  the bind address and `allowed_hosts` (protection against DNS rebinding); it runs as the
  outermost layer, so it also covers assets and unknown paths. An `allowed_hosts` entry
  without a port matches any port (reverse proxy on 443), one with a port matches
  exactly.
- **Origin check** (CSRF): every request with a method other than `GET`/`HEAD`/
  `OPTIONS`/`TRACE` needs an `Origin` whose authority is on the same allowlist (not
  compared with the request's `Host`, which a reverse proxy may rewrite); a missing or
  `null` Origin is rejected.
- If `bind` is not a loopback address and `auth` is `"none"`, the program refuses to
  start; with `auth = "password"` it refuses to start while no admin profile has a
  password (checked in `main` before binding), so a remote bind never comes up with
  nobody able to manage it.
- **Reach** (`security::classify_reach`, after the Host check): a request is *remote*
  when its TCP peer (`ConnectInfo`, from `into_make_service_with_connect_info`; a
  missing peer counts as remote) is not a loopback address, when its `Host` is an
  `allowed_hosts` name (a reverse proxy on the same computer connects from loopback),
  or when it carries a header that proxies add (`security::PROXY_HEADERS`: `Forwarded`,
  `X-Forwarded-For`, `X-Forwarded-Host`, `X-Real-IP`, `Tailscale-User-Login`; nginx
  rewrites `Host` to the upstream and Apache keeps `ProxyPreserveHost Off` by default).
  Header *values* are never read, so a header can only make a request remote, never
  local. Everything else is *local*. Remote requests are refused with a hint while
  `auth = "none"` (0.15.0 served `allowed_hosts` requests without any login; the
  change is in the changelog). The request span gets `remote = true`. Oneshot tests add
  the peer with `MockConnectInfo` (`tests/common`: `local`, `from_peer`).
- **Auth slot** (`security::require_auth`) on the read, write and admin groups: a local
  request passes to the profile middleware; a remote one needs a *password session*
  (see Profiles) in its cookies, else a page `GET` goes (`303`) to `/login?next=…` and
  fragments and `POST`s get `401` (the same split as the picker's). The picker and the
  login routes sit outside the slot and send remote requests to the login themselves
  (the PIN routes before a try is counted: a PIN is never checked remotely).
- **Admin pages remotely**: `[server] remote_admin = false` by default; the admin group
  then answers an admin on another device with `403` "On this computer only". Readers
  get `403` as everywhere.
- **Login cookie**: `SameSite=Strict`, `HttpOnly`, `Path` = base path; `Secure` when
  the login `POST`'s `Origin` is `https://` (set by the browser, already accepted by the
  Origin check), never from `X-Forwarded-Proto`, so plain HTTP in a private network
  (WireGuard) keeps working. "Remember this device" gives 400 days, else a browser
  session cookie.
- Routes are split into a *read* group, an *admin* group (`POST /scan`, `GET`/`POST
  /settings`, the maintenance actions `POST /settings/reread`, `/settings/content-cache`
  and `/settings/export`, profile management under `/settings/profiles`), a *user
  writes* group (annotations, reading position, `POST /reading/clear`, the own
  profile, `POST /logout`), the profile *picker* (`/profiles`, which needs no profile)
  and the *login* (`/login`, `/login/{id}`); the read, admin and write groups pass the
  auth slot, then resolve the request's profile, and the admin group answers `403` to a
  reader (see Profiles) and to a remote admin without `remote_admin`. The admin pages
  are the one place that shows absolute paths (the library root in *About*); error
  messages stay path-free.
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
  network (Tailscale / WireGuard / SSH tunnel). The library page shows the library
  folder only locally.
- **Accepted** (see `.debug/security-review.md`): the login page lists the profiles and
  which of them have a password to anyone who reaches it; a proxy on another machine
  shares one per-address budget among its clients, and one on this computer has none
  (rate-limit in the proxy); browser-session logins have no server-side expiry; an SSH
  tunnel arrives from loopback and counts as this computer.

## Logging (0.14.0)

`tracing` with `tracing-subscriber`, set up in `logging` (lib, so the tests build the
same subscriber) and `cli::init_logging`: stderr only, no log files and no rotation (a
daemon logs to journald or the container log), no metrics endpoint.

- **Filter**: `warn,shelfgauge=info` by default, `-v` debug, `-vv`/`--trace` trace,
  otherwise `RUST_LOG`. At trace level every closed span logs its duration
  (`FmtSpan::CLOSE`); the expensive steps have `debug` spans for it (scan phases
  `walk`, `detect`, `parse`, `write`, `cleanup`; `load` with `read`, `parse`, `store`;
  `find`; `library_stats`; `reattach`), never per block or element. Below trace
  nothing changes (the 100 000-book scan is not slower).
- **Formats**: `--log-format text` (default; ANSI colours only on a terminal and
  without `NO_COLOR`) or `json` (one object per line, the span chain as `spans`). A
  flag, not a config key: logging starts before the config is read.
- **Requests**: `web::trace` is the outermost layer (rejected `Host`/`Origin` requests
  included). Each request runs in an `info` span `request{id, method, route}`: `id` is
  a per-process counter in hex, `route` the matched route template or `-`. One event
  at the end has `status` and `latency_ms`: `warn` for 5xx, `info` for requests over
  one second ("slow request"), `debug` otherwise, `trace` for assets. The span is
  carried to blocking threads (`AppState::db`, `user_db`, `web::blocking`) and to the
  parser thread (`run_with_parser_stack`) together with the current subscriber
  (`logging::in_current_span`). Background scans run in a root span
  `scan{trigger}` (`startup`, `rescan`, `reread`, `cli`), never inside a request's span.
- **Panics**: the panic hook logs them through `tracing` (message, thread, location;
  the backtrace with `RUST_BACKTRACE`) on the panicking thread, so a handler panic
  carries its request id. The innermost layer (`trace::catch_panics`, no extra
  dependency) answers a panicking handler with the internal error page (500), which
  shows the request id as the error id. Parser panics caught by the scanner and the
  HTML parser (`logging::catch_expected_panic`) are expected input failures: logged at
  `debug`, reported with the book.
- **Never logged**: query strings and request bodies (search queries, notes, quotes,
  form values, among them the login form and the password forms of My profile); the
  headers `Authorization`, `Cookie`, `Set-Cookie`; profile names, PINs, passwords,
  their hashes and session tokens (the `shelfgauge-profile` cookie) — profiles appear
  only as `profile = N` (on the `request` span and in the events `profile selected`,
  `profile added`, `pin rejected`, `pin locked`, `pin changed`, `logged in`, `password
  rejected`, `password locked`, `password changed`, `password removed by an admin`,
  `profile deleted`); client addresses are not logged either (`remote = true` on the
  span says a request came from another device); `Profile` and the session `Token`
  have redacted `Debug` output; in V2 device
  tokens, pairing secrets, invite codes, recovery codes and their hashes, keys of any
  kind, public account ids and publication ids (`BookKey`, `PublicationId`, the hub's
  `Secret` and `AccountId` have redacted `Debug` output; checked for building and
  sealing a publication since 0.17.0 and for every hub route since Phase 24: the hub
  logs route templates, accounts as `account = N`, and no bodies). Books
  are named by id or library-relative path, never by absolute path at `info` and
  above. Untrusted values (`Host`/`Origin` headers, paths from archives, parser
  messages) are logged with `?` (Debug escapes newlines and control characters) or as
  fields, never interpolated with `{}`, so they cannot forge lines in `text`. New code
  uses structured fields (`book_id = id, error = %e`) instead of values in the
  message. `tests/logging.rs` checks the rule at trace level in both formats.

## Profiles (0.15.0)

Several people share one instance and one library; each *profile* has its own
annotations, reading positions and "Currently reading" list, theme and reader
settings. The index, scans, caches and `config.toml` stay shared. A profile is a
person, not authentication.

- **Data** (`profiles` module, `userdata.sqlite` schema v3, v4 since 0.16.0):
  `profiles` (integer id, name 1–32 characters unique ignoring case, color 0–7, role
  `admin`/`reader`, optional argon2id PHC PIN hash, optional password hash and its
  change time, `share_activity`, settings JSON or NULL, sort, created) and
  `profile_sessions` (SHA-256 of a 32-byte random token from `getrandom`, profile,
  created, last seen — written at most hourly, `level` 0 = local / 1 = password,
  `remote`). Sessions from before v4 are local ones. `annotations` and
  `reading_positions` were rebuilt with `profile_id … ON DELETE CASCADE`
  (`reading_positions` keyed by `(profile_id, book_key)`). The migration creates the
  *owner* (id 1, "Me", always admin, cannot be deleted) and moves all existing rows to
  it. At most 16 profiles. Per-person store functions take a `ProfileId`; another
  profile's annotation id is "not found". Groups, re-attaching (the threshold counts
  all profiles' quotes; each profile's position moves with its own dismissal), the
  post-scan cleanup and the retention work on content keys for all profiles.
- **Request profile** (`web::profile`): a middleware on the read, admin and write
  groups resolves a `CurrentProfile` extension — the owner while it is the only
  profile (no cookie, no picker, no avatar), else the session in the cookie
  `shelfgauge-profile` (HttpOnly, SameSite=Strict, `Path` = base path, 400 days,
  `Secure` when the `POST` that set it had an `https://` `Origin`). A remote request has
  no implicit owner and only counts password sessions. Without a session a page
  `GET` gets `303` to `/profiles?next=<local path>`; fragments (sidebar, scan status,
  annotation lists, find, covers, images — told apart by route) and every `POST` get
  `401`, and the scripts reload the page. `next` must be a local path under the base
  path. The extension is also put on the response, so error pages use the profile's
  theme and role; error pages of routes without the middleware (unknown paths, the
  picker) resolve the cookie's profile themselves. A second middleware answers `403` to a reader on the admin routes (built from
  one table that the role test iterates).
- **Picker and PIN**: `/profiles` (a page without the sidebar; tiles are buttons of
  POST forms), `POST /profiles/{id}/select` and `GET`/`POST /profiles/{id}/pin`. Every
  choice starts a new session (no fixation) and ends the sessions in the request's
  cookies. A PIN is 4–8 digits, hashed with `argon2` defaults on a blocking thread,
  one run at a time and outside the user-data lock; after 5 tries without the right
  PIN (malformed ones count) the profile is locked for 30 s, doubling to 15 min, in
  memory (`PinLimiter`). A try is counted before the hash is checked, so parallel
  requests get no more guesses and a locked profile costs no hashing; `422` with the
  tries left, `429` with the wait. Setting, changing or removing a PIN ends the profile's
  other sessions and renews the current token; an admin can remove another profile's
  PIN, `shelfgauge profiles reset-pin` the owner's.
- **PIN and password** (0.16.0): a PIN is not authentication and never makes a
  non-loopback bind allowed; it is asked only locally. The optional password
  (`profiles::password`: 10–128 code points after NFC normalization, no other rules,
  argon2id like the PIN, sharing its one-run-at-a-time lock) is the login from other
  devices: `/login` lists the profiles (without a password: disabled, "On this computer
  only"; with one profile it goes to the form directly), `/login/{id}` checks it and
  starts a password session. The two are independent; a remote request can neither
  change the PIN nor remove its own password (also not through the admin route). A
  wrong password counts like a wrong PIN (its own `PinLimiter`), and a non-loopback
  client also has `password::AddressLimiter`: 20 failed logins per 15 minutes per
  address, then every login from it is refused. Setting or changing a password ends the
  profile's other password sessions (the current one and local ones stay); removing
  it, by the person locally, an admin or `shelfgauge profiles remove-password`, ends
  all of them. The plan's `POST /profile/password` became the actions `set-password`
  and `remove-password` of `POST /profile`, like the PIN's.
- **Roles**: `admin` sees Settings, Rescan, maintenance and Settings → Profiles (add,
  rename, recolor, role, remove PIN, delete with typed-name confirmation and "Export
  first"); `reader` sees neither the gear nor Rescan. Everyone edits their own name,
  color, PIN, activity sharing and settings on My profile (`/profile`). Adding the
  second profile starts an owner session in the admin's browser.
- **Look**: with two or more profiles, and always on another device, an avatar
  (initials on the profile color) with a `<details>` menu (My profile, Switch profile
  or on another device Sign out, Profiles for admins; on narrow screens also Settings
  and Rescan, which leave the bar). `data-pcolor` on `<html>` sets
  `--accent` and `--on-accent` from a palette of 8 colors defined for each theme;
  color 0 is each theme's own accent, so one user sees no change. Every color has at
  least 4.5:1 against its theme's background and against the text on it
  (`.debug/TESTS/pcolor_contrast.py`).
- **Settings follow the person**: theme and reader settings (whitelist
  `profiles::SETTINGS`, equal to `theme.js`'s `OPTIONS` through
  `tests/fixtures/profiles/settings.json`) are rendered as `data-theme`/`data-r*` on
  `<html>`; `theme.js` applies a change at once and saves it with `POST
  /profile/settings` (JSON, debounced, `keepalive`; a failure shows a notice). Values
  older versions kept in `localStorage` are adopted once: while the owner's settings
  are NULL the page carries `data-adopt`. Sidebar state and focus mode stay in the
  browser (*This browser* on the Settings page).
- **Family activity**: opt-in per profile (off by default); the others see its
  "Currently reading" books as "Family is reading" on the library page and "Also
  reading" on the book card. Positions only, never annotations. Statistics show the
  current profile's reading ("Your reading").
- **Export**: the owner's Markdown files stay in `.shelfgauge/annotations/`, other
  profiles write to `.shelfgauge/annotations/profiles/<name> (<id>)/` (renamed with the
  profile, kept when it is deleted; the owner's stale-file sweep never descends into
  `profiles/`).
- **CLI**: `shelfgauge profiles list | add <name> [--admin] | remove <name> [--yes] |
  reset-pin <name> | set-password <name> | remove-password <name>` (`set-password`
  asks twice without echo through `rpassword`, or reads one line from a non-terminal
  stdin, removing only the line break); `shelfgauge annotations export [--profile <name>]`. Unknown names
  are errors that list the profiles.

## V2: hub — books on mobile devices (server built, instance side in progress)

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
- **Publication = structured data, never HTML** (format built in 0.17.0, see
  "Publication format"): an encrypted manifest, one blob per part (the part's
  `BookContent` blocks) and one per image (whitelisted types, shown through `blob:`
  URLs). Each blob is compressed, padded to the next size of a geometric sequence with
  ratio 9/8 (hides the size fingerprint of well-known books) and encrypted with
  AES-256-GCM (random nonce; associated data = format version + publication id + blob
  path). Block ids do not depend on the split, so annotations made on a device anchor
  exactly like local ones; a changed book is a new publication.
- **Rendering on the device runs the same Rust code in WASM** (`render` and
  `annotations::text`): blob data is untrusted, so trusted code turns it into escaped
  HTML exactly as the server does, under a strict CSP (`script-src 'self'
  'wasm-unsafe-eval'`, no inline code). That code is the core crate `shelfgauge-core`
  (see "Crates"), built for the device with `wasm-bindgen --target web` (no Node). The reader app is plain
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

### Publication format (0.17.0)

Built in 0.17.0, nothing is uploaded yet. Types and decoding are in the core crate
(`shelfgauge_core::publication`, so the device decodes with the same code); building
and sealing in the app (`src/publication/`).

- **Blobs**: `manifest`, `p/<n>` (parts, 0-based), `i/<n>` (images). The manifest
  (postcard) holds the metadata (title, authors, series, the `lang` tag), the content
  key, the block count, per part its range, title, running head and progress, the TOC
  entries, the chapter ticks, the image list and the cover: everything the reader page
  derives from the whole book (`render::outline` in the core), so a device needs only
  the part it shows. A part blob (postcard) holds the book's section structure cut down
  to the part's blocks (ids unchanged; `continued` says how many sections go on from
  the previous part) and the resolved targets of its internal links (text ↔ notes).
  `PartBlob::render` gives exactly the server's `render_part` output; all parts
  together give the book again (`publication::assemble`, ids checked by
  `BookContent::from_numbered`).
- **Sealing**: raw deflate (`flate2`; `DecompressionStream("deflate-raw")` on the
  device) → padding (`[len u32 BE][data][zeros]` up to the bucket: 256 bytes, then
  each size plus an eighth) → AES-256-GCM with the book key (`aes-gcm`; WebCrypto on
  the device; the core crate does no cryptography). A blob is `[format version u16
  BE][nonce 12][ciphertext + tag]`; associated data `[version u16 BE][publication id
  16][path UTF-8]`. The version is checked before decrypting (unknown versions are
  refused with a clear error) and authenticated through the associated data.
- **Versions**: `FORMAT_VERSION` (1) and `SUPPORTED_VERSIONS`; postcard is not
  self-describing, so any change to the format types or to the model is a new format
  version.
- **Refusals**: DRM-protected books; SVG and other non-raster images are left out.
- **Tests**: `tests/publication.rs` decodes the publication of every synthetic book in
  every split mode and compares part HTML, TOC, progress and the rendered text with the
  server's; interop vectors for the browser side are in `tests/fixtures/publication/`
  (encryption from a fixed key, nonce and padded plaintext; a raw deflate stream; a
  whole sealed publication; regenerate with `SHELFGAUGE_WRITE_VECTORS=1`).

### Hub server (0.18.0, in progress)

`src/hub/` in the app crate, `shelfgauge hub` in the same binary (no feature flag).
Built in Phase 24; the instance side and the reader app follow.

- **Data directory**: `hub.toml` (`hub::config`: `[server]` bind/port/allowed hosts/
  base path, default `127.0.0.1:8743`; `[accounts]` registration `invite` | `open`,
  `max_accounts`, `invite_days`; `[quotas]` per account bytes/publications/devices;
  `[limits]` `max_blob_bytes` 64 MiB, `publication_expiry_days`, per-device and
  per-address request rates), `hub.sqlite` (own `user_version` migrations: accounts,
  devices, pairings, invites, publications, blobs, takedowns, records) and `blobs/`
  (`<2 hex>/<publication id>/manifest|p/<n>|i/<n>`, paths built only from parsed ids,
  written to a temp file and renamed). A new hub is created only with an explicit
  `--data`; `main` branches to it before any library is opened.
- **Accounts and tokens**: the public account id (128 random bits) is in the API and
  the recovery code, the internal integer id in logs and `hub accounts`. Device
  tokens, pairing secrets (10 minutes, single use) and invite codes (one-time,
  `invite_days`) are 32 random bytes in base64url, stored as SHA-256 of that text; at
  registration the instance sends `recovery_hash` = base64url (unpadded) of SHA-256
  over the recovery secret's base64url *text* (not its raw bytes; `Secret::hash`),
  `POST /api/recover` the secret's text.
  Revoked devices keep their row (`revoked_at`) and lose access.
- **API** (JSON, binary fields base64url; `Authorization: Bearer` on all but `info`,
  `account` create, `pair`, `recover`): `GET /api/info` (format range, limits,
  quotas, registration), `POST/GET /api/account`, `POST /api/pairings` (new pairing
  secret), `POST /api/pair`, `POST /api/recover`, `GET /api/devices`, `DELETE
  /api/devices/{id}`, `GET /api/pub` (the account's publications with sizes and last
  opening), `PUT/GET /api/pub/{id}/{blob}` (raw sealed blobs; a PUT must parse as an
  envelope of a supported format version; the first blob creates the publication),
  `DELETE /api/pub/{id}`, `GET /api/sync?since=N&limit=M` and `POST /api/sync`.
  Errors are `{"error": code, "message": …}` and never echo the request; another
  account's publication answers `404` exactly like a missing one (also for `PUT`,
  which never overwrites it), a taken-down one `410`.
- **Sync records**: `(kind, id)` per account with `kind` in `shelf_item`,
  `annotation`, `position` and an opaque ciphertext; every change gets the account's
  next `seq`, a deletion keeps the row as a tombstone (`data: null`) with a new `seq`.
  A pull answers records after `since` in `seq` order with a cursor `seq` and `more`.
- **Limits**: body limits per route (64 KiB JSON, 16 MiB / 500 records per push, 1 MiB
  per record, `max_blob_bytes` per blob), quotas checked inside the write transaction
  (a refused blob is not written, a refused push stores nothing), a token bucket per
  device and a fixed hourly window per non-loopback address for the routes without a
  token (loopback peers, a proxy on the same machine, share one budget; forwarded
  headers are never read). Expiry (`publication_expiry_days`, last opening = the
  last manifest fetch, written at most hourly) runs on start and daily; `hub
  takedown` deletes a publication and remembers its id.
- **Layers**: the request trace (`request{id, method, route, account}`), the hardening
  headers with the §11 CSP (`script-src 'self' 'wasm-unsafe-eval'`, `img-src 'self'
  blob:`, no inline code) and `Referrer-Policy: no-referrer`, and the `Host` allowlist
  are shared with the library server (`web::security`, `web::trace`); no reach
  classification, no `Origin` check and no CORS (a bearer header is no ambient
  credential; the instance's client sends no `Origin`). API answers are `no-store`.
- **Reader shell**: `/`, `/pair` and `/r/{id}` serve one static page for any
  well-formed id without a database lookup (no existence oracle), with its files under
  `/assets/` from `assets/hub/` (excluded from the library's assets); a placeholder
  until the reader app.
- **Tests**: `tests/hub.rs` (router with a settable clock: invites, pairing expiry and
  single use, recovery, revocation, blob round trip, account isolation, quotas,
  expiry, takedown, sync paging and tombstones, body limits, rate limits, `Host`,
  shell, base path) and `tests/logging.rs`
  (`hub_secrets_and_publication_ids_are_never_logged`).

### Instance side (0.18.0, in progress)

`src/hub_client/` (Phase 25; accounts, pairing and devices so far; publishing and the
sync worker follow) and the page **My profile → Phone and other devices**
(`web::routes::hub`).

- **Config** `[hub]`: `url` (the hub for new accounts; `HubUrl` requires `https://`,
  `http://` only for a loopback host; normalized, no query, fragment or user info),
  `instance_name` (default: the library folder's name), `sync_interval_minutes`. Only
  in `config.toml`. An account keeps the URL it was made on.
- **HTTP client**: `ureq` 3 (blocking, run on a blocking thread) with rustls + `ring`
  and the platform verifier (the OS trust store, so a hub behind a private CA works
  once the CA is installed); no redirects (a 3xx is an error: following it would
  carry the bearer token),
  connect 10 s, call 60 s. `ClientError` never carries the request URL; the
  `ureq`/`ureq_proto` log targets are always off (at `trace` they would log hub paths,
  which hold publication ids).
- **Data** (`userdata.sqlite` v5): `hub_accounts` (one per profile: hub URL, public
  account id, account key, key id, device id and token, the recovery code until "I
  saved it", sync state), `hub_shelf` (books on the profile's shelf: content key,
  publication id, book key, own/other instance, state `uploading | published |
  expired | removing`), `hub_outbox` (changes to push, `STRICT`, keys of mixed type)
  filled by triggers on `annotations` and `reading_positions` for shelf books
  (`book_removed` deletions are not pushed). Deleting a profile deletes its hub rows.
- **Keys and records** (`hub_client::keys`): the account key (32 random bytes) never
  leaves the instance except inside the pairing link's fragment
  (`<hub>/pair#<pairing secret>.<account key, base64url>`). Records are
  `[version u16 BE][nonce 12][AES-256-GCM ciphertext + tag]` with associated data
  `"sg-record" ‖ version ‖ kind ‖ 0x00 ‖ id`, so the hub cannot move a ciphertext to
  another record; plaintext is JSON. Record ids: `annotation` = the UUID, `position` =
  base64url HMAC-SHA256(account key, `"position"` ‖ content key), `shelf_item` = the
  same with `"shelf"` (one shelf item per book per account). Device names are sealed
  the same way (kind `device`, empty id). Interop vectors for the reader app:
  `tests/fixtures/publication/records.json` (`SHELFGAUGE_WRITE_VECTORS=1`).
- **Recovery code**: `SG1-` and Crockford base32 in groups of four (account id,
  account key, recovery secret, 2 checksum bytes: 132 digits); case, dashes, spaces
  and `O`/`I`/`L` tolerated. The hub gets only SHA-256 of the secret's text. The
  instance keeps the code only until the person confirms it is saved.
- **Page**: create an account (with the invite code when the hub's `/api/info` says
  `invite`), then the recovery code (text and inline SVG QR code, `qrcode` with only
  its `svg` feature); join with a recovery code; pair a device (a one-time link shown
  once, `no-store`, never in a URL the instance serves); the device list (names opened
  with the account key; another device can be removed); leave (revokes this library's
  token on the hub, best effort, and forgets the account locally; refused while the
  recovery code waits for "I saved it", since the stored code is its only copy). Pages
  showing a secret are `no-store`; form bodies are never logged, only profile ids.
- **Tests**: `tests/hub_client.rs` (a real hub on a loopback port, helpers in
  `tests/common/hub.rs`) and `tests/logging.rs`
  (`hub_account_secrets_are_never_logged_by_the_library`).

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

## Crates (0.17.0)

The repository is a Cargo workspace of two crates with one version
(`[workspace.package]` in the root `Cargo.toml`), published together:

- **`shelfgauge`** (the root package): the library and the binary — parsers,
  containers, scanner, storage, caches, profiles, the web server, the CLI. `cargo
  install shelfgauge` installs it as before.
- **`shelfgauge-core`** (`crates/shelfgauge-core/`): what runs both on the server and on
  a reading device — `format::model` (the book model and block numbering),
  `config::{ReaderConfig, SplitMode}`, `render::{split, html, outline}`,
  `annotations::{text, anchor, find}` and `publication` (the publication format's
  types and decoding). It does no IO, parses no book files and does no cryptography; its
  only dependencies are `postcard`, `serde`, `thiserror` and `tracing`, and it builds for
  `wasm32-unknown-unknown` (checked in CI). Code that must render or count text exactly
  as the reader does belongs here; anything that touches files, SQLite, Tokio or a
  parser library does not.

`shelfgauge` re-exports these modules at their old paths (`shelfgauge::format::model`,
`shelfgauge::render::{split, html, outline}`, `shelfgauge::annotations::{text, anchor, find}`,
`shelfgauge::config::{ReaderConfig, SplitMode}`), so code and tests use one path for
both. The app depends on the core with `path` and an exact `version = "=X.Y.Z"` (one
line in `[workspace.dependencies]`; Cargo refuses to build while it differs from the
workspace version). The release workflow publishes with `cargo publish --workspace`,
the core first. The core's tracing targets are `shelfgauge_core::…`, which the
`shelfgauge=…` log filters cover by prefix.

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
| Logging | tracing + tracing-subscriber (text or JSON on stderr) |

## Out of scope for v1

The V2 hub (above), deduplication across the library (duplicates are only reported),
RAR/other archive formats, nested archives, multi-root libraries, text statistics across
the library (word or page counts would need every book parsed).
