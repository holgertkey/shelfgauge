# shelfgauge v0.16.0

A single-binary tool that indexes a folder tree of ebooks — FB2, EPUB, plain text and
(optionally) HTML files, plain or inside `.zip` archives — into a local SQLite
database and serves a browsable, searchable web library on `localhost`, with an
in-browser reader.

**Current version:** 0.16.0

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
shelfgauge annotations export [PATH] [--profile NAME]   # rewrite the Markdown export of bookmarks and quotes
shelfgauge profiles list [PATH]                # the profiles: id, name, role, PIN or not
shelfgauge profiles add NAME [PATH] [--admin]  # add a profile (a reader unless --admin)
shelfgauge profiles remove NAME [PATH] [--yes] # delete a profile with its reading data
shelfgauge profiles reset-pin NAME [PATH]      # remove a forgotten PIN
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
  and alignment of the book text. They apply at once, keeping your place in the text,
  and are saved in your profile, so they are the same on every device (settings that an
  older version kept in the browser are taken over once); **Defaults** resets them.
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
- **My profile** (`/profile`): the theme and a reset of the reading settings; with
  several profiles also the name, color, PIN and activity sharing (see
  [Profiles](#profiles)).
- **Settings** (the gear in the top bar, `/settings`): change the configuration from
  the browser (see [Configuration](#configuration)); manage the profiles; reset what
  this browser remembers (the sidebar, focus mode); maintenance: rescan, **Re-read all
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

### Profiles

Several people can share one library, each with their own bookmarks, quotes, reading
places, "Currently reading" list, theme and reading settings; the books, the index and
the configuration are shared.

- With one profile nothing changes: there is no profile choice and no avatar. Your
  data belongs to the *owner* profile ("Me" until you rename it on **My profile**).
- An admin adds profiles under **Settings → Profiles** (name, color, role, optional
  PIN). From then on each browser first asks **Who is reading?**; the choice is
  remembered in the browser (a cookie) until **Switch profile** in the avatar menu at
  the right of the top bar. The admin who adds the second profile stays signed in.
- **Roles**: an *admin* changes the settings, rescans and manages profiles; a *reader*
  reads and annotates and does not see the gear or **Rescan**. The owner is always an
  admin and cannot be deleted.
- **PIN** (optional, 4 to 8 digits): asked for when the profile is chosen. After 5 wrong
  tries the profile is locked for 30 seconds, then longer. Each person can set, change
  or remove their PIN on **My profile** (which also signs out their other devices); an
  admin can remove a forgotten PIN of another profile, and
  `shelfgauge profiles reset-pin NAME` removes the owner's. **A PIN is not a password
  protection**: it keeps family members apart, but anyone with access to the computer
  can read `.shelfgauge/userdata.sqlite`. It is asked only on this computer.
- **Password** (optional): the login from a phone or another computer, see
  [Remote access](#remote-access). Set on **My profile**; an admin can remove another
  profile's, and `shelfgauge profiles remove-password NAME` removes a forgotten one.
- **Colors**: a profile's color is its avatar and the color of links and buttons.
- **Family activity** (off by default): with **Show my reading to the other profiles**
  on **My profile**, the others see the books you are reading and how far you are, on
  the library page and on the book card. Bookmarks, quotes and notes are never shown
  to others.
- The library statistics show the reading figures and quotes of the current profile.
- **Export**: the owner's Markdown files are in `.shelfgauge/annotations/`, those of
  the other profiles in `.shelfgauge/annotations/profiles/<name> (<id>)/` (renamed with
  the profile; kept when a profile is deleted). `shelfgauge annotations export` rewrites
  all of them, `--profile NAME` one profile's.

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
| `server.auth` | `"none"` | Login from other devices: `"none"` serves only this computer; `"password"` lets each profile log in with its own password from a phone or another computer (see [Remote access](#remote-access)). Needed for a `bind` address other than loopback. |
| `server.remote_admin` | `false` | Whether admin profiles may use the settings page, rescans and profile management from other devices (with `auth = "password"`). |
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

On the computer with the library nothing needs to be set up: shelfgauge listens on
`127.0.0.1`, asks no password there, and profiles with a PIN ask for the PIN. To read
from a phone or another computer, turn on the **password login**: each person then logs
into their own profile with its password on other devices. The computer with the
library must be on; shelfgauge has no offline copy of the books.

1. Give an admin profile a password: on **My profile** → **Password**, or with
   `shelfgauge profiles set-password <name>` (asked twice without echo; in a script,
   one line on standard input). Every person who reads from other devices sets their
   own password on **My profile** (at least 10 characters; a few words are easier to
   type on a phone than a jumble).
2. In `.shelfgauge/config.toml`, section `[server]`: `auth = "password"`, plus the
   address or names under which the other devices reach the computer (below). Restart
   shelfgauge; it refuses to start while no admin has a password.

**The PIN is for this computer, the password for other devices.** The two are
independent: a PIN is never accepted from another device, and the password is never
asked on this computer. To lock a profile in both places, set both. A profile without a
password is shown as *On this computer only* on the login page. **Remember this device**
keeps the login for about 400 days; without it the login ends when the browser closes.
**Sign out** in the avatar menu ends it at once, **Sign out on other devices** on **My
profile** ends all others, and changing the password logs out the other devices that
used it. After 5 wrong passwords a profile is locked for 30 seconds, then longer; a
device with its own address is refused for 15 minutes after 20 failed logins on any
profiles. Settings, rescans and profile management stay on this computer unless
`remote_admin = true`.

**What counts as another device.** A request is from another device when it does not
come from a loopback address, when its `Host` is a name from `allowed_hosts`, or when it
carries a header that reverse proxies add (`Forwarded`, `X-Forwarded-For`,
`X-Forwarded-Host`, `X-Real-IP`, `Tailscale-User-Login`). shelfgauge never *trusts*
these headers (a client can send any value): they can only make a request "remote",
never "local". Requests from other devices with `auth = "none"` are refused with a hint.

- **Tailscale or WireGuard** (a private network, simplest): bind to the computer's
  address in that network and keep plain HTTP (WireGuard encrypts the traffic):

  ```toml
  [server]
  bind = "100.101.102.103"          # this computer's Tailscale address
  auth = "password"
  allowed_hosts = ["books-pc.tail1234.ts.net", "books-pc"]   # its MagicDNS names
  ```

  Then open `http://100.101.102.103:8742/` (or the MagicDNS name with the port) on the
  phone. `tailscale serve --bg 8742` works too (HTTPS on the `ts.net` name; add that
  name to `allowed_hosts` and keep `bind = "127.0.0.1"`).
- **Reverse proxy with HTTPS** on the same computer, forwarding to `127.0.0.1:8742`:
  add the public name to `allowed_hosts` (and set `base_path` on a sub-path). With
  [Caddy](https://caddyserver.com/docs/caddyfile/directives/reverse_proxy) (automatic
  HTTPS; it passes `Host` through unchanged):

  ```
  books.example.org {
  	reverse_proxy 127.0.0.1:8742
  }
  ```

  With nginx, pass the original `Host` (nginx replaces it with the upstream address by
  default, and Apache does too unless `ProxyPreserveHost On`):

  ```nginx
  location /books/ {
      proxy_pass http://127.0.0.1:8742;     # no URI after the port: the prefix stays
      proxy_set_header Host $host;
      proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
  }
  ```

  with `allowed_hosts = ["books.example.org"]` and `base_path = "/books"`. In Caddy use
  `handle /books*` for a sub-path (not `handle_path`, which strips the prefix). The
  proxy may still ask for its own login (Caddy `basic_auth`); people then log in twice.
  Behind a proxy on the same computer all clients share its address, so only the
  per-profile lockout applies; add a rate limit in the proxy if the library is public.
  A proxy on *another* machine shares one address budget among its clients.
- **SSH tunnel** (no configuration change): `ssh -L 8742:127.0.0.1:8742 user@server`,
  then open `http://localhost:8742/` locally. The tunnel arrives from loopback, so it
  counts as this computer: only for people who may use it as such.

shelfgauge has no built-in TLS. The `Secure` flag of the login cookie follows the
address the browser uses (`https://` through a proxy, plain `http://` in a private
network).

**Upgrading from 0.15.0**: requests through an `allowed_hosts` name were served without
any login before; now they need `auth = "password"`. Set a password for an admin
profile and `auth = "password"` before you upgrade a library that is used through a
reverse proxy.

Note: making a library reachable by others makes its books available to them; whether
that is allowed depends on the books' licenses and is the operator's responsibility.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the version history.

## License

MIT — see [LICENSE](LICENSE).

---

_shelfgauge v0.16.0 — MIT License_
