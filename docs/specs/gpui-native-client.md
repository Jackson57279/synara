# Native GPUI client and instabox hosts

**Status: draft spec (2026-11-08).** Nothing here is built yet. This document is the _what_ and the build order. Vocabulary follows `/CONTEXT.md`; the host model follows `docs/specs/2026-08-13-hosts-as-account-entities.md` and ADRs 0003, 0007, 0013, 0014, 0015. Decisions that belong to the maintainers are collected in [§13](#13-open-decisions) and are not settled by this draft.

## 1. Summary

Synara Desktop today is an Electron shell (~45k LOC main process) that bundles the React web UI (~412k LOC in `apps/web`) and supervises the TypeScript server (`apps/server`). This spec replaces **the client only** with a native Rust app built on GPUI, and adds **instabox.sh sandboxes as first-class remote hosts**.

- The TypeScript server stays. It is the product: provider adapters, orchestration, persistence, recovery. Rewriting it is out of scope.
- The native client speaks the existing WebSocket RPC (`packages/contracts`) and nothing else. It gets no private side channel the web client lacks.
- An instabox sandbox runs a full headless Synara server and appears in the host list like any other remote host. The native client never runs agents itself.
- The Electron app keeps shipping until the native client reaches parity on the surfaces in Phase 3 ([§11](#11-phases)). The two never run against the same data home at the same time.

### Goals

| Goal                     | Measure                                                                                                                       |
| ------------------------ | ----------------------------------------------------------------------------------------------------------------------------- |
| Faster, lighter desktop  | Cold start to interactive sidebar < 400 ms on M-series / Ryzen 6000; idle RSS of the client < 150 MB with 50 threads hydrated |
| Smooth streaming         | Transcript holds 120 fps on a 120 Hz display while one turn streams at 200 tok/s and a 2k-message thread is open              |
| Same behavior            | The sync, end-follow and recovery rules in [§5](#5-client-runtime) pass the same scenario tests the web client passes         |
| Cloud sandboxes as hosts | Create, connect, run a turn, preview a dev server, pause and resume an instabox host from the client, no terminal needed      |
| No credential exposure   | Provider API keys never land on an instabox disk in plain text ([§9.4](#94-provider-credentials))                             |

### Non-goals

- Porting the server to Rust, or changing the wire protocol to suit Rust.
- A browser build of the native client. ADR 0014 already keeps the hosted web client out of scope.
- iOS. The iOS app is separate; shared Rust crates may serve it later but this spec does not plan for it.
- Running agents directly inside instabox without a Synara server (the "execution backend" path, [§9.1](#91-why-the-whole-host-runs-in-the-sandbox)).

## 2. Architecture

```text
┌──────────────────────────── Native client (Rust, GPUI) ─────────────────────────────┐
│  UI crates (views)  ─►  synara-store (entities, reducer)  ─►  synara-rpc (Effect RPC)│
│        ▲                       ▲                                   │                 │
│        │                 synara-sync (cursor, buffer, fence)       │ WebSocket       │
│  synara-shell: windows, menus, tray, notifications, updater, keychain, deep links    │
│  synara-supervisor: spawns/monitors the local server  ─────────────┼────────┐        │
│  synara-hosts: transport race, device key, mint, instabox control  │        │        │
└────────────────────────────────────────────────────────────────────┼────────┼────────┘
                                                                     │        │ stdio/fd3
         loopback ws://127.0.0.1:<port>/ws?token=…  ◄────────────────┘        ▼
                                                              apps/server (Node, local)
         SSH forward / private-port URL / relay  ───────────►  apps/server (inside instabox VM)
                                                                    │
                                                    instabox outbound proxy adds provider keys
```

### 2.1 Process model

| Process                      | Owner               | Lifetime                                                                 |
| ---------------------------- | ------------------- | ------------------------------------------------------------------------ |
| Native client                | user                | app session                                                              |
| Local server (`apps/server`) | `synara-supervisor` | child of the client; stdin EOF ends it ([§7.1](#71-backend-supervision)) |
| Provider CLIs, PTYs, MCP     | server              | unchanged                                                                |
| CEF / Chromium helper        | client              | lazy, only when a browser pane opens ([§7.4](#74-embedded-browser))      |
| CUA, AppSnap helpers         | client (as today)   | lazy, external processes, unchanged binaries                             |
| Remote server (instabox)     | the sandbox         | independent of the client; survives client exit                          |

The server binary and its Node runtime ship inside the native bundle exactly as Electron ships them today, except the runtime is a plain `node` binary instead of `ELECTRON_RUN_AS_NODE`. This is the only server-side packaging change and it needs its own verification on Windows ([§12](#12-risks)).

### 2.2 Workspace layout

New top-level `native/` Cargo workspace. It is not a Bun workspace package and does not import from `apps/web`.

| Crate               | Responsibility                                                                                               |
| ------------------- | ------------------------------------------------------------------------------------------------------------ |
| `synara-contracts`  | Generated Rust types for every schema in `packages/contracts` ([§4](#4-protocol-and-codegen)). No logic.     |
| `synara-rpc`        | Effect RPC framing over WebSocket: requests, streams with per-chunk `Ack`, interrupts, ping/pong, negotiate. |
| `synara-sync`       | Snapshot/replay/live cursor algorithm, buffering, fences, tombstones, server-instance resets.                |
| `synara-store`      | GPUI entities for hosts, projects, threads, messages, turns, approvals, terminals, settings.                 |
| `synara-hosts`      | Host list, transport race, device key, DPoP, session-credential mint, instabox API client.                   |
| `synara-supervisor` | Local server spawn, readiness, restart, shutdown.                                                            |
| `synara-shell`      | Windows, menus, tray, notifications, deep links, updater, keychain, crash reports, power monitor.            |
| `synara-ui`         | Design tokens, theme, `text-ui` scale, disclosure motion, shared components on top of gpui-kit.              |
| `synara-transcript` | Transcript list, end-follow scroll, message/tool/diff renderers, markdown.                                   |
| `synara-composer`   | Rich text composer, mentions, attachments, slash commands, model/effort pickers.                             |
| `synara-terminal`   | Terminal view over `alacritty_terminal` (or libghostty-vt, [§13](#13-open-decisions)).                       |
| `synara-browser`    | Embedded Chromium pane and CDP bridge.                                                                       |
| `synara-app`        | The binary. Wires the crates together; no logic of its own.                                                  |

Rules carried over from `AGENTS.md`: schemas live only in `synara-contracts`; runtime helpers get explicit module paths, not a prelude barrel; provider-specific behavior stays on the server.

### 2.3 Toolchain

- GPUI through **gpui-kit 0.9** (the renamed Longbridge gpui-component). It pins one weekly `gpui-pre` snapshot; the workspace pins the same revision in `Cargo.lock` and moves only when gpui-kit does.
- Async: GPUI's foreground/background executors for UI work, plus a vendored `gpui_tokio` bridge for `tokio-tungstenite`, `reqwest` and `ssh2`/`russh`. One tokio runtime per app.
- Rust toolchain pinned in `native/rust-toolchain.toml` and mirrored in `.mise.toml`.
- Root `package.json` gets `native:build`, `native:test`, `native:lint` (clippy `-D warnings`), `native:fmt:check`, and `contracts:rust` (codegen, [§4.2](#42-codegen)). `bun run typecheck` fails when generated Rust is stale ([§4.3](#43-drift-gate)).

## 3. What changes and what does not

| Area                                      | Change                                                                                                                         |
| ----------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------ |
| `apps/server`                             | Small, additive: accept a plain Node parent, publish JSON Schema, an `instabox` host-launch mode ([§9.3](#93-server-changes)). |
| `packages/contracts`                      | Add the JSON Schema export script. No schema shape changes.                                                                    |
| `apps/web`                                | None. It stays the reference client and keeps shipping.                                                                        |
| `apps/desktop`                            | None until the cut-over. Then it is retired per [§11 Phase 6](#phase-6--cut-over).                                             |
| Release pipeline                          | New native jobs per OS; Electron jobs stay until cut-over.                                                                     |
| Data home (`~/.synara`, `~/.synara-beta`) | Same layout, same server owns it. The native client adds `client/` for window state and its own logs only.                     |

## 4. Protocol and codegen

### 4.1 The contract the client implements

From `packages/contracts/src/wsCompatibility.ts`, `apps/server/src/wsRpc.ts` and `apps/web/src/wsTransport.ts`:

- **Endpoints.** `GET /ws/negotiate` over plain HTTP (advertised as `transport.http-negotiate`; the legacy `GET /ws/bootstrap` socket stays for older clients), then the `GET /ws` upgrade. Compatibility: epoch 1, revisions `[3, 3]`. A client outside the window shows "update required" and does not retry.
- **Auth.** Loopback desktop: `?token=<SYNARA_AUTH_TOKEN>`. Anything else (SSH forward, LAN, Tailscale, relay, instabox URL): the host-minted session credential per ADR 0013, with a DPoP proof. An SSH forward is **not** loopback even though the socket is `127.0.0.1` on the client side.
- **Framing.** One JSON object per WebSocket text message. Client sends `Request`, `Ack`, `Interrupt`, `Ping`, `Eof`; server sends `Chunk`, `Exit`, `Defect`, `Pong`. Every stream `Chunk` needs an `Ack` before the server sends the next; the client acks after the chunk is applied to the store, which is the client's only backpressure lever.
- **Limits.** 20 concurrent streams and 8 thread streams per client. `synara-rpc` enforces them locally and queues rather than letting the server reject.
- **Surface.** 283 `Rpc.make` definitions in `packages/contracts/src/rpc.ts` across the RPC groups. The client generates all of them even where the UI does not use them yet, so the drift gate stays meaningful.

### 4.1.1 Exact handshake

Verified against `apps/server/src/wsCompatibility.ts` and `apps/server/src/wsRpc.ts`:

```text
GET /ws/negotiate
    ?x-synara-protocol-epoch=1
    &x-synara-protocol-min-revision=3
    &x-synara-protocol-max-revision=3
    &x-synara-client-build=native/<version>
    &x-synara-required-capability=orchestration.cursor-safe-streams      (repeat per capability)
→ 200 WsBootstrapNegotiateResult { protocolEpoch, negotiatedRevision, serverBuild,
                                   serverInstanceId, capabilities[], remoteAttachmentId? }
→ 426 WsCompatibilityError        { code, action: reload|update-client|update-server, … }

GET /ws  (upgrade)
    ?x-synara-protocol-epoch=1
    &x-synara-protocol-revision=<negotiatedRevision>
    &x-synara-client-build=native/<version>
    &x-synara-server-instance=<serverInstanceId>
    &token=<SYNARA_AUTH_TOKEN>            (loopback desktop only)
→ 426 WS_SERVER_GENERATION_CHANGED when the instance id no longer matches: renegotiate.
```

The client sends the same `WS_CLIENT_REQUIRED_CAPABILITIES` the web client sends: `orchestration.cursor-safe-streams`, `orchestration.thread-detail-snapshot`, `rpc.typed-errors`, `git.worktree-setup-progress`.

### 4.1.2 Exact frames

Effect RPC (`effect@4.0.0-rc.112`, `RpcSerialization.json`). One JSON value per text message; the server may also send a JSON array of messages in one frame, so the decoder accepts both.

| Direction | Frame                                                                                                                   |
| --------- | ----------------------------------------------------------------------------------------------------------------------- |
| C → S     | `{"_tag":"Request","id":7,"tag":"orchestration.subscribeShell","payload":{},"headers":[]}`                              |
| C → S     | `{"_tag":"Ack","requestId":7}` after each applied `Chunk`                                                               |
| C → S     | `{"_tag":"Interrupt","requestId":7,"interruptors":[]}` to cancel                                                        |
| C → S     | `{"_tag":"Ping"}` every 5 s; a missing `Pong` before the next ping is a dead socket                                     |
| C → S     | `{"_tag":"Eof"}` on orderly close                                                                                       |
| S → C     | `{"_tag":"Chunk","requestId":7,"values":[…]}` (non-empty array of stream items)                                         |
| S → C     | `{"_tag":"Exit","requestId":7,"exit":{"_tag":"Success","value":…}}`                                                     |
| S → C     | `{"_tag":"Exit","requestId":7,"exit":{"_tag":"Failure","cause":[{"_tag":"Fail","error":…}]}}` (also `Die`, `Interrupt`) |
| S → C     | `{"_tag":"Defect","defect":…}` (connection-wide; fail every pending request)                                            |
| S → C     | `{"_tag":"Pong"}`                                                                                                       |

Request ids come from a per-connection counter and go on the wire as decimal strings (`"1"`): the server's patched Effect build accepts only `bigint | string` ids, and a JSON number gets a `Defect` ("Invalid request id"). The server echoes ids back as strings, so the client decodes both forms. Unary RPCs answer with one `Exit`. Streaming RPCs answer with `Chunk`s and end with an `Exit`; the server waits for the `Ack` before sending the next `Chunk`.

### 4.2 Codegen

There is no contracts → JSON Schema pipeline today.

1. `packages/contracts/scripts/export-json-schema.ts` walks every exported schema reachable from the RPC groups and `orchestration.ts`, calls `Schema.toJsonSchemaDocument`, and writes `packages/contracts/dist/schema/*.json` plus an `rpc-manifest.json` (`tag`, payload ref, success ref, error ref, `stream: bool`, group).
2. `native/tools/contracts-codegen` (Rust, `typify`-based with Synara overrides) turns that into `synara-contracts/src/generated/*.rs`:
   - Tagged unions: events discriminate on `type`, stream items on `kind`; both become `#[serde(tag = …)]` enums with a final `#[serde(other)] Unknown` variant so a newer server cannot crash an older client.
   - Branded IDs (`ThreadId`, `ProjectId`, …) become newtypes over `Arc<str>`.
   - `Option` vs `null` vs missing follows the Effect schema exactly; the generator emits `#[serde(skip_serializing_if)]` only where the schema marks a field optional.
   - The RPC manifest becomes a typed client trait: `fn thread_turn_start(&self, req) -> RpcCall<Resp, Err>` and `fn orchestration_subscribe(&self, req) -> RpcStream<Item, Err>`.
3. Schemas the generator cannot express (transforms, refinements with custom decode) are listed in `native/tools/contracts-codegen/overrides.toml` with a hand-written Rust type and a round-trip test. The list starts empty and every entry needs a comment saying why.

### 4.3 Drift gate

- CI regenerates and fails on any diff.
- A golden-fixture suite: the server's existing contract tests dump encoded JSON for each schema to `packages/contracts/test-fixtures/`; Rust decodes and re-encodes each and compares structurally. This catches serde drift that the type generator alone would miss.

## 5. Client runtime

These are behavior contracts. The reference implementations are cited; the native client must match them, and the scenario tests in [§10](#10-testing) check both clients against the same server.

### 5.1 Connection lifecycle (`synara-rpc`, ref. `apps/web/src/wsTransport.ts`)

- States: `Idle → Negotiating → Open → Degraded(reason) → Reconnecting(attempt) → Open | Failed(nonRetryable)`.
- Reconnect backoff matches the web client (jittered exponential, capped). Browser `online`/`visibilitychange` signals map to the OS network-change and wake notifications from `synara-shell`.
- On reconnect, every live stream restores itself from its last cursor. If `/ws/bootstrap` reports a different server instance id, **all** resume cursors are dropped and every subscription re-snapshots.
- `ORCHESTRATION_SNAPSHOT_STALLED` is non-retryable: surface it, stop the stream, do not loop.
- Stream overflow from the server restarts that stream from a fresh snapshot.

### 5.2 Sync (`synara-sync`, ref. `apps/server/src/wsSnapshotLiveStream.ts`, `apps/web/src/routes/__root.tsx`)

- One global, non-contiguous `sequence`. Drop any event with `seq <= cursor`. Gaps are normal and never trigger a resync.
- Events that arrive while a snapshot is loading are buffered; the snapshot is applied, the cursor is **overwritten** with `snapshotSequence`, then buffered events newer than it are applied in order.
- Deletions are tombstones and win over late upserts for the same id.
- Fences, coalescing and flush timing follow the root router: high-rate deltas (assistant text, tool output) coalesce per frame; structural events (turn start/end, approval requested, thread deleted) flush immediately.
- The reducer is a pure function `(State, Event) -> State + Effects` in `synara-store`, tested without GPUI.

### 5.2.1 The two streams the client lives on

| Stream                          | Payload                                      | Items (`kind`)                                                                                                                                                                  |
| ------------------------------- | -------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `orchestration.subscribeShell`  | `{}`                                         | `snapshot` (`OrchestrationShellSnapshot`), `space-upserted`, `space-removed`, `space-order-updated`, `project-upserted`, `project-removed`, `thread-upserted`, `thread-removed` |
| `orchestration.subscribeThread` | `{ threadId, afterSequence?, batchReplay? }` | `snapshot` (`OrchestrationThreadDetailSnapshot`), `event` (`OrchestrationEvent`), `replay` (`OrchestrationEvent[]`, only with `batchReplay: true`)                              |

- The shell stream drives the sidebar. Every item except `snapshot` carries its own `sequence`; upserts replace the whole row, so the shell reducer needs no event semantics.
- One thread stream per open thread, at most 8. Opening a thread whose cursor is cached passes `afterSequence` and `batchReplay: true`, so the gap arrives as a single `replay` item applied in one store update.
- The thread reducer handles the `OrchestrationEvent` types that change what a transcript shows: `thread.message-sent` (append or extend a message; streaming deltas share a `messageId`), `thread.activity-appended`, `thread.session-set`, `thread.proposed-plan-upserted`, `thread.turn-diff-completed`, `thread.meta-updated`, `thread.reverted`, `thread.conversation-rolled-back` (both force a re-snapshot), `thread.deleted`. Any other type advances the cursor and nothing else.
- `historyRevisionSequence` in the detail snapshot tells the reducer to throw away local messages that a rollback removed.

### 5.2.2 Writing

Every user action is `orchestration.dispatchCommand` with a `ClientOrchestrationCommand` (discriminated on `type`), answered with `DispatchResult { sequence }`. The client never applies its own command optimistically past what the web client does: it shows the pending user message, then reconciles when the matching `thread.message-sent` arrives with the same `messageId`. A lost turn-start acknowledgement is settled with `orchestration.settleTurnDispatch`, never by resending.

### 5.3 Store and rendering

- Each thread, message and turn is a GPUI `Entity`. Views observe the narrowest entity they render, so a streaming token notifies one message view, not the transcript.
- Text deltas append to a rope per message; markdown re-parses only the open trailing block.
- Snapshots decode on the background executor and swap in on the foreground in one update.

### 5.4 Transcript end-follow (ref. `apps/web/src/components/chat/useChatTranscriptScroll.ts`, `chat-scroll.ts`)

Port the rules exactly; they are product rules, not web workarounds:

- Follow stays armed while the reader is at the bottom, through tool/work growth between messages and through the final settled layout.
- Any user scroll away from the bottom releases follow.
- Generic work, buffering, reconnecting, pending approvals and tool-only activity never re-arm a detached reader and never trigger message-arrival auto-stick.
- GPUI `list` (variable-height, measured lazily) is the transcript element. Its measurement must not feed a bottom-stick loop: follow is decided from scroll intent, then the list is asked to reveal the end once per frame after layout. No second measurement pass.
- No virtualization beyond what `list` already does until a measured need appears, matching the repo rule.

### 5.5 Typography, motion, menus

- `synara-ui` exposes `TextUi::{Xs, Sm, Base, Lg}` and `TextChat::*` driven by the Settings font size, mirroring `useAppTypography.ts`. Fixed sizes are allowed only for dialog titles and large headings; a clippy-style lint (`native/tools/lint-ui`) rejects raw `px` font sizes elsewhere, mirroring `uiFontSize.test.ts`.
- One `disclosure` module owns open/close timing and reduced-motion behavior, ported from `apps/web/src/lib/disclosureMotion.ts` with the same constants. No view defines its own transition.
- Menus: action variants share one parent row with a submenu (native menus on macOS/Windows, gpui-kit popover menu otherwise), as `contextMenuGroup.ts` does.

### 5.6 Errors the user can see

| Condition                              | Source                       | What the client does                                                        |
| -------------------------------------- | ---------------------------- | --------------------------------------------------------------------------- |
| `WsCompatibilityError` `update-client` | negotiate 426                | Banner with "Update Synara"; no retry loop                                  |
| `WsCompatibilityError` `update-server` | negotiate 426                | Host row says "Host needs update"; remote hosts offer "Update host"         |
| `WS_SERVER_GENERATION_CHANGED`         | `/ws` 426                    | Silent renegotiate, drop all cursors, re-snapshot                           |
| Socket closed, ping timeout            | transport                    | "Reconnecting…" in the titlebar status only; transcript keeps its state     |
| `ORCHESTRATION_SNAPSHOT_STALLED`       | thread stream `Exit` failure | Inline error on that thread with a "Retry" button; other threads unaffected |
| `Defect`                               | server                       | Fail every pending call, reconnect, log the defect to the client log        |
| Dispatch rejected                      | `dispatchCommand` failure    | Composer keeps the draft and shows the error under it                       |

### 5.7 Logging

`tracing` with a rolling file in `<data home>/client/logs/`, 5 × 10 MB. Frames are never logged in full: payloads may contain prompts and file contents. The transport logs `tag`, request id, byte length and timing only. This is also the rule for Beta diagnostics.

### 5.8 Actions and keybindings

Every command the web app registers keeps its id (for example `thread.new`, `sidebar.toggle`, `composer.send`). The native app defines them as GPUI actions in one `synara-app::actions` module and loads the same keybinding defaults as `KEYBINDINGS.md`, so a user's custom keybindings file keeps working.

## 6. Screen-by-screen port map

Sizes come from the web UI inventory. "Native" means built in GPUI; "Reuse" means an existing crate does most of it.

| #   | Surface group                                     | Web size | Native approach                                                      | Phase |
| --- | ------------------------------------------------- | -------- | -------------------------------------------------------------------- | ----- |
| 1   | App shell, window chrome, titlebar                | M        | Native; custom titlebar on Win/Linux, traffic lights on macOS        | 1     |
| 2   | Sidebar (hosts, projects, threads, hubs, groups)  | XL       | Native `uniform_list` per section; drag-reorder; context menus       | 2     |
| 3   | Transcript (messages, tool calls, diffs, images)  | XL       | `synara-transcript`; per-kind renderers                              | 2     |
| 4   | Markdown + code highlighting                      | L        | `pulldown-cmark` + tree-sitter highlight; same theme tokens          | 2     |
| 5   | Composer (Lexical: mentions, chips, paste, slash) | XL       | `synara-composer` on gpui-kit `Input` with an inline-chip layer      | 2     |
| 6   | Model / effort / approval pickers                 | M        | Native popovers driven by provider capabilities from contracts       | 2     |
| 7   | Approvals and permission prompts                  | M        | Native; keyboard-first                                               | 2     |
| 8   | Orchestration store / reducer                     | XL       | `synara-sync` + `synara-store` ([§5](#5-client-runtime))             | 1     |
| 9   | Settings (all panes)                              | XL       | gpui-kit form controls; one pane per module                          | 3     |
| 10  | Diff / file viewer / in-thread find               | L        | Native text view with tree-sitter; find bar                          | 3     |
| 11  | Terminal panes                                    | L        | `synara-terminal`                                                    | 3     |
| 12  | Command palette, keybindings                      | M        | gpui-kit; actions registry mirrors web command ids                   | 2     |
| 13  | Project import / relocation                       | M        | Native dialogs + OS file pickers                                     | 3     |
| 14  | Remote hosts, pairing, device code, devices       | M        | Native, per Slice D UI rules                                         | 4     |
| 15  | instabox host management                          | new      | Native ([§9.5](#95-client-ux-for-instabox-hosts))                    | 4     |
| 16  | Account, profile, sign-in                         | M        | System browser OAuth + deep link return                              | 3     |
| 17  | Notifications, toasts, announcements              | S        | `synara-shell` + one shared announcement sheet                       | 3     |
| 18  | Hubs, groups, project coordinator                 | L        | Native                                                               | 3     |
| 19  | MCP connectors, external MCP                      | M        | Native settings panes                                                | 3     |
| 20  | Browser pane and automation                       | L        | `synara-browser` ([§7.4](#74-embedded-browser))                      | 5     |
| 21  | Computer use (CUA), AppSnap                       | M        | Unchanged helpers; native viewer                                     | 5     |
| 22  | Editor integration (`docs/editor`)                | M        | Native; LSP stays server-side if it already is                       | 5     |
| 23  | Diagnostics (Beta only)                           | S        | Same allowlist and `diagnosticsRedaction.ts` rules, enforced in Rust | 3     |

Every visible string, control and layout in the native client is a UI change under `AGENTS.md`. Phase work starts from the current web UI as the visual reference and changes nothing about it without explicit maintainer sign-off per surface.

## 7. Native shell duties (replacing Electron main)

The Electron main process has about 25 responsibilities. Each one maps to a crate or is dropped.

| Electron duty                              | Native owner        | Notes                                                               |
| ------------------------------------------ | ------------------- | ------------------------------------------------------------------- |
| Spawn, monitor, restart, stop server       | `synara-supervisor` | [§7.1](#71-backend-supervision)                                     |
| Window state, multi-window, fullscreen     | `synara-shell`      | Persist to `client/windows.json`                                    |
| App/context menus                          | `synara-shell`      | Native menus via GPUI platform layer                                |
| Tray / dock badge                          | `synara-shell`      | `tray-icon` crate on Win/Linux                                      |
| Notifications                              | `synara-shell`      | `notify-rust` / UNUserNotificationCenter / WinRT toasts             |
| Auto-update (Stable and Beta feeds)        | `synara-shell`      | [§7.3](#73-updates-and-channels)                                    |
| Deep links (`synara://`, `synara-beta://`) | `synara-shell`      | Separate schemes per channel                                        |
| Keychain / safe storage                    | `synara-shell`      | `keyring` crate; device key in Secure Enclave / TPM where available |
| Power monitor (sleep/wake), network change | `synara-shell`      | Feeds reconnect                                                     |
| Crash reporting                            | `synara-shell`      | Beta only, same allowlist                                           |
| File/folder pickers, reveal in Finder      | `synara-shell`      | `rfd`                                                               |
| Clipboard with images                      | `synara-shell`      | GPUI clipboard + `arboard` for images                               |
| Embedded browser + CDP                     | `synara-browser`    | [§7.4](#74-embedded-browser)                                        |
| CUA / AppSnap helper launch                | `synara-shell`      | Same binaries and arguments                                         |
| SSH host bootstrap (ADR 0015)              | `synara-hosts`      | `russh`; same bootstrap script the desktop uses                     |
| Beta channel Stable→Beta one-way copy      | `synara-shell`      | Ported unchanged; the only cross-home code                          |
| Renderer IPC bridge (`preload`)            | dropped             | No renderer; the client calls Rust directly                         |
| Content security, sandboxing of renderer   | dropped             | No web content outside the browser pane                             |

### 7.1 Backend supervision

Port `apps/desktop/src/main.ts` and `apps/server/src/desktopParentLifetime.ts` exactly:

- Spawn the bundled `node` with the server entry. Env: everything Electron sets today, minus `ELECTRON_RUN_AS_NODE`, plus `UV_THREADPOOL_SIZE=16` and `SYNARA_AUTH_TOKEN` (fresh per launch, never logged).
- stdio `[pipe, pipe, pipe, pipe, ipc]`: fd3 carries the capability handshake. The `ipc` slot exists because the server expects it under Electron; with plain Node the supervisor passes a pipe the server treats as IPC, or the server learns a `--parent=native` flag ([§9.3](#93-server-changes)).
- stdin is held open and never written. EOF means "parent is gone, shut down". This is the shutdown path when the client crashes.
- Readiness: poll `GET /health` until `startupReady: true`.
- Restart backoff `500 ms × 2^n`, capped at 10 s, reset after a stable run.
- Clean shutdown: `POST /api/desktop/shutdown` with `Authorization: Bearer <token>`, wait, then SIGTERM, then SIGKILL (Windows: job object terminate). Process creation and teardown go through one Rust module that mirrors `packages/shared/src/platformProcess.ts`, including Windows quoting. Do not report cleanup as done unless the child's exit was observed.

**Implemented so far (`native/crates/synara-supervisor`, source-checkout path only):**

| Behavior                                                                                                                                                                            | Status                                            |
| ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------- |
| Env `SYNARA_PORT/HOST/HOME/AUTH_TOKEN`, `SYNARA_NO_BROWSER=1`, `SYNARA_DESKTOP_PARENT_STDIN=1`; inherited copies and `SYNARA_DESKTOP_SHUTDOWN_TOKEN`/`ELECTRON_RUN_AS_NODE` removed | done                                              |
| stdin held outside `tokio::process::Child` (its `wait()` closes stdin, which the server reads as owner loss)                                                                        | done, covered by a test                           |
| Readiness `GET /health` → `startupReady: true`; a spawn never resets the breaker, readiness does                                                                                    | done                                              |
| Backoff 500 ms doubling to 10 s; give up after 5 consecutive failed starts; `restart()` clears the breaker                                                                          | done                                              |
| 8 KiB output tail; give-up summary from the last `error` line; output drained (≤1 s) before summarising                                                                             | done                                              |
| Migration-recovery marker (`userdata/state.sqlite.migration-recovery.json`) blocks the start                                                                                        | done                                              |
| Adopt a healthy server on a fixed port instead of spawning                                                                                                                          | done (not restarted or stopped by the supervisor) |
| Own process group on Unix; after the stop timeout, SIGTERM then SIGKILL the whole group so provider children are not orphaned                                                       | done, covered by a test                           |
| Bundled Node entry, fd3 capability pipe, `UV_THREADPOOL_SIZE`, `/api/desktop/shutdown`, Windows job objects                                                                         | not started                                       |

**Known server issue (reported, not changed here):** when stdin closes while a Codex provider session is running, the server logs `Desktop owner connection closed; shutting down backend` but neither exits nor hits its own 20 s owner-lost watchdog within 25 s, so the supervisor has to escalate to the process group. With no provider session it exits in about a second. Reproduce with `synara-probe --spawn <repo> session` (hangs) and `--spawn <repo> smoke` (clean).

### 7.2 Single instance and data homes

- One instance per channel, enforced by a lock in the data home. A second launch forwards its deep link and exits.
- Stable reads and writes only `~/.synara`; Beta only `~/.synara-beta`. The client links no code that resolves the other path, except the Beta copy step.
- The Electron and native clients cannot both hold the lock. During the overlap phases the native client ships as a separate flavor ([§11](#11-phases)) with its own lock name but the **same** data home, so a user can switch back without migrating.

### 7.3 Updates and channels

- `cargo-packager` for bundles (`.dmg`/`.app`, NSIS/MSI, AppImage/deb/rpm) and its updater for delta-free signed updates.
- Two feeds, two identities, matching `BETA.md`. Tag `vX.Y.Z` builds Stable, `vX.Y.Z-beta.N` builds Beta. The release guide gains native signing steps (Developer ID + notarization, Authenticode, Linux sig). A local build is never reported as a signed release.
- `BETA_ONLY_FEATURES` is read from the server's capabilities and from a generated Rust copy of `packages/shared/src/betaFeatures.ts`. The server stays authoritative; the client only hides entry points.

### 7.4 Embedded browser

Browser automation drives the pane over CDP, so the pane must be Chromium. wry/WebKitGTK/WKWebView cannot serve CDP.

- **Option A (recommended): CEF** via `cef-rs`, windowed (native child view) on macOS/Windows, offscreen rendering into a GPUI texture on Linux/Wayland. Adds ~120 MB per platform, loaded only when a browser pane opens.
- **Option B: headless Chromium sidecar** with screencast frames drawn into GPUI and input forwarded over CDP. Smaller integration surface, worse latency and text input.
- Either way, the CDP endpoint is bound to loopback and handed to the server through the same capability the Electron app uses today, so server-side automation code does not change.

## 8. Remote hosts in the native client

The native client implements Slice D for real: device key, transport race, `grant → connect → mint → session`, close-code handling (`4401`/`4403` re-grant, `4404` offline, `1013` back off). Rust specifics:

- Device key: ES256 (P-256), non-exportable. Secure Enclave on macOS, TPM/CNG on Windows, a keyring-stored software key on Linux with a visible warning that it is software-backed.
- DPoP proofs per request; credential refresh before the ~1 h TTL.
- Transport order stays **loopback > LAN > Tailscale > SSH > relay**, with **instabox** added as a candidate class ([§9.2](#92-transports)).

Remote connections are Beta/Canary today and Stable rejects them. The native client inherits that gate; it does not widen it ([§13](#13-open-decisions)).

## 9. instabox hosts

### 9.1 Why the whole host runs in the sandbox

Two shapes were considered:

| Shape                                                                                   | Verdict      | Why                                                                                                                                                                                                                                             |
| --------------------------------------------------------------------------------------- | ------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Host in the sandbox**: headless Synara server inside the VM, reached as a remote host | **Chosen**   | Reuses the ADR 0015 host model end to end. Sessions survive the laptop closing. Matches how instabox integrates other agent workspaces.                                                                                                         |
| **Execution backend**: local server, only PTYs/files/processes run in the VM            | Rejected now | ~40 server files call `node:fs` / `node:child_process` directly. A per-capability backend (like the WSL cwd-keyed one in `platformProcess.ts`) needs that refactor first, and the 10-minute exec cap breaks long agent turns over the exec API. |

The execution-backend shape stays a possible later step once the filesystem and process seams exist; nothing in this spec blocks it.

### 9.2 Transports

An instabox host offers two candidates, both behind the session credential:

1. **SSH forward.** `russh` with an instabox SSH certificate (`POST /v1/machines/{id}/ssh-tokens`, or the CLI's certificate), forwarding to the server started with `--ssh-forward-port` / `SYNARA_SSH_FORWARD_PORT`. The server already accepts these as `via: "ssh-forward"` in `apps/server/src/hostConnectivity.ts`. Preferred: no public URL exists.
2. **Private port URL.** `instabox port publish <box> <port> --private` gives a URL with `?_token=`. The client stores the link token in the keychain and still presents the Synara session credential; the port token is a second lock, not the auth. A visit wakes an idle-paused box, so this transport also resumes it.

The relay is a third candidate when the box was enrolled with the account like any other host. The race treats instabox SSH like SSH and the private port like a LAN-class direct URL, ranked below SSH.

### 9.3 Server changes

All additive, all behind the existing remote-host gate:

- `--host-launch=instabox`: headless start, binds to loopback only, enables the SSH-forward listener, writes readiness to a well-known file so the client can poll it over exec.
- Holds an **activity lease** (`PUT /v1/machines/{id}/activity-leases/{taskID}`, TTL 300 s, renewed every 120 s) while any turn, PTY or provider request is live, and releases it when idle. Without this, a turn waiting silently on a model API can be paused as idle. The lease uses an instabox key scoped to that one machine, provisioned as an env Secret.
- On `SIGTERM` from a pause/stop, flushes the event log and checkpoints SQLite, the same path the desktop shutdown takes.
- `maxRunMinutes` is **not** extended by leases. The client shows it and the server warns 10 minutes before it hits.
- Enrollment: the device-code flow (ADR 0015) runs headless inside the VM on first boot; the client approves it automatically because it created the box with the same account.

### 9.4 Provider credentials

- API-key providers: keys live in **instabox Org Secrets** scoped to the provider's HTTPS host. The VM sees only a placeholder; the outbound proxy injects the real header. Synara's provider settings on that host store the placeholder, never the key.
- Subscription logins (CLI OAuth state stored in files) do not fit the header-injection model. They either log in inside the box (the token then lives on the box's disk, which the UI states plainly), or that provider is marked unavailable on instabox hosts. Which one is a maintainer decision ([§13](#13-open-decisions)).
- Before saving any snapshot that will be shared or reused, the server zeroes swap and drops provider caches, per the instabox snapshot guidance.

### 9.5 Client UX for instabox hosts

Structure only; visuals follow the Slice D tokens and need sign-off like every native surface.

- **Connect instabox**: paste or OAuth an instabox API key → stored in keychain → list workspaces (`GET /v1/orgs`) and allowed sizes.
- **New sandbox host**: name, size, idle pause, max run, which Org Secrets to attach. Created from the Synara snapshot ([§9.6](#96-snapshots-and-forks)). Progress shows the create operation's phases.
- Host row extras: state (running / paused / stopped), the transport in use, cost so far (`GET /v1/machines/{id}/usage`), and actions grouped under one **Sandbox** submenu: pause, resume, stop, fork, save snapshot, delete. Delete is the only destructive-colored item.
- **Dev-server previews**: a project's dev-server ports are published `--private` on request and open in the browser pane or the system browser. This replaces today's "previews unavailable on remote hosts" block in `packages/shared/src/remoteCapabilities.ts` **for instabox hosts only**, and only once the capability is advertised by the host.
- **Fork for parallel work**: "Fork host" copies the box (disks only; programs are not copied) and enrolls the copy as a new host. Each fork gets fresh Secret placeholders. Threads in the fork are new threads; nothing merges back automatically.
- **Paused host**: connecting to a box the user paused shows "Resume" instead of retrying; an idle-paused box resumes through `POST /v1/machines/{id}/wake` (waits up to 45 s).

### 9.6 Snapshots and forks

- A CI job builds the **Synara host snapshot** per release: base template, Node runtime, `apps/server` at the release version, git, ripgrep, common toolchains, no credentials. Published to the Zapdev instabox workspace; users create from it or from their own snapshot built on it.
- Version skew: a box's server version is shown on the host row. A client outside the WS compatibility window offers "Update host", which runs the server's own self-update path over exec, not a snapshot restore (restore wipes the disk).
- `instabox create --snapshot` uses the plan's default size unless size is given, so the client always passes size explicitly.

### 9.7 Limits the design must respect

| Limit                                        | Handling                                                                                                                            |
| -------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------- |
| Exec: 10 min per command, 32 at once         | Exec is only used for bootstrap, readiness, update. Never for agent work.                                                           |
| Idle pause (free: 30 min fixed)              | Activity lease while work is live ([§9.3](#93-server-changes)).                                                                     |
| Free plan: 1 running machine, 4 vCPU / 8 GiB | Create flow reads `allowedSpecs` and the running count, explains the limit before failing.                                          |
| 16 public ports per machine                  | Previews publish on demand and unpublish when the preview closes.                                                                   |
| `sandbox.suspended` webhooks                 | Not consumed by the client (no inbound endpoint). The client learns state by polling the machine on focus and on transport failure. |
| Starts rate limit (`429 rate_limited`)       | Retry with the server's `Retry-After`; never hot-loop wake.                                                                         |

## 10. Testing

| Layer           | Tooling                                                                                                                                             | What it proves                                                                  |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Contracts       | Golden fixtures ([§4.3](#43-drift-gate))                                                                                                            | Rust types encode/decode exactly what the server does                           |
| Sync reducer    | Plain `cargo test`, property tests (`proptest`)                                                                                                     | Cursor, buffering, tombstones, instance reset, out-of-order input               |
| Views           | GPUI `TestAppContext` / `VisualTestContext`                                                                                                         | End-follow rules, composer behavior, keyboard flows                             |
| Scenario parity | Shared scenario files run against a real `apps/server` with mock providers, once through the web client (Vitest) and once through the native client | Same state after the same event script, including reconnects mid-stream         |
| Supervision     | Spawn real server, kill client, assert child exit observed                                                                                          | stdin-EOF shutdown, restart backoff, Windows job-object teardown                |
| instabox        | Opt-in job with a real API key, tiny box, deleted after                                                                                             | Create, connect over SSH forward, run one mock turn, pause, wake, resume stream |
| Performance     | Recorded event traces replayed at speed; frame-time histogram                                                                                       | §1 goals                                                                        |
| Accessibility   | AccessKit tree snapshots                                                                                                                            | Labels and roles on every interactive control                                   |

Mock providers prove the client, not live provider success. The completion report for each phase says which providers were exercised live.

## 11. Phases

Each phase ends with a demo build and the listed exit criteria. No phase merges UI it has not had signed off.

### Phase 0 — Contracts and plumbing

- JSON Schema export, Rust codegen, drift gate, golden fixtures.
- `synara-rpc` against a real server: negotiate, call, stream with acks, reconnect.
- **Exit:** a CLI binary (`synara-native-probe`) connects to a running local server, subscribes to orchestration, and prints a live event log that matches the web client's store after the same session.

### Phase 1 — Shell and supervision

- `synara-supervisor`, single instance, window, empty sidebar populated from the store.
- **Exit:** native app starts its own server, shows real projects and threads, quits cleanly on all three OSes with the child exit observed. `bun run windows-runtime:check` passes for any process-boundary changes.

### Phase 2 — Read and chat

- Transcript, markdown, composer, pickers, approvals, command palette.
- **Exit:** a user can open any existing thread, start a turn on each provider adapter that is installed, approve tools, and the end-follow scenario suite passes.

### Phase 3 — Daily-driver parity

- Settings, terminal, diff/find, import/relocate, account, notifications, hubs/groups, MCP panes, Beta diagnostics.
- **Exit:** a maintainer uses the native Beta flavor for a week as their only client; open parity bugs are triaged with none blocking.

### Phase 4 — Remote and instabox hosts

- Slice D in Rust, instabox API client, snapshot pipeline, server `--host-launch=instabox`, activity leases, previews on instabox hosts.
- **Exit:** from a clean account: connect instabox, create a host, run a 20-minute silent turn without an idle pause, preview a dev server through a private port, close the laptop, reopen, and the stream resumes from its cursor.

### Phase 5 — Browser, CUA, editor

- `synara-browser` (CEF or sidecar), CUA/AppSnap viewer, editor integration.
- **Exit:** the existing browser-automation tests pass against the native pane.

### Phase 6 — Cut-over

- Native client becomes the Beta app, then Stable one release later. Electron stays buildable for one Stable release as a fallback, then `apps/desktop` is removed in a separate PR.
- **Exit:** signed releases on both channels; update from the last Electron Stable to the first native Stable verified on all three OSes.

## 12. Risks

| Risk                                                                             | Likelihood | Mitigation                                                                                              |
| -------------------------------------------------------------------------------- | ---------- | ------------------------------------------------------------------------------------------------------- |
| GPUI is pre-1.0 and weekly snapshots break APIs                                  | High       | Pin through gpui-kit; upgrade only on gpui-kit releases; keep platform glue in `synara-shell`.          |
| Linux/Wayland and Windows GPUI maturity lags macOS                               | Medium     | Phase 1 exit requires all three; budget for upstream fixes.                                             |
| Composer parity (Lexical behaviors: IME, paste, chips) is the hardest UI port    | High       | Start it in Phase 2 week 1; scenario tests from current composer bugs.                                  |
| Dropping `ELECTRON_RUN_AS_NODE` changes server runtime on Windows (patched deps) | Medium     | Ship the same Node version Electron embeds; run `windows-runtime:check` on packaged builds, not source. |
| CEF size and Wayland offscreen quality                                           | Medium     | Lazy download of CEF on first browser use; sidecar fallback.                                            |
| Two clients drift in behavior during overlap                                     | High       | Shared scenario suite is the merge gate for both.                                                       |
| Subscription provider logins inside instabox leave tokens on disk                | Medium     | Explicit decision ([§13](#13-open-decisions)); UI states it.                                            |
| instabox pricing/limits change (free-plan sandbox count changed on 2026-12-01)   | Medium     | Read limits from the API at runtime; never hardcode plan numbers in the client.                         |
| Effect schemas that `toJsonSchemaDocument` cannot express                        | Low        | Override list with round-trip tests.                                                                    |

## 13. Open decisions

These are the maintainers' calls. The spec assumes the first option in each row.

| Decision                            | Options                                                                                       | Assumed                   |
| ----------------------------------- | --------------------------------------------------------------------------------------------- | ------------------------- |
| UI framework base                   | gpui-kit 0.9 on its pinned gpui-pre / GPUI pinned straight from a Zed commit without gpui-kit | gpui-kit                  |
| Embedded browser                    | CEF / headless Chromium sidecar / drop the pane on native and use the system browser          | CEF                       |
| Terminal core                       | `alacritty_terminal` / libghostty-vt                                                          | `alacritty_terminal`      |
| Remote and instabox on Stable       | Keep Beta/Canary-only like today / promote with the native cut-over                           | Keep Beta/Canary-only     |
| Subscription logins on instabox     | Allow with on-disk tokens and a warning / disallow                                            | Allow with warning        |
| Native client flavor during overlap | Separate "Synara Native" Beta flavor sharing the Beta data home / replace Beta directly       | Separate flavor           |
| Snapshot publisher                  | Zapdev instabox workspace / each user builds their own from a script                          | Zapdev workspace + script |

## 14. Developing in an instabox sandbox

GPUI builds are heavy (a cold debug build of gpui-kit is several GB of `target/` and pins every core for minutes). The repository supports doing that work inside an instabox sandbox instead of on a laptop:

| Script                             | Runs on | Does                                                                                               |
| ---------------------------------- | ------- | -------------------------------------------------------------------------------------------------- |
| `native/scripts/box-sync.sh <box>` | laptop  | Bundles the current branch with `git bundle` and fast-forwards the checkout inside the box         |
| `native/scripts/box-setup.sh`      | box     | Installs GPUI's system libraries (X11, Wayland, Vulkan, fontconfig), Xvfb, and `bun install`       |
| `native/scripts/box-server.sh`     | box     | Starts a headless `apps/server` on `127.0.0.1:3773` with an isolated `SYNARA_HOME` and a dev token |
| `native/scripts/box-check.sh`      | box     | `cargo fmt --check`, `clippy -D warnings`, `cargo test`, then the probe against the running server |
| `native/scripts/box-screenshot.sh` | box     | Runs `synara-app` under Xvfb with lavapipe and saves a PNG of the window                           |

The box is a development convenience, not a dependency: every script also runs on a normal Linux machine. `CARGO_TARGET_DIR` lives on the box's persistent `/data` disk so a paused box resumes with a warm cache.

## 15. References

- Protocol: `packages/contracts/src/rpc.ts`, `orchestration.ts`, `wsCompatibility.ts`, `hostAuth.ts`; `apps/server/src/wsRpc.ts`, `wsSnapshotLiveStream.ts`.
- Reference client: `apps/web/src/wsTransport.ts`, `routes/__root.tsx`, `components/chat/useChatTranscriptScroll.ts`, `apps/web/src/chat-scroll.ts`, `lib/disclosureMotion.ts`, `hooks/useAppTypography.ts`.
- Shell: `apps/desktop/src/main.ts`, `apps/server/src/desktopParentLifetime.ts`, `packages/shared/src/platformProcess.ts`, `packages/shared/src/betaFeatures.ts`.
- Remote: `docs/remote-connections-v2.md`, `docs/specs/2026-08-13-hosts-as-account-entities.md`, `docs/specs/slice-d-clients.md`, `apps/server/src/hostConnectivity.ts`, `packages/shared/src/remoteCapabilities.ts`, `packages/shared/src/deviceKey.ts`.
- instabox: https://instabox.sh/docs (ports, SSH, auto-pause and activity leases, Org Secrets, snapshots, fork, machines API).
