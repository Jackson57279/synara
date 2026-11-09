#!/usr/bin/env bash
# Package the native client as a macOS app bundle and zip it.
#
#   native/scripts/build-macos-app.sh [OUT_DIR] BINARY [BINARY...]
#
# Pass one binary per architecture (for example the aarch64-apple-darwin and
# x86_64-apple-darwin release builds); more than one is merged with lipo into
# a universal binary. With no binaries, uses native/target/release/synara.
#
# The bundle is ad-hoc signed so Apple silicon will run it. It is not
# notarized: Gatekeeper blocks a downloaded copy until the user allows it
# (right-click > Open, or `xattr -dr com.apple.quarantine`).
#
# The app is the client only. It connects to a server with --url/--token,
# supervises a source checkout with --spawn, or uses --instabox. Finder
# launches it without arguments, so run the binary inside the bundle from a
# terminal to pass them.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:-$root/native/dist}"
shift || true
bins=("$@")
[[ ${#bins[@]} -gt 0 ]] || bins=("$root/native/target/release/synara")
for bin in "${bins[@]}"; do
  [[ -x "$bin" ]] || { echo "no binary at $bin" >&2; exit 1; }
done

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
app="$work/Synara Native.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

if [[ ${#bins[@]} -gt 1 ]]; then
  lipo -create -output "$app/Contents/MacOS/synara" "${bins[@]}"
  arch=universal
else
  install -m755 "${bins[0]}" "$app/Contents/MacOS/synara"
  arch="$(lipo -archs "${bins[0]}" | tr ' ' '-')"
fi
version="$("$app/Contents/MacOS/synara" --version | awk '{print $2}')"
cp "$root/apps/desktop/resources/icon.icns" "$app/Contents/Resources/synara.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Synara Native</string>
  <key>CFBundleDisplayName</key><string>Synara Native</string>
  <key>CFBundleIdentifier</key><string>com.emanueledipietro.synara.native</string>
  <key>CFBundleExecutable</key><string>synara</string>
  <key>CFBundleIconFile</key><string>synara</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
EOF

codesign --force --deep --sign - "$app"
codesign --verify --deep --strict "$app"

mkdir -p "$out"
target="$out/Synara-Native-$version-macos-$arch.zip"
rm -f "$target"
ditto -c -k --keepParent "$app" "$target"
echo "$target"
