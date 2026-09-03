# 0006 — Session Identity and Phase on `get_session_overview`

- **Status:** Accepted
- **Date:** 2026-09-03
- **Deciders:** Sim RaceCenter maintainers

## Context

The broadcast director reads `get_session_overview`, `get_relatives`, `get_standings` and
`replay_get_state` once per cycle and needs to know *which* session it is covering (practice,
qualifying, race) and *where* that session is in its life (waiting for cars, warmup, parade laps,
green, checkered, cooldown). The audit in
[issue #51](https://github.com/simracecenter/simulator-mcp-servers/issues/51) found:

- Session *type* was already reachable — `get_session_overview.sessionName` is the
  `SessionInfo.Sessions[SessionNum].SessionType` string and `get_standings.sessionType` is the same
  value — but was named as if it were a display label, so consumers did not treat it as a type.
- Session *phase* was **not** reachable from any tool. `TelemetrySnapshot` did not sample
  `SessionState`, `SessionFlags`, `SessionTimeRemain` or `SessionLapsRemain`, so a consumer could
  not distinguish "race session, cars in garage before the start" from "race session, green flag"
  without inferring it from `CarIdxTrackSurface` and lap counters.
- `SessionNum` was sampled into every snapshot and used to select the standings session and to
  build `sessionKey`, but was not exposed on its own, so consumers had to parse it out of
  `meta.sessionKey` to correlate an overview with a `get_standings(sessionNum)` call.

Per ADR 0005 every read is served from one immutable sampler-owned snapshot, so any new field has to
be sampled into `TelemetrySnapshot` first; it cannot be read ad hoc from the SDK at request time.

## Decision

1. **Extend `SessionOverview` additively.** Six optional, camelCase-serialised fields are added
   alongside the existing ones, which keep their names and semantics:

   | Field | Source | `null` when |
   | --- | --- | --- |
   | `sessionNum` | `SessionNum` telemetry var | never while a snapshot exists (disconnected/off-Windows overview) |
   | `sessionType` | same string as `sessionName` (`SessionInfo.Sessions[SessionNum].SessionType`) | session YAML unparseable, disconnected |
   | `sessionState` | `SessionState` telemetry var mapped via `session_state_name` | var absent, unknown enum value, disconnected |
   | `sessionFlags` | `SessionFlags` telemetry var, raw irsdk bitfield | var absent, disconnected |
   | `sessionTimeRemainSec` | `SessionTimeRemain` telemetry var | var absent, disconnected |
   | `sessionLapsRemain` | `SessionLapsRemain` telemetry var | var absent, disconnected |

   `sessionName` is kept unchanged for existing consumers; `sessionType` is the same value under a
   name that says what it is. Rejected alternative: renaming `sessionName` — that is a breaking
   change to a shipped tool for no data gain.

2. **Map `SessionState` to a closed string enum, unknown → `null`.** The irsdk enum is
   `0 Invalid, 1 GetInCar, 2 Warmup, 3 ParadeLaps, 4 Racing, 5 Checkered, 6 CoolDown`. This mirrors
   the `trackSurface` mapping on `RelativeEntry` (ADR 0005): the server never invents a name for a
   value it does not recognise. Rejected alternative: exposing the raw integer — consumers would
   each re-implement the enum and the two sims would diverge further. `sessionFlags` *is* exposed
   raw because it is a bitfield with dozens of flags and iRacing's public header is the authority;
   decoding it into a string list is deferred until a consumer needs it.

3. **The new telemetry vars are optional at sample time.** `read_optional_i32` /
   `read_optional_bits` / `read_optional_f64` treat a missing var as `None` (like
   `CarIdxTrackSurface`) rather than failing the whole snapshot, so an SDK build that omits one of
   them cannot take down every read. A var that is present with the wrong type is still an error,
   because that indicates a real contract break.

4. **Off-Windows and disconnected overviews return `SessionOverview::disconnected()`** — the same
   literal `"Disconnected"` strings as before plus `null` for every new field. Nothing about the
   non-Windows `SdkAdapter` behaviour changes.

5. **Nothing is added to the server that tracks phase over time.** Detecting a transition
   (e.g. `Warmup → ParadeLaps → Racing`) is the consumer's job by comparing successive snapshots,
   exactly as `sessionKey`/`sessionRevision` already require for session changes. The MCP surface
   stays stateless per the project's director/server split.

## Consequences

- Consumers can gate race-oriented logic on `sessionType` and `sessionState` from the single tool
  they already poll every cycle, with the values guaranteed to come from the same snapshot tick as
  `meta.sessionTick`.
- `sessionNum` on the overview lines up directly with `get_standings(sessionNum)` and with the third
  component of `meta.sessionKey`.
- `lmu-mcp` has its own `SessionOverview` type and is **not** changed by this ADR (ADR 0003 — one
  active simulator, separate tool surfaces). If LMU later exposes an equivalent phase, it should
  reuse the same field names and string values so a capability-aware consumer can share logic.
- The stub adapter reports a fixed `Racing` state with a one-hour remainder so Linux-side tests
  can exercise the new fields; the real sampler path is exercised only on Windows.

## Open follow-ups

- Decode `sessionFlags` into a string list once a consumer needs specific flags (caution, white,
  checkered, start-ready) rather than the raw bitfield.
- Server-pushed session-phase change notifications over the ADR 0004 SSE stream (see ADR 0005 open
  follow-ups).
