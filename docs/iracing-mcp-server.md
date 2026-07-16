# The `iracing-mcp` server

This document describes `crates/iracing-mcp` — the first `<sim>-mcp` server in this workspace —
covering why it exists, what it's used for, its full tool surface, and the technical approach
behind it. It doubles as the reference implementation: when a new simulator adapter (`lmu-mcp` and
beyond) is built, it should follow the same shape described here, deviating only where a specific
SDK genuinely forces a difference. See [ADR 0001](adr/0001-project-layout.md) for the workspace-wide
rationale this server implements.

## Why this server exists

A human Director running a broadcast has to simultaneously watch the race, decide what's
interesting, and manually drive replay/camera tooling to show it — that's a lot of manual,
low-level interaction for what is fundamentally a high-level intent ("show me the battle for P3").

The **Broadcast Agent** (an AI orchestrator, e.g. an LLM-driven agent) is meant to carry that
low-level burden instead: it holds the high-level intent and needs a way to (a) read what's
happening in the session and (b) drive the simulator's replay/camera systems on the Driver's
behalf — without either side needing to know each other's implementation details.

[Model Context Protocol (MCP)](https://modelcontextprotocol.io) is the contract in between. This
server exposes iRacing's telemetry and control surface (its shared-memory SDK and Win32 broadcast
messages) as a fixed set of MCP tools with typed inputs/outputs, so a Broadcast Agent can call
`get_standings` or `camera_focus` the same way regardless of which LLM or agent framework is
driving it. The server runs locally on the Rig (next to the simulator), hosted by the
[Director Console](../README.md) launcher, and speaks MCP over stdio or HTTP.

## What it's used for

- **Situational awareness** — read-only tools give the Broadcast Agent session state (who's
  racing, standings, gaps, weekend/weather info, available cameras) so it can decide *what* to show
  without polling raw telemetry itself.
- **Camera direction** — the agent points the broadcast camera at a specific car/driver, or toggles
  camera-tool UI state, using the same verified-command pattern a human would get from the in-sim
  camera tool.
- **Replay production** — the agent scrubs, searches, and plays back a recorded session to find
  and present a moment (an incident, an overtake, a lap), including a composite "cue up this
  window and hold it" tool built for producing a specific replay clip.
- **Voice/text driver resolution** — turning a spoken or typed name, initials, or car number into
  the stable `carIdx` every other tool needs, so the agent doesn't have to do fuzzy matching itself.

Every mutating tool **verifies its effect against telemetry** before returning success — the agent
never has to guess whether a command "took." This is a hard project-wide rule (see
[CONTRIBUTING.md](../CONTRIBUTING.md#coding-standards)), not an iRacing-specific nicety.

## Tool reference

All tools are namespaced flat (no simulator prefix) and returned by `tools/list`. Arguments use
`camelCase`. Every response — success or failure — is wrapped in the same envelope (see
[Response envelope](#response-envelope) below).

### Session & read-only tools

These have no side effects and never require being out of the car.

| Tool | Arguments | Returns |
| --- | --- | --- |
| `get_session_overview` | *(none)* | Connectivity + mode: `connected`, `isReplay`, `isInCar`, `sessionName`, `trackName`. Never errors — reports `connected: false` instead. |
| `get_weekend_info` | *(none)* | Static event/track/weather metadata for the current weekend (track, series/season/session IDs, weather). |
| `get_roster` | `includeSpectators?`, `includePaceCar?` (bool) | Drivers/cars/classes currently in the session. |
| `get_camera_groups` | *(none)* | All camera groups and their cameras for the current session (for building a camera picker). |
| `get_standings` | `sessionNum?` (int) | Current standings/timing per driver for a session (defaults to the live session if omitted). |
| `get_relatives` | *(none)* | Live field-order/gap view computed from telemetry arrays — who's near whom on track right now. |
| `resolve_driver` | `query` (string, required), `limit?` (int) | Maps a spoken/typed name, initials, or car number to a ranked list of `carIdx` candidates with confidence + match reason. |
| `replay_get_state` | *(none)* | Live replay + camera telemetry (frame, session time, playback speed, current camera/group/car) — the same snapshot the verification loop below polls internally; also useful standalone for UI. |
| `get_capabilities` | *(none)* | Returns `{ name, status, reason? }` for every tool in this list — `status` is always `supported` here since this is the mature reference implementation, but the same tool exists on `lmu-mcp` where support varies by tool. Lets an agent check support once instead of learning gaps from runtime errors. |

### Camera tools

Mutating; verified against `CamCarIdx`/`CamGroupNumber`/`CamCameraNumber`/`CamCameraState`
telemetry. Require being out of the car (see [Mode guard](#mode-guard)).

| Tool | Arguments | Behavior |
| --- | --- | --- |
| `camera_focus` | `carIdx` (int, required), `groupNumber?`, `cameraNumber?` (int or null) | Switches the active camera to a target car, optionally also switching group/camera. Verifies whichever of car/group/camera were actually requested. |
| `camera_set_state` | `camToolActive?`, `uiHidden?`, `useAutoShotSelection?`, `useTemporaryEdits?`, `useKeyAcceleration?`, `useKey10xAcceleration?`, `useMouseAimMode?` (all bool) | Sets camera-tool UI state bits (e.g. hide UI chrome for a clean broadcast shot) and verifies the resulting `CamCameraState` bitmask. |

### Replay tools

Mutating; verified against `replay_get_state`-shaped telemetry. Require being out of the car.

| Tool | Arguments | Behavior |
| --- | --- | --- |
| `replay_set_playback` | `speed` (int, required), `slowMotion?` (bool) | Sets replay playback speed (0 = paused) and verifies the telemetry reflects it. |
| `replay_seek_session_time` | `sessionNum`, `sessionTimeMs` (int, required), `toleranceMs?` (int) | Seeks to a session-relative timestamp; verifies landing within tolerance (default tolerance is deliberately generous — see [Known limitations](#known-limitations-worth-carrying-into-new-adapters)). |
| `replay_seek_frame` | `frame` (int, required), `mode?` (`begin` \| `current` \| `end`), `toleranceFrames?` (int) | Seeks to an absolute or mode-relative frame; verifies the observed frame. |
| `replay_search_event` | `mode` (required): `to_start`, `to_end`, `prev_session`, `next_session`, `prev_lap`, `next_lap`, `prev_frame`, `next_frame`, `prev_incident`, `next_incident` | Semantic replay jump (iRacing's own "search" concept — laps, incidents, session boundaries) with movement verification. |
| `replay_show_window` | `sessionNum`, `startTimeMs`, `focusCarIdx` (required); `endTimeMs?`, `cameraGroupNum?`, `speed?` (default `1`), `timeoutMs?` (default `2000`) | **Composite** tool: seeks to `startTimeMs`, focuses the camera on `focusCarIdx`, sets playback speed, and — if `endTimeMs` is given — plays until that time then pauses. Each of those four steps is verified independently and reported in a `steps[]` array, so a partial failure is diagnosable rather than an opaque timeout. Built for "cue up and hold this clip" in one call instead of four. |

### Mode guard

Every camera/replay tool above rejects the call with a `wrong_mode` error if the Driver is
currently on track or in the garage (`isOnTrack || isInGarage`) — these controls only make sense in
replay/spectator mode. This is checked once, consistently, rather than duplicated per tool (see
`ensure_out_of_car` in [`handler.rs`](../crates/iracing-mcp/src/handler.rs)).

### Response envelope

Every `tools/call` response — regardless of tool or outcome — has the same shape inside MCP's
`structuredContent` (and the equivalent JSON-encoded in `content[0].text`):

```jsonc
{
  "ok": true,
  "data": { /* tool-specific payload, or null on error */ },
  "warnings": [],
  "error": null // or { "code": "...", "message": "..." } when ok is false
}
```

Verified mutating tools additionally include, inside `data`, a `before`/`observed` (or `finalState`)
telemetry pair and a `verified: bool` so callers can see exactly what changed. `replay_show_window`
extends this with a `steps[]` array, one entry per sub-command.

`error.code` is one of:

| Code | Meaning |
| --- | --- |
| `not_connected` | The simulator isn't running/connected (see [Detecting a real connection](#detecting-a-real-connection-not-just-a-mapping)). |
| `wrong_mode` | Camera/replay tool called while on track or in the garage. |
| `invalid_arguments` | Arguments failed schema/range validation (includes unsupported replay speeds). |
| `target_not_found` | A requested car/driver/session couldn't be resolved. |
| `session_info_error` | The session YAML couldn't be parsed. |
| `missing_telemetry_var` / `invalid_telemetry_type` | A named SDK telemetry variable was absent or the wrong type — usually signals an SDK/version mismatch, not caller error. |
| `broadcast_error` | The underlying Win32 broadcast-message send failed. |
| `timeout` | A mutating tool's command was accepted but telemetry never verified it within the timeout — see [The verification loop](#the-verification-loop-command--poll--verify). |

## Technical implementation

### Layering

```
launcher (runner.rs)
  └─ constructs Arc<dyn IracingAdapter>  (SdkAdapter in production, StubAdapter in tests)
  └─ constructs IracingMcpHandler(adapter), wraps in Arc
  └─ hands the handler to mcp_core::transport::{stdio, http}::run_*

mcp-core                              iracing-mcp
┌─────────────────────────┐           ┌──────────────────────────────┐
│ JsonRpcRequest/Response │◄──────────┤ IracingMcpHandler             │
│ McpHandler trait        │  handle() │  - tools/list descriptors     │
│ stdio + HTTP transports │           │  - tools/call dispatch        │
└─────────────────────────┘           │  - verification-loop helpers  │
                                       └──────────────┬────────────────┘
                                                       │ Arc<dyn IracingAdapter>
                                       ┌──────────────▼────────────────┐
                                       │ IracingAdapter trait          │
                                       │  (domain-typed methods, one   │
                                       │   per capability, no MCP/     │
                                       │   JSON-RPC in this layer)     │
                                       └──────┬─────────────┬─────────┘
                                       SdkAdapter          StubAdapter
                                  (real iRacing SDK,   (in-memory fixture,
                                   #[cfg(windows)])       any platform)
```

This is a straight [hexagonal/ports-and-adapters](https://en.wikipedia.org/wiki/Hexagonal_architecture_(software))
split, deliberately: `mcp-core` knows nothing about iRacing; `IracingMcpHandler` knows MCP/JSON-RPC
shapes and the verification-loop pattern but nothing about *how* telemetry is actually read;
`IracingAdapter` and its two implementations are the only place that touches the real SDK. A future
`lmu-mcp` reuses `mcp-core` unchanged and implements its own `LmuAdapter` trait + `SdkAdapter`/
`StubAdapter` pair — the handler/tool-dispatch shape carries over almost mechanically.

### The adapter trait

[`IracingAdapter`](../crates/iracing-mcp/src/adapter/mod.rs) is an `async_trait` with one method
per capability (`get_roster`, `camera_focus`, `replay_seek_frame`, …), returning domain types
(`Roster`, `ReplayState`, …) and a shared `AdapterError` enum — never raw SDK types or JSON. Two
implementations exist behind `Arc<dyn IracingAdapter>`:

- **`SdkAdapter`** (`adapter/sdk.rs`, `#[cfg(windows)]` for its real body) — talks to the live SDK
  via the `iracing` crate (shared-memory telemetry + session YAML) and the `iracing-broadcast`
  crate (Win32 `SendNotifyMessageW` broadcast commands), plus a small amount of raw
  `OpenFileMappingW`/`MapViewOfFile` code where the `iracing` crate doesn't expose something the
  adapter needs (see [Detecting a real connection](#detecting-a-real-connection-not-just-a-mapping)).
  On non-Windows targets it compiles to a stub that returns `NotConnected` for everything, so the
  workspace builds and the non-adapter logic (handler dispatch, transports) is testable on Linux.
- **`StubAdapter`** (`adapter/stub.rs`) — an in-memory fixture implementation, platform-independent,
  used by every test in `crates/iracing-mcp/tests/` and the handler's own unit tests. It's not a
  mock framework; it's real (if canned) data, so tests exercise real serialization and verification
  logic, not assertions about call counts.

New adapters should keep this Sdk/Stub split even if the target SDK is cross-platform — the value
isn't "Windows vs. Linux," it's having a fast, deterministic double for CI and local iteration that
doesn't need the real sim running.

### The verification loop: command → poll → verify

Every mutating tool follows the same shape, implemented in `handler.rs` rather than the adapter:

1. Snapshot current telemetry (`before`).
2. Send the command (a broadcast message, in iRacing's case — fire-and-forget at the SDK level).
3. Poll telemetry on a short interval (50ms) until either the expected state is observed or a
   timeout elapses.
4. Return `verified: true` + the observed state, or a `timeout` error with whatever was last
   observed — never a bare "command sent, good luck."

`replay_show_window` composes four of these in sequence, each with its **own** timeout budget
(critical: an earlier version of this shared one deadline across all four steps, which meant the
first step's polling could exhaust the whole budget and starve every later step — always give each
verified step of a composite tool its own fresh deadline).

This loop currently lives inline in `iracing-mcp`'s `handler.rs`, not `mcp-core`, on purpose: ADR
0001 D5 deferred extracting it until a second simulator crate actually needs the identical pattern,
to avoid guessing at the right abstraction from a single example. When `lmu-mcp` is built, if its
SDK also has a fire-and-forget command / polled-telemetry shape, that's the trigger to lift this
into `mcp-core` as a generic helper both crates share.

### Detecting a real connection, not just a mapping

A subtlety worth carrying into any new adapter: **"the SDK's shared-memory mapping opened
successfully" is not the same as "the simulator is actually connected right now."** iRacing ships a
background Windows service that keeps its telemetry memory-mapped (with the last-known values
frozen) even after the sim UI fully exits. Checking only "did `OpenFileMappingW`/the wrapper crate's
`Connection::new()` succeed" will happily report success against stale, frozen data.

The real signal is the SDK header's status bitfield (`irsdk_stConnected`, bit 0) — but the
`iracing` crate this adapter depends on reads that bit internally and never exposes it. `sdk.rs`
therefore does its own minimal raw read of just that header field
(`ensure_sim_connected`/the status check inlined into `read_session_yaml_from_view`) rather than
trusting the wrapper crate's notion of "connected." Any new SDK wrapper should be checked for the
same gap before assuming "opens" means "live."

### Transport & wire format

`mcp-core::transport` provides two interchangeable transports, both generic over `McpHandler`:

- **stdio** (`transport::stdio::run_stdio`) — line-delimited JSON-RPC over stdin/stdout, for local
  agent runners that spawn the server as a child process.
- **HTTP** (`transport::http::run_http`) — `POST /mcp` (JSON-RPC) + `GET /healthz`, for remote/LAN
  agent hosts. `build_router` is exposed separately so it can be exercised in tests without binding
  a real socket.

`IracingMcpHandler::handle` implements the `McpHandler` trait and only understands three JSON-RPC
methods: `initialize` (protocol/version/capabilities handshake), `tools/list` (returns the
descriptors summarized in [Tool reference](#tool-reference)), and `tools/call` (dispatches by
`params.name` to one of the per-tool private methods, each of which validates arguments via
`serde`, calls the adapter, and wraps the result in the [response envelope](#response-envelope)).

### Wiring it up

The Director Console (`launcher/src/runner.rs`) constructs the adapter and handler and picks a
transport based on config/CLI flags:

```rust
let adapter = Arc::new(iracing_mcp::adapter::SdkAdapter);
let handler = Arc::new(iracing_mcp::IracingMcpHandler::new(adapter));
match transport {
    TransportKind::Stdio => mcp_core::transport::stdio::run_stdio(handler).await?,
    TransportKind::Http => mcp_core::transport::http::run_http(bind, handler).await?,
}
```

`SdkAdapter` is constructed unconditionally in production — there is no stub-selection path a
misconfiguration could accidentally trigger outside of tests.

### Known limitations worth carrying into new adapters

- **Composite-tool tolerances need real-hardware tuning, not guessed defaults.** `replay_show_window`'s
  seek-verification tolerance was found to be tighter than the actual seek granularity iRacing
  delivers live (a hardcoded ±100ms missed by 120-170ms in practice; widened to ±300ms). Anything
  with a "verify within N ms/frames" contract should be validated against a live sim, not just a
  stub, before shipping a default.
- **Not every SDK-reported speed/mode is guaranteed to be honored.** `replay_set_playback` verifies
  cleanly for speeds `0`/`1`/`2` but was observed timing out for `3`/`4`/`8` in one live replay
  session — confirmed (by polling well past the timeout) that the sim genuinely never reached those
  speeds, rather than it being a slow-to-verify artifact. This looks like sim/session-side behavior,
  not an adapter defect, but it means "the SDK docs say this value is valid" shouldn't be assumed to
  mean "this value is honored in every session type" without live verification.

## Testing this server

- `cargo test -p iracing-mcp` — unit tests plus `tests/http_transport.rs` and
  `tests/verification_regressions.rs`, all against `StubAdapter`; run on any platform, including CI.
- `cargo test -p iracing-mcp --test live_mcp_suite -- --ignored --test-threads=1` — the same tool
  surface against a **real, running iRacing instance** in replay/spectator mode. Kept `#[ignore]`
  deliberately (never runs in CI); run manually on a Windows Rig before merging changes that touch
  `sdk.rs` or the verification loop.
- Manual smoke test via the launcher's HTTP transport (`cargo run -p launcher -- --headless --sim
  iracing --transport http --bind 127.0.0.1:8765`, then `POST /mcp` with `tools/call`) is the way to
  exercise the not-connected path (quit iRacing entirely and confirm every tool still returns a
  clean `not_connected` error rather than hanging, panicking, or silently returning stale data).
