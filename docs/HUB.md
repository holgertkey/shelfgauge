# Deploying a hub

A hub (`shelfgauge hub`) keeps encrypted copies of books and reading data for the
phones of one or more libraries; what it does and how libraries use it is in the
README, "Running a hub". This guide is about putting one on the internet: first on
your own computer through ngrok, to read on a real phone, then on a server with
Caddy. The files it uses are in [`deploy/hub/`](../deploy/hub/):

- [`Caddyfile`](../deploy/hub/Caddyfile): Caddy in front of the hub, with HTTPS;
- [`shelfgauge-hub.service`](../deploy/hub/shelfgauge-hub.service): the systemd unit;
- [`hub.toml.example`](../deploy/hub/hub.toml.example): the settings of a hub on the
  internet.

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

1. **DNS**: an `A` (and `AAAA`, if the server has IPv6) record of your name, e.g.
   `hub.example.org`, pointing to the server.

2. **Firewall**: only SSH, HTTP and HTTPS (`ufw allow 22,80,443/tcp && ufw enable`).
   Port 80 is needed for the certificate and the redirect to HTTPS; 8743 stays closed.

3. **The program and its user**: copy the `shelfgauge` binary of a
   [release](https://github.com/holgertkey/shelfgauge/releases) to
   `/usr/local/bin/`, then

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

**Updates**: replace the binary and `systemctl restart shelfgauge-hub`; the database
migrates itself (from 0.18–0.20 see the README, "Upgrading from 0.18–0.20").

## Other proxies

nginx works too: pass `proxy_set_header Host $host;` and
`proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;`, and add the proxy's
address to `trusted_proxies` if it runs on another machine. Under a path
(`https://example.org/hub/`), set `[server] base_path = "/hub"` and let the proxy pass
the path unchanged.
