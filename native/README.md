# Synara native client

Rust workspace for the native GPUI client described in
[`docs/specs/gpui-native-client.md`](../docs/specs/gpui-native-client.md).
The TypeScript server (`apps/server`) is unchanged; these crates talk to it over
the same WebSocket RPC the web client uses.

| Crate               | What it does                                                                    |
| ------------------- | ------------------------------------------------------------------------------- |
| `synara-protocol`   | Wire frames, negotiate types, orchestration types, generated RPC manifest       |
| `synara-rpc`        | Effect RPC client over one WebSocket: calls, streams with acks, ping, interrupt |
| `synara-sync`       | Pure shell and thread reducers: cursors, buffering, tombstones, streaming merge |
| `synara-session`    | Reconnecting session that owns the shell stream and per-thread streams          |
| `synara-supervisor` | Spawns and supervises a local `apps/server` (readiness, backoff, shutdown)      |
| `synara-instabox`   | instabox API client: machines, ports, activity leases, wake                     |
| `synara-probe`      | CLI that connects, prints the sidebar and streams a thread; used for checks     |
| `synara-app`        | The GPUI desktop app, binary `synara` (needs GPU/windowing libraries)           |

## Build

```bash
cd native
cargo test                                 # headless crates (default members)
cargo build -p synara-app                  # the GPUI app, binary `synara`
```

`synara-app` is a workspace member but not a default member, so plain
`cargo build`/`cargo test` stay fast and work on machines without
X11/Wayland/Vulkan development packages.

## Running the app

```bash
synara --url http://127.0.0.1:3773 --token "$SYNARA_AUTH_TOKEN"  # existing server
synara --spawn ~/synara                     # supervise apps/server; home ~/.synara-native
synara --instabox synara-native             # server in a sandbox, via port-forward
```

`--spawn` never uses the desktop apps' `~/.synara` or `~/.synara-beta`. Closing
the window stops a spawned server (process group included) or closes the
port-forward; an instabox sandbox keeps running until its idle timer pauses it.

## Platforms and packages

| Platform | Build host                          | Package script                  | Output                                         |
| -------- | ----------------------------------- | ------------------------------- | ---------------------------------------------- |
| Linux    | glibc ≤ 2.36 (e.g. `rust:bookworm`) | `scripts/build-appimage.sh`     | `Synara-Native-<v>-x86_64.AppImage`            |
| Windows  | Windows, MSVC + Windows SDK         | `scripts/build-windows-zip.ps1` | `Synara-Native-<v>-windows-x64.zip`            |
| macOS    | macOS with Xcode                    | `scripts/build-macos-app.sh`    | `Synara-Native-<v>-macos-universal.zip` (.app) |

`.github/workflows/native-client.yml` checks, tests, and packages all three.
Release builds need the platform's own shader compiler (`fxc.exe` on Windows,
`xcrun metal` on macOS), so they cannot be cross-built from Linux; a
`x86_64-pc-windows-gnu` debug build links but is only good for type checks.
Wine cannot run the Windows build: its DirectWrite stubs
`IDWriteFontSet::GetMatchingFonts`, which GPUI needs to load any font.

Packages are unsigned. The macOS app is ad-hoc signed but not notarized, so a
downloaded copy needs right-click > Open once. On Windows, release builds open
without a console window; on macOS, cmd-q quits.

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
