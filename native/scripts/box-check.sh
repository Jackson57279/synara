#!/usr/bin/env bash
# Run the native workspace checks: fmt, clippy, tests, manifest drift, and
# (when a server is running) the probe against it.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$root/native"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$root/native/target}"

echo "== fmt"
cargo fmt --all --check
echo "== clippy"
cargo clippy --workspace --all-targets -- -D warnings
echo "== test"
cargo test --workspace

echo "== manifest drift"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
(cd "$root" && bun packages/contracts/scripts/export-rpc-manifest.ts "$tmp" >/dev/null)
diff -u crates/synara-protocol/src/generated/rpc-manifest.json "$tmp/rpc-manifest.json"
diff -u crates/synara-protocol/src/generated/rpc_manifest.rs "$tmp/rpc_manifest.rs"

port="${SYNARA_PORT:-3773}"
if curl -fsS "http://127.0.0.1:${port}/health" >/dev/null 2>&1; then
  echo "== probe"
  cargo run -q -p synara-probe -- --url "http://127.0.0.1:${port}" \
    --token "${SYNARA_AUTH_TOKEN:-native-dev-token}" check
else
  echo "== probe skipped (no server on ${port})"
fi
echo "all checks passed"
