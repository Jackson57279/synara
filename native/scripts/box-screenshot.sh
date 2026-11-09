#!/usr/bin/env bash
# Render synara-app under Xvfb with Mesa's software Vulkan (lavapipe) and save
# a PNG of the screen.
#
# Usage: native/scripts/box-screenshot.sh [out.png] [seconds] [-- synara args...]
# Without app args it connects to the dev server on ${SYNARA_PORT:-3773}.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="${1:-$HOME/synara-app.png}"
wait_s="${2:-12}"
shift $(( $# < 2 ? $# : 2 ))
[[ "${1:-}" == "--" ]] && shift
app_args=("$@")
if [[ ${#app_args[@]} -eq 0 ]]; then
  app_args=(--url "http://127.0.0.1:${SYNARA_PORT:-3773}" --token "${SYNARA_AUTH_TOKEN:-native-dev-token}")
fi
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/native/target}"

(cd "$root/native" && cargo build -q -p synara-app)
bin="$CARGO_TARGET_DIR/debug/synara"

display=":99"
Xvfb "$display" -screen 0 1440x900x24 -nolisten tcp >/dev/null 2>&1 &
xvfb_pid=$!
trap 'kill "$app_pid" "$xvfb_pid" 2>/dev/null || true' EXIT
sleep 1

export DISPLAY="$display" WAYLAND_DISPLAY= VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}"
"$bin" "${app_args[@]}" >"$HOME/synara-app.log" 2>&1 &
app_pid=$!
sleep "$wait_s"

if ! kill -0 "$app_pid" 2>/dev/null; then
  echo "synara-app exited early" >&2
  tail -40 "$HOME/synara-app.log" >&2
  exit 1
fi
import -window root -display "$display" "$out"
echo "saved $out"
