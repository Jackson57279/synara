#!/usr/bin/env bash
# Package the native client as an AppImage.
#
#   native/scripts/build-appimage.sh [BINARY] [OUT_DIR]
#
# BINARY defaults to native/target/release/synara. Build it on an old glibc
# (the bookworm container in native/README or any glibc <= 2.36 host) so the
# AppImage runs on current distros; this script refuses binaries that need a
# newer glibc than SYNARA_APPIMAGE_MAX_GLIBC (default 2.36).
#
# The AppImage is the client only. It connects to a server with --url/--token,
# supervises a source checkout with --spawn, or uses --instabox.
#
# Needs appimagetool on PATH or in APPIMAGETOOL.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
bin="${1:-$root/native/target/release/synara}"
out="${2:-$root/native/dist}"
max_glibc="${SYNARA_APPIMAGE_MAX_GLIBC:-2.36}"
tool="${APPIMAGETOOL:-appimagetool}"

[[ -x "$bin" ]] || { echo "no binary at $bin" >&2; exit 1; }

need="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sed 's/GLIBC_//' | sort -uV | tail -1)"
if [[ "$(printf '%s\n%s\n' "$need" "$max_glibc" | sort -V | tail -1)" != "$max_glibc" ]]; then
  echo "$bin needs glibc $need, newer than $max_glibc; build it in an older container" >&2
  exit 1
fi

version="$("$bin" --version | awk '{print $2}')"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
app="$work/Synara-Native.AppDir"
mkdir -p "$app/usr/bin" "$app/usr/share/icons/hicolor/512x512/apps"

install -m755 "$bin" "$app/usr/bin/synara"
strip "$app/usr/bin/synara" 2>/dev/null || true
install -m644 "$root/apps/desktop/resources/app-icon-linux.png" \
  "$app/usr/share/icons/hicolor/512x512/apps/synara-native.png"
cp "$app/usr/share/icons/hicolor/512x512/apps/synara-native.png" "$app/synara-native.png"

cat > "$app/synara-native.desktop" <<EOF
[Desktop Entry]
Type=Application
Name=Synara Native
Comment=Synara native desktop client
Exec=synara
Icon=synara-native
Categories=Development;
Terminal=false
EOF

cat > "$app/AppRun" <<'EOF'
#!/bin/sh
here="$(dirname "$(readlink -f "$0")")"
exec "$here/usr/bin/synara" "$@"
EOF
chmod 755 "$app/AppRun"

mkdir -p "$out"
target="$out/Synara-Native-$version-x86_64.AppImage"
ARCH=x86_64 "$tool" --no-appstream "$app" "$target" >&2
echo "$target"
