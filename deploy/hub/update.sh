#!/bin/bash
# Installs an official shelfgauge release on a hub server and switches one hub to it;
# see docs/HUB.md, "Updates". setup.sh installs it as /usr/local/sbin/shelfgauge-hub-update.
#
#   shelfgauge-hub-update [--dry-run] [--downgrade] vX.Y.Z [production|staging]
#
# 1. downloads the Linux archive, SHA256SUMS and the attestation bundle of that GitHub
#    release; checks the sum and the attestation (made by release.yml for that tag);
# 2. copies the hub's database (hub-before-X.Y.Z.sqlite, the last three kept), since
#    migrations go forward only;
# 3. installs /usr/local/lib/shelfgauge/X.Y.Z/shelfgauge, points the hub's link
#    (/usr/local/bin/shelfgauge-hub, -staging) at it and restarts the hub's unit;
# 4. waits for /api/info to answer the new version and runs `hub check`;
# 5. on a failure after the switch: the old link and the database copy back, restart,
#    exit 1.
#
# As the forced command of a deploy key (authorized_keys of the user `deploy`) it runs as
#   command="/usr/local/sbin/shelfgauge-hub-update production",restrict ssh-ed25519 ...
# with the target fixed per key (one key for staging, one for production, each in its
# own GitHub Environment), takes nothing but `vX.Y.Z` from SSH_ORIGINAL_COMMAND (no
# options) and runs itself as root through one sudoers line.
#
# Settings: /etc/shelfgauge-hub/update.conf (written by setup.sh):
#   REPO=holgertkey/shelfgauge        the GitHub repository of the releases
#   PRODUCTION_DOMAIN=hub.example.org
#   STAGING_DOMAIN=staging.example.org (empty: no staging hub)
# For trials only (test-update.sh): RELEASE_URL (a folder with the release files instead
# of GitHub's download URL), ATTESTATION=skip, and SCHEME=http for `hub check` of a hub
# without a certificate.
set -euo pipefail
umask 022

self=/usr/local/sbin/shelfgauge-hub-update
conf=${SHELFGAUGE_UPDATE_CONF:-/etc/shelfgauge-hub/update.conf}
lib=/usr/local/lib/shelfgauge
target_triple=x86_64-unknown-linux-gnu
version_re='^v[0-9]+\.[0-9]+\.[0-9]+$'

die() {
  echo "shelfgauge-hub-update: $*" >&2
  exit 1
}
say() { echo "== $*"; }

# --- arguments -------------------------------------------------------------------------
if [ "$(id -u)" != 0 ] && [ -n "${SSH_ORIGINAL_COMMAND+x}" ]; then
  # A deploy key: the target comes from its forced command, the client sends only the
  # version (or the version and the same target); nothing else reaches the root part.
  key_target=${1:-}
  case "$key_target" in
    production | staging) ;;
    *) die "the deploy key's forced command names no target (production or staging)" ;;
  esac
  read -r -a words <<<"$SSH_ORIGINAL_COMMAND"
  [ "${#words[@]}" -ge 1 ] && [ "${#words[@]}" -le 2 ] || die "expected: vX.Y.Z"
  [[ "${words[0]}" =~ $version_re ]] || die "not a release version: ${words[0]}"
  [ "${words[1]:-$key_target}" = "$key_target" ] || die "this key deploys only to $key_target"
  exec sudo -n "$self" -- "${words[0]}" "$key_target"
fi
[ "$(id -u)" = 0 ] || die "run as root"

dry_run=0
downgrade=0
while [ $# -gt 0 ]; do
  case "$1" in
    --dry-run) dry_run=1 ;;
    --downgrade) downgrade=1 ;;
    --) shift; break ;;
    -*) die "unknown option: $1" ;;
    *) break ;;
  esac
  shift
done
[ $# -ge 1 ] && [ $# -le 2 ] || die "usage: shelfgauge-hub-update [--dry-run] [--downgrade] vX.Y.Z [production|staging]"
tag=$1
target=${2:-production}
[[ "$tag" =~ $version_re ]] || die "not a release version: $tag"
version=${tag#v}

[ -r "$conf" ] || die "no settings in $conf (setup.sh writes them)"
REPO='' PRODUCTION_DOMAIN='' STAGING_DOMAIN='' RELEASE_URL='' ATTESTATION='' SCHEME=https
# shellcheck source=/dev/null
. "$conf"
[ -n "$REPO" ] || die "REPO is not set in $conf"

case "$target" in
  production)
    unit=shelfgauge-hub
    data=/srv/shelfgauge-hub
    link=/usr/local/bin/shelfgauge-hub
    port=8743
    domain=$PRODUCTION_DOMAIN
    ;;
  staging)
    unit=shelfgauge-hub-staging
    data=/srv/shelfgauge-hub-staging
    link=/usr/local/bin/shelfgauge-hub-staging
    port=8744
    domain=$STAGING_DOMAIN
    ;;
  *) die "unknown target: $target" ;;
esac
[ -n "$domain" ] || die "no domain for $target in $conf"
grep -q "^ExecStart=$link " "/etc/systemd/system/$unit.service" 2>/dev/null ||
  die "$unit.service does not start $link (run setup.sh first)"

exec 9>/run/shelfgauge-hub-update.lock
flock -n 9 || die "another update is running"

# The version the hub answers with (empty when it does not answer). Never fails: under
# `set -e` and `pipefail` a failing `x=$(running_version)` would end the script
# silently, e.g. while the hub restarts, before the check and the rollback.
running_version() {
  { curl -fsS -m 5 -H "Host: $domain" "http://127.0.0.1:$port/api/info" 2>/dev/null || true; } |
    sed -n 's/.*"version":"\([^"]*\)".*/\1/p'
}

# 0 when version $1 is older than $2.
older() {
  [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" = "$1" ]
}

# --- 1. download and check -------------------------------------------------------------
work=$(mktemp -d /tmp/shelfgauge-update.XXXXXX)
trap 'rm -rf "$work"' EXIT
archive="shelfgauge-$tag-$target_triple.tar.gz"
bundle="shelfgauge-$tag.sigstore.json"
base=${RELEASE_URL:-https://github.com/$REPO/releases/download/$tag}

fetch() {
  if [[ "$base" == /* ]]; then
    cp "$base/$1" "$work/$1"
  else
    # github.com downloads have timed out from servers for minutes at a time.
    curl -fsSL --retry 5 --retry-delay 10 --retry-all-errors -m 300 -o "$work/$1" "$base/$1"
  fi
}

say "$target: downloading $tag from $base"
fetch SHA256SUMS
fetch "$archive"
(cd "$work" && grep -E "^[0-9a-f]{64}  $archive\$" SHA256SUMS | sha256sum -c --status) ||
  die "$archive does not match SHA256SUMS (or is not listed in it)"
echo "checksum ok"

if [ "$ATTESTATION" = skip ]; then
  echo "WARNING: ATTESTATION=skip in $conf: the attestation is not checked (trials only)" >&2
else
  fetch "$bundle"
  command -v gh >/dev/null || die "gh is not installed (setup.sh installs it)"
  # No token and no login: the bundle comes with the release, the trust root from
  # Sigstore's public TUF repository.
  mkdir -p "$work/gh"
  env -u GH_TOKEN -u GITHUB_TOKEN GH_CONFIG_DIR="$work/gh" GH_NO_UPDATE_NOTIFIER=1 \
    gh attestation verify "$work/$archive" --bundle "$work/$bundle" \
    --repo "$REPO" \
    --signer-workflow "$REPO/.github/workflows/release.yml" \
    --source-ref "refs/tags/$tag" \
    --deny-self-hosted-runners >"$work/attestation.log" 2>&1 ||
    { cat "$work/attestation.log" >&2; die "the attestation of $archive does not verify"; }
  echo "attestation ok ($REPO, release.yml, refs/tags/$tag)"
fi

tar -xzf "$work/$archive" -C "$work"
new_bin="$work/shelfgauge-$tag-$target_triple/shelfgauge"
[ -x "$new_bin" ] || die "the archive has no shelfgauge binary"
reported=$("$new_bin" --version 2>&1) || die "the new binary does not run here: $reported"
[ "$reported" = "shelfgauge $version" ] || die "the binary reports '$reported', not $version"

current=$(running_version)
old_target=$(readlink -f "$link" 2>/dev/null || true)
echo "running: ${current:-not answering}; link: ${old_target:-none}"
dest="$lib/$version/shelfgauge"
if [ "$current" = "$version" ] && [ "$old_target" = "$dest" ] && cmp -s "$new_bin" "$dest"; then
  say "$target already runs $version"
  exit 0
fi
if [ -n "$current" ] && [[ "$current" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]] && older "$version" "${current%%+*}"; then
  # Staging often runs an intermediate build (push-staging.sh) ahead of the release.
  if [ "$target" = staging ] && [[ "$old_target" == "$lib"/dev-* ]]; then
    downgrade=1
  fi
  [ "$downgrade" = 1 ] || die "$version is older than the running $current (--downgrade, by hand only)"
  echo "downgrade from $current (the database copy of that update is the one to restore)" >&2
fi

if [ "$dry_run" = 1 ]; then
  say "dry run: would install $dest, point $link at it, restart $unit and check $SCHEME://$domain"
  exit 0
fi

# --- 2. database copy ------------------------------------------------------------------
copies="/var/lib/shelfgauge-hub-update/$target"
install -d -m 0700 -o shelfgauge -g shelfgauge "$copies"
db_copy=
if [ -f "$data/hub.sqlite" ]; then
  db_copy="$copies/hub-before-$version.sqlite"
  rm -f "$db_copy"
  say "copying the database to $db_copy"
  runuser -u shelfgauge -- sqlite3 "$data/hub.sqlite" ".backup '$db_copy'"
  [ "$(runuser -u shelfgauge -- sqlite3 "$db_copy" 'PRAGMA quick_check;')" = ok ] ||
    die "the database copy failed its check; nothing changed"
  # The last three copies are kept.
  find "$copies" -maxdepth 1 -name 'hub-before-*.sqlite' -printf '%T@ %p\n' |
    sort -rn | tail -n +4 | cut -d' ' -f2- | xargs -r rm -f --
fi

# --- 3. install and switch -------------------------------------------------------------
if [ ! -e "$dest" ] || ! cmp -s "$new_bin" "$dest"; then
  install -d -m 0755 "$lib/$version"
  install -m 0755 "$new_bin" "$dest.new"
  mv -f "$dest.new" "$dest"
fi

switch_link() { # switch_link <path>
  ln -sfn "$1" "$link.new"
  mv -Tf "$link.new" "$link"
}

rollback() {
  echo "shelfgauge-hub-update: $1; rolling back" >&2
  journalctl -u "$unit" -n 20 --no-pager >&2 || true
  systemctl stop "$unit" || true
  if [ -n "$old_target" ]; then
    switch_link "$old_target"
  else
    rm -f "$link"
  fi
  if [ -n "$db_copy" ]; then
    install -m 0640 -o shelfgauge -g shelfgauge "$db_copy" "$data/hub.sqlite.restore"
    rm -f "$data/hub.sqlite-wal" "$data/hub.sqlite-shm"
    mv -f "$data/hub.sqlite.restore" "$data/hub.sqlite"
  fi
  if [ -n "$old_target" ]; then
    systemctl start "$unit" || true
    sleep 3
    echo "after the rollback the hub answers: $(running_version || true)" >&2
  fi
  exit 1
}

say "switching $link to $dest and restarting $unit"
switch_link "$dest"
systemctl restart "$unit" || rollback "$unit did not restart"

# --- 4. check --------------------------------------------------------------------------
answered=
for _ in $(seq 30); do
  answered=$(running_version)
  [ "$answered" = "$version" ] && break
  sleep 1
done
[ "$answered" = "$version" ] || rollback "the hub answers '${answered:-nothing}' instead of $version after 30 s"
"$dest" hub check "$SCHEME://$domain" || rollback "hub check $SCHEME://$domain failed"
logger -t shelfgauge-hub-update "$target updated to $version (was ${current:-not answering})"
say "$target runs $version"
