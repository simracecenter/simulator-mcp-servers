# ADR 0002: LMU Telemetry Access Model & `LmuAdapter` Design

- **Status:** Accepted, **amended 2026-07-13** — see [Amendment](#amendment-2026-07-13-live-verification-reveals-a-rest-api-pivot-away-from-shared-memory-only) below. The original research/decision below is kept for record; the Amendment supersedes D1/D2/D4 on primary access path.
- **Date:** 2026-07-13
- **Deciders:** repo owner (margic), simracecenter org

## Context

[ADR 0001 D1](0001-project-layout.md#d1--workspace-shape-shared-core-crate--one-crate-per-simulator--a-launcher-crate)
reserves a future `crates/lmu-mcp` crate for Le Mans Ultimate (LMU), implementing the same
`XAdapter` (real SDK + Stub) shape already proven by `iracing-mcp`'s `IracingAdapter`. Before that
crate is scaffolded, we need to know **how LMU exposes telemetry and accepts commands**, so the
`LmuAdapter` trait can be designed with the right read/write model from the start rather than
retrofitted later. This is docs-only research (no throwaway spike — LMU only runs on Windows, not
in this Linux dev container) tracked via the ["LMU adapter research" project
card](https://github.com/orgs/simracecenter/projects/1/views/2?pane=issue&itemId=210617448).

## Research findings

**LMU is built by Studio 397 on the rFactor 2 engine lineage** (Studio 397 also develops rFactor 2
itself). Its telemetry/scoring access model follows the same pattern as rFactor 2's, exposed via
the community-maintained, GPL-3.0-licensed
[`rF2SharedMemoryMapPlugin`](https://github.com/TheIronWolfModding/rF2SharedMemoryMapPlugin)
("rFactor 2 Internals Shared Memory Plugin"), authored by a Studio 397 developer and already
tracking LMU-specific compatibility work (see its `Monitor/rF2SMMonitor` commit history). No
separate first-party LMU SDK is publicly documented; this community plugin is the de facto
standard third-party telemetry tool for the rF2/LMU engine family.

### Read path: shared-memory-mapped file (polling), not a broadcast channel

The plugin is an in-process game plugin (`.dll` dropped into `Bin64/Plugins`) that `memcpy`s engine
internals into several **named, versioned shared-memory buffers**, each on its own refresh rate:

| Buffer | Refresh rate | Contents |
| --- | --- | --- |
| `rF2Telemetry` | 50 FPS | Per-vehicle physics (position, speed, damage, tyres, fuel, etc.) |
| `rF2Scoring` | 5 FPS | Session/standings state |
| `rF2Rules` | 3 FPS | Rules/flags state |
| `rF2MultiRules` | on callback | Multiclass rules |
| `rF2ForceFeedback` | 400 FPS | FFB state |
| `rF2PitInfo` | 100 FPS | Pit menu/lane state |
| `rF2Weather` | 1 FPS | Weather state |
| `rF2Extended` | 5 FPS + callback | Derived/workaround state (damage, session transitions) |

External tools (C++ or any language via the shared-memory ABI; a C# sample ships with the plugin)
map these buffers read-only and poll them — architecturally the same shape as iRacing's telemetry
access (`crates/iracing-mcp`'s `SdkAdapter` reads a shared-memory variable map via the `iracing`
crate), **not** a fire-and-forget broadcast/message-based push model.

### Command path: shared-memory *input buffers*, not an OS broadcast message

This is the key difference from iRacing. iRacing's control path
([`crates/iracing-mcp/src/adapter/sdk.rs`](../../crates/iracing-mcp/src/adapter/sdk.rs)) is a
**one-way Windows broadcast message** (`IRSDK_BROADCASTMSG`, sent via `SendNotifyMessageW` to
`HWND_BROADCAST`) for commands like camera switch/state and replay seek/search — fully separate
from the shared-memory telemetry read path, with no built-in acknowledgement (hence the existing
verification-loop pattern: send, then poll telemetry to confirm the effect landed).

The rF2 plugin instead exposes **input buffers in the same shared-memory family** as the read
buffers: `rF2HWControl` (restricted inputs, pit menu), `rF2WeatherControl`, `rF2RulesControl`
(experimental), and `rF2PluginControl` (dynamic buffer subscription), each with its own
read/apply rate (e.g. `HWControl` read at 5 FPS with a 100 ms boost to 50 FPS after an update).
There is **no camera-switch or replay-seek input buffer** in the current plugin — those concepts
don't appear in the published input buffer list at all, unlike iRacing's dedicated broadcast codes
for exactly that.

## Decision

### D1 — `LmuAdapter`'s read path mirrors `IracingAdapter`'s polling shape

Design `LmuAdapter` around the same "connect once, poll shared memory per call" shape as
`SdkAdapter`, not a push/subscribe model. Domain read methods (`get_session_overview`,
`get_standings`, `get_relatives`, etc.) map onto the same buffer family (`rF2Scoring`,
`rF2Telemetry`) iRacing's equivalents map onto its telemetry var map — this part of the trait
should look structurally like `IracingAdapter`, just backed by different memory layouts.

### D2 — `LmuAdapter`'s command path uses input buffers, with no broadcast-style verification gap to bridge

Where `IracingAdapter` needs an explicit "send broadcast, then poll telemetry to verify" loop
because the OS broadcast message has no acknowledgement, `LmuAdapter`'s input-buffer commands
(`HWControl`, `WeatherControl`, `RulesControl`) are read back from the **same shared-memory
family** already being polled for state — so verification can reuse the adapter's normal read path
without a separate out-of-band mechanism. **Camera/replay control parity with iRacing is not
confirmed** — the plugin's documented input buffers do not include a camera-switch or replay-seek
equivalent; this needs live verification against a running LMU instance once implementation
starts (see Open follow-ups).

### D3 — Draft `LmuAdapter` trait shape

```rust
pub type LmuAdapterRef = Arc<dyn LmuAdapter>;

#[async_trait]
pub trait LmuAdapter: Send + Sync {
    // Read path — same polling shape as IracingAdapter, backed by rF2Scoring/rF2Telemetry.
    async fn get_session_overview(&self) -> SessionOverview;
    async fn get_session_data(&self) -> Result<SessionData, AdapterError>;
    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError>;
    async fn get_roster(&self, include_spectators: bool) -> Result<Roster, AdapterError>;
    async fn get_standings(&self, session_num: Option<i32>) -> Result<Standings, AdapterError>;
    async fn get_relatives(&self) -> Result<Relatives, AdapterError>;
    async fn get_weather(&self) -> Result<WeatherState, AdapterError>; // rF2Weather (new vs iRacing)
    async fn get_pit_info(&self) -> Result<PitInfoState, AdapterError>; // rF2PitInfo (new vs iRacing)

    // Command path — input buffers, verified via the read path above (no broadcast gap to bridge).
    async fn pit_menu_command(&self, control: HwControlCommand) -> Result<(), AdapterError>; // rF2HWControl
    async fn set_weather(&self, weather: WeatherControl) -> Result<(), AdapterError>; // rF2WeatherControl

    // Unconfirmed — no known input buffer for these; verify against a live LMU instance
    // before implementing. May need to be omitted or reworked once confirmed.
    async fn camera_focus(&self, car_idx: i32) -> Result<(), AdapterError>;
    async fn replay_seek_session_time(&self, session_time_ms: i32) -> Result<(), AdapterError>;
}
```

`WeatherState`, `PitInfoState`, `HwControlCommand`, and `WeatherControl` are new domain types (no
iRacing equivalent) modeling `rF2Weather`/`rF2PitInfo`/`rF2WeatherControl`/`rF2HWControl` shapes;
exact fields are deferred to implementation, once the plugin's `Include/` C++ headers are mapped to
Rust structs (mirroring how `sdk.rs` maps the `iracing` crate's telemetry vars today).

### D4 — Plugin distribution is an open question, not decided here

Whether `simulator-mcp-servers`/the launcher bundles `rF2SharedMemoryMapPlugin`'s DLL, documents it
as a manual user prerequisite (drop into `Bin64/Plugins`), or something else, is **not decided by
this ADR** — tracked as an open follow-up below, to be resolved once `lmu-mcp` implementation
actually starts.

## Consequences

**Positive**
- Confirms the "trait + real SDK adapter + Stub" shape from ADR 0001 D1 transfers cleanly to LMU —
  no rework of the workspace shape needed.
- The command-path difference (input buffers vs. OS broadcast) means `LmuAdapter` doesn't need to
  replicate `IracingAdapter`'s explicit send-then-poll verification helper — it can just reuse its
  own read methods, which is simpler.
- The plugin's GPL-3.0 license is compatible with this repo's own GPL-3.0 license (ADR 0001 D7),
  avoiding the non-OSS vendored-material friction iRacing's reference sources raised.

**Negative / trade-offs**
- No known input-buffer equivalent for camera control or replay seek/search — if LMU broadcast
  spectating/replay tooling is wanted at parity with the iRacing adapter, that may not be
  achievable through this plugin at all, or may require a different/newer plugin capability not
  yet public. This is a real scope risk for feature parity, not just an implementation detail.
- Relying on a third-party (non-Studio-397-official) plugin means version/layout compatibility with
  LMU is not guaranteed by Studio 397 directly; the plugin's own release history shows breaking
  memory-layout changes have happened before and require a matching plugin version pin.
- Distribution of the plugin DLL is unresolved (D4) — implementation can't fully start until that's
  decided.

## Open follow-ups

- [x] ~~Verify `rF2SharedMemoryMapPlugin`... actually works against a running LMU instance~~ —
      **superseded, see [Amendment](#amendment-2026-07-13-live-verification-reveals-a-rest-api-pivot-away-from-shared-memory-only)**:
      live-verified against a running LMU instance that (a) the plugin is **not installed**, (b)
      LMU's current install layout has **no classic `Bin64/Plugins` folder at all**, and (c) our
      `adapter/sdk.rs` map names (`$rF2SMMP_*$` vs. real `$rFactor2SMMP_*$`) and struct layouts
      were wrong versus the plugin's real headers. The shared-memory plugin path is now
      deprioritized in favor of a live local REST API found on the same running instance.
- [x] Resolve camera/replay control parity: confirm whether any input buffer (current or newer
      plugin version) supports camera switching or replay seek/search; if not, decide whether
      `LmuAdapter` ships without those methods or omits them from the trait entirely. Resolved in
      #7: no known input buffer exists for either; `LmuAdapter` includes both methods for surface
      parity with `IracingAdapter`, returning `AdapterError::NotSupported` unconditionally in both
      `Sdk` and `Stub`. **Updated by Amendment**: LMU's REST API exposes `/rest/watch/focus`
      (confirmed live), so camera-focus parity looks achievable after all — not permanently
      blocked as previously believed. Further tracked in #9.
- [x] ~~Decide plugin distribution (D4)~~ — **superseded, see Amendment**: with the plugin path
      deprioritized, bundling/distributing its DLL is no longer the primary distribution question.
      Revisit only if/when the shared-memory path is revived for data the REST API can't provide.
- [ ] Pin a specific plugin version — **deferred/lower priority**, only relevant if the
      shared-memory path is revived later for data the REST API doesn't cover (e.g. tire/FFB
      telemetry at telemetry-rate, not yet confirmed available over REST).
- [x] ~~confirm write semantics of `/rest/watch/focus` (PUT/POST, payload shape, verifiable
      effect)~~ — **resolved, see [Follow-up (2026-07-13)](#follow-up-2026-07-13-full-openapi-spec-found-reads-wired-up-camera-type-control-added)**:
      `PUT /rest/watch/focus/{slotId}` confirmed (car focus) and `PUT
      /rest/watch/focus/{cameraType}/{trackSideGroup}/false` confirmed (camera type), both verified
      live and now implemented.
- [ ] **New (Amendment):** explore the `127.0.0.1:6398` endpoint (HTTP 426 Upgrade Required on
      every path tried) — likely a WebSocket push feed for live telemetry/standings, which could
      replace polling entirely for read-heavy tools.
- [ ] **New (Amendment):** confirm whether the REST API's port (6397/6398) is stable/configurable
      across LMU versions/installs, and whether it's available unconditionally or only once a
      session is loaded, before `lmu-mcp` depends on it unconditionally.
- [x] ~~determine whether the REST API alone can satisfy `get_weather`/`get_pit_info`/
      `pit_menu_command`/`set_weather`~~ — **partially resolved**: `get_weather`/`get_pit_info` are
      now wired to confirmed endpoints (`sessionInfo`/`GetGameState`/`standings`).
      `pit_menu_command`/`set_weather` remain open — candidate write endpoints exist in the OpenAPI
      spec but their request-body shape is undocumented and untested; see the
      [2026-07-13 Follow-up](#follow-up-2026-07-13-full-openapi-spec-found-reads-wired-up-camera-type-control-added).

## Amendment (2026-07-13): live verification reveals a REST API — pivoting away from shared-memory-only

**Context:** #7 shipped `crates/lmu-mcp` against this ADR's original shared-memory-only design,
built without access to a live LMU instance or the plugin's real headers (explicitly flagged as a
blocking gap in #7's Done criteria). With LMU actually running, this section records what
live verification found, and revises this ADR's decision accordingly. It also directly confirms a
concern raised in [#9's comment thread](https://github.com/simracecenter/simulator-mcp-servers/issues/9):
LMU has more than one way to access session data/commands, and the original research (D1-D4 above)
assumed only one of them.

### What was verified live

1. **`crates/lmu-mcp/src/adapter/sdk.rs`'s shared-memory implementation is wrong**, independent of
   whether the plugin is installed: its map names (`$rF2SMMP_<Type>$`) don't match the real
   plugin's convention (`$rFactor2SMMP_<Type>$`, confirmed from the plugin's own
   `Source/rFactor2SharedMemoryMap.cpp`), and its struct layouts omit the separate
   `rF2MappedBufferVersionBlock` header the real buffers are prefixed with (confirmed from
   `Include/rF2State.h`).
2. **The plugin is not installed on this LMU instance** — confirmed via direct
   `MemoryMappedFile.OpenExisting` probes against both the real and previously-assumed map names;
   neither exists.
3. **LMU's current install layout has no classic `Bin64/Plugins` folder at all** — its directory
   structure (`Bin/` containing only `UI.zip`, `Core/`, `Installed/`, `Packages/`, `Manifests/`, a
   top-level `PluginsAdapter.exe`) differs substantially from classic rFactor2. Where a
   shared-memory plugin DLL would even need to go for this LMU build is **unknown** — this ADR's
   original "drop into `Bin64/Plugins`" instruction (D4, `docs/lmu-mcp-server.md`) does not apply
   as written and should not be trusted until re-confirmed.
4. **LMU exposes a live local REST API, independent of any third-party plugin.** With LMU running,
   its process has two loopback-only listening ports:
   - `127.0.0.1:6397` — a working REST API. Confirmed live (GET-only; no mutating calls attempted
     against the running session):
     - `GET /rest/sessions` → 200, rich session-settings JSON (e.g. `SESSSET_AI_Aggression`,
       `SESSSET_Damage`, etc.).
     - `GET /rest/watch/focus` → 200, returns the current focus slot id (e.g. `0`).
     - `GET /rest/watch/standings` → 200, a JSON array with one rich object per car, including
       `slotID`, `focus`/`hasFocus` booleans, resolved `driverName`/`fullTeamName`/`carClass`
       strings, lap/sector times, position, pit state, etc. — this alone covers most of what
       `get_standings`/`get_roster`/`get_relatives` need, with richer/pre-resolved data than raw
       `rF2Scoring` would give.
     - Many guessed paths 404 (`isVR`, `swap`, `pause`, `nextGrp`/`prevGrp`, `nextCam`/`prevCam`,
       `version`, `state`, `timings`) — the API surface is real but not yet fully mapped; don't
       assume undocumented paths exist.
     - `OPTIONS` is not supported (404) — can't rely on it to discover allowed methods.
   - `127.0.0.1:6398` — returned HTTP 426 Upgrade Required on every path tried, consistent with a
     WebSocket endpoint for live push updates (not yet explored).
   - This directly confirms the core claim in
     [issue #9's research comment](https://github.com/simracecenter/simulator-mcp-servers/issues/9#issuecomment-4961764866)
     (that LMU has a REST API usable for broadcast control, e.g. `LMU Broadcast Control`-style
     tools hitting `/rest/watch/focus/{slotId}`) — verified independently by direct local probing,
     not taken on faith.

### Decision (supersedes D1, D2, D4 above)

**`LmuAdapter` pivots to LMU's local REST API (`127.0.0.1:6397`, and the likely-WebSocket
`:6398` once explored) as its primary data and command source, rather than the
`rF2SharedMemoryMapPlugin` shared-memory buffers D1-D3 were designed around.** Rationale:

- The REST API works today with **zero plugin installation**, resolving D4's distribution question
  by making it largely moot for the primary path.
- It already exposes richer, pre-resolved data (driver/team names, per-car flags) than raw
  shared-memory structs would, reducing `lmu-mcp`'s own parsing/mapping burden.
- It appears to resolve the camera-focus gap (D2, #9) that the shared-memory plugin could not.
- The shared-memory plugin path remains a **possible secondary/supplementary source** later, only
  if the REST API turns out not to cover something `LmuAdapter` needs (e.g. high-rate telemetry
  like tire temps/FFB) — but should not be assumed necessary until that gap is confirmed.

This changes `LmuAdapter`'s transport shape from D1's "shared-memory poll" to an HTTP-client model:
commands become `PUT`/`POST` requests (verification semantics TBD — the REST API's write behavior
hasn't been tested yet), and reads become `GET` requests, both through `crates/mcp-core`'s existing
HTTP-client-capable dependencies rather than raw `winapi` shared-memory calls. The
`mcp_core::verify::verify_loop` helper from #8 still applies: send the command, then poll a read
endpoint (e.g. `/rest/watch/focus`) to confirm the effect landed — the same pattern, just over
HTTP instead of shared memory.

### Impact on open issues

- **#7 (Implement LmuAdapter)** — the shipped `Sdk` adapter (shared-memory-based) needs rework
  against this amendment before it can be considered live-verified. Its Done criteria (blocking
  manual live verification) are not satisfied by the shared-memory implementation as shipped.
- **#9 (camera/replay parity gap)** — no longer assumed permanently blocked; `/rest/watch/focus`
  is a promising real mechanism, pending write-semantics confirmation.

### Write path confirmed live (2026-07-13, with explicit permission)

A full command → poll → verify round trip was run against the live instance and then reverted:

1. Baseline: `GET /rest/watch/focus` → `0`, cross-checked against `standings`' `hasFocus`.
2. Command: `PUT /rest/watch/focus/1` (path-parameter style, matching the pattern from #9's
   comment) → **`200 OK`, empty body**.
3. Verify: polling `GET /rest/watch/focus` every 100ms (2s timeout) observed `1` — **verified**.
4. Cross-check: `GET /rest/watch/standings` afterward showed slot `1`'s `focus`/`hasFocus` flip to
   `true` and slot `0`'s flip to `false`, consistent across both read endpoints.
5. Restored original focus (`PUT /rest/watch/focus/0`), re-verified back to `0` — session left
   exactly as found.

This **confirms `camera_focus` is real and implementable** via `PUT /rest/watch/focus/{slotId}`,
and that `mcp_core::verify::verify_loop` (#8)'s existing send-then-poll shape applies unchanged —
just over HTTP instead of shared memory/broadcast. Issue #9's gap is resolved for this specific
method.

### Not yet done (see Open follow-ups above)

Only `camera_focus` has a confirmed working command. Other commands (`set_weather`,
`pit_menu_command`) haven't been tested — their payload shape is unlikely to be a single scalar
path parameter like focus, so the same convention shouldn't be assumed without testing each one.
The `:6398` endpoint, port stability, and full read coverage (weather/pit info) also remain open.

### Follow-up (2026-07-13): full OpenAPI spec found; reads wired up; camera-type control added

The REST API serves its own full OpenAPI spec at **`/swagger-schema.json`** (the Swagger UI at
`/swagger/index.html` is real, but its default spec path `/swagger/v1/swagger.json` 404s — the UI's
`swagger-initializer.js` points to the actual non-default path). This ~140-endpoint spec is
authoritative and revealed far more than manual endpoint-guessing had found, though it has no
request/response body schemas (auto-generated without doc annotations).

**Confirmed live (read-only, session/track identity + weather + pit info)** — resolving several
"not yet implemented" gaps from the initial pivot:
- `GET /rest/watch/sessionInfo` — track/session identity (`trackName`, `session`), temps, elapsed/
  end time, `darkCloud` (cloudiness, matches the old rF2 `mDarkCloud` concept exactly).
- `GET /rest/sessions/GetGameState` — game phase, `PitState`, `isReplayActive`,
  `inControlOfVehicle`, and a `closeestWeatherNode` (sic) giving rain chance/wind speed.
- `GET /rest/watch/standings`'s existing per-car entries also carry `pitstops`/`penalties`, reused
  for `get_pit_info`'s counts rather than adding a new fetch.

`get_session_data`, `get_weekend_info`, `get_weather`, and `get_pit_info` are now wired to these —
no longer `not_yet_implemented`.

**Confirmed live (camera-type control, with explicit permission during an offline practice
session)** — `PUT /rest/watch/focus/{cameraType}/{trackSideGroup}/{shouldAdvance}` works and maps
exactly onto the classic rF2/ISI `mCameraType` enum (`1`=cockpit → `"COCKPIT"`, `2`=nosecam →
`"NOSECAM"`, `3`=swingman → `"SWINGMAN"`, `4`/`5`=trackside variants → a group name containing
`"Trackside"`, though the exact camera within that group isn't deterministic call-to-call).
`camera_focus` now accepts optional `cameraType`/`trackSideGroup` and verifies whichever of
car-focus/camera-type was actually requested — mirroring `iracing-mcp`'s `camera_focus` exactly.

**Tested, but not confirmable (deprioritized, not implemented)**:
- `POST /rest/replay/toggleactive` flips `GET /rest/replay/isActive`, but had no other observable
  effect (`sessionInfo.currentEventTime` kept climbing at real-time rate regardless).
- `PUT /rest/watch/replaytime/{time}` and `PUT /rest/watch/replayCommand/play|pause` returned `200`
  but showed no confirmable seek/playback effect from the live-monitor context.
- `GET /rest/watch/play/{id}` (load a recorded replay file — one exists: `"Fuji Speedway P1 1"`)
  **errored**: `"Unable to process the request when not in SETUP state"` — a real API-level
  rejection, not reachable from the current context without more investigation into how to reach a
  "SETUP" state via the API.
- `POST /rest/replay/CameraController/setCamera`, `POST /rest/garage/PitMenu/loadPitMenu`, and the
  weather-write endpoints (`POST /rest/sessions/weather/{session}/{node}/{setting}` and
  `.../{preset}`) have no visible request-body schema in the spec and weren't guessed at blind.

This reinforces the working principle from this investigation: **prefer confirmable, verifiable
commands** (clear before/after effect via a `GET`) over ones that merely return `200` — a `200`
response alone isn't evidence a command actually did anything.
