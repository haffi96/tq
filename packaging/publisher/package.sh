#!/usr/bin/env bash
# Packages one tq-publisher binary: a tarball for every platform (used by
# install.sh) and, for Linux, a .deb. One compile, two distributions.
#
#   packaging/publisher/package.sh <version> <platform> <binary> <outdir>
#   platform: linux-x86_64 | linux-aarch64 | macos-aarch64
#
# Needs dpkg-deb for the .deb (Ubuntu: dpkg; macOS: brew install dpkg).
set -euo pipefail

[ $# -eq 4 ] || { sed -n '2,8p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
VERSION=$1
PLATFORM=$2
BINARY=$3
OUT=$4
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)

case "$PLATFORM" in
  linux-x86_64) DEB_ARCH=amd64 ;;
  linux-aarch64) DEB_ARCH=arm64 ;;
  macos-aarch64) DEB_ARCH="" ;;
  *) echo "unknown platform $PLATFORM" >&2; exit 2 ;;
esac
[ -x "$BINARY" ] || { echo "$BINARY is not an executable" >&2; exit 1; }
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# --- tarball ------------------------------------------------------------------

NAME="tq-publisher-$VERSION-$PLATFORM"
mkdir "$WORK/$NAME"
cp "$BINARY" "$WORK/$NAME/tq-publisher"
chmod 0755 "$WORK/$NAME/tq-publisher"
cp "$ROOT/config/publisher.example.json" "$HERE/install.sh" "$HERE/tq-publisher.service" \
  "$HERE/dev.tq.publisher.plist" "$WORK/$NAME/"
# No macOS extended attributes: GNU tar warns about each one.
COPYFILE_DISABLE=1 tar --no-xattrs -C "$WORK" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "$OUT/$NAME.tar.gz"

# --- .deb ---------------------------------------------------------------------

[ -n "$DEB_ARCH" ] || exit 0
DEB="$WORK/deb"
mkdir -p "$DEB/DEBIAN" "$DEB/usr/bin" "$DEB/lib/systemd/system" "$DEB/usr/share/tq-publisher"
cp "$BINARY" "$DEB/usr/bin/tq-publisher"
chmod 0755 "$DEB/usr/bin/tq-publisher"
cp "$HERE/tq-publisher.service" "$DEB/lib/systemd/system/"
cp "$ROOT/config/publisher.example.json" "$DEB/usr/share/tq-publisher/"
chmod 0644 "$DEB/lib/systemd/system/tq-publisher.service" "$DEB/usr/share/tq-publisher/publisher.example.json"

cat >"$DEB/DEBIAN/control" <<EOF
Package: tq-publisher
Version: $VERSION
Architecture: $DEB_ARCH
Maintainer: haffi96 <haffi96@users.noreply.github.com>
Depends: libc6 (>= 2.31), passwd
Section: net
Priority: optional
Homepage: https://github.com/haffi96/tq
Description: tq publisher: camera uplink and teleop over QUIC
 Reads pre-encoded H.264/H.265 camera streams over TCP and sends them to a
 tq relay over WebTransport, one stream per GOP. Applies operator teleop
 commands behind a dead-man switch. Runs as the tq-publisher systemd service.
EOF

# The config is not a conffile: postinst seeds /etc/tq from the example
# once, and upgrades never touch it (same as install.sh).
cat >"$DEB/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = configure ]; then
  if ! id tq >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin tq
  fi
  mkdir -p /etc/tq
  fresh=0
  if [ ! -f /etc/tq/publisher.json ]; then
    cp /usr/share/tq-publisher/publisher.example.json /etc/tq/publisher.json
    fresh=1
  fi
  if [ ! -f /etc/tq/publisher.env ]; then
    printf '# Secrets and settings for tq-publisher (KEY=value).\nTQ_SHARED_SECRET=\n#RUST_LOG=info,quinn=warn\n' >/etc/tq/publisher.env
  fi
  chgrp tq /etc/tq /etc/tq/publisher.json /etc/tq/publisher.env
  chmod 0750 /etc/tq
  chmod 0640 /etc/tq/publisher.json /etc/tq/publisher.env
  if [ -d /run/systemd/system ]; then
    systemctl daemon-reload
    systemctl enable tq-publisher >/dev/null 2>&1 || true
    if [ $fresh = 1 ]; then
      echo "tq-publisher: edit /etc/tq/publisher.json and /etc/tq/publisher.env, then: systemctl start tq-publisher"
    else
      systemctl try-restart tq-publisher || true
    fi
  fi
fi
EOF

cat >"$DEB/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
  systemctl disable --now tq-publisher >/dev/null 2>&1 || true
fi
EOF

cat >"$DEB/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
  systemctl daemon-reload || true
fi
if [ "$1" = purge ]; then
  rm -rf /etc/tq
  userdel tq >/dev/null 2>&1 || true
fi
EOF
chmod 0755 "$DEB/DEBIAN/postinst" "$DEB/DEBIAN/prerm" "$DEB/DEBIAN/postrm"

# xz, not zstd: dpkg before 1.21.18 (Ubuntu 20.04, JetPack 5) cannot read zstd.
DEB_FILE="$OUT/tq-publisher_${VERSION}_$DEB_ARCH.deb"
dpkg-deb --root-owner-group -Zxz --build "$DEB" "$DEB_FILE" >/dev/null
echo "$DEB_FILE"
