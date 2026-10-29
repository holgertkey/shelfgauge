# shelfgauge v0.21.0

A single-binary tool that indexes a folder tree of ebooks — FB2, EPUB, plain text and
(optionally) HTML files, plain or inside `.zip` archives — into a local SQLite
database and serves a browsable, searchable web library on `localhost`, with an
in-browser reader.

**Current version:** 0.21.0

## Status

Early development (0.x): the configuration and the URLs may still change between
versions. `shelfgauge [PATH]` indexes a library of FB2, EPUB,
TXT and (if enabled) HTML books (plain files and `.zip` archives, any number of books per archive) in the background and
serves it as a web library: browse by author or by folder, open a book card (cover,
series, genres, annotation), search by title, author, series or annotation, and read
books in the browser, with search inside the open book. A statistics page shows what
is in the library, the state of the index and duplicates. See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design.

**Reading on phones, for the whole family**: put the library on a [hub](#running-a-hub)
(self-hosted, end-to-end encrypted) and press **Read on phone** on a book card: one QR
code opens the book on your phone and signs the phone in. **Invite someone** (Settings → Profiles) gives
a link that works from anywhere; the person picks an address like `grandma@smiths` and a
password, reads every book the library sends, offline too, and their bookmarks and
notes sync with the library.

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
shelfgauge hub [--data DIR]                    # run a hub (see Running a hub)
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
- **Notes**: a note link (`[1]`) shows the note in a panel at the bottom, without
  leaving the page; **Open in notes** goes to the notes. After that, or after any other
  link in the text to another place in the book, **Back to text** in the status line
  (and above the text) returns to where you were reading.
- **Turning pages on a phone or tablet**: tap the right side of the screen or swipe to
  the left for the next page, tap the left side or swipe to the right for the previous
  one; a tap in the middle shows or hides the top bar. Scrolling with a finger works as
  usual. The text slides briefly in the direction of the turn, and the line you read
  last stays in view, lit up for a moment. In focus mode a page at rest shows whole
  lines only: a line cut by the bottom edge is hidden until it opens the next page. At the end of a chapter the next one opens,
  at its start the previous one at its end. Links and a selection are left alone, and a
  tap just beside a link (such as a tiny note reference) opens the link.
- **Focus mode** (`f`, or the button next to **Aa**) hides the sidebar and the top
  bar; the top bar comes back when you scroll up or move the mouse to the top edge, and
  stays on a page shorter than the window. On a phone focus mode is always on: the
  sidebar button opens the sidebar over the text, and a jump from it closes it again.
  On a computer the browser remembers the choice. A thin
  running head shows the current chapter (before the first one the author and title),
  and a status line how much of the book you have read (both can be turned off in
  **Aa**). In the status line a thin line maps the whole book: ticks for chapters and
  bookmarks, and a dot for where you are. Pointing at the line shows the place (percentage and chapter); a click goes
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

All sections can also be changed in the browser: `[server]` and `[hub]` on **Settings →
Server and hub** (on this computer only), the others on the **Settings** page. It edits the
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
| `[server]` | After a restart: **Settings → Server and hub** saves the address, port, other host names, the login from other devices and remote admin, then offers **Restart shelfgauge** (the program starts again with the same command line). Only on this computer, never from another device; `base_path` stays in `config.toml`. The page shows the running values and which command-line options override them. |
| `[hub]` | After a restart, on the same page; the hub's address is locked while the library is on its hub (delete it there first). |

| Key | Default | Meaning |
|---|---|---|
| `server.bind` | `"127.0.0.1"` | Address to listen on. Another address (e.g. `"0.0.0.0"` for the home network) needs `auth = "password"`; see [Remote access](#remote-access). |
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
| `hub.url` | `""` | The [hub](#running-a-hub) for reading on phones, e.g. `"https://hub.example.org"`; empty = none. HTTPS is required (`http://` only for `127.0.0.1`, `::1` or `localhost`); certificates are checked against the computer's trust store. It is used when the library is put on the hub; it cannot change while the library is there. |
| `hub.instance_name` | `""` | The name of this library on your other devices; empty = the library folder's name. |
| `hub.sync_interval_minutes` | `5` | Minutes between syncs with the hub; `1`–`1440`. |

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
2. On **Settings → Server and hub** (on this computer), or in `.shelfgauge/config.toml`,
   section `[server]`: `auth = "password"`, plus the address or names under which the
   other devices reach the computer (below). Restart shelfgauge (**Restart shelfgauge**
   on that page); it refuses to start while no admin has a password.

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
- **Home Wi-Fi** (for trying it out; the traffic is not encrypted): listen on all
  addresses and add the computer's address in the network, so the computer itself
  keeps using `http://127.0.0.1:8742/` without a password:

  ```toml
  [server]
  bind = "0.0.0.0"
  auth = "password"
  allowed_hosts = ["192.168.1.105"]   # this computer's address (hostname -I)
  ```

  Then open `http://192.168.1.105:8742/` on the phone. With `bind` set to that address
  instead, `127.0.0.1` is not served at all. Everyone in the network reaches the login
  page; open the port in the firewall if needed (`sudo ufw allow 8742/tcp`).
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

## Running a hub

A hub keeps encrypted copies of books and of reading data (bookmarks, quotes, the
reading position) for the phones and other devices of one or more libraries, so books
can be read when the computer with the library is off. It never sees a title, a line
of text or a password: everything is encrypted before it is uploaded, and the keys
stay on the library and on its members' devices. The devices read in the hub's reader
app (see **Reading on your phone** below).

```sh
shelfgauge hub --data /srv/shelfgauge-hub     # creates the directory and hub.toml
shelfgauge hub invite --data /srv/shelfgauge-hub
```

The data directory holds `hub.toml` (settings, restart after editing), `hub.sqlite`
and `blobs/`. Without `--data`, the current directory must already be a hub. A library
puts itself on the hub with a one-time **invite code** from `shelfgauge hub invite`
(valid 7 days), unless `[libraries] registration = "open"`. Other commands:
`shelfgauge hub libraries` lists the libraries by internal number with their accounts,
devices and usage (no handles, names or secrets), `shelfgauge hub takedown
<publication-id>` deletes a publication (the id from its `/r/<id>` link) and refuses
that id from now on, `shelfgauge hub delete-library <number>` deletes a library with
its accounts and everything they store, and `shelfgauge hub quota <number> --max-bytes
2GiB` gives one library its own quotas (also `--max-publications`; `0` is no limit,
`default` the hub's value; without options it shows them).

The hub listens on `127.0.0.1:8743` and has no TLS of its own: **put it behind a
reverse proxy with HTTPS**, which phones also need for offline reading. With Caddy:

```
hub.example.org {
	reverse_proxy 127.0.0.1:8743
}
```

and in the `[server]` section of `hub.toml` `allowed_hosts = ["hub.example.org"]` and
`trusted_proxies = ["127.0.0.1", "::1"]`, so the hub counts its limits per client
instead of per proxy (with nginx, pass `proxy_set_header Host $host;` as for a library
and `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`). Settings in
`hub.toml`:

- `[libraries]`: `registration` (`"invite"` or `"open"`), `max_libraries` (0 = no
  limit), `invite_days` (7), `new_libraries_per_address_per_day` (3, with open
  registration), `expiry_days` (an account none of whose own devices made a request
  for that many days is deleted, and a library none of whose devices nor the library
  itself was seen; 0 = never, the default), `member_invite_max_days` (7: how long the
  invites libraries give their members stay valid at most);
- `[quotas]`: `max_bytes` and `max_publications` per library (its books and all its
  members' marks; 0 = no limit, the default), `max_devices` per account (5);
- `[limits]`: `max_blob_bytes` (one part or image of a book, 64 MiB),
  `max_total_bytes` (all libraries together; beyond it the hub takes no new books or
  marks; 0 = no limit, the default), `publication_expiry_days` (a book not opened for
  that many days is deleted; 0 = never, the default), `requests_per_minute` per
  device (600) and `public_requests_per_hour` (60) for creating libraries, signing in
  and redeeming invites per client address (an IPv6 address counts by its /64);
  wrong passwords slow down further attempts for that address, but never lock it;
- `[passwords]`: `memory_kib` (19456) and `iterations` (2), the Argon2id cost of new
  and changed passwords; they can be raised, never lowered below these values (the
  reader refuses weaker ones);
- `[server] trusted_proxies`: addresses or ranges (`10.0.0.0/8`) of reverse proxies
  whose `X-Forwarded-For` names the client;
- `[operator]`: `name` and `contact` (an e-mail address or `https://` URL for abuse
  reports) and `terms_file` (`terms.txt` in the data directory, plain text), shown at
  `/terms` and on the reader's My account page.

**Upgrading from 0.18–0.20**: the accounts of those versions are not carried over
(books are published again by the library, and every member gets an invite). Upgrade
**the library first**, start it once with the old hub still running (it pulls the
marks made on phones one last time), **then the hub**: on a database with old accounts
it refuses to start, says what is there and offers `shelfgauge hub migrate --data
<dir>`, which deletes the old accounts, devices, publications and marks (takedowns
stay). An `[accounts]` section in `hub.toml` must be renamed to `[libraries]`
(`max_accounts` → `max_libraries`, `new_accounts_per_address_per_day` →
`new_libraries_per_address_per_day`, `account_expiry_days` → `expiry_days`). Phones
that read through the old hub forget their old pairing and say so.

**Putting the library on the hub**: on **Settings → Server and hub** (on the library's
computer) enter the hub's address and restart; then **My profile → Phone and other
devices → Put the library on the hub** asks for the library's **handle** and your
name. Together they make your address on the hub, e.g. `olga@smiths`: names and handles
are 3 to 32 Latin letters, digits, dots, dashes or underscores. The handle can be
changed on Settings → Server and hub (every member's address changes; their devices
stay signed in). **Delete the library on this hub** removes it with all its accounts
and books there (nothing on the computer).

**Read on phone**: the book card has **Read on phone** for admins. The library
uploads an encrypted copy (DRM-protected books never leave the library), shows the
progress, and then the book's link and a QR code. Scanned with your own phone the
first time, it also signs the phone in to your account (later: **Pair a new phone
with this book**). The link opens on the devices of every member of the library and
nowhere else; send it by any means. **Delete for everyone** deletes the hub's copy
for every member (their readers then say it is no longer on the hub); the library
keeps its own marks. A copy deleted by
the hub's `publication_expiry_days` shows as expired with **Publish again**.

**Invite someone**: **Settings → Profiles → Invite someone** asks only a name (e.g.
"Grandma"), adds a profile for that person and shows a link and a QR code that work
from any network. The person opens it, chooses the name part of their address
(`grandma@smiths`), a password (at least 10 characters; a passphrase is easiest) and
how they appear, and reads at once. They sign in on up to 5 devices (each browser is
one) with the address and the password, and manage those devices themselves in **My
account**. The invite page tells them what you can do: this computer holds the keys of
every account of the library, so you could read their notes, and you can **Reset
password** (a new link: they keep their address, books and notes; you never learn the
password). **Sign out all devices** is for a lost phone; **Remove** deletes the
person's account on the hub and the profile here, and their devices drop the library's
books at the next connection. If someone may have kept a copy of the library's key,
**Change library key** (My profile → Phone and other devices) seals new books under a
new one; members get it automatically. Every member can open every book the library
has put on the hub. People invited this way read only through the hub: they are not
offered on this computer's profile picker.

**Syncing**: the library syncs the bookmarks, quotes, notes and reading positions of
every account's books with the hub in the background: soon after a change and every
`sync_interval_minutes` (and **Sync now**). Each person's marks stay theirs; the
newest change wins, also a deletion. Marks of a book the library no longer has wait
there while the book is on a shelf and attach when it appears.

**Reading on your phone**: open a book link or an invite on the phone, or the hub's
address and **Sign in**. The shelf shows the books you opened, those you are reading
first. Any modern browser works; the hub must be reached over HTTPS (the browser's
encryption and offline storage need it). A book reads as in the library: the same
text layout, contents, notes in a panel, focus mode with the status line, reading
settings (kept on the phone), search in the book, bookmarks and quotes with notes; the
reading position is saved and continues on whichever device you stopped. A book once
opened is kept on the phone (encrypted) and opens without a connection, also when the
hub is down; marks made meanwhile sync when it is back. A person in several libraries
signs in to each (**Add a library** in My account); the shelf then has a library
switcher and **All shelves**. A browser shared by two people should be signed out of
all accounts before it changes hands.

On an iPhone or iPad, **add the reader to the home screen** (Share → Add to Home
Screen): Safari deletes the data of sites not visited for a week, which would sign the
browser out and drop its copies, while a home screen app keeps them (your marks are
safe in the library either way; sign in again if it happens).

**My account** (the button at the right of the top bar on My books) shows each
account's address and devices (**Remove**; a new device is announced on the others),
**Change password**, **Change name** and **Leave library** (these ask the current
password), what the phone keeps offline (**Remove downloaded books** frees the space),
the hub's version, and who runs it with their contact and terms of use when the hub
names them. **Sign out of this library** sends the marks made on the phone and removes
the library's books, marks and keys from the browser (**Sign out of all** before
handing the browser to someone else). A forgotten password: ask the library's admin for **Reset password**.

**Book files** (`.sgbook`): instead of putting a book on the hub, **Download book
file** on the book card writes it into one file that you pass by any means
(messenger, e-mail, USB, a synced folder) and open in the reader with **Open file** on
My books, or by dropping it on the page. The hub keeps no copy; marks and position
still sync through it. The file opens on the devices of every member of the library,
also without a connection, and is useless to anyone else. An installed reader opens it
by double-click on a computer with Chrome or Edge, and through **Share** on an Android
phone; on an iPhone or iPad and in Firefox use Open file. Books protected by DRM are
never exported. Each download is a new copy; files downloaded earlier keep opening
(files made before 0.21.0 do not). Removing the book from the shelf removes the copy
from the phone; opening the file again puts the book back.

Without `trusted_proxies`, all clients behind a proxy on the same machine share one
budget for creating libraries, signing in and redeeming invites, and with open
registration also the 3 new libraries a day. The hub logs no passwords, tokens,
secrets, addresses, library, account or publication ids, nor client addresses;
`--log-format json` suits a log collector.

**Checking a hub**: `shelfgauge hub check https://hub.example.org` (no data directory
needed) fetches every file of the reader app from the hub and compares it with the
copy built into the program: with the same version as the hub, "All N files … match"
means the hub serves the released reader unchanged. It shows what the hub serves now
and to you, not what it serves to others, and not the copy a phone already keeps.

### Running a public hub

What a hub for people you do not know needs beyond the recipe above:

- **A server**: a small VPS with local SSD and traffic included; books are stored
  encrypted as they are uploaded (a plain-text book is 0.1–1.5 MB, an illustrated one
  tens of MB).
- **HTTPS**: Caddy as above (it gets and renews the certificate itself); a firewall
  that lets in only 22, 80 and 443 (`ufw allow 22,80,443/tcp`).
- **Its own user** (`useradd --system shelfgauge`, owner of the data directory) **and a
  systemd unit**, `/etc/systemd/system/shelfgauge-hub.service`:

  ```ini
  [Unit]
  Description=shelfgauge hub
  After=network-online.target
  Wants=network-online.target

  [Service]
  User=shelfgauge
  ExecStart=/usr/local/bin/shelfgauge hub --data /srv/shelfgauge-hub --log-format json
  Restart=on-failure
  NoNewPrivileges=yes
  ProtectSystem=strict
  ProtectHome=yes
  PrivateTmp=yes
  ReadWritePaths=/srv/shelfgauge-hub

  [Install]
  WantedBy=multi-user.target
  ```

- **Limits in `hub.toml`** (a start; adjust to the disk): `[libraries]
  registration = "open"` (or `"invite"` for a closed beta), `max_libraries` set (never
  0 on an open hub), `new_libraries_per_address_per_day = 3`, `expiry_days = 365`;
  `[quotas] max_bytes = 1073741824` (1 GiB per library), `max_publications = 300`,
  `max_devices = 5`; `[limits] publication_expiry_days = 180` and `max_total_bytes`
  at about 80 % of the free disk; `trusted_proxies` for the proxy.
- **Terms and contact**: `[operator] name` and `contact`, and a `terms.txt` written by
  you for your jurisdiction (this project gives no legal text). Points to cover: what
  is stored (encrypted books and marks, library handles and account names, device
  names, times of use), that the hub
  cannot read them, the limits and when books and accounts are deleted, how to report
  abuse and how takedowns work (`hub takedown <publication id>`), no warranty.
- **Backups**: the database first, then the blobs (a blob without a database row is
  harmless, the reverse is a broken book):
  `sqlite3 /srv/shelfgauge-hub/hub.sqlite ".backup /backup/hub.sqlite"`, then
  `rsync -a /srv/shelfgauge-hub/blobs/ /backup/blobs/` (or Litestream for the database
  and the same `rsync`). Restore both, with `hub.toml` and `terms.txt`, into a data
  directory and start the hub.
- **Watching it**: an external check of `GET https://hub.example.org/api/info` (it
  says `"accepting": false` when `max_total_bytes` is reached), disk space, and the
  logs of the unit (`journalctl -u shelfgauge-hub`); keep proxy access logs short or
  off (Caddy writes none unless `log` is set).
- **Updates**: replace the binary and restart; the database migrates itself (from
  0.18–0.20, see "Upgrading" above: `hub migrate` and the order library, then hub). Visitors
  can compare the hub with a release using `shelfgauge hub check`.

## Changelog

See [CHANGELOG.md](CHANGELOG.md) for the version history.

## License

MIT — see [LICENSE](LICENSE).

---

_shelfgauge v0.21.0 — MIT License_
