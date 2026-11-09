# Synara native client

Rust workspace for the native GPUI client described in
[`docs/specs/gpui-native-client.md`](../docs/specs/gpui-native-client.md).
The TypeScript server (`apps/server`) is unchanged; these crates talk to it over
the same WebSocket RPC the web client uses.

| Crate                | What it does                                                                      |
| -------------------- | --------------------------------------------------------------------------------- |
| `synara-protocol`    | Wire frames, negotiate types, orchestration types, generated RPC manifest         |
| `synara-rpc`         | Effect RPC client over one WebSocket: calls, streams with acks, ping, interrupt   |
| `synara-sync`        | Pure shell and thread reducers: cursors, buffering, tombstones, streaming merge   |
| `synara-session`     | Reconnecting session that owns the shell stream and per-thread streams            |
| `synara-supervisor`  | Spawns and supervises a local `apps/server` (readiness, backoff, shutdown)        |
| `synara-instabox`    | instabox API client: machines, ports, activity leases, wake                       |
| `synara-probe`       | CLI that connects, prints the sidebar and streams a thread; used for checks       |
| `synara-app`         | The GPUI desktop app (built separately; needs GPU/windowing libraries)            |

## Build

```bash
cd native
cargo test                                 # headless crates
cargo build --manifest-path crates/synara-app/Cargo.toml
```

`synara-app` is outside the workspace members so `cargo test` stays fast and
works on machines without X11/Wayland/Vulkan development packages.

## Regenerating the RPC manifest

```bash
bun packages/contracts/scripts/export-rpc-manifest.ts native/crates/synara-protocol/src/generated
```

CI fails when the committed files differ from a fresh export.

## Working in an instabox sandbox

See `native/scripts/`. `box-sync.sh <box>` pushes the current branch into the
box, `box-setup.sh` installs build dependencies, `box-server.sh` starts a
headless server, `box-check.sh` runs the checks, and `box-screenshot.sh` renders
the app under Xvfb.
