# ADR 0008: Director Telemetry Publisher Role

- Status: Proposed
- Date: 2026-09-10
- Engineering issues:
  - https://github.com/simracecenter/simulator-mcp-servers/issues/57
  - https://github.com/simracecenter/simulator-mcp-servers/issues/59
  - https://github.com/simracecenter/simulator-mcp-servers/issues/69
  - https://github.com/simracecenter/simulator-mcp-servers/issues/75

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
Director replaces its previous grant; the local Clear Directors action revokes
all grants. Pairing establishes trust only: the Director that sends
`publisher_start` first sends `publisher_configure` with its own ingest
destination. Grants remain in launcher configuration while another exclusive
role is active, so returning the Rig to `publisher` does not require pairing
again. The five-strike lockout, publisher-only grant scope, digest-only
credential persistence, and revocation protect the exception to ADR 0007's
rule that credential issuance is a Rust API rather than an unauthenticated
network endpoint.

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

## Limits

Installer, autostart, and wheel controls remain out of scope. The default
listener remains unchanged for simulator roles.

## References

- [ADR 0003](0003-single-active-simulator-constraint.md)
- [ADR 0007](0007-protected-http-transport.md)
