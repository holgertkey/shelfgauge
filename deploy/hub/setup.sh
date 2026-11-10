#!/bin/bash
# Sets up a shelfgauge hub server on Debian 13; see docs/HUB.md, "Automatic setup".
# Run as root from a copy of deploy/hub/ on the server, with setup.env next to it
# (from setup.env.example):
#
#   scp -r deploy/hub root@server:shelfgauge-setup
#   ssh root@server 'cd shelfgauge-setup && cp setup.env.example setup.env'  # then edit
#   ssh root@server shelfgauge-setup/setup.sh --check     # what would change (diffs)
#   ssh root@server shelfgauge-setup/setup.sh [--version vX.Y.Z]
#
# Idempotent: a second run changes nothing, and a run after an edit of setup.env changes
# only what the edit is about. It never overwrites a hub's hub.toml or data. It installs
# no shelfgauge binary of its own: the first deploy does (update.sh), or --version, which
# runs that update for production (and staging) at the end. A server set up by hand per
# docs/HUB.md (a plain /usr/local/bin/shelfgauge) is moved to per-hub links with the
# same binary; production restarts once for that.
#
# What it does: packages (ufw, sqlite3, unattended-upgrades, Caddy and gh from their
# apt repositories, keys checked against pinned fingerprints); key-only SSH; the
# firewall (22, 80, 443); security updates with a nightly reboot when one is needed;
# the user `shelfgauge`, the hubs' data directories, units and Caddy sites; update.sh as
# /usr/local/sbin/shelfgauge-hub-update and the user `deploy` that may run only it; the
# backup (pull or restic) and the site check.
set -euo pipefail
umask 022

here=$(cd "$(dirname "$0")" && pwd)
install_version=
check=0
while [ $# -gt 0 ]; do
  case "$1" in
    --check) check=1 ;;
    --version)
      install_version=${2:-}
      shift
      ;;
    *)
      echo "usage: setup.sh [--check] [--version vX.Y.Z]" >&2
      exit 2
      ;;
  esac
  shift
done

die() {
  echo "setup.sh: $*" >&2
  exit 1
}
say() { echo "== $*"; }
# x <command...>: runs it, or with --check only says it would.
x() {
  if [ "$check" = 1 ]; then
    echo "would run: $*"
  else
    "$@"
  fi
}

[ "$(id -u)" = 0 ] || die "run as root"
[ -r "$here/setup.env" ] || die "no $here/setup.env (copy setup.env.example and edit it)"
[ "$check" = 1 ] || chmod 600 "$here/setup.env"
HUB_DOMAIN='' STAGING_DOMAIN='' OPERATOR_NAME='' OPERATOR_CONTACT='' ADMIN_USER=root
ADMIN_SSH_KEY='' DEPLOY_SSH_KEY='' DEPLOY_STAGING_SSH_KEY='' REPO=holgertkey/shelfgauge BACKUP=none BACKUP_SSH_KEY=''
RESTIC_REPOSITORY='' BACKUP_HC_URL='' WATCH_HC_URL='' REBOOT_TIME=04:00
# shellcheck source=/dev/null
. "$here/setup.env"

domain_re='^[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?(\.[A-Za-z0-9]([A-Za-z0-9-]*[A-Za-z0-9])?)+$'
key_re='^(ssh-ed25519|ecdsa-sha2-nistp[0-9]+|ssh-rsa|sk-ssh-ed25519@openssh.com) [A-Za-z0-9+/=]+( .*)?$'
[[ "$HUB_DOMAIN" =~ $domain_re ]] || die "HUB_DOMAIN is not a domain name: '$HUB_DOMAIN'"
[ -z "$STAGING_DOMAIN" ] || [[ "$STAGING_DOMAIN" =~ $domain_re ]] ||
  die "STAGING_DOMAIN is not a domain name: '$STAGING_DOMAIN'"
[ "$STAGING_DOMAIN" != "$HUB_DOMAIN" ] || die "STAGING_DOMAIN must differ from HUB_DOMAIN"
[[ "$ADMIN_SSH_KEY" =~ $key_re ]] || die "ADMIN_SSH_KEY is not a public key line"
[ -z "$DEPLOY_SSH_KEY" ] || [[ "$DEPLOY_SSH_KEY" =~ $key_re ]] || die "DEPLOY_SSH_KEY is not a public key line"
[ -z "$DEPLOY_STAGING_SSH_KEY" ] || [[ "$DEPLOY_STAGING_SSH_KEY" =~ $key_re ]] ||
  die "DEPLOY_STAGING_SSH_KEY is not a public key line"
[ -z "$DEPLOY_SSH_KEY" ] || [ "$DEPLOY_SSH_KEY" != "$DEPLOY_STAGING_SSH_KEY" ] ||
  die "DEPLOY_SSH_KEY and DEPLOY_STAGING_SSH_KEY must be different keys"
[[ "$ADMIN_USER" =~ ^[a-z_][a-z0-9_-]*$ ]] || die "ADMIN_USER is not a user name"
[[ "$REPO" =~ ^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$ ]] || die "REPO is not owner/name"
[[ "$REBOOT_TIME" =~ ^[0-2][0-9]:[0-5][0-9]$ ]] || die "REBOOT_TIME is not HH:MM"
case "$BACKUP" in
  pull) [[ "$BACKUP_SSH_KEY" =~ $key_re ]] || die "BACKUP=pull needs BACKUP_SSH_KEY" ;;
  restic | none) ;;
  *) die "BACKUP is pull, restic or none" ;;
esac
[ -z "$install_version" ] || [[ "$install_version" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] ||
  die "--version takes vX.Y.Z"
# shellcheck source=/dev/null
. /etc/os-release
[ "${ID:-}" = debian ] && [ "${VERSION_ID:-}" = 13 ] ||
  echo "WARNING: written for Debian 13, this is ${PRETTY_NAME:-unknown}" >&2

changed=()
# put <dest> <mode> [owner[:group]]: the file's content from stdin; replaced only when
# it differs. Returns 0 when it changed.
put() {
  local dest=$1 mode=$2 owner=${3:-root:root} tmp
  tmp=$(mktemp)
  cat >"$tmp"
  if [ -f "$dest" ] && cmp -s "$tmp" "$dest" &&
    [ "$(stat -c '%a %U:%G' "$dest")" = "$mode ${owner%%:*}:${owner#*:}" ]; then
    rm -f "$tmp"
    return 1
  fi
  changed+=("$dest")
  if [ "$check" = 1 ]; then
    echo "would change: $dest ($mode ${owner%%:*}:${owner#*:})"
    # Files only root may read can hold secrets (a ping URL): no content shown.
    case "$mode" in
      600 | 640 | 440) echo "    (content not shown)" ;;
      *)
        diff -u "$dest" "$tmp" 2>/dev/null | tail -n +3 | sed 's/^/    /' || true
        [ -f "$dest" ] || sed 's/^/    + /' "$tmp"
        ;;
    esac
  else
    install -D -m "$mode" -o "${owner%%:*}" -g "${owner#*:}" "$tmp" "$dest"
    echo "changed: $dest"
  fi
  rm -f "$tmp"
}
# put_file <source in deploy/hub> <dest> <mode>
put_file() { put "$2" "$3" <"$here/$1"; }

# key_line <authorized_keys> <owner> <line>: the key in the file with exactly this line
# (options included); another line with the same key is replaced.
key_line() {
  local file=$1 owner=$2 line=$3 blob tmp
  blob=$(awk '{for (i = 1; i <= NF; i++) if ($i ~ /^AAAA/) { print $i; exit }}' <<<"$line")
  grep -qxF -- "$line" "$file" 2>/dev/null && return 0
  changed+=("$file")
  if [ "$check" = 1 ]; then
    echo "would change: $file: ${line:0:100}..."
    return 0
  fi
  install -d -m 0700 -o "$owner" -g "$(id -gn "$owner")" "$(dirname "$file")"
  touch "$file"
  tmp=$(mktemp)
  grep -vF -- "$blob" "$file" >"$tmp" || true
  printf '%s\n' "$line" >>"$tmp"
  [ -s "$file" ] && cp -a "$file" "$file.bak-$(date +%F-%H%M%S)"
  install -m 0600 -o "$owner" -g "$(id -gn "$owner")" "$tmp" "$file"
  rm -f "$tmp"
  echo "changed: $file"
}

# put_sudoers <dest>: like put, but the content must pass visudo before it is installed
# (a broken file in /etc/sudoers.d breaks sudo as a whole).
put_sudoers() {
  local tmp
  tmp=$(mktemp)
  cat >"$tmp"
  visudo -cf "$tmp" >/dev/null || { rm -f "$tmp"; die "visudo refuses the content of $1"; }
  put "$1" 440 <"$tmp" || true
  rm -f "$tmp"
}

apt_install() {
  local missing=() p
  for p in "$@"; do
    dpkg-query -W -f '${Status}' "$p" 2>/dev/null | grep -q 'install ok installed' || missing+=("$p")
  done
  [ ${#missing[@]} -eq 0 ] && return 0
  x env DEBIAN_FRONTEND=noninteractive apt-get install -y -q "${missing[@]}"
  changed+=("packages: ${missing[*]}")
}

# apt_repo <name> <key url> <fingerprint of the primary key> <keyring> <sources line>
apt_repo() {
  local name=$1 url=$2 fpr=$3 keyring=$4 line=$5 tmp got
  if [ ! -s "$keyring" ]; then
    tmp=$(mktemp)
    curl -fsSL --retry 3 -m 60 "$url" -o "$tmp"
    got=$(gpg --show-keys --with-colons "$tmp" 2>/dev/null | awk -F: '$1 == "fpr" { print $10; exit }')
    [ "$got" = "$fpr" ] || die "the $name signing key is $got, expected $fpr"
    gpg --dearmor <"$tmp" >"$tmp.gpg" 2>/dev/null || cp "$tmp" "$tmp.gpg"
    x install -D -m 0644 "$tmp.gpg" "$keyring"
    rm -f "$tmp" "$tmp.gpg"
    changed+=("$keyring")
  fi
  if put "/etc/apt/sources.list.d/$name.list" 644 <<<"$line"; then
    apt_updated=0
  fi
}

# --- packages --------------------------------------------------------------------------
say "packages"
[ "$check" = 1 ] || apt-get update -q >/dev/null
apt_updated=1
apt_install ca-certificates curl gpg sudo ufw sqlite3 openssl unattended-upgrades
apt_repo caddy-stable https://dl.cloudsmith.io/public/caddy/stable/gpg.key \
  65760C51EDEA2017CEA2CA15155B6D79CA56EA34 /usr/share/keyrings/caddy-stable-archive-keyring.gpg \
  "deb [signed-by=/usr/share/keyrings/caddy-stable-archive-keyring.gpg] https://dl.cloudsmith.io/public/caddy/stable/deb/debian any-version main"
apt_repo github-cli https://cli.github.com/packages/githubcli-archive-keyring.gpg \
  2C6106201985B60E6C7AC87323F3D4EA75716059 /etc/apt/keyrings/githubcli-archive-keyring.gpg \
  "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/githubcli-archive-keyring.gpg] https://cli.github.com/packages stable main"
[ "$apt_updated" = 1 ] || x apt-get update -q
apt_install caddy gh
[ "$BACKUP" != restic ] || apt_install restic

# --- the system ------------------------------------------------------------------------
say "automatic updates (security, Caddy, gh; reboot at $REBOOT_TIME when needed)"
put /etc/apt/apt.conf.d/52shelfgauge-upgrades 644 <<EOF || true
// shelfgauge hub (setup.sh): Caddy and gh from their repositories with the system,
// a reboot at night when an update needs one. Debian's 50unattended-upgrades is untouched.
Unattended-Upgrade::Origins-Pattern {
        "origin=cloudsmith/caddy/stable,codename=any-version";
        "origin=gh,codename=stable";
};
Unattended-Upgrade::Automatic-Reboot "true";
Unattended-Upgrade::Automatic-Reboot-Time "$REBOOT_TIME";
Unattended-Upgrade::Remove-Unused-Kernel-Packages "true";
Unattended-Upgrade::Remove-Unused-Dependencies "true";
EOF
put /etc/apt/apt.conf.d/20auto-upgrades 644 <<'EOF' || true
APT::Periodic::Update-Package-Lists "1";
APT::Periodic::Unattended-Upgrade "1";
EOF

if [ -d /etc/systemd/resolved.conf.d ] || systemctl is-active -q systemd-resolved 2>/dev/null; then
  if put /etc/systemd/resolved.conf.d/no-llmnr.conf 644 <<'EOF'; then
# shelfgauge hub (setup.sh): no LLMNR responder on the public network (port 5355).
[Resolve]
LLMNR=no
EOF
    x systemctl restart systemd-resolved
  fi
fi

say "SSH: keys only, admin $ADMIN_USER"
if [ "$ADMIN_USER" = root ]; then
  root_login=prohibit-password
else
  root_login=forced-commands-only
  if ! id "$ADMIN_USER" >/dev/null 2>&1; then
    x useradd --create-home --shell /bin/bash --groups sudo "$ADMIN_USER"
    changed+=("user $ADMIN_USER")
  fi
  put_sudoers /etc/sudoers.d/90-shelfgauge-admin <<<"$ADMIN_USER ALL=(ALL) NOPASSWD:ALL"
fi
admin_home=$(getent passwd "$ADMIN_USER" | cut -d: -f6)
key_line "$admin_home/.ssh/authorized_keys" "$ADMIN_USER" "$ADMIN_SSH_KEY"
if put /etc/ssh/sshd_config.d/00-shelfgauge.conf 644 <<EOF; then
# shelfgauge hub (setup.sh): key logins only.
PasswordAuthentication no
KbdInteractiveAuthentication no
PermitRootLogin $root_login
EOF
  [ "$check" = 1 ] || sshd -t || die "sshd refuses its configuration"
  x systemctl reload ssh
fi

say "firewall: 22, 80, 443"
for port in 22 80 443; do
  # A rule of its own or one of a list (`22,80,443/tcp`).
  if ! ufw status | awk '$2 == "ALLOW" { print $1 }' | grep -qE "(^|,)$port(,|/tcp$)"; then
    x ufw allow "$port/tcp" >/dev/null
    changed+=("ufw $port/tcp")
  fi
done
if ! ufw status | grep -q '^Status: active'; then
  x ufw default deny incoming >/dev/null
  x ufw --force enable
  changed+=("ufw enabled")
fi

# --- the hubs --------------------------------------------------------------------------
say "the hubs"
if ! id shelfgauge >/dev/null 2>&1; then
  x useradd --system --home-dir /srv/shelfgauge-hub --shell /usr/sbin/nologin shelfgauge
  changed+=("user shelfgauge")
fi

# toml_q <text>: a TOML basic string.
toml_q() {
  local t=${1//\\/\\\\}
  printf '"%s"' "${t//\"/\\\"}"
}
# hub_toml <example> <data> <domain> <max_total_bytes or empty>: a first hub.toml only.
hub_toml() {
  local example=$1 data=$2 domain=$3 total=$4 line
  [ -e "$data/hub.toml" ] && return 0
  changed+=("$data/hub.toml")
  if [ "$check" = 1 ]; then
    echo "would write a first $data/hub.toml from $example"
    return 0
  fi
  install -d -m 0750 -o shelfgauge -g shelfgauge "$data"
  while IFS= read -r line; do
    case "$line" in
      'allowed_hosts = '*) line="allowed_hosts = [$(toml_q "$domain")]" ;;
      'name = ""') line="name = $(toml_q "$OPERATOR_NAME")" ;;
      'contact = ""') line="contact = $(toml_q "$OPERATOR_CONTACT")" ;;
      'max_total_bytes = '*) [ -z "$total" ] || line="max_total_bytes = $total" ;;
    esac
    printf '%s\n' "$line"
  done <"$here/$example" >"$data/hub.toml.new"
  chown shelfgauge:shelfgauge "$data/hub.toml.new"
  chmod 0640 "$data/hub.toml.new"
  mv "$data/hub.toml.new" "$data/hub.toml"
  echo "changed: $data/hub.toml (check it: docs/HUB.md)"
}
free=$(df --output=avail -B1 /srv 2>/dev/null | tail -1 || df --output=avail -B1 / | tail -1)
# About 70 % of the free disk, leaving room for the staging hub and the backup spool.
hub_toml hub.toml.example /srv/shelfgauge-hub "$HUB_DOMAIN" $((free * 7 / 10))
[ -z "$STAGING_DOMAIN" ] || hub_toml hub-staging.toml.example /srv/shelfgauge-hub-staging "$STAGING_DOMAIN" ''

# Binaries: /usr/local/lib/shelfgauge/<version>/shelfgauge, one link per hub, and
# /usr/local/bin/shelfgauge for the hub commands (following production).
lib=/usr/local/lib/shelfgauge
x install -d -m 0755 "$lib"
plain=/usr/local/bin/shelfgauge
if [ ! -e /usr/local/bin/shelfgauge-hub ] && [ -f "$plain" ] && [ ! -L "$plain" ]; then
  v=$("$plain" --version | awk '{print $2}')
  [[ "$v" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "$plain reports version '$v'"
  if [ ! -e "$lib/$v/shelfgauge" ]; then
    x install -D -m 0755 "$plain" "$lib/$v/shelfgauge"
  fi
  if [ -e "$lib/$v/shelfgauge" ]; then
    cmp -s "$plain" "$lib/$v/shelfgauge" || die "$plain differs from $lib/$v/shelfgauge"
  fi
  x ln -s "$lib/$v/shelfgauge" /usr/local/bin/shelfgauge-hub
  changed+=("/usr/local/bin/shelfgauge-hub -> $lib/$v/shelfgauge")
  would_link=1
fi
# has_link <path>: the link is there (or, with --check, would be by now).
would_link=${would_link:-0}
has_link() { [ -e "$1" ] || { [ "$check" = 1 ] && [ "$would_link" = 1 ]; }; }
if has_link /usr/local/bin/shelfgauge-hub; then
  if [ -f "$plain" ] && [ ! -L "$plain" ] && [ -e /usr/local/bin/shelfgauge-hub ]; then
    cmp -s "$plain" /usr/local/bin/shelfgauge-hub ||
      die "$plain differs from what /usr/local/bin/shelfgauge-hub runs; remove one of them"
    x rm -f "$plain"
  fi
  if [ "$(readlink "$plain" 2>/dev/null)" != shelfgauge-hub ]; then
    x ln -sfn shelfgauge-hub "$plain"
    changed+=("$plain -> shelfgauge-hub")
  fi
  if [ -f "$plain" ] && [ ! -L "$plain" ] && [ "$check" = 1 ] && [ ! -e /usr/local/bin/shelfgauge-hub ]; then
    echo "would run: rm -f $plain"
  fi
  if [ -n "$STAGING_DOMAIN" ] && [ ! -e /usr/local/bin/shelfgauge-hub-staging ]; then
    x ln -s "$(readlink -f /usr/local/bin/shelfgauge-hub)" /usr/local/bin/shelfgauge-hub-staging
    changed+=("/usr/local/bin/shelfgauge-hub-staging")
  fi
fi

# unit <name>: installs the unit; restarts it when the unit changed and it can run.
unit() {
  local name=$1 link=$2
  if put_file "$name.service" "/etc/systemd/system/$name.service" 644; then
    x systemctl daemon-reload
    x systemctl enable -q "$name"
    if has_link "$link"; then
      x systemctl restart "$name"
      [ "$check" = 1 ] || echo "restarted $name"
    fi
  elif has_link "$link" && ! systemctl is-active -q "$name"; then
    x systemctl enable -q --now "$name"
    changed+=("started $name")
  fi
}
unit shelfgauge-hub /usr/local/bin/shelfgauge-hub
if [ -n "$STAGING_DOMAIN" ]; then
  unit shelfgauge-hub-staging /usr/local/bin/shelfgauge-hub-staging
elif [ -f /etc/systemd/system/shelfgauge-hub-staging.service ]; then
  echo "NOTE: STAGING_DOMAIN is empty, but shelfgauge-hub-staging is installed (left as it is)" >&2
fi

say "Caddy"
{
  echo "# Written by shelfgauge's setup.sh (deploy/hub/Caddyfile, Caddyfile.staging);"
  echo "# changes here are replaced by its next run. No log directive: no access log."
  echo
  echo "$HUB_DOMAIN {"
  printf '\treverse_proxy 127.0.0.1:8743\n}\n'
  if [ -n "$STAGING_DOMAIN" ]; then
    echo
    echo "$STAGING_DOMAIN {"
    printf '\treverse_proxy 127.0.0.1:8744\n}\n'
  fi
} >/etc/caddy/Caddyfile.new
if cmp -s /etc/caddy/Caddyfile.new /etc/caddy/Caddyfile; then
  rm -f /etc/caddy/Caddyfile.new
elif [ "$check" = 1 ]; then
  echo "would change: /etc/caddy/Caddyfile (then reload Caddy)"
  diff -u /etc/caddy/Caddyfile /etc/caddy/Caddyfile.new | tail -n +3 | sed 's/^/    /' || true
  rm -f /etc/caddy/Caddyfile.new
  changed+=(/etc/caddy/Caddyfile)
else
  caddy validate --config /etc/caddy/Caddyfile.new --adapter caddyfile >/dev/null 2>&1 ||
    die "Caddy refuses /etc/caddy/Caddyfile.new"
  [ -f /etc/caddy/Caddyfile ] && cp -a /etc/caddy/Caddyfile "/etc/caddy/Caddyfile.bak-$(date +%F-%H%M%S)"
  chmod 644 /etc/caddy/Caddyfile.new
  mv /etc/caddy/Caddyfile.new /etc/caddy/Caddyfile
  systemctl reload caddy 2>/dev/null || systemctl restart caddy
  changed+=(/etc/caddy/Caddyfile)
fi

# --- deploys ---------------------------------------------------------------------------
say "updates and deploys"
put_file update.sh /usr/local/sbin/shelfgauge-hub-update 755 || true
put /etc/shelfgauge-hub/update.conf 644 <<EOF || true
# Written by setup.sh; read by shelfgauge-hub-update.
REPO=$REPO
PRODUCTION_DOMAIN=$HUB_DOMAIN
STAGING_DOMAIN=$STAGING_DOMAIN
EOF
if [ -n "$DEPLOY_SSH_KEY$DEPLOY_STAGING_SSH_KEY" ]; then
  if ! id deploy >/dev/null 2>&1; then
    # A shell is needed to run the forced command; the key can run nothing else.
    x useradd --system --create-home --home-dir /var/lib/shelfgauge-deploy --shell /bin/bash deploy
    x passwd -l deploy >/dev/null
    changed+=("user deploy")
  fi
  # One key per target, each in its own GitHub Environment: the staging job (no
  # reviewer) holds a key that cannot deploy to production.
  [ -z "$DEPLOY_SSH_KEY" ] || key_line /var/lib/shelfgauge-deploy/.ssh/authorized_keys deploy \
    "command=\"/usr/local/sbin/shelfgauge-hub-update production\",restrict $DEPLOY_SSH_KEY"
  [ -z "$DEPLOY_STAGING_SSH_KEY" ] || key_line /var/lib/shelfgauge-deploy/.ssh/authorized_keys deploy \
    "command=\"/usr/local/sbin/shelfgauge-hub-update staging\",restrict $DEPLOY_STAGING_SSH_KEY"
  # sudo's regular expressions (1.9.10 and later): exactly what update.sh passes on
  # ([.], not a backslash, which sudoers reads twice).
  put_sudoers /etc/sudoers.d/50-shelfgauge-deploy <<'EOF'
# Written by setup.sh: the deploy key's forced command runs update.sh as root, with a
# release version and a target only.
deploy ALL=(root) NOPASSWD: /usr/local/sbin/shelfgauge-hub-update ^-- v[0-9]+[.][0-9]+[.][0-9]+ (production|staging)$
EOF
fi

# --- backups ---------------------------------------------------------------------------
say "backups: $BACKUP"
case "$BACKUP" in
  pull)
    put_file shelfgauge-hub-backup-send /usr/local/sbin/shelfgauge-hub-backup-send 755 || true
    key_line /root/.ssh/authorized_keys root \
      "restrict,command=\"/usr/local/sbin/shelfgauge-hub-backup-send\" $BACKUP_SSH_KEY"
    ;;
  restic)
    put_file shelfgauge-hub-backup /usr/local/sbin/shelfgauge-hub-backup 755 || true
    if [ ! -e /etc/shelfgauge-hub/backup.env ]; then
      [ -n "$RESTIC_REPOSITORY" ] || die "BACKUP=restic needs RESTIC_REPOSITORY"
      x install -d -m 0700 /etc/shelfgauge-hub
      if [ "$check" = 0 ] && [ ! -e /etc/shelfgauge-hub/restic-password ]; then
        (umask 077 && openssl rand -base64 33 >/etc/shelfgauge-hub/restic-password)
        echo "NOTE: a new restic password in /etc/shelfgauge-hub/restic-password: keep a copy off the server" >&2
      fi
      put /etc/shelfgauge-hub/backup.env 600 <<EOF || true
# Written once by setup.sh; add the provider's credentials (docs/HUB.md, "Backups").
# Backblaze B2 through its S3-compatible API: a key for the bucket only, without
# deleteFiles (restic then only hides files, and the bucket keeps hidden versions for
# 30 days), and the region of the bucket's endpoint.
RESTIC_REPOSITORY=$RESTIC_REPOSITORY
RESTIC_PASSWORD_FILE=/etc/shelfgauge-hub/restic-password
AWS_ACCESS_KEY_ID=
AWS_SECRET_ACCESS_KEY=
AWS_DEFAULT_REGION=
HC_URL=$BACKUP_HC_URL
EOF
    fi
    put_file shelfgauge-hub-backup.service /etc/systemd/system/shelfgauge-hub-backup.service 644 &&
      x systemctl daemon-reload
    put_file shelfgauge-hub-backup.timer /etc/systemd/system/shelfgauge-hub-backup.timer 644 &&
      x systemctl daemon-reload
    systemctl is-enabled -q shelfgauge-hub-backup.timer || {
      x systemctl enable -q --now shelfgauge-hub-backup.timer
      changed+=("shelfgauge-hub-backup.timer")
    }
    ;;
  none) ;;
esac

# --- the site check --------------------------------------------------------------------
say "site check"
if [ -n "$WATCH_HC_URL" ] || [ -e /etc/shelfgauge-hub-watch.env ]; then
  hc=$WATCH_HC_URL
  [ -n "$hc" ] || hc=$(sed -n 's/^HC_URL=//p' /etc/shelfgauge-hub-watch.env)
  put /etc/shelfgauge-hub-watch.env 600 <<EOF || true
HUB_DOMAIN=$HUB_DOMAIN
HC_URL=$hc
EOF
  if ! id shelfgauge-watch >/dev/null 2>&1; then
    x useradd --system --no-create-home --shell /usr/sbin/nologin shelfgauge-watch
    changed+=("user shelfgauge-watch")
  fi
  put_file shelfgauge-hub-watch /usr/local/sbin/shelfgauge-hub-watch 755 || true
  r=1
  put_file shelfgauge-hub-watch.service /etc/systemd/system/shelfgauge-hub-watch.service 644 && r=0
  put_file shelfgauge-hub-watch.timer /etc/systemd/system/shelfgauge-hub-watch.timer 644 && r=0
  [ "$r" = 1 ] || x systemctl daemon-reload
  systemctl is-enabled -q shelfgauge-hub-watch.timer || {
    x systemctl enable -q --now shelfgauge-hub-watch.timer
    changed+=("shelfgauge-hub-watch.timer")
  }
else
  echo "no WATCH_HC_URL: no site check"
fi

# --- a first version -------------------------------------------------------------------
if [ -n "$install_version" ]; then
  say "installing $install_version"
  x /usr/local/sbin/shelfgauge-hub-update "$install_version" production
  [ -z "$STAGING_DOMAIN" ] || x /usr/local/sbin/shelfgauge-hub-update "$install_version" staging
fi

if [ ${#changed[@]} -eq 0 ]; then
  say "nothing changed"
elif [ "$check" = 1 ]; then
  say "--check: ${#changed[@]} changes would be made:"
  printf '  %s\n' "${changed[@]}"
else
  say "${#changed[@]} changes:"
  printf '  %s\n' "${changed[@]}"
fi
