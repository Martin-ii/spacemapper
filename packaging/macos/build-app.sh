#!/usr/bin/env bash
# Builds the macOS app into dist/:
#   Spacemapper.app                       the application bundle (ad-hoc signed)
#   Spacemapper-VERSION-macos-arm64.zip   the same bundle, zipped for download
#
# Requirements: macOS, cargo, and the system tools sips, iconutil, codesign
# and ditto. The binary is built for the Mac this runs on.
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
APP=dist/Spacemapper.app
ZIP=dist/Spacemapper-$VERSION-macos-arm64.zip

echo "==> Building spacemapper $VERSION"
cargo build --release --locked

# ---------------------------------------------------------------- bundle
rm -rf "$APP" "$ZIP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
install -m755 target/release/spacemapper "$APP/Contents/MacOS/spacemapper"
sed "s/@VERSION@/$VERSION/g" packaging/macos/Info.plist > "$APP/Contents/Info.plist"

# ---------------------------------------------------------------- icon
# The largest PNG is 512 px; sips scales it for the 16 px and 1024 px slots.
ICONS=$(mktemp -d)
trap 'rm -rf "$ICONS"' EXIT
SET=$ICONS/spacemapper.iconset
mkdir "$SET"
SRC=assets/icon
sips -z 16 16     "$SRC/spacemapper-512.png" --out "$SET/icon_16x16.png"      >/dev/null
cp "$SRC/spacemapper-32.png"  "$SET/icon_16x16@2x.png"
cp "$SRC/spacemapper-32.png"  "$SET/icon_32x32.png"
cp "$SRC/spacemapper-64.png"  "$SET/icon_32x32@2x.png"
cp "$SRC/spacemapper-128.png" "$SET/icon_128x128.png"
cp "$SRC/spacemapper-256.png" "$SET/icon_128x128@2x.png"
cp "$SRC/spacemapper-256.png" "$SET/icon_256x256.png"
cp "$SRC/spacemapper-512.png" "$SET/icon_256x256@2x.png"
cp "$SRC/spacemapper-512.png" "$SET/icon_512x512.png"
sips -z 1024 1024 "$SRC/spacemapper-512.png" --out "$SET/icon_512x512@2x.png" >/dev/null
iconutil -c icns "$SET" -o "$APP/Contents/Resources/spacemapper.icns"

# ---------------------------------------------------------------- sign + zip
echo "==> Signing (ad-hoc) and zipping"
codesign --force --deep -s - "$APP"
ditto -c -k --keepParent --norsrc --noextattr "$APP" "$ZIP"
echo "==> $APP"
echo "==> $ZIP"
