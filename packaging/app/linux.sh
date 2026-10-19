#!/usr/bin/env bash
# Builds tq-app for Linux and its release tarball:
#   <outdir>/tq-app-<version>-linux-<arch>.tar.gz   (what the in-app updater downloads)
#
#   packaging/app/linux.sh [outdir]           # default dist/app
#
# The tarball holds tq-app-<version>-linux-<arch>/ with the binary, a
# .desktop entry and its icon. Building needs GPUI's system libraries (see docs/install.md).
set -euo pipefail
cd "$(dirname "$0")/../.."
OUT=${1:-dist/app}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/tq-app/Cargo.toml | head -n1)
[ -n "$VERSION" ] || { echo "no version in crates/tq-app/Cargo.toml" >&2; exit 1; }
ARCH=$(uname -m)
# gpui-kit needs Rust 1.95; CI selects it with RUSTUP_TOOLCHAIN.
TOOLCHAIN=${RUSTUP_TOOLCHAIN:-1.95}

# shellcheck disable=SC2086 # CARGO_FLAGS may be empty or several flags
RUSTUP_TOOLCHAIN=$TOOLCHAIN cargo build --release ${CARGO_FLAGS---locked} -p tq-app
# The updater runs the new binary's --version before it installs it.
GOT=$(target/release/tq-app --version)
[ "$GOT" != "${GOT#"tq-app $VERSION "}" ] || { echo "tq-app --version says '$GOT', expected $VERSION" >&2; exit 1; }

NAME="tq-app-$VERSION-linux-$ARCH"
mkdir -p "$OUT"
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
mkdir "$WORK/$NAME"
cp target/release/tq-app "$WORK/$NAME/tq-app"
chmod 0755 "$WORK/$NAME/tq-app"
cp crates/tq-app/assets/tq-app.png "$WORK/$NAME/tq-app.png"
cat >"$WORK/$NAME/tq-app.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=tq
Comment=tq operator / viewer
Exec=tq-app
Icon=tq-app
StartupWMClass=tq-app
Terminal=false
Categories=Network;Video;
EOF
tar -C "$WORK" -czf "$OUT/$NAME.tar.gz" "$NAME"
echo "$OUT/$NAME.tar.gz"
