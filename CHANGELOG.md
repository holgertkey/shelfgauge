# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Build numbers (`+NNN`) do not get their own sections.

## [Unreleased]

## [0.16.0] - 2026-10-20

### Added

- **Remote access with password login**: with `auth = "password"` in the `[server]`
  section of `config.toml`, each person logs into their own profile with its password
  from a phone or another computer (over Tailscale / WireGuard, or through a reverse
  proxy with HTTPS). On the computer with the library nothing changes: no password is
  asked there, and PINs work as before. The README's "Remote access" section has
  recipes for Tailscale, Caddy and nginx.
- An optional **password** per profile (10 to 128 characters, Unicode-normalized,
  stored as an argon2id hash), set on My profile or with
  `shelfgauge profiles set-password NAME`; `remove-password NAME` and Settings →
  Profiles remove it. The PIN is for this computer, the password for other devices; a
  PIN is never accepted from another device.
- **Login page** (`/login`): the profiles with a password, a password form with
  "Remember this device" (about 400 days, else until the browser closes), **Sign out**
  in the avatar menu. After 5 wrong passwords a profile is locked for 30 seconds,
  doubling up to 15 minutes; a device is refused for 15 minutes after 20 failed logins.
- `remote_admin` in `[server]` (off by default): whether admins may use the settings
  page, rescans and profile management from other devices.
- **Phones**: touch targets of at least 44 px, the sidebar as an overlay that a tap
  beside it closes, a house icon in the top bar that leads to the library page, a top bar that keeps room for the search field (focus mode is in the
  Aa panel there; every book opens in focus mode, with the status line of the position
  and the marks), quoting with the touch selection (the Quote button keeps clear of the
  selection handles), and room for notches and the home indicator.

### Changed

- A request through a name from `allowed_hosts`, or with a header that reverse proxies
  add (`X-Forwarded-For`, `Forwarded`, `X-Forwarded-Host`, `X-Real-IP`,
  `Tailscale-User-Login`), now counts as coming from another device: it needs
  `auth = "password"` and a login, and is refused with a hint otherwise. **Before
  upgrading** a library that is used through a reverse proxy, set a password for an
  admin profile (`shelfgauge profiles set-password NAME`) and `auth = "password"`.
- `bind` may be a non-loopback address with `auth = "password"`; shelfgauge then
  refuses to start while no admin profile has a password.
- The library page shows the library folder only on the computer with the library.
- `shelfgauge profiles list` shows whether a profile has a password.
- `userdata.sqlite` schema version 4 (migrated automatically; sessions from before stay
  valid on this computer only).

### Security

- Requests through a reverse proxy are no longer served without a login (see Changed).
- Forwarded headers are never trusted: they can only make a request "remote". The
  login cookie is `Secure` when the browser uses `https://`, judged by the `Origin` of
  the login, no longer by `X-Forwarded-Proto`.
- Passwords, login forms and session tokens are never logged; requests from other
  devices are marked `remote=true` in the request log.

## [0.15.0] - 2026-10-19

### Added

- **Profiles**: several people can share one library, each with their own bookmarks,
  quotes, reading places, "Currently reading", theme and reading settings. With one
  profile nothing changes: your data belongs to the owner profile ("Me" until
  renamed). An admin adds profiles under Settings → Profiles; then each browser asks
  "Who is reading?" (a picker with round avatars) and remembers the choice; the avatar
  menu in the top bar has My profile and Switch profile.
- An optional **PIN** per profile (4 to 8 digits, stored as an argon2id hash): after 5
  wrong tries the profile is locked for 30 seconds, doubling up to 15 minutes. Each
  person can set, change or remove their PIN; an admin can remove another profile's
  PIN. A PIN is not authentication: it keeps family members apart and does not allow
  listening on a non-loopback address.
- **Roles**: admins change the settings, rescan and manage profiles; readers read and
  annotate (the admin pages answer 403, and the gear and Rescan are hidden).
- **My profile** (`/profile`): name, color, PIN, the theme, a reset of the reading
  settings, "Sign out on other devices" and "Show my reading to the other profiles".
- **Profile colors**: eight colors for every theme, used for the avatar and as the
  accent of links and buttons; color 0 is each theme's own accent.
- **Family activity** (off by default): "Family is reading" on the library page and
  "Also reading" on the book card show the books of the profiles that share their
  reading; never bookmarks, quotes or notes.
- `shelfgauge profiles list`, `add NAME [--admin]`, `remove NAME [--yes]` and
  `reset-pin NAME`; `shelfgauge annotations export --profile NAME`.

### Changed

- The theme and the reading settings are saved in the profile instead of the browser,
  so they are the same on every device; settings that a browser kept before are taken
  over once. The settings page's "This browser" section keeps the sidebar and focus
  mode resets; the theme and the reading settings moved to My profile.
- The Markdown export of profiles other than the owner goes to
  `.shelfgauge/annotations/profiles/<name> (<id>)/`; the owner's stays in
  `.shelfgauge/annotations/`.
- The statistics page shows the current profile's reading ("Your reading").
- Request log lines carry the profile id (`profile=2`); profile names, PINs and session
  tokens are never logged.
- `userdata.sqlite` schema version 3 (migrated automatically; older shelfgauge
  versions cannot open it afterwards).

## [0.14.0] - 2026-10-18

### Added

- **Request logging**: every HTTP request runs under a short id with its method and
  route template (never the query string); it ends with a line with the status and
  latency. Failed requests (5xx) are logged as warnings and requests slower than one
  second as "slow request" at the default level, the rest with `-v`. Lines logged
  while a request is handled, also on worker and parser threads, carry its id.
- The "Internal error" page shows the request's error id to look up in the log.
- `--log-format json`: one JSON object per line, with the request and book spans as
  fields, for log collectors.
- With `-vv`/`--trace`, the duration of the expensive steps (scan phases, loading and
  parsing a book, search in the book, statistics, re-attaching annotations).
- Background scans log in a `scan{trigger}` span (`startup`, `rescan`, `reread`,
  `cli`).

### Changed

- A panic in a request handler is answered with the internal error page instead of a
  dropped connection; panics are logged through the log (with the request id) instead
  of the default panic message.
- Colours in the log only on a terminal and never when `NO_COLOR` is set.
- Log lines use fields (`book_id=7 error=…`) instead of values in the message;
  untrusted values (archive entry names, headers, parser messages) are escaped, so they
  cannot start a forged log line. Logs never contain query strings, request bodies or
  credentials, and name books by id or library-relative path.
- Release builds use link-time optimization with one codegen unit and strip symbols:
  the binary is about a third smaller (about 10 MB on Linux instead of 14.7 MB).

## [0.13.0] - 2026-10-18

### Added

- **Settings page** (`/settings`, the gear in the top bar, also in reading mode):
  change `[library]`, `[reader]`, `[annotations]` and `[scan]` from the browser.
  `config.toml` is edited in place: comments, key order and unknown keys are kept and
  only changed keys are written; the values are checked as at startup (errors are
  shown at their field) and the file is written atomically. A save is refused when the
  file was changed since the page was opened. `[library]`, `[reader]` and
  `[annotations]` apply at once, `[scan]` at the next scan; the page tells how many
  books a change would remove from the index and offers **Rescan now**. `[server]` is
  shown read-only, with the command-line overrides.
- *This browser* on the settings page: the theme, and resetting the reading settings,
  the sidebar (width, hidden state, scroll position, Authors/Folders) and focus mode.
- Maintenance on the settings page: **Re-read all books** (a scan that parses every
  book again, also unchanged ones), clear the content cache, export the annotations
  (as `shelfgauge annotations export`), clear "Currently reading".
- *About* on the settings page: version, library and config paths, the sizes of the
  index, the annotations database and the caches, the last scan, and unknown keys in
  `config.toml`.

### Changed

- The body limit of annotation writes follows the current `max_note_length`.
- On narrow screens the theme menu leaves the top bar (it is on the settings page and
  in the reader's **Aa** panel; `t` still switches the theme).

## [0.12.0] - 2026-10-17

### Added

- **Library statistics** (`/stats`, linked from the library page): books, authors,
  series, genres and the size on disk (with the part in zip archives); bars for
  formats, languages, genres, the top series and authors; the state of the index (the
  last scan, books that could not be read, DRM, duplicates, books without a cover or an
  author); reading and quotes (books in progress and finished, bookmarks, quotes and
  the latest three). Every figure with a list behind it links to it.
- Book lists behind the figures: `/books?format=`, `?language=`, `?genre=`,
  `?missing=cover` and `?missing=author`, by title, with pages.
- **Duplicates** (`/duplicates`), reported and never merged or hidden: identical
  copies (the same content in several files, across plain files and zip archives) and
  books that are possibly the same book (the same title and authors, different
  contents, e.g. an FB2 and an EPUB).
- **Books that could not be read** (`/errors`): the web version of
  `shelfgauge errors`, with the books protected by DRM listed separately.
- FB2 genres are shown by name ("Fantasy" for `sf_fantasy`) on the book card and the
  statistics, and link to the list of the genre's books; unknown codes and EPUB
  subjects are shown as they are.
- The library page shows the books added last with their covers, one line of figures
  (books, authors, size on disk) and a notice when books could not be read, are
  protected by DRM or have identical copies.

### Changed

- The library page is reworked: "Currently reading" shows cards with the cover, a
  progress bar and a **Continue** button; an empty library shows the supported formats
  (FB2, EPUB, TXT, `.fbz`, optional HTML) and the Rescan button; the keyboard
  shortcuts (updated with the keys of 0.11.0) are collapsed under "Keyboard
  shortcuts".
- The index schema is now version 4 (scans history, a CRC32 for plain files, an index
  on titles). **The first scan after the upgrade reads every plain book file once more**
  to compute its CRC32 (files are not parsed again; on a large library on a slow disk
  this takes a while). Older versions of shelfgauge cannot open the upgraded index.

## [0.11.0] - 2026-10-16

### Added

- Search within the open book: in reading mode the top bar's search field (`/` or `s`)
  finds a word or phrase anywhere in the book as you type, ignoring case, `ё`/`е`, soft
  hyphens and line breaks. Results appear in a sidebar tab (shown only while there is a
  query) labelled with a magnifier and the number of matches, grouped by chapter (notes as "Notes"), with the text around
  each match; they lead to the place, where the matches on the page are highlighted.
  `n` / `Shift+N` go to the next or previous result across parts, `Esc` leaves the
  search field and then clears the search. At most 500 results are listed (a note
  after them gives the total). Following a result does not move the reading
  position. Works without JavaScript too (the form
  opens the read page with the results). Full-text search across the library is not
  planned.
- `shelfgauge errors [PATH]` lists the books that could not be read at the last scan,
  one `path: reason` line per book.
- `--trace` as the long form of `-vv` (trace logging).

### Changed

- The Markdown export writes quotes, notes and the "place not found" notice as plain
  text paragraphs instead of `>` block quotes, and replaces no-break spaces with plain
  ones (some editors show them as visible characters).
- The paragraph a link leads to (contents, bookmarks, quotes, the status line's track)
  is highlighted only briefly: the highlight fades out instead of staying.
- Contents marks only the current section (the one at the top of the screen, followed
  while scrolling) instead of every entry of the current part.
- The Quote button on a text selection uses the accent color.
- A book without a table of contents has no Contents tab.
- The Bookmarks tab no longer has a "Bookmark this place" button; `b` and the top-bar
  button bookmark the place.
- The reader's sidebar tabs are a bit smaller, so four fit in a row. The book link and
  the tabs stay at the top while the sidebar scrolls, and `‹ ›` buttons next to the
  book link page through the open tab (like Page Up / Page Down).
- In reading mode the top bar's search field searches the open book (on library pages
  it searches the library as before).

### Fixed

- Plain text books in UTF-16 without a byte order mark (LE or BE) are recognized by
  their line breaks and spaces instead of being misread as a legacy code page. The
  content cache is rebuilt (`PARSER_VERSION` 4).

## [0.10.0] - 2026-10-15

### Added

- HTML books (`.html`, `.htm`, `.xhtml`), plain and inside `.zip` archives, when
  `"html"` is added to `[scan] formats` (off by default, as most HTML files are not
  books). Pages are parsed like a browser does (html5ever), in the encoding of their
  `<meta charset>` (checked against the bytes) or a detected one (windows-1251, KOI8-R
  ...). Scripts, styles, forms, page numbers and link-only navigation are left out. A
  lib.ru page holding the whole text in a `<pre>` is read like a text file
  (paragraphs, verse, volumes and chapters). Sections come from the headings, with
  part-like headings (`BOOK ONE`, `ЧАСТЬ ПЕРВАЯ`) above chapters of the same level;
  links within the page work. Title, authors, language and description come from
  Dublin Core metas, `<meta name="author">`/`"description"`, `<html lang>` or the
  `<title>` (`Author. Title`, Project Gutenberg titles). Pages with less than 5 000
  characters of text or mostly links are not books. Pictures are not shown yet.

### Changed

- The XHTML conversion of EPUB moved to a module shared with HTML; EPUB books are read
  exactly as before.

## [0.9.0] - 2026-10-15

### Added

- Plain text books (`.txt`), plain and inside `.zip` archives, indexed by default. The
  encoding is detected (UTF-8 with or without a BOM, UTF-16 with a BOM, windows-1251,
  KOI8-R, CP866 and other code pages). Paragraphs are found per file: indented
  paragraphs (lib.ru), blank-line separated hard-wrapped blocks (Project Gutenberg) or
  one paragraph per line; verse keeps its line breaks and `* * *` lines become rules.
  Lines such as `Глава 1`, `CHAPTER IV`, `ЧАСТЬ ПЕРВАЯ` or `FIRST EPILOGUE` become
  chapters and parts (a table of contents made of such lines stays text); a book
  without them is split by size for reading. The title, and the author for
  `Author - Title.txt`, come from the file name.
- Zipped FB2 books: `.fbz` files, and `.fb2` files that are really zip archives.
- `[scan] formats` lists the formats to index (default `["fb2", "epub", "txt"]`); books
  of a format taken off the list are removed at the next scan and come back with their
  ids when it is listed again.
- Files that are not books — text files under 2 000 characters or named like
  `readme.txt`, `license.txt` — are skipped without an error; `shelfgauge scan` reports
  them as "not books".

### Changed

- Books that failed to parse are parsed again once by the new parser version (zipped
  `.fb2` files that used to fail now open).

## [0.8.0] - 2026-10-15

### Added

- EPUB books (EPUB 2.0.1 and 3.x), as plain `.epub` files and inside `.zip` archives:
  metadata (title, authors with their sort names, series from EPUB 3 collections or
  Calibre, subjects as genres, the description as annotation, language, cover), and
  the text in the reader. The table of contents comes from the navigation document
  (the NCX in EPUB 2), else from the headings; a chapter whose heading is missing gets
  its title from the table of contents. Footnotes (`aside` and other elements marked as
  notes), endnote sections and non-linear documents are collected in a notes section
  after the text, and note references and links between the book's files lead to the
  right place. Lists (also nested), preformatted text, horizontal rules and line breaks
  are shown; stylesheets, embedded fonts and scripts are not used, and only raster
  images are shown (never SVG). Real-world XHTML is accepted: HTML named entities such
  as `&nbsp;`, a DOCTYPE, an undeclared `epub:` prefix, unbalanced tags.
- DRM-protected EPUBs (Adobe ADEPT, Readium LCP, Apple FairPlay) are indexed with their
  readable metadata and get the status "Protected by DRM": marked "DRM" in lists,
  explained on the book card and in the reader, never opened, never decrypted, not
  counted as broken and not parsed again on upgrades. EPUB font obfuscation is not DRM.

### Changed

- The index gets schema version 3 (the `protected` status, and the formats each archive
  was listed for); the update runs on the first start and keeps the index. The first
  scan after it lists every `.zip` archive again (without unpacking known books), so
  EPUB books in archives indexed before are found. The content cache is rebuilt (`PARSER_VERSION` 2);
  block numbers of FB2 books do not change, so links, bookmarks and quotes keep working.

### Fixed

- Browser reader views (Firefox Reader View and the like) no longer take an epigraph's
  or a poem's author line for the book's author and cut it out of the text: the
  reading page names the title and authors in `<meta>` tags, and the line's class no
  longer contains `author`.
- Reading pages carry the book's language (`<html lang>`, from the book's metadata when
  it is a valid tag), so the browser hyphenates the text and reader views read it aloud
  in the right language; the UI around the text stays marked as English.

## [0.7.0] - 2026-10-14

### Added

- Reader settings: an **Aa** panel in the reader's top bar sets the font (sans or a
  serif system font stack with Cyrillic-capable faces first; no bundled fonts), text
  size (14–28px; keys `-`, `+`, `0`), line spacing, column width (up to the full
  window) and alignment (left or justified) of the book text, and shows the color themes
  as swatches; **Defaults** resets them. Settings are kept per browser, applied before
  the page is drawn, and a change keeps the place at the top of the screen.
- Focus mode (`f` or a top-bar button): hides the sidebar and the top bar, which slides
  back on scroll up or at the top edge of the window; `Esc`, `f` or the sidebar button
  leave it. It is remembered across books. A thin running head shows the authors and
  title, a status line the current chapter and the reading progress in the whole book
  (the book card's figure, updated while scrolling); a checkbox in **Aa** turns them off.
  A thin line in the status line maps the whole book: faint chapter ticks, bookmark
  ticks and the current position; hovering shows the percentage and chapter, a click
  (a second tap on touch screens) jumps there, and Back returns. Reading mode accepts
  `?progress=` (ten-thousandths of the main text) for such jumps.
- A `black` theme (true black, for OLED screens).

### Fixed

- Top-bar buttons that need JavaScript (e.g. the bookmark button) are hidden without it.
- A long chapter cut by `reader.max_part_size` no longer ends in a tiny extra piece
  (e.g. "Chapter (3/3)" holding a single line): it is cut into as few pieces as the limit
  allows, of about equal size. In `split = "size"`, a part shorter than a quarter of
  `part_size` (the end of a body, or the rest before a very large block) joins its
  neighbour. Links with `?part=N` may point to a different part; block links (`?at=`),
  bookmarks, quotes and the reading position are not affected.

## [0.6.0] - 2026-10-13

### Added

- Bookmarks and quotes: select text in the reader and choose **Quote** to keep it with
  an optional note; press `b` (or use the top-bar button, or "Bookmark this place" in
  the sidebar) to bookmark the place at the top of the screen. Saved quotes are
  highlighted in the text (CSS Custom Highlight API, with a `<mark>` fallback). The
  text of a quote is immutable, its note editable.
- Reader sidebar tabs **Contents | Bookmarks | Quotes** and two collapsed lists on the
  book card, with note editing and deleting through plain forms; all lists are rendered
  by the server and work without JavaScript.
- Reading position: saved while reading (after scrolling stops and a dwell time; never
  in notes, never for a page opened from a bookmark or mark link, `from=mark`); the book
  card offers **Continue reading** / **Read from the start** and shows the progress.
  `reader.remember_position` turns it off.
- **Currently reading** on the library page: unfinished books, most recently read
  first, with progress and a **Continue** link; each can be marked as finished, and
  **Clear list** empties the list without forgetting where you stopped (a book comes
  back when you read it again). Its length is `library.reading_list_size` (10).
- Annotations are stored in `.shelfgauge/userdata.sqlite` (separate from the index,
  own migrations, UUIDs, soft deletes) and belong to the book's content, so they survive
  rescans, moves and renames. When a book file is replaced or moved between a plain file
  and an archive, they are re-attached when the book is opened if at least 80 % of the
  quotes are found in it (the others are listed as "place not found"). Annotations of a
  book removed from the library are deleted after a complete scan, restored if the book
  comes back and purged after `annotations.removed_book_retention_days`.
- Markdown export, one file per book in `.shelfgauge/annotations/`, kept up to date
  after every change (one way: edits to the files are overwritten);
  `shelfgauge annotations export [PATH]` rewrites all files and removes stale generated
  ones.
- Config: `[annotations]` with `max_text_length`, `max_note_length` (10 000 characters
  each by default) and `removed_book_retention_days` (30); `reader.remember_position`.

## [0.5.0] - 2026-10-12

### Added

- FB2 books with unbalanced tags (a stray close tag such as `</section>`, unclosed
  elements, a truncated end) are repaired and parsed instead of failing; comments,
  CDATA and attribute values are left untouched and all parser limits still apply.
- Books that failed to parse with an older parser are parsed again by the next scan
  (schema v2: `books.parser_version`), so parser improvements reach existing libraries
  without touching the files.
- In-tree mutation fuzzer (`tests/mutation.rs`): corrupted FB2 files through parse →
  split → render and corrupted zip archives through listing and reading must never
  panic; `SHELFGAUGE_FUZZ_ITERATIONS` / `SHELFGAUGE_FUZZ_SEED` for long runs.
- Release workflow: pushing a version tag (`v0.5.0`) checks that it matches
  `Cargo.toml`, runs the tests, builds binaries for Linux, Windows and macOS, creates
  the GitHub release (with a short note linking to `CHANGELOG.md`) and publishes the
  crate to crates.io (needs the `CARGO_REGISTRY_TOKEN` secret).
- `reader.max_part_size` (default 300 000 characters, `0` = off): in every split mode a
  longer part, e.g. a book without chapters, is cut into equal pieces between whole
  blocks ("Title (1/3)", ...) instead of becoming one huge page (a 200 MB book used to
  render as a single 212 MB page).
- The sidebar can be resized by dragging its border (or with `←`/`→` on the focused
  border; double-click restores the default); the width is remembered per browser.
- README: installation, configuration reference (checked against the default config by a
  test), remote access via SSH tunnel or an authenticating reverse proxy, legal note.

### Changed

- The author list and author pages are 3–10 times faster on large libraries (the list
  walks the authors index instead of grouping every book-author link).
- Huge archives are scanned in at most `threads × 4` jobs: an archive with 200 000
  entries took 219 s, now 41 s.
- The index is upgraded to schema version 2 when a library is opened; afterwards 0.4.0
  and older builds refuse it ("created by a newer version").
- Parser log lines name the book (e.g. which book was repaired).

### Security

- Error messages stored for books and shown in the UI no longer contain absolute server
  paths, only library-relative ones.

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
