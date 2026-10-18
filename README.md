# shelfgauge v0.14.0

A single-binary tool that indexes a folder tree of ebooks — FB2, EPUB, plain text and
(optionally) HTML files, plain or inside `.zip` archives — into a local SQLite
database and serves a browsable, searchable web library on `localhost`, with an
in-browser reader.

**Current version:** 0.14.0

## Status

Early development (0.x): the configuration and the URLs may still change between
versions. `shelfgauge [PATH]` indexes a library of FB2, EPUB,
TXT and (if enabled) HTML books (plain files and `.zip` archives, any number of books per archive) in the background and
serves it as a web library: browse by author or by folder, open a book card (cover,
series, genres, annotation), search by title, author, series or annotation, and read
books in the browser, with search inside the open book. A statistics page shows what
is in the library, the state of the index and duplicates. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design.

## Installation

Released versions are on [crates.io](https://crates.io/crates/shelfgauge), and binaries
for Linux (x86_64), Windows (x86_64) and macOS (Apple Silicon) are attached to the
[GitHub releases](https://github.com/holgertkey/shelfgauge/releases):

```sh
cargo install --locked shelfgauge
```

`--locked` builds with the dependency versions the release was tested with.

Or build from source with a current stable Rust toolchain:

```sh
cargo build --release
# the binary is target/release/shelfgauge (shelfgauge.exe on Windows)
```

## Usage

```sh
shelfgauge [PATH]        # serve the library at PATH (default: current directory)
shelfgauge scan [PATH]   # update the library index and exit
shelfgauge errors [PATH] # list the books that could not be read, with the reason
shelfgauge annotations export [PATH]   # rewrite the Markdown export of bookmarks and quotes
```

Options: `--port <PORT>`, `--bind <ADDR>`, `--no-scan` (serve without scanning first),
`-v`/`--verbose` (debug logging), `-vv`/`--trace` (trace logging with the duration of
each step; without these `RUST_LOG` is used), `--log-format text|json` (see
[Logs](#logs)), `-V` (version).

A new library is created only when PATH is given explicitly (e.g. `shelfgauge .`): its
working data goes into `.shelfgauge/` inside PATH (configuration, index, cover and
content caches), so the library stays portable with its books. Without PATH, the current
directory must already be a library.

`shelfgauge PATH` prints its address (`http://127.0.0.1:8742/` by default) and scans the
library in the background while serving the existing index. Only changed files are read
again; moved or renamed books keep their identity. Ctrl+C stops the server; a running
scan keeps what it has indexed so far, and the next scan completes it.

Books that could not be read are counted as errors in the scan summary and marked
`error` in the library lists. `shelfgauge errors` lists them, one `path: reason` line
per book (`archive.zip/entry.fb2` for a book inside an archive). Files that could not be
opened at all (I/O errors) are not recorded: `scan` logs them as warnings and reads them
again at the next scan.

### Logs

Logs go to stderr: warnings and a line per scan by default, more with `-v`. Each HTTP
request runs under an id; with `-v` it ends with a line like

```
2026-10-18T07:37:27.581520Z DEBUG request{id=e method=GET route=/book/{id}/read}: shelfgauge::web::trace: request status=200 latency_ms=3.175
```

Failed requests (5xx) are logged as warnings and requests slower than a second as
"slow request" even without `-v`; lines logged while a request is handled (reading a
book, a repaired file) carry the same `request{…}` prefix. The "Internal error" page
shows the request's error id, so the matching lines can be found in the log. Logs show
the route (`/book/{id}/read`), never the query string, the request body or credentials;
books are named by id or library-relative path.

`--log-format json` writes one JSON object per line (with the request and book spans
as fields) for log collectors. Colours are used only on a terminal and never when
`NO_COLOR` is set. There are no log files: under systemd the journal keeps the log
(`journalctl -u shelfgauge`), in Docker the container log. A minimal unit:

```ini
[Unit]
Description=shelfgauge library
After=network.target

[Service]
ExecStart=/usr/local/bin/shelfgauge /srv/books
User=books
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

### Formats

- **FB2** (FictionBook 2), in any of the usual encodings; also zipped as `.fbz` (or a
  `.fb2` file that is really a zip).
- **EPUB** 2 and 3. The table of contents comes from the book's navigation document
  (NCX in EPUB 2), footnotes and endnotes are collected after the text, and links
  inside the book work. Stylesheets, embedded fonts and scripts are not used: the
  reader shows the text in its own typography.
- **TXT**: plain text in UTF-8 (with or without a BOM), UTF-16 with a BOM, or a legacy
  code page (windows-1251, KOI8-R, CP866 and the other common ones are recognized).
  Paragraphs are found per file — indented paragraphs (as on lib.ru), blank-line
  separated hard-wrapped blocks (as on Project Gutenberg), or one paragraph per line —
  and verse keeps its line breaks. Lines such as `Глава 1`, `CHAPTER IV` or
  `ЧАСТЬ ПЕРВАЯ` become chapters and parts; a book without them is split by size for
  reading. The title (and the author, for `Author - Title.txt`) comes from the file
  name. Text files that are short (under 2 000 characters) or named like `readme.txt`,
  `license.txt` are not books: they are skipped, not listed as errors.
- **HTML** (`.html`, `.htm`, `.xhtml`), **off by default** — most HTML files in a folder
  are pages, not books. Add `"html"` to `scan.formats` for a library of single-file HTML
  books (lib.ru pages, Project Gutenberg HTML, saved books). Pages are read like a
  browser does (tag soup included), in their declared encoding (`<meta charset>`) or a
  detected one; scripts, forms and link-only navigation are left out, and a lib.ru page
  whose whole text is in one `<pre>` is read like a text file. Chapters come from the
  headings; title, author and language from the Dublin Core metadata or the `<title>`.
  Pictures are not shown yet. Pages under 5 000 characters of text, or mostly links,
  are not books.
- **DRM**: books protected by DRM (Adobe ADEPT, Readium LCP, Apple FairPlay) are listed
  with their metadata and marked "DRM", but shelfgauge does not open them — read them in
  the app of the store you got them from. EPUB font obfuscation is not DRM; such books
  open normally.

### In the browser

- The **library page** shows the books you are reading (cover, progress, **Continue**,
  **Mark as finished**), the books added last, one line of figures (books, authors,
  size on disk) with a link to the statistics, and a short notice when something needs
  attention: books that could not be read or are protected by DRM, and books with
  identical copies. An empty library shows the supported formats and **Rescan**.
- **Library statistics** (`/stats`): books, authors, series, genres and the size on
  disk (and how much of it is in zip archives); bars for formats, languages, genres, the
  top series and authors; the state of the index (the last scan, failures, DRM,
  duplicates, books without a cover or an author); and reading and quotes (books in
  progress and finished, bookmarks, quotes and the latest three). Every figure with a
  list behind it is a link: `/books?format=epub`, `?language=ru`, `?genre=<code>`,
  `?missing=cover` or `?missing=author`. FB2 genre codes are shown by name ("Fantasy");
  EPUB subjects as they are. The figures are computed once after each scan.
- **Books that could not be read** (`/errors`): each with its path in the library and the
  reason, and the books protected by DRM; fix or replace the files and rescan.
- **Duplicates** (`/duplicates`) are listed, never merged or hidden: *identical copies*
  (the same content in several files, e.g. a book loose and inside a zip, by size and
  CRC32) and books that are *possibly the same book* (the same title and authors,
  different contents, e.g. an FB2 and an EPUB). Books without an author are not
  matched by title alone.
- The sidebar lists the library **by author** or **by folder** (zip archives are shown as
  folders); the choice is remembered per browser.
- A book card shows the cover and metadata; **Read** opens the book. Books are split into
  parts as configured in `[reader]`, with the table of contents in the sidebar.
- A link to `/book/<id>/read?at=b<n>` opens a book at a given block; block numbers do not
  depend on how the book is split.
- The border between the sidebar and the main area can be dragged (or moved with `←`/`→`
  when focused) to resize the sidebar; double-click restores the default width. The
  width is remembered per browser.
- Themes: auto (system), light, dark, sepia, navy, black.
- While reading, the **Aa** button (top bar) sets the theme, the font (sans or serif,
  from the fonts installed on your system), the text size, line spacing, column width
  and alignment of the book text. Settings are kept per browser and apply at once,
  keeping your place in the text; **Defaults** resets them.
- **Focus mode** (`f`, or the button next to **Aa**) hides the sidebar and the top bar;
  the top bar comes back when you scroll up or move the mouse to the top edge. A thin
  running head shows the author and title, and a status line the current chapter and
  how much of the book you have read (both can be turned off in **Aa**). Between them a
  thin line maps the whole book: ticks for chapters and bookmarks, and a dot for where
  you are. Pointing at the line shows the place (percentage and chapter); a click goes
  there (on a touch screen, tap twice), and Back returns. `Esc` or `f`
  leaves it, and it stays on for the next book until you do.
- **Search in the book**: while reading, the search field in the top bar (`/` or `s`)
  finds a word or phrase anywhere in the open book, as you type (at least 2
  characters); the results appear in a new sidebar tab marked with a magnifier and
  the number of matches. Case, `ё`/`е`,
  soft hyphens and line breaks do not matter; it matches any part of a word, so a word
  start finds its forms too. Results are listed by chapter with the text around each
  match (at most 500; refine the query for more) and lead to the place, where every
  match on the page is highlighted. `n` / `Shift+N` go to the next or previous result,
  also across parts; `Esc` leaves the search field, a second `Esc` clears the search
  and closes the tab. Following a result does not move your reading position. The
  library search (on library pages) covers titles, authors, series and descriptions,
  not the books' text.
- While reading, the book link and the tabs stay at the top of the sidebar; the `‹` `›`
  buttons next to the book link page through the open tab (contents, bookmarks, quotes
  or search results).
- Keys (also under **Keyboard shortcuts** on the library page): `/` or `s` search, `t`
  next theme, `m` sidebar mode, `Esc` leave the search field; while reading, `/` or `s` search in the book, `n` / `Shift+N` next / previous
  result, `←`/`→` previous/next part, `b` bookmark, `f` focus mode, `-`/`+`
  text size (`0` resets it); in the **Quote** dialog `Ctrl+Enter` saves, `Esc` cancels.
- **Rescan** (top bar) updates the index while the library stays usable.
- **Settings** (the gear in the top bar, `/settings`): change the configuration from
  the browser (see [Configuration](#configuration)); reset what this browser remembers
  (theme, reading settings, the sidebar, focus mode); maintenance: rescan, **Re-read all
  books** (parses every book again, also unchanged ones, e.g. after an update of
  shelfgauge), clear the content cache, export the annotations, clear "Currently
  reading"; and facts about the library (version, paths, sizes, the last scan, unknown
  keys in `config.toml`).

### Bookmarks, quotes and the reading position

- While reading, select text and choose **Quote** to keep it as a quote, with an optional
  note. Press `b` (or the bookmark button in the top bar) to bookmark the place at the
  top of the screen. Saved quotes are highlighted in the text.
- The reader's sidebar has three tabs: **Contents**, **Bookmarks** and **Quotes** (a book
  without a table of contents has no Contents tab). A book
  card lists its bookmarks and quotes too, where notes can be edited and entries deleted.
  A quote's text cannot be changed; a selection lies within one part of the book.
- The reading position is saved as you read (after you stay at a place for a few
  seconds; never in footnotes, and not while you look at a bookmark or quote). The book
  card then offers **Continue reading** and shows how much you have read.
- The library page lists the books you are **currently reading** (unfinished, most
  recently read first) with a **Continue** link. The ✓ icon marks a book as finished;
  **Clear list** empties the list but keeps where you stopped, and a book comes back
  once you read it again.
- Bookmarks and quotes belong to the book's content: they survive rescans, moves and
  renames. If a book file is replaced (e.g. a corrected edition), they are moved to it
  when it is opened and most of the quotes are found in it; quotes that are not found
  are marked "place not found". Quotes of a book removed from the library are deleted
  after `annotations.removed_book_retention_days`, unless it comes back.
- Everything is stored in `.shelfgauge/userdata.sqlite`. Unlike the rest of
  `.shelfgauge/`, it cannot be rebuilt by rescanning: **back it up** (copy it while
  shelfgauge is not running).
- For each book with bookmarks or quotes, a Markdown file is kept up to date in
  `.shelfgauge/annotations/` (`<Authors> — <Title> (<key>).md`): bookmarks, then the
  quotes by chapter with their notes and links back into the reader. The export is
  **one way**: edits to these files are overwritten and never read back.
  `shelfgauge annotations export [PATH]` rewrites all of them (e.g. after restoring
  `userdata.sqlite` from a backup) and removes stale generated files; other files in the
  folder are left alone.

Books whose XML has unbalanced tags (e.g. a stray `</section>`) are repaired when they
are parsed; books that cannot be read are listed with the reason on their card.

## Configuration

`.shelfgauge/config.toml` is created with commented defaults on first run. Missing keys
use their defaults, unknown keys are reported as warnings, and invalid values stop the
program with a message naming the key. `--port` and `--bind` override the file.

All sections but `[server]` can also be changed on the **Settings** page. It edits the
file in place, keeping your comments, key order and unknown keys, writes only the keys
you changed, and checks the new values the way the program does at startup, so a save
never produces a file the next start rejects. If the file was changed since the page
was opened (by hand, in another tab), the save is refused and the form shows the file
as it is now. When changes take effect:

| Section | Takes effect |
|---|---|
| `[library]` | At once (the next page). |
| `[reader]` | At once: parts are made when a page is opened; links to places in books keep working. |
| `[annotations]` | At once: the limits for the next quote or note, the retention at the next cleanup after a scan. |
| `[scan]` | At the next scan. The page tells how many books a change would remove from the index (a format turned off, a new exclude pattern) and offers **Rescan now**; no scan starts by itself. |
| `[server]` | Only in `config.toml`, after a restart: changing the address or port from the browser could cut off access to the page itself. The page shows the running values and which command-line options override them. |

| Key | Default | Meaning |
|---|---|---|
| `server.bind` | `"127.0.0.1"` | Address to listen on. Only loopback addresses are accepted (there is no built-in authentication; see [Remote access](#remote-access)). |
| `server.port` | `8742` | Fixed port. If it is busy, shelfgauge exits with an error instead of picking another one: the port is part of the browser origin. |
| `server.allowed_hosts` | `[]` | Extra host names accepted in the `Host` and `Origin` headers, e.g. `["books.example.org"]` behind a reverse proxy. An entry without a port matches any port. |
| `server.base_path` | `"/"` | URL prefix when served on a sub-path of a reverse proxy, e.g. `"/books"` (letters, digits, `-`, `.`, `_`, `~`, `/`). |
| `scan.exclude` | `[]` | Gitignore-style patterns of paths not to index, in addition to `.shelfgauge/`. |
| `scan.formats` | `["fb2", "epub", "txt"]` | The book formats to index: `"fb2"` (with `.fbz`), `"epub"`, `"txt"`, `"html"` (not listed by default, see [Formats](#formats)). Books of a format taken off the list are removed from the library at the next scan (their quotes and notes are kept as for any removed book). |
| `library.page_size` | `100` | Entries per page of the author list, folders and search results. |
| `library.reading_list_size` | `10` | Books shown under "Currently reading" on the library page (most recently read first); `0` hides the list. |
| `reader.split` | `"section"` | How a book is split into parts: `"none"` (one part), `"section"` or `"size"`. |
| `reader.section_depth` | `1` | For `"section"`: split at sections down to this depth (1 = top level). Very short untitled parts (a copyright notice, an epigraph) join the next part. |
| `reader.part_size` | `50000` | For `"size"`: approximate number of characters per part (cut between whole blocks; a short rest joins the previous part). |
| `reader.max_part_size` | `300000` | The longest part in characters, in every mode: a longer part (e.g. a book without chapters) is cut into as few equal pieces as the limit allows, between whole blocks, titled "Title (1/3)", ... `0` disables the limit. |
| `reader.remember_position` | `true` | Remember where you stopped reading each book: the book card offers "Continue reading" and shows the progress. `false` stops saving and hides both (saved positions are kept). |
| `annotations.max_text_length` | `10000` | The longest text that can be saved as a quote, in characters (about 3–4 book pages); `1`–`1000000`. |
| `annotations.max_note_length` | `10000` | The longest note, in characters; `1`–`1000000`. Lowering a limit never changes saved quotes. |
| `annotations.removed_book_retention_days` | `30` | Days to keep the bookmarks and quotes of a book that was removed from the library, in case it comes back (a moved folder, an unmounted drive); `0` deletes them at the next scan. |

## Remote access

shelfgauge listens on `127.0.0.1` and has no authentication, so it refuses a non-loopback
`bind`. It accepts requests only for the host names `localhost`, `127.0.0.1` and `[::1]`
with its port, plus `server.allowed_hosts` (protection against DNS rebinding), and every
state-changing request (rescan) must come with a matching `Origin` (CSRF protection).

- **SSH tunnel** (simplest): `ssh -L 8742:127.0.0.1:8742 user@server`, then open
  `http://localhost:8742/` locally. No configuration change is needed.
- **Reverse proxy with HTTPS** on the same host, forwarding to `127.0.0.1:8742`. The proxy
  must add authentication. Add the public host name to `server.allowed_hosts` (and set
  `server.base_path` if the library is served on a sub-path). For example with
  [Caddy](https://caddyserver.com/docs/caddyfile/directives/basic_auth) (automatic HTTPS;
  create the password hash with `caddy hash-password`):

  ```
  books.example.org {
  	basic_auth {
  		reader <hashed password>
  	}
  	reverse_proxy 127.0.0.1:8742
  }
  ```

  and in `.shelfgauge/config.toml`: `allowed_hosts = ["books.example.org"]`. On a
  sub-path, set `base_path` (e.g. `"/books"`) and forward the prefix unchanged: in Caddy
  use `handle /books*` (not `handle_path`, which strips it); in nginx use
  `location /books/ { proxy_pass http://127.0.0.1:8742; }` (no URI after the port). With nginx,
  `proxy_pass http://127.0.0.1:8742;` works the same way (nginx sends the upstream
  address as `Host` unless `proxy_set_header Host $host;` is set; either is accepted
  when the public name is in `allowed_hosts`).
- **Private network** (Tailscale, WireGuard): combine it with an SSH tunnel or a reverse
  proxy as above, since shelfgauge itself only listens on loopback.

Note: making a library reachable by others makes its books available to them; whether
that is allowed depends on the books' licenses and is the operator's responsibility.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the version history.

## License

MIT — see [LICENSE](LICENSE).

---

_shelfgauge v0.14.0 — MIT License_
