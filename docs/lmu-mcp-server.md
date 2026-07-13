# The `lmu-mcp` server

This document describes `crates/lmu-mcp` — the Le Mans Ultimate (LMU) `<sim>-mcp` server — covering
why it exists, what it's used for, its tool surface, the technical approach behind it, and its
current limitations. It follows the shape set by
[`docs/iracing-mcp-server.md`](iracing-mcp-server.md) (the reference implementation), deviating
only where LMU's telemetry/control model genuinely differs — see
[ADR 0002](adr/0002-lmu-adapter-design.md) for the design rationale.

## Why this server exists

Same motivation as `iracing-mcp`: a Broadcast Agent needs a uniform, typed way to read LMU session
state and drive pit/weather commands on the Driver's behalf, without needing to know this crate
talks to a completely different simulator underneath. See
[`docs/iracing-mcp-server.md`'s "Why this server exists"](iracing-mcp-server.md#why-this-server-exists)
for the full rationale — it applies unchanged here.

## Prerequisite: install `rF2SharedMemoryMapPlugin`

Unlike iRacing (whose SDK ships with the sim itself), LMU has no first-party telemetry SDK. This
server reads/writes the community-maintained, GPL-3.0-licensed
[`rF2SharedMemoryMapPlugin`](https://github.com/TheIronWolfModding/rF2SharedMemoryMapPlugin)'s
shared-memory buffers instead (see [ADR 0002](adr/0002-lmu-adapter-design.md)'s research section).

**Before `lmu-mcp` can talk to a running LMU instance, the engineer/Driver must manually install
the plugin**: download the latest release from the plugin's repository and drop its `.dll` into
LMU's `Bin64/Plugins` directory. This server does **not** bundle or auto-install the plugin —
distribution (bundle vs. document) is an open ADR 0002 follow-up (D4), deliberately deferred.

> **Plugin version pin:** the exact `rF2SharedMemoryMapPlugin` version/commit this adapter was
> implemented and verified against should be recorded here and in
> [`crates/lmu-mcp/src/adapter/sdk.rs`](../crates/lmu-mcp/src/adapter/sdk.rs)'s module doc comment
> once the manual live-verification step (see [Known limitations](#known-limitations)) has actually
> been performed against a real installation — it is **not** pinned as of this change, which was
> written without access to the plugin's real headers or release history.

## Tool reference

All tools are namespaced flat (no simulator prefix) and returned by `tools/list`. Arguments use
`camelCase`. Every response uses the same [envelope as `iracing-mcp`](iracing-mcp-server.md#response-envelope).

### Read-only tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `get_session_overview` | *(none)* | Connectivity + mode: `connected`, `isReplay`, `isInCar`, `sessionName`, `trackName`. Never errors — reports `connected: false` instead. |
| `get_session_data` | *(none)* | Track name, session type, game phase, elapsed/end session time, driver count. |
| `get_weekend_info` | *(none)* | Static event/track/weather metadata for the current weekend. |
| `get_roster` | `includeSpectators?` (bool) | Drivers/cars/classes currently in the session (`rF2Scoring`). |
| `get_standings` | `sessionNum?` (int) | Current standings/timing per driver (`rF2Scoring`). |
| `get_relatives` | *(none)* | Live field-order/gap view computed from scoring data. |
| `get_weather` | *(none)* | Current weather: rain, cloudiness, ambient/track temperature, wind (`rF2Weather`). |
| `get_pit_info` | *(none)* | Current pit menu/lane state for the player's car (`rF2PitInfo`). |

### Command tools

Mutating; verified by polling the corresponding read tool above, per
[ADR 0002 D2](adr/0002-lmu-adapter-design.md#d2--lmuadapters-command-path-uses-input-buffers-with-no-broadcast-style-verification-gap-to-bridge) —
LMU's input buffers are read back from the same shared-memory family already being polled, unlike
iRacing's one-way broadcast messages, so no separate verification mechanism is needed.

| Tool | Arguments | Behavior |
| --- | --- | --- |
| `pit_menu_command` | `controlName` (string, required), `value` (number, required), `timeoutMs?` (int) | Writes an `rF2HWControl` command. Only `request_pit`/`cancel_pit`/`confirm_pit` control names have a modeled, verifiable effect on `get_pit_info`'s state today; other control names are accepted and reported verified as soon as one post-send poll succeeds — see [Known limitations](#known-limitations). |
| `set_weather` | `raining` (number 0..1, required), `cloudiness?`, `ambientTempC?` (number), `tolerance?` (default `0.05`), `timeoutMs?` (int) | Writes an `rF2WeatherControl` command and verifies the resulting `rF2Weather.raining` value lands within `tolerance`. |

### Not-yet-supported tools

Present in the tool set and the `LmuAdapter` trait for surface parity with `iracing-mcp`'s
`camera_focus`/`replay_seek_session_time` (per ADR 0002 D3), but **both `Sdk` and `Stub`
implementations return a `not_supported` error unconditionally** — no known `rF2` input buffer
exists for camera switching or replay seek/search as of ADR 0002's research. Tracked further in
issue #9; do not build assumption of these working into a Broadcast Agent's LMU flows yet.

| Tool | Arguments |
| --- | --- |
| `camera_focus` | `carIdx` (int, required) |
| `replay_seek_session_time` | `sessionTimeMs` (int, required) |

## Technical implementation

### Layering

Identical shape to `iracing-mcp` (see
[its "Layering" diagram](iracing-mcp-server.md#layering)) — `mcp-core` is unchanged and reused as-is:

```
launcher (runner.rs)
  └─ constructs Arc<dyn LmuAdapter>  (SdkAdapter in production, StubAdapter in tests)
  └─ constructs LmuMcpHandler(adapter), wraps in Arc
  └─ hands the handler to mcp_core::transport::{stdio, http}::run_*
```

### The adapter trait

[`LmuAdapter`](../crates/lmu-mcp/src/adapter/mod.rs) is an `async_trait`, one method per capability,
domain-typed returns, a shared `AdapterError` enum — mirroring `IracingAdapter`'s shape (per ADR
0002 D3). Two implementations exist behind `Arc<dyn LmuAdapter>`:

- **`SdkAdapter`** (`adapter/sdk.rs`, `#[cfg(windows)]` for its real body) — opens
  `rF2SharedMemoryMapPlugin`'s named shared-memory mappings directly via raw `winapi` calls
  (`OpenFileMappingW`/`MapViewOfFile`/`UnmapViewOfFile`), matching the precedent already reviewed in
  `crates/iracing-mcp/src/adapter/sdk.rs`. On non-Windows targets it compiles to a stub that returns
  `NotConnected`/`NotSupported` for everything, so the workspace builds and Stub-backed tests run on
  Linux.
- **`StubAdapter`** (`adapter/stub.rs`) — an in-memory fixture, platform-independent, used by every
  test in `crates/lmu-mcp/tests/` and the handler's own unit tests.

### The verification loop: command → poll → verify

`pit_menu_command`/`set_weather` reuse [`mcp_core::verify::verify_loop`](../crates/mcp-core/src/verify.rs) —
the same generic send-poll-verify helper `iracing-mcp`'s replay/camera tools use, promoted into
`mcp-core` in #8. `lmu-mcp` was the second crate to need this shape, confirming the promotion.

## Known limitations

- **The `Sdk` adapter's shared-memory struct layouts are a best-effort reconstruction, not verified
  against the plugin's real headers.** This implementation was written in a Linux dev container
  with no access to `rF2SharedMemoryMapPlugin`'s actual `Include/` headers or release history — see
  [`adapter/sdk.rs`](../crates/lmu-mcp/src/adapter/sdk.rs)'s module doc comment for the full caveat.
  **This must be manually verified live against a running LMU instance on Windows before being
  trusted** (see issue #7's blocking Done criterion) — struct field offsets, buffer names, and
  command effects may all need correction once checked against the real plugin.
- **`pit_menu_command`'s generic verification is weak for unrecognized control names.** Only
  `request_pit`/`cancel_pit`/`confirm_pit` have a real, checkable effect on `get_pit_info`'s state
  today; any other `controlName` is reported `verified: true` on the first successful poll, not a
  genuine confirmation the command took effect. Tightening this requires knowing the plugin's real
  `rF2HWControl` control-name surface, which is part of the same pending manual verification above.
- **`camera_focus`/`replay_seek_session_time` are not implemented** — see
  [Not-yet-supported tools](#not-yet-supported-tools) above and issue #9.
- **Plugin distribution is not automated.** See [Prerequisite](#prerequisite-install-rf2sharedmemorymapplugin)
  above — bundling remains an open ADR 0002 follow-up (D4).
