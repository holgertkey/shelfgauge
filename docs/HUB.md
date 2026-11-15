# Deploying a hub

A hub (`shelfgauge hub`) keeps encrypted copies of books and reading data for the
phones of one or more libraries; what it does and how libraries use it is in the
README, "Running a hub". This guide is about putting one on the internet: first on
your own computer through ngrok, to read on a real phone, then on a server with
Caddy, by hand or with one script ("Automatic setup"). The files it uses are in
[`deploy/hub/`](../deploy/hub/):

- [`Caddyfile`](../deploy/hub/Caddyfile): Caddy in front of the hub, with HTTPS;
- [`shelfgauge-hub.service`](../deploy/hub/shelfgauge-hub.service): the systemd unit;
- [`hub.toml.example`](../deploy/hub/hub.toml.example): the settings of a hub on the
  internet (a library's `max_bytes` holds its books, records and catalog; since 0.25
  a library uploads the Explore thumbnails ahead, up to 5 % of it);
- [`setup.sh`](../deploy/hub/setup.sh) with
  [`setup.env.example`](../deploy/hub/setup.env.example): the whole server in one
  script; [`update.sh`](../deploy/hub/update.sh): installing a release;
  [`push-staging.sh`](../deploy/hub/push-staging.sh): a build of your own on a staging
  hub; the staging hub's unit, Caddy site and settings (`*staging*`); the site check
  and the backups (`shelfgauge-hub-watch*`, `shelfgauge-hub-backup*`, `backup-pull/`).

The hub listens on `127.0.0.1:8743` and has no TLS of its own. Phones need HTTPS (the
reader's encryption and offline storage work only there), so something in front of
the hub ends TLS and passes requests on: ngrok for a test, Caddy on a server. Both run
on the hub's machine, which is what the hub expects by default:

- **`[server] allowed_hosts`** must name the public host name: the hub answers only
  to the names it knows (protection against DNS rebinding). Without it every request
  through the proxy gets `403 Forbidden: unknown Host header`, and the hub says so at
  start and in its log.
- **`[server] trusted_proxies`** is `["127.0.0.1", "::1"]` by default: Caddy and
  ngrok name the client in `X-Forwarded-For`, so each client has its own budget for
  signing in and invites. A hub created before this default has `trusted_proxies = []`
  in its `hub.toml`, which means "trust no proxy" (all clients then share one budget):
  delete that line.

## Trying it from a phone with ngrok

[ngrok](https://ngrok.com/) gives your computer a public HTTPS address without a
server, a domain or open ports. Its free plan is enough to try a hub with a few
books.

1. **Install the ngrok agent** and connect it to your account
   (`ngrok config add-authtoken <token>`, from the ngrok dashboard). The dashboard's
   **Domains** page shows your free dev domain, e.g. `your-name.ngrok-free.dev` (older
   accounts have `.ngrok-free.app`). It stays the same for your account, which
   matters: a library cannot move to another hub address while it is on a hub.

2. **Create the hub** and stop it (Ctrl+C) once it says it is serving:

   ```sh
   shelfgauge hub --data ~/shelfgauge-hub
   ```

3. **Name the domain** in `~/shelfgauge-hub/hub.toml`, in the `[server]` section:

   ```toml
   allowed_hosts = ["your-name.ngrok-free.dev"]
   ```

4. **Start the hub and ngrok**, each in its own terminal:

   ```sh
   shelfgauge hub --data ~/shelfgauge-hub
   ngrok http 8743 --url https://your-name.ngrok-free.dev
   ```

   ngrok passes the `Host` header on unchanged, so the hub sees its public name, and
   appends the client's address to `X-Forwarded-For`.

5. **Put the library on the hub**: `shelfgauge hub invite --data ~/shelfgauge-hub`
   prints an invite code. In the library, **Settings → Server and hub**: hub address
   `https://your-name.ngrok-free.dev`, save and restart; then **My profile → Phone and
   other devices → Put the library on the hub** with the code.

6. **On the phone**: **Read on phone** on a book card shows a QR code; scanned with
   your phone, it opens the book and signs the phone in. **Settings → Profiles →
   Invite someone** does the same for another person.

The hub and ngrok must run while phones fetch books and sync; a book once opened stays
readable on the phone without them.

**Free plan limits** (from ngrok's documentation; check the current ones there):

- Before HTML pages, ngrok shows a warning page; **Visit** on it sets a cookie that
  lifts it for 7 days. The library's requests and the reader's calls to the hub are
  not affected (the reader sends ngrok's `ngrok-skip-browser-warning` header).
  Because of this page, Chrome cannot install the reader as an app behind a free
  ngrok domain (it fetches the app manifest without the cookie); reading in the
  browser works.
- A phone updates its reader (after the hub is updated, or the first time after
  shelfgauge 0.22.0) only with that cookie, since the browser fetches the new version
  without the reader's header. When **My account** shows the hub's version as unknown,
  or a new hub version does not arrive, open `https://your-name.ngrok-free.dev/terms`
  on the phone (a page the reader does not keep offline), press **Visit**, then open
  the shelf again.
- 1 GB of traffic and 20 000 HTTP requests a month; illustrated books use the
  traffic up fast.

**Trying the Caddyfile on the laptop** (optional; [Caddy](https://caddyserver.com/docs/install)
installed): with the hub running, start Caddy on plain HTTP and point ngrok at it.
The hub's settings stay the same. In this chain Caddy does not trust the ngrok agent
and replaces its `X-Forwarded-For` with `127.0.0.1`, so all clients share one budget;
it is only for trying the file:

```sh
SHELFGAUGE_HUB_DOMAIN=:8080 caddy run --config deploy/hub/Caddyfile
ngrok http 8080 --url https://your-name.ngrok-free.dev
```

## On a server

A small VPS with local SSD and traffic included is enough (a plain-text book is
0.1–1.5 MB encrypted, an illustrated one tens of MB). The commands are for Debian or
Ubuntu, as root.

These are the steps by hand; "Automatic setup" below does them with one script.

1. **DNS**: an `A` (and `AAAA`, if the server has IPv6) record of your name, e.g.
   `hub.example.org`, pointing to the server.

2. **Firewall**: only SSH, HTTP and HTTPS (`ufw allow 22,80,443/tcp && ufw enable`).
   Port 80 is needed for the certificate and the redirect to HTTPS; 8743 stays closed.

3. **The program and its user**: copy the `shelfgauge` binary of a
   [release](https://github.com/holgertkey/shelfgauge/releases) to
   `/usr/local/bin/shelfgauge-hub` (the unit starts that name) and link the usual name
   to it (`ln -s shelfgauge-hub /usr/local/bin/shelfgauge`), then

   ```sh
   useradd --system --home-dir /srv/shelfgauge-hub --shell /usr/sbin/nologin shelfgauge
   mkdir -p /srv/shelfgauge-hub
   cp hub.toml.example /srv/shelfgauge-hub/hub.toml   # from deploy/hub/
   chown -R shelfgauge: /srv/shelfgauge-hub
   ```

   Edit `/srv/shelfgauge-hub/hub.toml`: the lines marked `CHANGE` (`allowed_hosts`
   with your name, `[operator]`, `max_total_bytes` at about 80 % of the free disk), and
   `registration` (`"invite"` for people you know, `"open"` for anyone). Write the
   terms of use to `/srv/shelfgauge-hub/terms.txt` if the hub is open to people you do
   not know (see the README, "Running a public hub").

4. **The service**: copy `shelfgauge-hub.service` to `/etc/systemd/system/`, then
   `systemctl daemon-reload && systemctl enable --now shelfgauge-hub`. The log is in
   `journalctl -u shelfgauge-hub` (JSON lines; no client addresses, no secrets).

5. **Caddy**: install it from its
   [Debian/Ubuntu repository](https://caddyserver.com/docs/install#debian-ubuntu-raspbian),
   copy `deploy/hub/Caddyfile` to `/etc/caddy/Caddyfile` and give it the domain:
   either replace `{$SHELFGAUGE_HUB_DOMAIN}` in the file with it, or set the variable
   for the service with `systemctl edit caddy`:

   ```ini
   [Service]
   Environment=SHELFGAUGE_HUB_DOMAIN=hub.example.org
   ```

   then `systemctl restart caddy`. Caddy gets and renews the certificate itself and
   writes no access log (the file has no `log` directive).

6. **Check it**: `shelfgauge hub check https://hub.example.org` (from any computer)
   compares the reader app the hub serves with the copy in your program, and
   `curl https://hub.example.org/api/info` answers with the hub's version.
   `sudo -u shelfgauge shelfgauge hub invite --data /srv/shelfgauge-hub` gives a
   library its invite code (run hub commands as the hub's user, so the files they
   write stay its own).

**Moving from the ngrok test to the server**: a library cannot change its hub while it
is on one. On **Settings → Server and hub**, first **delete the library on this hub**
(the test hub), then enter the new address, restart and put the library on the new
hub; every member gets a new invite.

**Backups**: the database first, then the blobs (a blob without a database row is
harmless, the reverse is a broken book):

```sh
sqlite3 /srv/shelfgauge-hub/hub.sqlite ".backup /backup/hub.sqlite"
rsync -a /srv/shelfgauge-hub/blobs/ /backup/blobs/
```

(or Litestream for the database and the same `rsync`). To restore, put both, with
`hub.toml` and `terms.txt`, into a data directory and start the hub.

**Watching it**: an external check of `GET https://hub.example.org/api/info` (it says
`"accepting": false` when `max_total_bytes` is reached), disk space, and
`journalctl -u shelfgauge-hub`.

**Expiry and takedowns**: `publication_expiry_days` frees the hub's disk by deleting
its copy of a book nobody opened for that many days; the phones that have the book
keep theirs and read it offline as long as it is on their Bookshelf. A phone without
a copy asks the book's library for it when the book is opened, and the library sends
it again (a new copy on the hub) the next time its computer is on.
`shelfgauge hub takedown <publication-id>` deletes the hub's copy too, and a phone that
has one deletes it the next time it opens the book with a connection.

**Keeping books only until delivered (a mailbox)**: with a small
`publication_expiry_days` (3, say) the hub keeps the libraries' catalogs, shelves,
marks and positions, but a book only for the few days its readers need to download
it (the first open on a device downloads the whole book). Far less disk, and far
fewer books stored on your server. The cost: a book sent to a phone and not opened
within those days, or opened on a new device later, comes only once its library's
computer is on again. The reader shows the number of days (from `/api/info`). The
default, 0, keeps books until their library deletes them.

**Updates**: replace the binary and `systemctl restart shelfgauge-hub`; the database
migrates itself (from 0.18–0.20 see the README, "Upgrading from 0.18–0.20"). The
migrations go forward only: copy the database first
(`sqlite3 hub.sqlite ".backup hub-before.sqlite"`), so an older version can be put
back. `update.sh` (below) does all of it.

## Automatic setup

[`setup.sh`](../deploy/hub/setup.sh) sets up a fresh Debian 13 server (a plain image,
no control panel: it would take ports 80 and 443) with everything of "On a server",
and can be run again at any time: a second run changes nothing, and it never
overwrites a hub's `hub.toml` or data. Debian 13, since the release binary needs
glibc 2.39 or close (Debian 12 has 2.36).

```sh
tar -C deploy/hub -c . | ssh root@server 'mkdir -p shelfgauge-setup && tar -C shelfgauge-setup -x'
ssh root@server 'cd shelfgauge-setup && cp setup.env.example setup.env && nano setup.env'
ssh root@server shelfgauge-setup/setup.sh --check              # what it would change
ssh root@server shelfgauge-setup/setup.sh --version v0.24.0    # set up, install a release
```

`setup.env` holds your values: the hub's name and the staging hub's
(`STAGING_DOMAIN`, empty for none), the operator, the admin login and its key, the
deploy keys, the backup and the check. What the script does:

- packages, with Caddy and `gh` from their own apt repositories (their signing keys
  checked against fingerprints in the script); key-only SSH
  (`PermitRootLogin prohibit-password` for a root admin, `forced-commands-only` with
  an admin user);
  `ufw` with 22, 80 and 443;
- `unattended-upgrades` for Debian's security updates, Caddy and `gh`, with a reboot at
  `REBOOT_TIME` (04:00) when an update needs one. shelfgauge itself never updates by
  itself;
- the user `shelfgauge`, `/srv/shelfgauge-hub` (and `-staging`) with a first
  `hub.toml` (the production one with about 70 % of the free disk as
  `max_total_bytes`: check it), the units and the Caddy sites;
- `update.sh` as `/usr/local/sbin/shelfgauge-hub-update`, and the user `deploy` for
  CI (below);
- the backup and the site check (below).

It installs no binary of its own: `--version` (or the first deploy) runs
`shelfgauge-hub-update`. A server set up by hand (a plain `/usr/local/bin/shelfgauge`)
is moved to the layout below with the same binary, and production restarts once.

**Binaries**: each version in `/usr/local/lib/shelfgauge/<version>/shelfgauge`; the
units start a link per hub, `/usr/local/bin/shelfgauge-hub` and `-staging`, so the two
hubs can run different versions; `/usr/local/bin/shelfgauge` follows production (for
`hub invite` and the other commands).

## Staging

A second hub on the same server for trying a version before your users get it: its
own name (`staging.example.org`, its own origin on the phones, so its reader app,
storage and sign-ins never mix with production's), port 8744, data directory
`/srv/shelfgauge-hub-staging` (throwaway: not backed up), and a short
`publication_expiry_days` ([`hub-staging.toml.example`](../deploy/hub/hub-staging.toml.example)).
Use it from a test library only; a library of real people stays on production.

A build of your own reaches staging by hand, never production:

```sh
cargo build --release
deploy/hub/push-staging.sh root@server staging.example.org
```

For quick checks between releases a debug build does too: it is made in seconds and
its version shows the build number (`0.24.0+007`). Strip its debug symbols first (the
binary shrinks to about a third; the version stays) and pass it as the third argument:

```sh
cargo build
strip -o /tmp/shelfgauge target/debug/shelfgauge
deploy/hub/push-staging.sh root@server staging.example.org /tmp/shelfgauge
```

It copies the binary to `/usr/local/lib/shelfgauge/dev-<version>-<sha>/` (a build
needing a newer glibc than the server's is refused), copies the staging database,
switches the staging link, restarts the staging hub, waits until the new process
answers and runs `hub check`; on a failure the old binary and database come back.

A debug build's reading view also sends the app's log lines to the hub (only a debug
build has `POST /debug/log` and loads `assets/hub/debuglog.js`): what a script
passes to `window.sgLog(...)`, and errors in the page, each line tagged with the
page's random tag and the seconds since it loaded. They go into the staging hub's log
with the target `shelfgauge::client`:

```sh
ssh root@server journalctl -u shelfgauge-hub-staging --since -30min -o cat | grep shelfgauge::client
```

Hub commands for the staging hub use its own binary, which may be newer than
production's (and may have migrated its database):
`sudo -u shelfgauge /usr/local/bin/shelfgauge-hub-staging hub invite --data /srv/shelfgauge-hub-staging`.

## Installing a release

```sh
shelfgauge-hub-update [--dry-run] [--downgrade] vX.Y.Z [production|staging]
```

downloads the Linux archive of that GitHub release with its `SHA256SUMS` and
attestation bundle, checks the sum and the attestation with `gh attestation verify`
(made by `release.yml` for exactly that tag, on GitHub's runners; no token needed),
refuses a version older than the running one (unless `--downgrade`), copies the
database (`/var/lib/shelfgauge-hub-update/<hub>/hub-before-<version>.sqlite`, the last
three kept), switches the hub's link, restarts it, waits for `/api/info` to answer the
new version and runs `hub check`. If the hub does not come up, the old link and the
database copy come back and the command fails. `--dry-run` stops after the checks.

Update the hub before the libraries: a new hub accepts what the previous release's
libraries send, and the phones get the new reader app from the hub by themselves.
[`test-update.sh`](../deploy/hub/test-update.sh) tries a good update, a damaged
archive, a binary that does not start and a downgrade on the staging hub.

## Deploying from CI

[`deploy.yml`](../.github/workflows/deploy.yml) puts each release on the server: when
the Release workflow succeeds for a tag (or by hand, with a version) it updates
staging, then waits for a reviewer before production. It connects as the user
`deploy`, whose keys can run nothing but `shelfgauge-hub-update`, each for one hub:

```
command="/usr/local/sbin/shelfgauge-hub-update production",restrict ssh-ed25519 AAAA... shelfgauge-deploy-production
command="/usr/local/sbin/shelfgauge-hub-update staging",restrict ssh-ed25519 AAAA... shelfgauge-deploy-staging
```

The job sends only the version; the script takes nothing else (no options) and runs
itself as root through one `sudoers` line that allows exactly those arguments. A
leaked key can install an official, attested release on its own hub, nothing more.

Setting it up:

1. two keys (`ssh-keygen -t ed25519 -N '' -C shelfgauge-deploy-production -f production`,
   the same for staging); the public halves into `setup.env` (`DEPLOY_SSH_KEY`,
   `DEPLOY_STAGING_SSH_KEY`), then `setup.sh`;
2. in the repository's Settings → Environments: `public-hub-staging` (no reviewer)
   and `public-hub` (you as required reviewer), both limited to the branch `main` (the
   workflow runs from it, also when a tag started the release), each with the secret
   `DEPLOY_SSH_KEY` holding its private key;
3. repository variables `DEPLOY_HOST` (the server's address) and
   `DEPLOY_KNOWN_HOSTS` (its host key lines, from `ssh-keyscan`, compared with the
   fingerprint on the provider's console): the job never accepts an unknown host key.

Without `DEPLOY_HOST` the workflow skips (a fork, or no server yet). The automatic
start uses `deploy.yml` from the default branch.

[`attest-check.yml`](../.github/workflows/attest-check.yml) tries the attestation
steps on two small files and verifies the bundle as `update.sh` does, on a push to
`main` that changes `release.yml`, itself or `update.sh` (or by hand): a problem with
the attestation shows up with CI, before a tag is pushed.

## Backups

The database and the books' blobs are the hub (all encrypted on the phones' side; the
database also holds account password hashes and wrapped keys). `setup.sh` offers two
ways, `BACKUP` in `setup.env`:

- **`pull`**: a computer of yours fetches the backup. Its key (`BACKUP_SSH_KEY`) has a
  forced command in root's `authorized_keys`,
  [`shelfgauge-hub-backup-send`](../deploy/hub/shelfgauge-hub-backup-send), which
  writes one tar stream (an online `.backup` snapshot of the database first, `hub.toml`,
  `terms.txt`, `blobs/`) whatever the client asks. On that computer,
  [`backup-pull/`](../deploy/hub/backup-pull/) stores it in a
  [restic](https://restic.net/) repository daily (a systemd user timer; 7 daily,
  4 weekly, 6 monthly snapshots; a check on Sundays; Healthchecks.io pings).
- **`restic`**: the server pushes the same data nightly to a restic repository at
  another provider (an SFTP storage box, S3, Backblaze B2), with its settings and the
  provider's credentials in `/etc/shelfgauge-hub/backup.env` and the password in
  `/etc/shelfgauge-hub/restic-password` (keep a copy off the server; without it the
  backups cannot be read). Give the server a key that cannot delete, so whoever takes
  it over cannot wipe the backups. With Backblaze B2 (10 GB free) through its
  S3-compatible API:

  ```sh
  b2 account authorize                      # the master application key, on your computer
  b2 bucket create --lifecycle-rule '{"daysFromHidingToDeleting": 30, "daysFromUploadingToHiding": null, "fileNamePrefix": ""}' your-bucket allPrivate
  b2 key create --bucket your-bucket shelfgauge-hub-server listBuckets,listFiles,readFiles,writeFiles
  ```

  restic then only hides files it no longer needs, and B2 deletes a hidden version
  30 days later: 30 days to notice. `backup.env` gets
  `RESTIC_REPOSITORY=s3:https://s3.<region>.backblazeb2.com/your-bucket/shelfgauge-hub`
  (the endpoint is on the Buckets page), the key as `AWS_ACCESS_KEY_ID` and
  `AWS_SECRET_ACCESS_KEY`, and `AWS_DEFAULT_REGION=<region>`.

With another `BACKUP` than `pull`, `setup.sh` removes the pull backup's key line from
root's `authorized_keys` (keeping a copy) and `shelfgauge-hub-backup-send`.

Keep the backups at another provider than the server: a provider's own backup lives
in the same data centre and ends with the account.

**Restoring** (try it once, into the staging hub): stop the hub, put `hub.sqlite`,
`hub.toml`, `terms.txt` and `blobs/` into the data directory (owned by `shelfgauge`;
no `hub.sqlite-wal` or `-shm` next to it), start it. For staging, keep its own
`hub.toml` (port 8744, its name). From a pull backup:

```sh
restic dump latest /hub.tar | tar -x -C restored/
sqlite3 restored/hub.sqlite 'PRAGMA integrity_check;'
```

## Checks

[`shelfgauge-hub-watch`](../deploy/hub/shelfgauge-hub-watch) (every 5 minutes, as an
unprivileged user) fetches `https://<hub>/api/info` over each address in DNS (IPv4 and
IPv6), checks `accepting`, the certificate's days left (Caddy renews 30 days before)
and the disk, and pings a Healthchecks.io check (`WATCH_HC_URL`); a problem pings
`/fail` with the list, and no ping at all (the server down or suspended) is noticed by
Healthchecks itself.

## Other proxies

nginx works too: pass `proxy_set_header Host $host;` and
`proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`, and add the proxy's
address to `trusted_proxies` if it runs on another machine. The hub holds a
library's check and a reader's sync open for up to 25 s (long polling, 0.25): keep
`proxy_read_timeout` above that (nginx's default 60 s is fine). Caddy needs no
setting. Under a path
(`https://example.org/hub/`), set `[server] base_path = "/hub"` and let the proxy pass
the path unchanged.
