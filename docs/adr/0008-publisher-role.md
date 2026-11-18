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

## References

- [ADR 0003](0003-single-active-simulator-constraint.md)
- [ADR 0007](0007-protected-http-transport.md)
