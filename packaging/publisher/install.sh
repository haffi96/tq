#!/usr/bin/env bash
# Installs or upgrades tq-publisher from GitHub Releases (tags publisher-vX.Y.Z)
# on Linux (x86_64, aarch64) and macOS (Apple silicon), as a systemd or
# launchd service.
#
#   curl -fsSL <raw url of this file> | bash                   # newest release
#   curl -fsSL <raw url of this file> | bash -s -- --version 0.2.0
#   ./install.sh --file tq-publisher-0.2.0-linux-aarch64.tar.gz  # offline
#   ./install.sh --uninstall
#
# No token needed. A GITHUB_TOKEN (or `gh` login) is used when present, which
# raises GitHub's API rate limit. See docs/install.md.
#
# Layout: <prefix>/bin/tq-publisher (default /usr/local), the config in
# /etc/tq/publisher.json and secrets in /etc/tq/publisher.env (both kept on
# upgrade). On Linux the service runs as the `tq` system user.
set -euo pipefail

REPO=${TQ_REPO:-haffi96/tq}
API=${TQ_API:-https://api.github.com}
TAG_PREFIX=publisher-v
VERSION=""
FILE=""
PREFIX=/usr/local
CONFIG_DIR=/etc/tq
SERVICE=1
UNINSTALL=0
FORCE=0

usage() {
  cat <<EOF
Installs or upgrades tq-publisher from GitHub Releases ($REPO).

Options:
  --version X.Y.Z     install this release instead of the newest
  --file PATH         install from a downloaded tarball (no network)
  --prefix DIR        binary goes to DIR/bin (default $PREFIX)
  --config-dir DIR    config and secrets (default $CONFIG_DIR)
  --no-service        do not install or restart the system service
  --uninstall         stop the service and remove the binary (keeps the config)
  --force             install even though the .deb is installed
  --repo OWNER/NAME   GitHub repository (default $REPO)
EOF
}

die() { echo "install.sh: $*" >&2; exit 1; }
say() { echo "==> $*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION=${2#v}; shift 2 ;;
    --file) FILE=$2; shift 2 ;;
    --prefix) PREFIX=$2; shift 2 ;;
    --config-dir) CONFIG_DIR=$2; shift 2 ;;
    --no-service) SERVICE=0; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --force) FORCE=1; shift ;;
    --repo) REPO=$2; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done

OS=$(uname -s)
case "$OS/$(uname -m)" in
  Linux/x86_64) PLATFORM=linux-x86_64 ;;
  Linux/aarch64|Linux/arm64) PLATFORM=linux-aarch64 ;;
  Darwin/arm64) PLATFORM=macos-aarch64 ;;
  *) die "no build for $OS $(uname -m)" ;;
esac

# Root only where needed: system paths need it, a --prefix in your home does not.
writable() { local d=$1; while [ ! -e "$d" ]; do d=$(dirname "$d"); done; [ -w "$d" ]; }
SUDO=""
if [ "$(id -u)" -ne 0 ]; then
  if ! writable "$PREFIX/bin" || ! writable "$CONFIG_DIR" || { [ $SERVICE = 1 ] && [ "$OS" = Darwin ]; } ||
    { [ $SERVICE = 1 ] && [ -d /run/systemd/system ]; }; then
    command -v sudo >/dev/null || die "needs root: run as root or install sudo"
    SUDO=sudo
  fi
fi

BIN="$PREFIX/bin/tq-publisher"
UNIT=/etc/systemd/system/tq-publisher.service
PLIST=/Library/LaunchDaemons/dev.tq.publisher.plist
systemd() { [ "$OS" = Linux ] && [ -d /run/systemd/system ]; }

if [ $UNINSTALL = 1 ]; then
  if [ $SERVICE = 1 ] && systemd && [ -f $UNIT ]; then
    $SUDO systemctl disable --now tq-publisher || true
    $SUDO rm -f $UNIT
    $SUDO systemctl daemon-reload
  fi
  if [ $SERVICE = 1 ] && [ "$OS" = Darwin ] && [ -f $PLIST ]; then
    $SUDO launchctl bootout system/dev.tq.publisher 2>/dev/null || true
    $SUDO rm -f $PLIST
  fi
  $SUDO rm -f "$BIN"
  say "removed $BIN; $CONFIG_DIR is kept"
  exit 0
fi

if [ "$OS" = Linux ] && [ $FORCE = 0 ] && command -v dpkg-query >/dev/null &&
  dpkg-query -W -f='${Status}' tq-publisher 2>/dev/null | grep -q "install ok installed"; then
  die "tq-publisher is installed from the .deb; upgrade with apt/dpkg, or pass --force"
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# --- download -----------------------------------------------------------------

TOKEN=${GITHUB_TOKEN:-${GH_TOKEN:-}}
if [ -z "$TOKEN" ] && [ -z "$FILE" ] && command -v gh >/dev/null; then
  TOKEN=$(gh auth token 2>/dev/null || true)
fi

# GET from the API. The token goes to curl on stdin, not on its command line.
fetch() { # url accept [outfile]
  local cfg="header = \"Accept: $2\"
header = \"X-GitHub-Api-Version: 2022-11-28\"
user-agent = \"tq-publisher-install\""
  [ -n "$TOKEN" ] && cfg="$cfg
header = \"Authorization: Bearer $TOKEN\""
  if [ $# -ge 3 ]; then
    printf '%s\n' "$cfg" | curl -fsSL --retry 2 -K - -o "$3" "$1"
  else
    printf '%s\n' "$cfg" | curl -fsSL --retry 2 -K - "$1"
  fi
}
api() { # path [what is missing on 404]
  local hint="rate limited or no access: export GITHUB_TOKEN or log in with gh"
  [ -n "$TOKEN" ] && hint="${2:-does the token have read access to $REPO?}"
  fetch "$API/$1" application/vnd.github+json || die "GitHub API $1 failed ($hint)"
}

# API URL of the asset called $2 in release JSON $1 (also works for private repos).
asset_url() {
  # shellcheck disable=SC2020 # both separators become newlines
  printf '%s' "$1" | tr ',{' '\n\n' | awk -v name="$2" '
    /"url": *"[^"]*\/releases\/assets\/[0-9]+"/ { u = $0 }
    index($0, "\"name\"") && index($0, "\"" name "\"") { print u; exit }' |
    sed -E 's/.*"url": *"([^"]*)".*/\1/'
}

if [ -z "$FILE" ]; then
  command -v curl >/dev/null || die "needs curl"
  if [ -z "$VERSION" ]; then
    LIST=$(api "repos/$REPO/releases?per_page=100")
    VERSION=$(printf '%s' "$LIST" | grep -o "\"tag_name\": *\"${TAG_PREFIX}[^\"]*\"" |
      sed -E "s/.*\"$TAG_PREFIX([^\"]*)\"/\1/" | grep -E '^[0-9]+\.[0-9]+\.[0-9]+$' |
      sort -t. -k1,1n -k2,2n -k3,3n | tail -n1 || true)
    [ -n "$VERSION" ] || die "no $TAG_PREFIX* release in $REPO"
  fi
  NAME="tq-publisher-$VERSION-$PLATFORM.tar.gz"
  say "tq-publisher $VERSION ($PLATFORM) from $REPO"
  RELEASE=$(api "repos/$REPO/releases/tags/$TAG_PREFIX$VERSION" "no release $TAG_PREFIX$VERSION in $REPO, or no access")
  URL=$(asset_url "$RELEASE" "$NAME")
  SUMS_URL=$(asset_url "$RELEASE" SHA256SUMS)
  [ -n "$URL" ] || die "release $TAG_PREFIX$VERSION has no $NAME"
  [ -n "$SUMS_URL" ] || die "release $TAG_PREFIX$VERSION has no SHA256SUMS"
  fetch "$URL" application/octet-stream "$WORK/$NAME" || die "download of $NAME failed"
  fetch "$SUMS_URL" application/octet-stream "$WORK/SHA256SUMS" || die "download of SHA256SUMS failed"
  EXPECTED=$(awk -v n="$NAME" '$2 == n || $2 == "*" n { print $1 }' "$WORK/SHA256SUMS")
  if command -v sha256sum >/dev/null; then
    ACTUAL=$(sha256sum "$WORK/$NAME" | awk '{ print $1 }')
  else
    ACTUAL=$(shasum -a 256 "$WORK/$NAME" | awk '{ print $1 }')
  fi
  [ -n "$EXPECTED" ] && [ "$EXPECTED" = "$ACTUAL" ] || die "checksum mismatch for $NAME"
  FILE="$WORK/$NAME"
fi

# --- install ------------------------------------------------------------------

tar -xzf "$FILE" -C "$WORK"
SRC=$(find "$WORK" -mindepth 1 -maxdepth 1 -type d -name 'tq-publisher-*' | head -n1)
[ -n "$SRC" ] && [ -x "$SRC/tq-publisher" ] || die "$FILE is not a tq-publisher tarball"
case "$(basename "$SRC")" in
  *"-$PLATFORM") ;;
  *) die "$(basename "$FILE") is not for $PLATFORM" ;;
esac

$SUDO mkdir -p "$PREFIX/bin"
$SUDO cp "$SRC/tq-publisher" "$BIN.new"
$SUDO chmod 0755 "$BIN.new"
$SUDO mv -f "$BIN.new" "$BIN"
say "installed $("$BIN" --version)"

GROUP=""
if [ "$OS" = Linux ] && [ $SERVICE = 1 ] && { [ "$(id -u)" = 0 ] || [ -n "$SUDO" ]; }; then
  if ! id tq >/dev/null 2>&1; then
    $SUDO useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin tq
    say "created system user tq"
  fi
  GROUP=tq
fi

FRESH=0
$SUDO mkdir -p "$CONFIG_DIR"
if [ ! -f "$CONFIG_DIR/publisher.json" ]; then
  $SUDO cp "$SRC/publisher.example.json" "$CONFIG_DIR/publisher.json"
  FRESH=1
fi
if [ ! -f "$CONFIG_DIR/publisher.env" ]; then
  printf '# Secrets and settings for tq-publisher (KEY=value).\nTQ_SHARED_SECRET=\n#RUST_LOG=info,quinn=warn\n' |
    $SUDO tee "$CONFIG_DIR/publisher.env" >/dev/null
fi
$SUDO chmod 0750 "$CONFIG_DIR"
$SUDO chmod 0640 "$CONFIG_DIR/publisher.json" "$CONFIG_DIR/publisher.env"
if [ -n "$GROUP" ]; then
  $SUDO chgrp "$GROUP" "$CONFIG_DIR" "$CONFIG_DIR/publisher.json" "$CONFIG_DIR/publisher.env"
fi

if [ $SERVICE = 1 ] && systemd; then
  sed -e "s#/usr/bin/tq-publisher#$BIN#" -e "s#/etc/tq/#$CONFIG_DIR/#g" "$SRC/tq-publisher.service" |
    $SUDO tee $UNIT >/dev/null
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable tq-publisher >/dev/null 2>&1
  if [ $FRESH = 1 ]; then
    say "edit $CONFIG_DIR/publisher.json and $CONFIG_DIR/publisher.env, then: sudo systemctl start tq-publisher"
  else
    $SUDO systemctl restart tq-publisher
    say "restarted tq-publisher (journalctl -u tq-publisher -f)"
  fi
elif [ $SERVICE = 1 ] && [ "$OS" = Darwin ]; then
  sed -e "s#/usr/local/bin/tq-publisher#$BIN#" -e "s#/etc/tq/#$CONFIG_DIR/#g" "$SRC/dev.tq.publisher.plist" |
    $SUDO tee $PLIST >/dev/null
  $SUDO chmod 0644 $PLIST
  if [ $FRESH = 1 ]; then
    say "edit $CONFIG_DIR/publisher.json and $CONFIG_DIR/publisher.env, then: sudo launchctl bootstrap system $PLIST"
  else
    $SUDO launchctl bootout system/dev.tq.publisher 2>/dev/null || true
    $SUDO launchctl bootstrap system $PLIST
    say "restarted tq-publisher (log: /var/log/tq-publisher.log)"
  fi
elif [ $SERVICE = 1 ]; then
  say "no systemd here: run $BIN --config $CONFIG_DIR/publisher.json yourself"
elif [ $FRESH = 1 ]; then
  say "edit $CONFIG_DIR/publisher.json and $CONFIG_DIR/publisher.env"
fi
