#!/usr/bin/env bash
# Start a headless Synara server for native-client development.
#
# Binds 127.0.0.1:${SYNARA_PORT:-3773} with an isolated SYNARA_HOME so it never
# touches a real ~/.synara. Prints /health once the server reports startupReady.
set -euo pipefail

port="${SYNARA_PORT:-3773}"
home_dir="${SYNARA_NATIVE_HOME:-$HOME/.synara-native-dev}"
token="${SYNARA_AUTH_TOKEN:-native-dev-token}"
log="${SYNARA_NATIVE_SERVER_LOG:-$HOME/synara-native-server.log}"

cd "$(dirname "$0")/../../apps/server"
mkdir -p "$home_dir"

if curl -fsS "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
  echo "server already running on ${port}"
  exit 0
fi

SYNARA_MODE=web SYNARA_PORT="$port" SYNARA_HOST=127.0.0.1 SYNARA_HOME="$home_dir" \
  SYNARA_NO_BROWSER=1 SYNARA_AUTH_TOKEN="$token" \
  nohup bun run src/index.ts >"$log" 2>&1 &
echo "server pid $!, log $log"

for _ in $(seq 1 90); do
  if body="$(curl -fsS "http://127.0.0.1:${port}/health" 2>/dev/null)" && [[ "$body" == *'"startupReady":true'* ]]; then
    echo "ready on http://127.0.0.1:${port}"
    exit 0
  fi
  sleep 1
done
echo "server did not become ready" >&2
tail -40 "$log" >&2
exit 1
