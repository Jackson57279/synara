#!/usr/bin/env bash
# Run the native workspace checks: fmt, clippy, tests, manifest drift, the
# probe against a running server (smoke + session), and the probe against a
# server it spawns through synara-supervisor.
#
# SYNARA_NATIVE_SKIP_SPAWN=1 skips the spawned-server pass (it takes ~35 s
# because the server hangs on shutdown with a provider session running).
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root/native"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/native/target}"

echo "== fmt"
cargo fmt --all --check
echo "== clippy"
cargo clippy --all-targets -- -D warnings
if [[ "${SYNARA_NATIVE_APP:-1}" == 1 ]]; then
  echo "== clippy (synara-app)"
  cargo clippy -p synara-app --all-targets -- -D warnings
fi
echo "== test"
cargo test
if [[ "${SYNARA_NATIVE_APP:-1}" == 1 ]]; then
  cargo test -p synara-app
fi

echo "== manifest drift"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
(cd "$root" && bun packages/contracts/scripts/export-rpc-manifest.ts "$tmp" >/dev/null)
diff -u crates/synara-protocol/src/generated/rpc-manifest.json "$tmp/rpc-manifest.json"
diff -u crates/synara-protocol/src/generated/rpc_manifest.rs "$tmp/rpc_manifest.rs"

port="${SYNARA_PORT:-3773}"
if curl -fsS "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
  echo "== probe (running server)"
  for cmd in smoke session; do
    cargo run -q -p synara-probe -- --url "http://127.0.0.1:${port}" \
      --token "${SYNARA_AUTH_TOKEN:-native-dev-token}" --timeout 60 "$cmd"
  done
else
  echo "== probe skipped (no server on ${port})"
fi

if [[ "${SYNARA_NATIVE_SKIP_SPAWN:-0}" != 1 ]]; then
  echo "== probe (spawned server)"
  cargo run -q -p synara-probe -- --spawn "$root" --timeout 60 session
fi
echo "all checks passed"
