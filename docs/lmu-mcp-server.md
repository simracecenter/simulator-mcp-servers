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

> **The full API surface is documented at `http://localhost:6397/swagger/index.html`** (its actual
> spec is served from `/swagger-schema.json`, not the Swashbuckle default `/swagger/v1/swagger.json`).
> This ~140-endpoint spec (discovered 2026-07-13) is how `get_session_data`/`get_weekend_info`/
> `get_weather`/`get_pit_info`/camera-type switching below were confirmed, rather than guessed at.
> It has no request/response body schemas (auto-generated without doc annotations), so mutating
> endpoints still need live confirmation before trusting them — see
> [ADR 0002's Amendment](adr/0002-lmu-adapter-design.md#amendment-2026-07-13-live-verification-reveals-a-rest-api-pivot-away-from-shared-memory-only)
> for exactly what's been confirmed vs. only found-in-the-spec-but-untested.

## Tool reference

All tools are namespaced flat (no simulator prefix) and returned by `tools/list`. Arguments use
`camelCase`. Every response uses the same [envelope as `iracing-mcp`](iracing-mcp-server.md#response-envelope).

Not every tool below is equally well-supported yet — call `get_capabilities` (see
[Capability discovery](#capability-discovery)) to check before relying on one, rather than
discovering gaps via a `not_supported`/`not_yet_implemented` error.

### Read-only tools

| Tool | Arguments | Returns |
| --- | --- | --- |
| `get_session_overview` | *(none)* | Connectivity + mode: `connected`, `isReplay`, `isInCar`, `sessionName`, `trackName` — all real, confirmed live via `GET /rest/watch/sessionInfo` + `GET /rest/sessions/GetGameState`. Never errors — reports `connected: false` instead. |
| `get_session_data` | *(none)* | Track name, session type, game phase, elapsed/end time, driver count — from `GET /rest/watch/sessionInfo` + `GET /rest/sessions/GetGameState`. |
| `get_weekend_info` | *(none)* | Static event/track/weather metadata for the current weekend — same sources as `get_session_data`, plus `sessionInfo.darkCloud`/`GetGameState`'s nearest weather node. |
| `get_roster` | `includeSpectators?` (bool, currently a no-op) | Drivers/cars/classes currently in the session, from `GET /rest/watch/standings`. |
| `get_standings` | `sessionNum?` (int, currently ignored — no session filter found on the endpoint) | Current standings/timing per driver, from `GET /rest/watch/standings`. |
| `get_relatives` | *(none)* | Live field-order/gap view, derived from the same `GET /rest/watch/standings` response. |
| `get_weather` | *(none)* | Current weather: ambient/track temp (`sessionInfo`), rain chance and wind speed (`GetGameState`'s nearest weather node), cloudiness (`sessionInfo.darkCloud`). Wind-speed unit is unconfirmed — passed through as reported. |
| `get_pit_info` | *(none)* | Pit state for the player's car — `pitState` from `GetGameState`, `inPits`/pitstop/penalty counts from the player's own `GET /rest/watch/standings` entry. |

### Command tools

| Tool | Arguments | Behavior |
| --- | --- | --- |
| `camera_focus` | `carIdx` (int, required), `cameraType?` (int), `trackSideGroup?` (int), `timeoutMs?` (int, default `1000`) | **Confirmed working live (2026-07-13).** Sends `PUT /rest/watch/focus/{carIdx}` and, if `cameraType` is given, also `PUT /rest/watch/focus/{cameraType}/{trackSideGroup}/false` (`trackSideGroup` defaults to `0`). Verifies whichever was requested via `GET /rest/watch/focus` + `GET /rest/replay/CameraController/getCameraInfo`. `cameraType` follows the classic rF2/ISI enum: `1`=cockpit, `2`=nosecam, `3`=swingman, `4`/`5`=trackside variants (exact camera within a trackside group isn't deterministic — only the group is verified for those). |
| `pit_menu_command` | `controlName` (string, required), `value` (number, required), `timeoutMs?` (int) | **Not yet implemented** — the spec has a `POST /rest/garage/PitMenu/loadPitMenu` candidate, but its request body shape is undocumented and untested. |
| `set_weather` | `raining` (number 0..1, required), `cloudiness?`, `ambientTempC?` (number), `tolerance?`, `timeoutMs?` (int) | **Not yet implemented** — the spec has `POST /rest/sessions/weather/{session}/{node}/{setting}` and `.../{preset}` candidates, but body/semantics are untested. |

### Not-yet-supported tools

| Tool | Arguments | Why |
| --- | --- | --- |
| `replay_seek_session_time` | `sessionTimeMs` (int, required) | A `PUT /rest/watch/replaytime/{time}` endpoint exists in the spec, but live-testing found no observable seek effect from the current live-monitor context (`currentEventTime` kept climbing at real-time rate regardless). Loading a saved replay file (`GET /rest/watch/play/{id}`) — which might be the missing prerequisite — errored `"not in SETUP state"` from this context. Tracked in issue #9; don't build assumption of this working into a Broadcast Agent's LMU flows yet. |

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
#8, applied here over HTTP instead of shared memory: `PUT` the command(s), then poll `GET
/rest/watch/focus` + `GET /rest/replay/CameraController/getCameraInfo` (combined into one
[`CameraFocusState`](../crates/lmu-mcp/src/adapter/mod.rs)) until whichever of car-focus/camera-type
was requested is reflected, or a timeout elapses — mirroring `iracing-mcp`'s `camera_focus`, which
also "verifies whichever of car/group/camera were actually requested".

## Known limitations

- **`pit_menu_command`/`set_weather` aren't wired to LMU's REST API yet.** The full OpenAPI spec
  (`/swagger-schema.json`) has plausible candidates (`POST /rest/garage/PitMenu/loadPitMenu`,
  `POST /rest/sessions/weather/{session}/{node}/{setting}` and `.../{preset}`), but their request
  bodies aren't documented in the spec and haven't been live-tested. They return
  `not_yet_implemented` rather than being guessed at.
- **`replay_seek_session_time` isn't implemented** — a candidate endpoint exists
  (`PUT /rest/watch/replaytime/{time}`) but showed no observable effect in live testing from the
  current live-monitor context; see [Not-yet-supported tools](#not-yet-supported-tools) above and
  issue #9.
- **`camera_focus`'s trackside camera verification only checks the camera *group*, not the exact
  camera.** Live-testing found the exact trackside camera picked for a given `cameraType`/
  `trackSideGroup` isn't fully deterministic (the same request landed on different camera names
  across calls) — only that the resulting group name contains `"Trackside"` is a reliable
  invariant. `cameraType=0` ("TV cockpit" per the classic rF2 enum) is untested and treated as an
  alias for `1` (cockpit) as a best-effort guess.
- **The REST API's port (`6397`) is hardcoded and not confirmed stable/configurable** across LMU
  versions or installs.
- **A `:6398` endpoint returns HTTP 426 Upgrade Required** on every path tried — likely a WebSocket
  push feed, not yet explored; could replace polling for read-heavy tools if confirmed.
- **Some field units are unconfirmed**, notably `get_weather`'s `windSpeedMs` — passed through as
  reported by the REST API rather than assuming a specific unit.
- **Plugin distribution is moot for now** — the shared-memory plugin path is deprioritized (see
  [Prerequisite](#prerequisite-none--lmus-rest-api-needs-no-plugin-install) above), so bundling it
  isn't a live concern unless that path is revisited.
