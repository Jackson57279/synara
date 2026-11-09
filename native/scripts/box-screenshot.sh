#!/usr/bin/env bash
# Render synara-app under Xvfb with Mesa's software Vulkan (lavapipe) and save
# a PNG of the screen.
#
# Usage: native/scripts/box-screenshot.sh [out.png] [seconds]
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:-$HOME/synara-app.png}"
wait_s="${2:-12}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/native/target}"

cargo build -q --manifest-path "$root/native/crates/synara-app/Cargo.toml"
bin="$CARGO_TARGET_DIR/debug/synara-app"

display=":99"
Xvfb "$display" -screen 0 1440x900x24 -nolisten tcp >/dev/null 2>&1 &
xvfb_pid=$!
trap 'kill "$app_pid" "$xvfb_pid" 2>/dev/null || true' EXIT
sleep 1

export DISPLAY="$display" WAYLAND_DISPLAY= VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}"
"$bin" >"$HOME/synara-app.log" 2>&1 &
app_pid=$!
sleep "$wait_s"

if ! kill -0 "$app_pid" 2>/dev/null; then
  echo "synara-app exited early" >&2
  tail -40 "$HOME/synara-app.log" >&2
  exit 1
fi
import -window root -display "$display" "$out"
echo "saved $out"
