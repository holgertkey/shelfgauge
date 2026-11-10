#!/bin/bash
# Trials of update.sh on a server set up by setup.sh, against its STAGING hub only (it
# restarts it several times; production is never touched). Run as root on the server:
#
#   test-update.sh vX.Y.Z
#
# vX.Y.Z is a published release whose Linux archive it downloads once from GitHub; the
# release files are then served from a local folder (RELEASE_URL, ATTESTATION=skip in a
# settings file of its own), so the trials can break them:
#   1. a good update: staging moved to a copy of the binary first, then updated to the
#      release (the link switches, /api/info answers, hub check passes);
#   2. the same update again: nothing to do;
#   3. a damaged archive: refused before anything changes;
#   4. a binary that does not start (a newer fake version): installed, restarted, rolled
#      back to the old link and database;
#   5. an older version: refused without --downgrade;
#   6. an update of a hub that does not answer at the start (stopped): installed and
#      started (update.sh once ended silently when /api/info did not answer).
# The attestation itself is checked by the first real deploy of a release that has one.
set -uo pipefail

[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
[ $# = 1 ] && [[ "$1" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo "usage: test-update.sh vX.Y.Z" >&2; exit 2; }
tag=$1
version=${tag#v}
update=/usr/local/sbin/shelfgauge-hub-update
link=/usr/local/bin/shelfgauge-hub-staging
lib=/usr/local/lib/shelfgauge
triple=x86_64-unknown-linux-gnu
[ -x "$update" ] || { echo "no $update (setup.sh first)" >&2; exit 1; }
# shellcheck source=/dev/null
. /etc/shelfgauge-hub/update.conf
[ -n "${STAGING_DOMAIN:-}" ] || { echo "no staging hub in update.conf" >&2; exit 1; }

work=$(mktemp -d /tmp/shelfgauge-test-update.XXXXXX)
before=$(readlink -f "$link" || true)
# shellcheck disable=SC2329 # run by the EXIT trap
cleanup() {
  # Staging back on the binary it ran before the trials.
  if [ -n "$before" ] && [ "$(readlink -f "$link")" != "$before" ]; then
    ln -sfn "$before" "$link"
    systemctl restart shelfgauge-hub-staging
  fi
  rm -rf "$work" "$lib/dev-test-update" "$lib/9.9.9"
}
trap cleanup EXIT

failed=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; failed=1; }
answer() {
  curl -fsS -m 5 -H "Host: $STAGING_DOMAIN" http://127.0.0.1:8744/api/info 2>/dev/null |
    sed -n 's/.*"version":"\([^"]*\)".*/\1/p'
}
# release <dir> <tag> <binary>: a release folder like GitHub's.
release() {
  local dir=$1 t=$2 bin=$3 name
  name="shelfgauge-$t-$triple"
  mkdir -p "$dir/$name"
  cp "$bin" "$dir/$name/shelfgauge"
  tar -czf "$dir/$name.tar.gz" -C "$dir" "$name"
  rm -rf "${dir:?}/$name"
  (cd "$dir" && sha256sum "$name.tar.gz" >SHA256SUMS)
}
# run_update <release dir> <tag> [options]
run_update() {
  local dir=$1 t=$2
  shift 2
  printf 'REPO=%s\nPRODUCTION_DOMAIN=%s\nSTAGING_DOMAIN=%s\nRELEASE_URL=%s\nATTESTATION=skip\n' \
    "$REPO" "$PRODUCTION_DOMAIN" "$STAGING_DOMAIN" "$dir" >"$work/update.conf"
  SHELFGAUGE_UPDATE_CONF="$work/update.conf" "$update" "$@" "$t" staging
}

echo "== downloading $tag"
gh_url="https://github.com/$REPO/releases/download/$tag/shelfgauge-$tag-$triple.tar.gz"
curl -fsSL --retry 5 --retry-delay 10 --retry-all-errors -m 300 -o "$work/real.tar.gz" "$gh_url" ||
  { echo "cannot download $gh_url" >&2; exit 1; }
tar -xzf "$work/real.tar.gz" -C "$work"
real="$work/shelfgauge-$tag-$triple/shelfgauge"
"$real" --version

echo "== 1. a good update"
install -D -m 0755 "$real" "$lib/dev-test-update/shelfgauge"
ln -sfn "$lib/dev-test-update/shelfgauge" "$link"
systemctl restart shelfgauge-hub-staging
sleep 3
release "$work/good" "$tag" "$real"
if run_update "$work/good" "$tag" >"$work/1.log" 2>&1 &&
  [ "$(readlink -f "$link")" = "$lib/$version/shelfgauge" ] && [ "$(answer)" = "$version" ]; then
  pass "updated to $version, the link switched"
else
  fail "good update"; cat "$work/1.log"
fi

echo "== 2. the same update again"
if run_update "$work/good" "$tag" >"$work/2.log" 2>&1 && grep -q "already runs" "$work/2.log"; then
  pass "nothing to do"
else
  fail "second update"; cat "$work/2.log"
fi

echo "== 3. a damaged archive"
cp -r "$work/good" "$work/bad"
printf 'x' >>"$work/bad/shelfgauge-$tag-$triple.tar.gz"
link_before=$(readlink -f "$link")
if ! run_update "$work/bad" "$tag" >"$work/3.log" 2>&1 && grep -q "does not match SHA256SUMS" "$work/3.log" &&
  [ "$(readlink -f "$link")" = "$link_before" ]; then
  pass "refused, nothing changed"
else
  fail "damaged archive"; cat "$work/3.log"
fi

echo "== 4. a binary that does not start"
# shellcheck disable=SC2016 # the script's own $1
printf '#!/bin/sh\n[ "$1" = --version ] && { echo "shelfgauge 9.9.9"; exit 0; }\nexit 1\n' >"$work/broken"
chmod +x "$work/broken"
release "$work/broken-release" v9.9.9 "$work/broken"
link_before=$(readlink -f "$link")
if ! run_update "$work/broken-release" v9.9.9 >"$work/4.log" 2>&1 && grep -q "rolling back" "$work/4.log" &&
  [ "$(readlink -f "$link")" = "$link_before" ] && [ "$(answer)" = "$version" ]; then
  pass "rolled back, staging answers $version again"
else
  fail "rollback"; cat "$work/4.log"
fi

echo "== 5. an older version"
printf '#!/bin/sh\necho "shelfgauge 0.0.1"\n' >"$work/old"
chmod +x "$work/old"
release "$work/old-release" v0.0.1 "$work/old"
if ! run_update "$work/old-release" v0.0.1 >"$work/5.log" 2>&1 && grep -q "is older than" "$work/5.log"; then
  pass "refused without --downgrade"
else
  fail "downgrade"; cat "$work/5.log"
fi

echo "== 6. a hub that does not answer at the start"
ln -sfn "$lib/dev-test-update/shelfgauge" "$link"
systemctl stop shelfgauge-hub-staging
if run_update "$work/good" "$tag" >"$work/6.log" 2>&1 && grep -q "running: not answering" "$work/6.log" &&
  [ "$(readlink -f "$link")" = "$lib/$version/shelfgauge" ] && [ "$(answer)" = "$version" ]; then
  pass "installed and started"
else
  fail "a stopped hub"; cat "$work/6.log"
fi

[ "$failed" = 0 ] && echo "== all trials passed" || echo "== some trials FAILED"
exit "$failed"
