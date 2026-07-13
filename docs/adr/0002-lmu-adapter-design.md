# ADR 0002: LMU Telemetry Access Model & `LmuAdapter` Design

- **Status:** Accepted
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

- [ ] Verify `rF2SharedMemoryMapPlugin` (or whatever plugin version is current at implementation
      time) actually works against a running LMU instance — buffer layouts, refresh rates, and
      input-buffer behavior confirmed live, on Windows, before writing `crates/lmu-mcp` code.
- [ ] Resolve camera/replay control parity: confirm whether any input buffer (current or newer
      plugin version) supports camera switching or replay seek/search; if not, decide whether
      `LmuAdapter` ships without those methods or omits them from the trait entirely.
- [ ] Decide plugin distribution (D4): bundle the DLL, document manual install, or another
      approach.
- [ ] Pin a specific plugin version once implementation starts, given its history of breaking
      memory-layout changes between versions.
