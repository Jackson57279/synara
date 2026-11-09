#!/usr/bin/env bash
# Drive synara-app under Xvfb with xdotool and take screenshots along the way.
#
# Usage: native/scripts/box-drive.sh STEP... [-- synara args...]
#   click X Y      left-click at screen coordinates
#   type TEXT      type text into the focused element
#   key KEYS       xdotool key names, e.g. Return or shift+Return
#   sleep SECONDS
#   shot NAME      save ~/NAME.png
#
# Without app args it connects to the dev server on ${SYNARA_PORT:-3773}.
# The window opens centered on a 1440x900 screen, so its content starts at
# (120, 50).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/native/target}"

steps=()
while [[ $# -gt 0 && "$1" != "--" ]]; do
  steps+=("$1")
  shift
done
[[ "${1:-}" == "--" ]] && shift
app_args=("$@")
if [[ ${#app_args[@]} -eq 0 ]]; then
  app_args=(--url "http://127.0.0.1:${SYNARA_PORT:-3773}" --token "${SYNARA_AUTH_TOKEN:-native-dev-token}")
fi

(cd "$root/native" && cargo build -q -p synara-app)
bin="$CARGO_TARGET_DIR/debug/synara"

display=":98"
Xvfb "$display" -screen 0 1440x900x24 -nolisten tcp >/dev/null 2>&1 &
xvfb_pid=$!
app_pid=""
trap 'kill $app_pid "$xvfb_pid" 2>/dev/null || true' EXIT
sleep 1

export DISPLAY="$display" WAYLAND_DISPLAY= VK_ICD_FILENAMES="${VK_ICD_FILENAMES:-/usr/share/vulkan/icd.d/lvp_icd.json}"
"$bin" "${app_args[@]}" >"$HOME/synara-app.log" 2>&1 &
app_pid=$!
sleep 6

i=0
while [[ $i -lt ${#steps[@]} ]]; do
  if ! kill -0 "$app_pid" 2>/dev/null; then
    echo "synara-app exited" >&2
    tail -40 "$HOME/synara-app.log" >&2
    exit 1
  fi
  case "${steps[$i]}" in
  click)
    xdotool mousemove "${steps[$((i + 1))]}" "${steps[$((i + 2))]}" click 1
    i=$((i + 3))
    ;;
  type)
    xdotool type --delay 20 -- "${steps[$((i + 1))]}"
    i=$((i + 2))
    ;;
  key)
    xdotool key "${steps[$((i + 1))]}"
    i=$((i + 2))
    ;;
  sleep)
    sleep "${steps[$((i + 1))]}"
    i=$((i + 2))
    ;;
  shot)
    import -window root -display "$display" "$HOME/${steps[$((i + 1))]}.png"
    echo "saved ~/${steps[$((i + 1))]}.png"
    i=$((i + 2))
    ;;
  *)
    echo "unknown step: ${steps[$i]}" >&2
    exit 2
    ;;
  esac
done
