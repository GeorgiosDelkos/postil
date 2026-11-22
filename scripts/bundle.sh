#!/usr/bin/env bash
# Builds the release binary and wraps it in a macOS app bundle, target/release/Postil.app, so
# the Dock, Finder, Cmd-Tab and the About box show Postil's name and icon. A bare binary run
# with `cargo run` gets the generic icon: macOS reads both from the bundle's Info.plist.
#
# Usage: scripts/bundle.sh, then open target/release/Postil.app. Needs only what macOS ships:
# sips and iconutil make the .icns from assets/icon-1024.png.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"

cargo build --release --locked
# `cargo pkgid` ends in `#<version>`, or in `#postil@<version>` when the package name differs
# from its directory's.
version="$(cargo pkgid | sed 's/.*[#@]//')"

app="target/release/Postil.app"
if [[ -e "$app" ]]; then
  trash "$app"
fi
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/postil "$app/Contents/MacOS/postil"

# The .icns holds the icon at every size macOS asks for, from 16 to 1024 px.
work="$(mktemp -d)"
trap 'trash "$work"' EXIT
iconset="$work/Postil.iconset"
mkdir "$iconset"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" assets/icon-1024.png --out "$iconset/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" assets/icon-1024.png \
    --out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$iconset" -o "$app/Contents/Resources/Postil.icns"

# The identifier is reverse DNS of the project's home, which Postil does not own a domain for.
cat >"$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>Postil</string>
  <key>CFBundleDisplayName</key>
  <string>Postil</string>
  <key>CFBundleIdentifier</key>
  <string>io.github.georgiosdelkos.postil</string>
  <key>CFBundleExecutable</key>
  <string>postil</string>
  <key>CFBundleIconFile</key>
  <string>Postil</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>${version}</string>
  <key>CFBundleVersion</key>
  <string>${version}</string>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
</dict>
</plist>
PLIST
plutil -lint "$app/Contents/Info.plist" >/dev/null

echo "built $app ($version)"
