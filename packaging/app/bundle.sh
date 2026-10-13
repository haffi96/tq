#!/usr/bin/env bash
# Builds tq-app.app and its release files:
#   dist/tq-app.app
#   dist/tq-app-<version>-macos-arm64.zip   (what the in-app updater downloads)
#   dist/tq-app-<version>-macos-arm64.dmg   (for people)
#
#   packaging/app/bundle.sh [outdir]          # default dist/app
#
# Signs ad hoc unless CODESIGN_IDENTITY names a Developer ID. Ad hoc builds
# are not notarized: the first open from a download needs right-click > Open
# (or xattr -dr com.apple.quarantine). Updates the app installs itself are
# not quarantined.
set -euo pipefail
cd "$(dirname "$0")/../.."
OUT=${1:-dist/app}
VERSION=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/tq-app/Cargo.toml | head -n1)
[ -n "$VERSION" ] || { echo "no version in crates/tq-app/Cargo.toml" >&2; exit 1; }
# gpui-kit needs Rust 1.95; CI selects it with RUSTUP_TOOLCHAIN.
TOOLCHAIN=${RUSTUP_TOOLCHAIN:-1.95}

# shellcheck disable=SC2086 # CARGO_FLAGS may be empty or several flags
RUSTUP_TOOLCHAIN=$TOOLCHAIN cargo build --release ${CARGO_FLAGS---locked} -p tq-app
APP="$OUT/tq-app.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/tq-app "$APP/Contents/MacOS/tq-app"
cat >"$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>tq-app</string>
  <key>CFBundleDisplayName</key><string>tq</string>
  <key>CFBundleIdentifier</key><string>dev.tq.app</string>
  <key>CFBundleExecutable</key><string>tq-app</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
EOF
plutil -lint "$APP/Contents/Info.plist" >/dev/null
codesign --force --deep --options runtime --sign "${CODESIGN_IDENTITY:--}" "$APP"
codesign --verify --deep --strict "$APP"

ZIP="$OUT/tq-app-$VERSION-macos-arm64.zip"
DMG="$OUT/tq-app-$VERSION-macos-arm64.dmg"
rm -f "$ZIP" "$DMG"
ditto -c -k --keepParent "$APP" "$ZIP"
STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -quiet -volname "tq-app $VERSION" -srcfolder "$STAGE" -fs HFS+ -format UDZO -ov "$DMG"
echo "$APP"
echo "$ZIP"
echo "$DMG"
