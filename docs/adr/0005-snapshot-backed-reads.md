# 0005 — Snapshot-Backed Reads, Dedicated SDK Ownership, and Read Metadata

- **Status:** Proposed
- **Date:** 2026-08-29
- **Deciders:** Sim RaceCenter maintainers

## Context

Every read tool in `iracing-mcp` reaches into the simulator independently. `read_session_yaml()`
opens the shared-memory mapping, copies the session string out, and hands it to the caller;
`session_data_sync`, `replay_state_sync`, `get_standings` and `get_relatives` each construct their
own `iracing::Connection`. The only cache is a raw `String` keyed on `session_info_update`
(`SessionYamlCache` in `adapter/sdk.rs`), so all four YAML consumers re-run `parse_session_root` on
every call. None of this work is offloaded from the Tokio runtime: there is no `spawn_blocking` or
`block_in_place` anywhere in the workspace, so Win32 mapping, memory copying and YAML parsing all
run on runtime worker threads.

Two consequences drive this ADR. The first is latency: a broadcast cycle issues several reads and
pays the mapping-plus-parse cost once per read. The second, and more damaging, is **incoherence** —
`get_relatives` and `get_standings` describe two different instants, with no way for a client to
tell how far apart they were or whether either is stale. Nothing in any payload carries a tick, a
session time, a capture timestamp, or a session identity, so `margic/sandbox` reconstructs all of
it: a six-field session fingerprint in `director.py`, a 20 s slow-read staleness guess
(`DEFAULT_SNAPSHOT_MAX_AGE_SEC` in `sense_reads.py`), and a negative-lap heuristic
(`entry_in_world`) standing in for world state the server never sends.

## Decision

### 1. One OS thread owns the SDK connection for the process lifetime

A dedicated `std::thread` (the *sampler*) creates the `iracing::Connection` and the shared-memory
view once and keeps them. Async adapter methods no longer touch Win32; they read published state or,
for commands, hand a request to the sampler over a channel.

*Rejected:* wrapping each existing adapter body in `tokio::task::spawn_blocking`. It removes the
blocking-on-the-runtime problem and nothing else — every call still remaps and reconnects, reads
stay mutually incoherent, and there is still no place to put a sampling loop. Also rejected:
`block_in_place`, which only works on the multi-thread scheduler and still serializes on a worker.

### 2. Reads serve a published immutable snapshot, and never sample on demand

The sampler publishes `Arc<TelemetrySnapshot>` — telemetry values plus the parsed session document
as `Arc<YamlValue>` — and readers take an `Arc` clone. A read is a pointer copy and a serialization,
so all reads within one broadcast cycle describe instants at most one sample apart, and the parsed
YAML is shared rather than re-parsed per consumer. This supersedes the raw-string
`SessionYamlCache`.

*Rejected:* on-demand sampling behind a mutex (keeps per-call latency and reintroduces
incoherence); caching each domain type separately with its own TTL (N independent staleness stories,
which is the problem the client already has).

### 3. The sampler is event-driven for telemetry and change-driven for session YAML

The thread waits on the SDK's data-ready signal rather than polling a timer, so the publish cadence
follows the simulator's own tick instead of a number we invent. The session document is re-read and
re-parsed only when `sessionInfoUpdate` changes; it is otherwise carried forward by `Arc` clone.

*Rejected:* a fixed poll interval (either lags the sim or burns CPU, and picking the number is a
broadcast-cadence decision masquerading as an SDK one).

### 4. Freshness and identity travel in a `meta` sibling of `data`, not inside domain types

The tool envelope becomes `{ok, data, meta, warnings, error}`, where `meta` carries
`sessionTick`, `sessionTime`, `capturedAtUnixMs`, `ageMs`, `stale`, `sessionKey`, `sessionRevision`
and `serverElapsedMs`. Every read gets the same block for free, and `SessionOverview`, `Roster`,
`Standings`, `Relatives` and `CameraGroupList` keep their current shapes.

*Rejected:* adding freshness fields to each domain struct (five near-duplicate definitions, five
migrations, and no answer for tools that return neither); a top-level sibling of `result` outside
the envelope (invisible to clients that unwrap `data`, which is exactly what `sandbox`'s
`step_executor.py` does).

### 5. `sessionKey` identifies the session; `sessionRevision` counts transitions

`sessionKey` is derived from the authoritative iRacing identity (`subSessionId`, `sessionId` and the
current session number) and is stable for as long as that session is the active one.
`sessionRevision` is a process-monotonic counter incremented whenever `sessionKey` changes, the sim
disconnects or reconnects, or the session document reports a session transition. It never decreases
within a process, and it is *not* stable across a server restart — a client seeing a familiar
revision with an unfamiliar `sessionKey` must invalidate.

*Rejected:* exposing raw `sessionInfoUpdate` as the revision (it also ticks for edits that are not
session transitions, and it resets); a content hash of the session document (changes constantly,
tells a client nothing about *what* changed).

### 6. Disconnection degrades reads instead of failing them

When the sim goes away the sampler stops publishing; the last snapshot remains readable with
`connected: false` and a growing `ageMs`, and `stale: true` once age exceeds the staleness
threshold. Reads succeed with stale-marked data so a director can keep making decisions; commands
continue to fail with `NotConnected`, because accepting a command we cannot deliver is a lie. On
reconnect the sampler rebuilds the connection and bumps `sessionRevision`.

*Rejected:* erroring every read while disconnected (turns a brief sim hiccup into a broadcast
outage, and the client cannot distinguish "gone" from "gone for 40 ms").

### 7. The metadata types live in `mcp-core`; the sampler is iRacing-only for now

`SnapshotMeta` and the envelope change go in `mcp-core` so `lmu-mcp` populates the same block from
whatever it has — `capturedAtUnixMs` and its own session identity — leaving `sessionTick` null. LMU
keeps its existing per-call access path; no LMU sampler is built here, and none is implied. This
keeps ADR 0001's shared tool surface honest without pretending the two simulators expose comparable
telemetry access (ADR 0002), and it does not touch the single-active-simulator constraint
(ADR 0003).

*Rejected:* an iRacing-private metadata shape (clients would need two parsers for one field block);
building an LMU sampler at the same time (no measured need, and LMU's access model is different
enough that the abstraction would be guesswork).

## Consequences

- Read latency becomes serialization cost; the mapping/parse cost moves off the request path
  entirely and is paid once per sim tick regardless of how many tools are called.
- Reads taken within one cycle are coherent, and clients can prove it from `sessionTick`.
- `sandbox` can delete its session fingerprint, its 20 s staleness guess, and eventually its
  in-world heuristic; until it does, everything above is additive and it keeps working unchanged.
- One always-warm SDK connection replaces per-call connections, so the server holds simulator
  resources for its whole lifetime rather than in bursts.
- `get_capabilities` can report live mode (driving vs. not) from the snapshot instead of guessing
  statically, without becoming an SDK read of its own.
- The sampler is a new failure domain: if its thread dies, every read serves an ever-staler
  snapshot. It must log loudly and mark `connected: false` rather than exit silently.

## Open follow-ups

- A composite `get_broadcast_snapshot` tool serving several sections from one snapshot tick.
- Server-pushed notifications (session change, pit transitions, incidents) over the ADR 0004 SSE
  stream, replacing client-side event derivation.
- `trackSurface`/`inWorld` on `RelativeEntry`, sourced from the same snapshot.
