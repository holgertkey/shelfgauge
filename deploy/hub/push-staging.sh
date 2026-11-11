#!/bin/bash
# Puts a local build on the staging hub (never on production); see docs/HUB.md,
# "Staging". Run on the development machine from the repository root, with the
# server's admin key in the SSH agent:
#
#   cargo build --release
#   deploy/hub/push-staging.sh <ssh-host> <staging-domain> [binary]
#
# or, for a quick check between releases, a debug build with its symbols stripped
# (its version shows the +BUILD number):
#
#   cargo build && strip -o /tmp/shelfgauge target/debug/shelfgauge
#   deploy/hub/push-staging.sh <ssh-host> <staging-domain> /tmp/shelfgauge
#
# e.g. `deploy/hub/push-staging.sh root@hub.example.org staging.example.org`. Copies the
# binary (default target/release/shelfgauge) to
# /usr/local/lib/shelfgauge/dev-<version>-<sha>/ on the server, copies the staging
# database, points /usr/local/bin/shelfgauge-hub-staging at the binary, restarts
# shelfgauge-hub-staging, waits until the new process answers /api/info and runs
# `hub check`; on a failure the old link and the database copy come back. The unit, the
# link and the data directory are fixed here: this script cannot touch production.
# Official releases go through update.sh, never through this script.
set -euo pipefail

[ $# -ge 2 ] && [ $# -le 3 ] || {
  echo "usage: deploy/hub/push-staging.sh <ssh-host> <staging-domain> [binary]" >&2
  exit 2
}
host=$1
domain=$2
bin=${3:-target/release/shelfgauge}
[ -x "$bin" ] || { echo "no binary at $bin (cargo build --release, or pass a binary)" >&2; exit 1; }
[[ "$domain" =~ ^[A-Za-z0-9.-]+(:[0-9]+)?$ ]] || { echo "not a domain: $domain" >&2; exit 1; }

# Debian 13 has glibc 2.41; a build on a newer system may need more.
max_glibc=2.41
needed=$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)
if [ "$(printf '%s\n%s\n' "$needed" "$max_glibc" | sort -V | tail -1)" != "$max_glibc" ]; then
  echo "the binary needs glibc $needed; the server has $max_glibc" >&2
  exit 1
fi

# The full version with +BUILD (a release build's --version shows only the base).
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
[ -n "$version" ] || version=$("$bin" --version | awk '{print $2}')
sha=$(sha256sum "$bin" | cut -d' ' -f1)
name="dev-$version-${sha:0:8}"
[[ "$name" =~ ^dev-[0-9A-Za-z.+-]+$ ]] || { echo "odd version: $version" >&2; exit 1; }
echo "== pushing $bin ($name, needs glibc $needed) to $host, staging https://$domain"

ssh -o BatchMode=yes "$host" "cat > /tmp/shelfgauge-$name" <"$bin"

# The server side. Values reach it as arguments, checked above.
ssh -o BatchMode=yes "$host" bash -s -- "$name" "$sha" "$domain" <<'REMOTE'
set -euo pipefail
name=$1 sha=$2 domain=$3
[ "$(id -u)" = 0 ] || { echo "the admin login must be root" >&2; exit 1; }
unit=shelfgauge-hub-staging
link=/usr/local/bin/shelfgauge-hub-staging
data=/srv/shelfgauge-hub-staging
port=8744
dir=/usr/local/lib/shelfgauge/$name
upload=/tmp/shelfgauge-$name
grep -q "^ExecStart=$link " "/etc/systemd/system/$unit.service" ||
  { echo "$unit.service does not start $link" >&2; exit 1; }
[ "$(sha256sum "$upload" | cut -d' ' -f1)" = "$sha" ] || { echo "upload damaged" >&2; exit 1; }

install -d -m 0755 "$dir"
install -m 0755 "$upload" "$dir/shelfgauge"
rm -f "$upload"
"$dir/shelfgauge" --version >/dev/null

old=$(readlink -f "$link" || true)
old_pid=$(systemctl show -p MainPID --value "$unit")
copy=
if [ -f "$data/hub.sqlite" ]; then
  copies=/var/lib/shelfgauge-hub-update/staging
  install -d -m 0700 -o shelfgauge -g shelfgauge "$copies"
  copy="$copies/hub-before-$name.sqlite"
  runuser -u shelfgauge -- sqlite3 "$data/hub.sqlite" ".backup '$copy'"
fi

switch() { ln -sfn "$1" "$link.new"; mv -Tf "$link.new" "$link"; }
rollback() {
  echo "push-staging: $1; rolling back to $old" >&2
  journalctl -u "$unit" -n 20 --no-pager >&2 || true
  systemctl stop "$unit" || true
  [ -n "$old" ] && switch "$old"
  if [ -n "$copy" ]; then
    install -m 0640 -o shelfgauge -g shelfgauge "$copy" "$data/hub.sqlite.restore"
    rm -f "$data/hub.sqlite-wal" "$data/hub.sqlite-shm"
    mv -f "$data/hub.sqlite.restore" "$data/hub.sqlite"
  fi
  systemctl start "$unit" || true
  exit 1
}

switch "$dir/shelfgauge"
systemctl restart "$unit" || rollback "$unit did not restart"
# A release build reports only MAJOR.MINOR.PATCH, so the version cannot tell the
# builds apart: the new process must run exactly the pushed file and answer.
ok=
for _ in $(seq 30); do
  pid=$(systemctl show -p MainPID --value "$unit")
  if [ "$pid" != 0 ] && [ "$pid" != "$old_pid" ] &&
    [ "$(sha256sum "/proc/$pid/exe" 2>/dev/null | cut -d' ' -f1)" = "$sha" ] &&
    curl -fsS -m 2 -H "Host: $domain" "http://127.0.0.1:$port/api/info" >/dev/null 2>&1; then
    ok=1
    break
  fi
  sleep 1
done
[ -n "$ok" ] || rollback "the new binary did not answer within 30 s"
"$dir/shelfgauge" hub check "https://$domain" || rollback "hub check failed"

# Keep the last five pushed builds (and whatever a link points at).
keep=$(readlink -f /usr/local/bin/shelfgauge-hub 2>/dev/null || true)
find /usr/local/lib/shelfgauge -mindepth 1 -maxdepth 1 -name 'dev-*' -printf '%T@ %p\n' |
  sort -rn | tail -n +6 | cut -d' ' -f2- | while read -r d; do
    case "$keep $old" in *"$d/"*) continue ;; esac
    rm -rf -- "$d"
  done
find /var/lib/shelfgauge-hub-update/staging -maxdepth 1 -name 'hub-before-dev-*.sqlite' \
  -printf '%T@ %p\n' 2>/dev/null | sort -rn | tail -n +4 | cut -d' ' -f2- | xargs -r rm -f --
logger -t shelfgauge-push-staging "staging runs $name (was ${old:-none})"
echo "== staging runs $name"
REMOTE
