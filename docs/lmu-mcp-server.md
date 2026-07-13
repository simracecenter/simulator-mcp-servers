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

## Prerequisite: none — LMU's REST API needs no plugin install

**Update (2026-07-13):** this section originally required manually installing the third-party
[`rF2SharedMemoryMapPlugin`](https://github.com/TheIronWolfModding/rF2SharedMemoryMapPlugin).
Live testing against a running LMU instance found that plugin **wasn't installed and LMU's real
install layout has no classic `Bin64/Plugins` folder to put it in** — and, independently, that LMU
exposes a **local REST API** (`127.0.0.1:6397`) with zero plugin/install step required at all. See
[ADR 0002's Amendment](adr/0002-lmu-adapter-design.md#amendment-2026-07-13-live-verification-reveals-a-rest-api-pivot-away-from-shared-memory-only)
for the full story. `lmu-mcp` now talks to that REST API directly — just have LMU running.

The shared-memory plugin path is deprioritized, not deleted from the historical record: if a future
gap needs data the REST API doesn't expose (see [Known limitations](#known-limitations)), it may be
revisited, but shouldn't be assumed necessary.

## Tool reference

All tools are namespaced flat (no simulator prefix) and returned by `tools/list`. Arguments use
`camelCase`. Every response uses the same [envelope as `iracing-mcp`](iracing-mcp-server.md#response-envelope).

Not every tool below is equally well-supported yet — call `get_capabilities` (see
[Capability discovery](#capability-discovery)) to check before relying on one, rather than
discovering gaps via a `not_supported`/`not_yet_implemented` error.

### Read-only tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `get_session_overview` | *(none)* | Connectivity + mode: `connected`, `isReplay`, `isInCar`, `sessionName`, `trackName`. Never errors — reports `connected: false` instead. `sessionName`/`trackName`/`isReplay` are currently placeholders (unconfirmed over REST — see Known limitations). |
| `get_session_data` | *(none)* | **Not yet implemented** — no confirmed REST endpoint for track/session identity yet. |
| `get_weekend_info` | *(none)* | **Not yet implemented** — same gap as `get_session_data`. |
| `get_roster` | `includeSpectators?` (bool, currently a no-op) | Drivers/cars/classes currently in the session, from `GET /rest/watch/standings`. |
| `get_standings` | `sessionNum?` (int, currently ignored — no session filter found on the endpoint) | Current standings/timing per driver, from `GET /rest/watch/standings`. |
| `get_relatives` | *(none)* | Live field-order/gap view, derived from the same `GET /rest/watch/standings` response. |
| `get_weather` | *(none)* | **Not yet implemented** — no confirmed REST endpoint for weather yet. |
| `get_pit_info` | *(none)* | **Not yet implemented** — no confirmed REST endpoint for pit info yet. |

### Command tools

| Tool | Arguments | Behavior |
| --- | --- | --- |
| `camera_focus` | `carIdx` (int, required), `timeoutMs?` (int, default `1000`) | **Confirmed working live (2026-07-13).** Sends `PUT /rest/watch/focus/{carIdx}` and verifies via `GET /rest/watch/focus`, the same send-then-poll shape `iracing-mcp` uses. |
| `pit_menu_command` | `controlName` (string, required), `value` (number, required), `timeoutMs?` (int) | **Not yet implemented** — no confirmed REST endpoint for pit-menu commands yet. |
| `set_weather` | `raining` (number 0..1, required), `cloudiness?`, `ambientTempC?` (number), `tolerance?`, `timeoutMs?` (int) | **Not yet implemented** — no confirmed REST endpoint for weather commands yet. |

### Not-yet-supported tools

| Tool | Arguments | Why |
| --- | --- | --- |
| `replay_seek_session_time` | `sessionTimeMs` (int, required) | No known LMU API (REST or shared-memory) supports replay seeking — LMU's REST surface looks built for *live directing*, not *replay production*. Tracked in issue #9; don't build assumption of this working into a Broadcast Agent's LMU flows. |

### Capability discovery

| Tool | Arguments | Returns |
| --- | --- | --- |
| `get_capabilities` | *(none)* | An array of `{ name, status, reason? }` — `status` is `supported`, `degraded`, or `unsupported` — for every tool above, reflecting this build's real support against a live LMU instance. Call this once up front so an agent can plan around gaps instead of discovering them via runtime errors. |

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
domain-typed returns, a shared `AdapterError` enum — mirroring `IracingAdapter`'s shape. Two
implementations exist behind `Arc<dyn LmuAdapter>`:

- **`SdkAdapter`** (`adapter/sdk.rs`) — a plain `reqwest` HTTP client against LMU's local REST API
  (`127.0.0.1:6397`, confirmed live). Unlike `iracing-mcp`'s SDK adapter (and this crate's own
  original shared-memory design), there's no `#[cfg(windows)]` split — an HTTP client behaves
  identically on every target, so Linux CI naturally gets a clean `NotConnected` error since
  nothing's listening on that port there.
- **`StubAdapter`** (`adapter/stub.rs`) — an in-memory fixture, platform-independent, used by every
  test in `crates/lmu-mcp/tests/` and the handler's own unit tests. Implements more than
  `SdkAdapter` currently does (e.g. weather/pit commands) for testability — `get_capabilities`
  describes `SdkAdapter`'s real-world support, not the Stub's.

### The verification loop: command → poll → verify

`camera_focus` reuses [`mcp_core::verify::verify_loop`](../crates/mcp-core/src/verify.rs) — the same
generic send-poll-verify helper `iracing-mcp`'s replay/camera tools use, promoted into `mcp-core` in
#8, applied here over HTTP instead of shared memory: `PUT` the command, then poll `GET
/rest/watch/focus` until it reflects the new value or a timeout elapses.

## Known limitations

- **Most read/command tools beyond `get_session_overview`/`get_roster`/`get_standings`/
  `get_relatives`/`camera_focus` aren't wired to LMU's REST API yet** — no confirmed endpoint was
  found for session/track identity, weather, or pit info during initial live testing. They return
  `not_yet_implemented` rather than being guessed at; see
  [ADR 0002's Amendment](adr/0002-lmu-adapter-design.md#amendment-2026-07-13-live-verification-reveals-a-rest-api-pivot-away-from-shared-memory-only)
  for what was actually probed.
- **`get_session_overview`'s `sessionName`/`trackName`/`isReplay` are placeholders** — no REST
  endpoint exposing track/session identity or replay-mode state has been found yet.
- **The REST API's port (`6397`) is hardcoded and not confirmed stable/configurable** across LMU
  versions or installs.
- **`replay_seek_session_time` isn't implemented** — see [Not-yet-supported tools](#not-yet-supported-tools)
  above and issue #9.
- **A `:6398` endpoint returns HTTP 426 Upgrade Required** on every path tried — likely a WebSocket
  push feed, not yet explored; could replace polling for read-heavy tools if confirmed.
- **Plugin distribution is not automated.** See [Prerequisite](#prerequisite-install-rf2sharedmemorymapplugin)
  above — bundling remains an open ADR 0002 follow-up (D4).
