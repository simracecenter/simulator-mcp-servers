# ADR 0008: Director Telemetry Publisher Role

- Status: Proposed
- Date: 2026-09-10
- Engineering issues:
  - https://github.com/simracecenter/simulator-mcp-servers/issues/57
  - https://github.com/simracecenter/simulator-mcp-servers/issues/59
  - https://github.com/simracecenter/simulator-mcp-servers/issues/69
  - https://github.com/simracecenter/simulator-mcp-servers/issues/75
  - https://github.com/simracecenter/simulator-mcp-servers/issues/103

## Context

Director needs to manage the telemetry publisher on a Rig over MCP. The
publisher library is imported from
`margic/director-narrative-core`, branch
`devin/1789000598-local-destination`, PR [#81](https://github.com/margic/director-narrative-core/pull/81),
at source SHA `f88ee0d7bb100fdbd141d19b00049b12ad4dac07`.

## Proposed Decision

`publisher` is a third exclusive launcher role behind the same single
`SwappableHandler`. ADR 0003 D1 is preserved: one role runs per host,
publisher is not a simulator, and it never runs alongside one. The role
handler owns one in-process publisher engine on a dedicated
`publisher-engine` thread.

The ingest URL, certificate fingerprint, and driver display name live in
`[publisher]`. The token is stored in the Windows current-user DPAPI as
`publisher.token` and held only in memory by the engine configuration; it is
never passed through argv, environment variables, logs, status, or debug
output. Stopping requests shutdown, waits up to five seconds for the engine
thread, and reports an unknown state if it does not join in that window.
There is no auto-restart. Switching roles stops the engine.

The mutating tools `publisher_configure`, `publisher_start`, and
`publisher_stop` are gated by ADR 0007's exact-name allowlists, exactly like
camera tools.

## Pairing and rig TLS

The Director pairing contract is defined by
`simracecenter/director` PR #9,
[`docs/12-rig-pairing.md`](https://github.com/simracecenter/director/blob/main/docs/12-rig-pairing.md).
The publisher listener originally served HTTPS with a self-signed certificate
persisted per Rig, with Director pinning its fingerprint (TOFU, Director ADR
0014). Since the 2026-09-22 amendment below the listener is plaintext HTTP; the
persisted certificate remains only as the source of the Rig fingerprint shown
in the Console and used to key grants. `POST /pair` mints a
persistent credential after validating the one-shot three-digit code. A Rig
may retain independently revocable grants for multiple Directors, identified
by their normalized ingest certificate fingerprints. Re-pairing the same
Director replaces its previous grant; the Console lists each grant with its
own Remove action (Clear Directors revokes all), and shows the current ingest
destination, resolved to the paired Director by ingest fingerprint. Pairing
establishes trust only: the Director that sends `publisher_start` first sends
`publisher_configure` with its own ingest destination. Grants remain in
launcher configuration while another exclusive role is active, so returning
the Rig to `publisher` does not require pairing again. The five-strike lockout,
publisher-only grant scope, digest-only credential persistence, and revocation
protect the exception to ADR 0007's rule that credential issuance is a Rust API
rather than an unauthenticated network endpoint.

## Plaintext publisher control (2026-09-22 amendment)

Director's `publisher_stop` failed with OpenSSL `WRONG_VERSION_NUMBER` once
Rigs answered plaintext on 8765 while Director still pinned HTTPS. Publisher
control is now plaintext HTTP end to end: `bind_rustls` is replaced by
`axum_server::bind` for the publisher role, `/pair` and `/mcp` keep the bearer
credential, Origin and session checks, and the LAN-only deployment rule of
ADR 0007 is the transport boundary. Director-side pairing no longer discovers
or stores a Rig certificate pin (`simracecenter/director` #67). The
Director ingest destination handed over in `/pair` must still be `https://`;
that check is unchanged.

## Durable ingest delivery (2026-09-12 amendment)

The original design queued events in memory on the 60 Hz sampling thread and
performed HTTP POSTs, token refreshes, and retry sleeps inline on that loop.
It also dropped the queue on failure and kept nothing across a process kill,
so batches in flight at an abrupt exit were silently lost.

The transport is now split. `PublisherTransport` performs one POST at a time
and owns no buffer. A `DeliveryService` worker thread owns the transport and
an on-disk `Outbox` under the publisher data directory. The sampling loop
only pushes into a bounded in-memory queue (oldest event dropped and counted
when full). The worker persists each batch to `outbox/batch-<seq>.json`
before posting, deletes the file only after a 2xx acknowledgement, and
re-delivers recovered files in order on the next start; the receiver
deduplicates re-deliveries by event `id`. The outbox is bounded
(`512` pending batches ≈ 10 000 events): at the bound the oldest file is
dropped and counted, and a corrupt file is quarantined to `*.json.corrupt`
rather than stalling later deliveries. A terminal 4xx from the receiver
(schema, oversize — everything but 408/429) is a definitive rejection: the
file is quarantined and its events counted `events_rejected_total` instead of
retried forever. `accepted`/`rejected`/`duplicate`
receipt fields and `events_lost_total`/`outbox_pending_batches` are surfaced
through `status.json` and the `publisher_status` MCP snapshot, so loss and
backlog are visible to operators instead of silent.

Shutdown persists whatever is still queued, then drains the outbox on a
bounded best-effort pass (single-attempt posts, ~4 s budget); unacknowledged
files wait for the next launch.

## Wheel controls (2026-10-10 amendment)

The publisher owns learned wheel-button bindings for `focus_me`, `broadcast_toggle`,
`approve_yes`, and `approve_no`. Approval answers share a 2,000 ms cooldown and are emitted as
the car-scoped `DRIVER_APPROVAL_ANSWERED` event with the requesting car, driver identity, request
identity, and `answer` (`yes` or `no`). The launcher starts the Raw Input listener for the whole
Publisher role. Bindings are learned in the settings page's WHEEL BUTTONS section and stored in
`%APPDATA%\SimRaceCenter\controls.toml`. Presses reach the in-process engine through `ControlRelay`
and are dropped while the engine is stopped. Installer and autostart remain out of scope, and the
default listener remains unchanged for simulator roles.

## Run-story fields (2026-11-17 amendment)

A review of a real multi-class run found three gaps at the source (issue #103).

**Class position.** The publisher reads `PlayerCarClassPosition` and
`CarIdxClassPosition` (both optional) and adds a class position to every event
that already carried an overall one: `class_position` on `LAP_COMPLETED`,
`PIT_ENTRY`, `PIT_EXIT` and `DRIVER_MATERIAL`; `class_position_from` /
`class_position_to` on `OVERTAKE` and `OVERTAKE_FOR_LEAD`; `car_class_position`
(the car `car_race_position` describes) plus `leaderClassPosition` /
`followerClassPosition` on the three `BATTLE_*` events. A class key is
**omitted**, never `null`, when the SDK has no value. Overall-position keys keep
their names and meaning.

**Lap counter.** `lap` is `u32` from the SDK read to the wire. It was `u8`,
produced by a truncating cast, so lap 256 read as lap 0: the rig's own lap
processing stopped for that lap, `LAP_COMPLETED` for lap 255 was replaced by
one for lap 0, and later laps collided with per-lap state from the start of the
race. On the wire `lap` is a JSON number as before.

**`CAR_FINISHED`.** `RACE_CHECKERED` marks the flag falling for the overall
leader; the rest of the field is still racing. `CAR_FINISHED` (car-scoped,
subject role `DRIVER`) is emitted when the rig's own car takes the flag: its
completed-lap counter advances while the flag is out, or it leads overall and
crossed the line within one second of the flag falling. It carries `position`,
`class_position`, `laps_completed`, `best_lap_time_s`, `last_lap_time_s`,
`incident_count` (`PlayerCarMyIncidentCount`) and, when the SDK exposes them,
`driver_incident_count` and `team_incident_count`. It is emitted at most once
per `SessionNum`, in any kind of session that goes checkered (the envelope's
`context.sessionType` says which), and never when the publisher connected with
the flag already out or the car does not reach the line.

### Payload contract versioning

`PAYLOAD_CONTRACT_VERSION` stays at `3`. The rule, written down here because
it was implicit until now:

- The version moves when an existing key changes meaning or shape, or when a
  consumer must be able to gate on a set of keys being present on every event
  of a type (revisions 2 and 3 were both of that kind).
- It does **not** move for optional keys that are omitted when there is no
  value, or for a new event type. A version cannot promise a key that is
  sometimes absent, and a new type is announced by
  `contracts/publisher-event-catalog.json`, which exists for that purpose.

A bump is also not free: Director's ingest schema accepts an enumerated set of
contract versions, so a publisher that moved to `4` for an additive change
would have every batch rejected by every Director already deployed.

### Rollout order

A consumer validates `type` against its own copy of the catalog. Director must
take the regenerated catalog (and decide what to do with `CAR_FINISHED`)
**before** a publisher carrying this change is released to a Rig: until then a
Director rejects the batch that contains a `CAR_FINISHED`, and the publisher
treats that 4xx as terminal and quarantines the batch (see "Durable ingest
delivery" above), losing the events that shared it.

## Incident facts (2026-11-19 amendment)

Director ranks incidents for replay and needs to know how hard each car hit
and what became of it. The facts are measured here, at the source, and sent as
optional keys and one new event. Nothing in this amendment changes a key that
was already on the wire.

### Cluster facts on `INCIDENT_CLUSTER`

A cluster is corroborated by one or more hard events, and each hard event is
an `INCIDENT_ALERT` the publisher already sent. The cluster now repeats what
those alerts measured, so a consumer does not have to find and join them.

| Key | Value |
| --- | --- |
| `hardEvents[]` | One entry per corroborating hard event. Already carried `carIdx`, `sessionTime`, `lapDistPct` and `reason`; gains the three keys below. |
| `hardEvents[].car` | Full car reference, shaped as the entries of `involvedCars`. A car the roster cannot resolve gets the same index-only stub as everywhere else. |
| `hardEvents[].speedDropMps` | The alert's own `speedDropMps`. |
| `hardEvents[].severityNormalized` | The alert's own `severityNormalized`, 0 to 1. |
| `impactNormalized` | 0 to 1: the largest `severityNormalized` in `hardEvents`. `0` when the list is empty. |
| `corroboration` | `slowed_and_hard_event` or `multiple_hard_events` (unchanged). |
| `detectedSessionTime` | When the detector fired (unchanged). `onsetSessionTime` / `sessionTime` stay the earliest hard event and the replay seek point. |

**Why `impactNormalized` and not `severityNormalized`.** A cluster already
sends `severity` (the number of cars involved) and `severityNormalized` (that
count mapped onto 0 to 1, with a floor of 0.5 for a cluster made of two hard
events). Every Director already deployed reads `severityNormalized` on a
cluster and raises the event to `emergency` at 0.5 or more. A hard speed drop
is 0.5 or more by construction, so putting the impact under that key would
change what airs on a Director that was never updated, and removing the key
would change it the other way. Both keys therefore stay exactly as they were,
meaning how much of the field is caught up, and the violence of the incident
goes under a new key that changes nothing until a consumer reads it. A contract
test pins the cluster's `severityNormalized` to the car-count value.

### `INCIDENT_OUTCOME`

An alert says a car hit something. It cannot say whether the car drove on or
was finished, because that is not known yet. `INCIDENT_OUTCOME` (car-scoped,
subject role `INCIDENT`, the envelope `car` being the car it is about) is sent
once per car per incident when it is known.

The publisher opens a watch on a car at each hard event, that is, each
`INCIDENT_ALERT`. A second hard event for the same car while its watch is open
extends the watch and does not open another, so a car that is hit twice gets
one outcome. On each incident cadence (`incident_cluster_interval_ms`) the
watch records what the car is doing. It closes 20 s after the latest hard
event, and never later than 60 s after the first, or at once when the car
leaves the world (`CarIdxTrackSurface` −1: tow, retirement or disconnect). No
watch is opened for a car on pit road or out of the world. Watches are dropped
on a session clock restart and on a session transition, as the other
detectors' state is, so an incident in the last seconds of a session may have
no outcome.

| Key | Value |
| --- | --- |
| `carIdx` | The car. |
| `onsetSessionTime` | The first hard event's time. Equal to the `sessionTime` of that car's `INCIDENT_ALERT`; with `carIdx` it is the join key for a single-car incident. |
| `sessionTime` | When the outcome was emitted. |
| `incidentKey` | The `incidentKey` of the cluster the car's incident belongs to. Omitted for a single-car incident. |
| `windowSeconds` | How long the car was watched: 20, more if extended, less if it left the world. |
| `stoppedSeconds` | Time in the world, off pit road and under 5 m/s. |
| `offTrackSeconds` | Time on the off-track surface (`CarIdxTrackSurface` 0). |
| `leftWorld` | The car left the world inside the window. |
| `pitted` | The car was on pit road at some point inside the window. |
| `wentBackwards` | The car's lap distance ran backwards by 5 m or more in total, off pit road. |
| `positionFrom`, `positionTo`, `placesLost` | Overall position at the onset and at the end of the window (the last one seen while the car was in the world), and the difference, never below 0. All three are omitted when the car had no position at the onset. |
| `classPositionFrom`, `classPositionTo` | The same in class. Each is omitted when the SDK has no value. |

Optional keys are omitted, never `null`. As on every event, each snake_case
field also appears under its original name for the transition window.

A watch takes the key of a cluster when the cluster detector, on a cadence
while the watch is open, has the car among a live cluster's cars. A car that
left the world before the cluster was raised has already reported, without
`incidentKey`; it joins by `carIdx` and `onsetSessionTime`, which match its
entry in the cluster's `hardEvents`.

**Limits.**

- Speed is known directly only for the rig's own car. For every other car it
  is derived from lap distance, so "stopped" means not moving along the track:
  a car spinning on the spot, or sliding sideways across the circuit, reads as
  stopped.
- The same derivation makes `wentBackwards` blind across the start/finish
  line: a fall in lap distance of more than half a lap is taken to be the
  line, not a car reversing.
- `positionTo` and `placesLost` are only as fresh as iRacing's positions, and
  the probe showed they are usually stale 20 s after an incident (see "Probe
  findings" below). Treat `placesLost` as a lower bound.
- Samples are taken on the incident cadence (250 ms by default), so the
  durations are accurate to about that.

### Live probe

Three later decisions rest on iRacing behaviour that cannot be checked from
code: whether `CarIdxSessionFlags` is filled for cars other than the rig's,
whether other cars' incident totals are visible while a race is running, and
whether `CarIdxPosition` moves within seconds of a car stopping or only at the
next timing line. The publisher carries a diagnostic to settle them from one
real session (`src/probe.rs`).

It is off unless the environment variable `SRC_PUBLISHER_PROBE` is `1`. Off, it
costs one branch per frame and reads nothing extra. On, it only writes lines
to the publisher log, prefixed `[probe]`; it publishes nothing and changes no
event. The lines carry car indices and counts, never driver names.

- **On connect**, for each of `CarIdxSessionFlags`,
  `CarIdxTrackSurfaceMaterial`, `LatAccel`, `LongAccel` and `VertAccel`:
  whether the SDK variable header has it, its type and its count.
- **At each hard event**, and again 5 s and 20 s later: that car's
  `CarIdxSessionFlags` (hex) and `CarIdxTrackSurfaceMaterial`, or `absent`,
  and its `CarIdxPosition`.
- **Each time the session results change**, per session in the YAML's
  `ResultsPositions`: how many cars are listed, how many have a non-zero
  `Incidents`, the total, and each distinct `ReasonOutStr` with its count.

The two per-car arrays are read straight from the telemetry buffer, only when
a sample is due, and apart from the frame the engine uses: a variable that is
missing, of another type or out of bounds reads as absent and cannot cost a
frame.

The probe has to be run in an official race: other drivers' incident counts
may be hidden there and visible in a test session. Its findings are below.

### Probe findings (2026-11-21)

One official IMSA race weekend on the Rig with launcher v0.8.17 and
`SRC_PUBLISHER_PROBE=1`: qualifying and two races of 38 cars (Director run
`run-1795268965`). Since v0.8.17 the probe's lines are in the launcher log.
The probe took 967 samples at 327 hard events, of which 313 have all three
samples.

**The variables.** All five are in the SDK header on every connect:
`CarIdxSessionFlags` (bitfield, 72), `CarIdxTrackSurfaceMaterial` (int, 72),
and `LatAccel`, `LongAccel`, `VertAccel` (float, 1).

**`CarIdxSessionFlags` is filled for every car**, not only the rig's. The bit
names below are those of `irsdk_Flags` in `irsdk_defines.h`; P-3 takes the
values from the header, not from this table.

| Value seen | Bits | Where |
| --- | --- | --- |
| `0x00040000` | servicible | The normal state: 275 of 327 events hold it at all three samples |
| `0x00050000` | servicible, black | 12 events, already set at the hard event: a car serving a penalty, not a car that crashed |
| `0x000c0000` | servicible, furled | Set at the hard event in 5 events, and raised 5 to 20 s after it in 12 more: the warning flag |
| `0x00150000` | servicible, black, repair | One car, set 5 to 20 s after a speed drop: the meatball |
| `0x00060000` | servicible, disqualify | One car, in the same second as `ReasonOut` `DQ` appeared in the session results |
| `0x00000000` | none | 6 samples, at 5 or 20 s: the car had left |

So P-3 goes ahead, with three bits rather than two: `repairFlag`,
`blackFlag` and `disqualified`. A black flag that is already set when the hard
event fires marks a penalty slowdown, and the outcome should say so
(`blackFlagAtOnset`), because such a car slows on purpose.

**`CarIdxTrackSurfaceMaterial` is filled for other cars.** At the hard event:
asphalt 201, grass 76, gravel 22, painted lines 16, rumble strips 6, dirt 3,
concrete 3. At 5 s and 20 s, about a quarter of the cars read −1 (not in the
world: towed or reset). Grass and gravel can therefore be told apart from a
paved run-off. No package uses it yet.

**Other cars' incident totals are hidden during an official race.**
`ResultsPositions` listed every car with `Incidents` 0 from the green flag to
the checkered flag of race 1. At the checkered flag all 38 cars changed at once
(323 points in total). Qualifying totals appeared only when qualifying ended.
`ReasonOut` is live: the disqualification above appeared at once. So incident
points cannot score other cars' incidents while a race runs. The rig car's own
count (`incident_count_increase`, 19 alerts) remains the only live source.

**`CarIdxPosition` is usually stale 20 s after an incident.** It had changed at
5 s in 18 of 313 events and at 20 s in 88. The data cannot tell whether
positions move only at timing lines or simply move slowly. Either way the
20-second window sees only part of the places lost. `placesLost` stays a lower
bound. The window is not lengthened, because the outcome would arrive too late
to rank a replay.

**Impact is measured at the threshold, not at the bottom of the drop.** Not a
probe question, but the same race shows it. A `speed_drop` alert fires on the
first frame where speed falls to `HARD_DROP_RATIO` (0.5) of the recent peak, so
its `severityNormalized` is about 0.5 by construction: 150 of 326 alerts read
0.50, and 34 read 0.51. Clusters inherit it: 112 of 151 have
`impactNormalized` 0.5. The value does not separate a hard crash from a lift.
The measure has to come from the outcome window, which already watches the car
for 20 s (P-5 in Director's docs/26).

The `placesLost` limit above is now settled. Per-car flags (P-3) and the peak
speed loss (P-5) follow below. Surface material and incident totals stay unused
until a use is shown.

### Per-car flags and peak speed loss on `INCIDENT_OUTCOME` (v0.8.18)

These follow from the findings above. Every key is optional and omitted when
there is no value.

| Key | Value |
| --- | --- |
| `peakSpeedLossNormalized` | 0 to 1: `(peak − lowest) / peak`. The peak is the alert's `previous_speed_mps`, the speed before the drop. The lowest is the lowest speed in the first 3 s after the first hard event, in the world and off pit road, starting from the onset frame. 1.0 when the car left the world inside those 3 s. Omitted when the car was doing under 1 m/s before the hard event. |
| `lowestSpeedMps` | That lowest speed. 0 when the car left the world inside the 3 s. Omitted with the key above. |
| `repairFlag` | The repair bit (`0x00100000`) was set at any sample in the window. |
| `blackFlag` | The black bit (`0x00010000`) was set at any sample in the window. |
| `blackFlagAtOnset` | The black bit was already set on the hard event's frame: a car slowing to serve a penalty. |
| `disqualified` | The disqualify bit (`0x00020000`) was set at any sample in the window. |

The four flag keys are omitted when the SDK does not expose
`CarIdxSessionFlags`. The frame now carries the variable as
`car_idx_session_flags`, read when its type is bitfield or int.

`INCIDENT_ALERT` gains `blackFlag`: the black bit on the frame the alert
fired, omitted without per-car flags. A consumer can then tell a penalty
slowdown when the alert arrives, without waiting 20 s for the outcome's
`blackFlagAtOnset`. A strong alert may otherwise air before its outcome. The
engine sets the key, because the detector has no frame.

**Why the outcome and not the alert.** The alert has to go out on the frame the
drop crosses its threshold, or the replay seek point is lost. So its
`severityNormalized` stays the value at the threshold, and the cluster's
`impactNormalized` stays the largest of those. A consumer that wants the size
of the hit reads `peakSpeedLossNormalized` when the outcome arrives. The 3 s
window ends long before the outcome's 20 s, so the key costs no delay.

**Limits.** For every car but the rig's, speed is derived from lap distance, as
for `stoppedSeconds`. A car that spins on the spot therefore reads as a full
loss, even when it drives away undamaged.

### Payload contract version

`PAYLOAD_CONTRACT_VERSION` stays at `3`, by the rule under "Payload contract
versioning" above: every key added here is optional for the consumer, no
existing key changes meaning or shape, and the new event type is announced
through `contracts/publisher-event-catalog.json`.

### Rollout order

As for `CAR_FINISHED`: Director must take the regenerated catalog, with
`INCIDENT_OUTCOME` in it, **before** a publisher that emits the event is
installed on a Rig. Until then a Director rejects every batch that contains an
`INCIDENT_OUTCOME`, the publisher treats that 4xx as terminal and quarantines
the batch, and the events that shared the batch are lost with it. The cluster
keys above need no ordering: a Director that does not know them ignores them.

## Field-wide pit events (v0.8.19)

Stream wFDmf2QUKOY (run-1795474273) aired pit stops as incident replays. In
iRacing the pit-entry approach is its own track surface, `AproachingPits` (2),
and `on_pit_road` is still false there, so the pit-entry braking halved a car's
speed and read as a `speed_drop`. 38 of the run's 146 `INCIDENT_ALERT`s were
this, all at lapDistPct ≈ 0.94 with surface 2 → 2 and severity ≈ 0.50, and
about 30 of its 68 `INCIDENT_CLUSTER`s were built from them. In the same run
`PIT_ENTRY` / `PIT_EXIT` fired only for the rig's own car, so the Director had
no pit story for the rest of the field.

### Incident detectors ignore the pit area

"In the pit area" means `on_pit_road`, surface `InPitStall` (1) or surface
`AproachingPits` (2). The basic incident detector resets a car's speed window
there, raises no `speed_drop` / `surface_drop` alert when either side of the
comparison is in the pit area, does not arm the off-track check from a pit-area
surface, and drops incident points gained there. The cluster detector neither
counts pit-area cars as slowed nor feeds their speed into the bucket baseline.
The outcome tracker opens no watch for a car in the pit area and marks a watch
`pitted` when its car reaches one.

### Pit events for every car

`PIT_ENTRY` and `PIT_EXIT` now fire for every car. No event type is added, so
the event catalog is unchanged. A per-car tracker reads the surface, pit flag,
position and class position straight from the frame arrays, because the car
registry skips cars reporting position 0, as a car in its pit stall does.

- **Entry** fires on the first of: surface `OnTrack` (3) → `AproachingPits` (2)
  while moving (≥ 5 m/s), or `on_pit_road` rising. **`PIT_ENTRY` therefore now
  fires at the pit-approach braking**, a few seconds earlier than before. An
  approach the car drives back out of (back on track for 0.5 s without
  reaching pit road) is dropped silently: the entry already went out, and the
  case is rare enough to accept.
- **Exit** fires when `on_pit_road` has fallen and stayed false for 0.5 s, so a
  one-frame flicker at the pit-lane line is not two stops.
- A car first seen already on pit road (out of the garage, or a stop under way
  at connect) gets no entry. Its exit carries `from_garage: true`. This also
  applies to the rig: a publisher that starts in the pit stall no longer sends
  a `PIT_ENTRY` for it.
- A clock restart clears every car's state, and surface `NotInWorld` (-1)
  clears that car's state, without an event.

Payload keys (snake_case, with the usual camelCase copies):

| Event | Key | Value |
| --- | --- | --- |
| both | `car_idx` | The pitting car. The envelope `car` and `subject` are this car. |
| both | `player_car_idx` | The rig's car index, **only** when the pitting car is the rig's. Omitted for any other car, so rig payloads keep their old shape and an older Director falls back to the envelope `car`. |
| both | `position`, `class_position` | The pitting car's, at the event. |
| `PIT_ENTRY` | `detected_by` | `approach` or `pit_road`. |
| `PIT_EXIT` | `stop_s` | Seconds stopped (below 1 m/s) in the pit stall. |
| `PIT_EXIT` | `pit_lane_s` | Seconds on pit road. Omitted when the car was first seen on pit road. |
| `PIT_EXIT` | `drive_through` | The entry was seen and `stop_s` is 0. |
| `PIT_EXIT` | `from_garage` | No entry was seen. |

Subject role: `DRIVER` for the rig's own car, `TRIGGER` for any other car. No
role is added.

`PAYLOAD_CONTRACT_VERSION` stays at `3`: every new key is optional and
`player_car_idx` keeps its meaning where present. No rollout order is needed.
A Director that does not know field pit events sees more `PIT_ENTRY` /
`PIT_EXIT` events, keyed on the envelope car.

## References

- [ADR 0003](0003-single-active-simulator-constraint.md)
- [ADR 0007](0007-protected-http-transport.md)
