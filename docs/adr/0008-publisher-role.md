# ADR 0008: Director Telemetry Publisher Role

- Status: Proposed
- Date: 2026-09-10
- Engineering issues:
  - https://github.com/simracecenter/simulator-mcp-servers/issues/57
  - https://github.com/simracecenter/simulator-mcp-servers/issues/59

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
The publisher listener serves HTTPS with a self-signed certificate persisted
per Rig, and Director performs TOFU by pinning the certificate fingerprint
supplied during pairing (see Director ADR 0014). `POST /pair` mints one
persistent credential after validating the one-shot six-digit code. The
five-strike lockout, single-pairing conflict response, publisher-only grant
scope, digest-only credential persistence, and unpair revocation protect the
exception to ADR 0007's rule that credential issuance is a Rust API rather
than an unauthenticated network endpoint.

## Limits

Installer, autostart, and wheel controls remain out of scope. The default
listener remains unchanged for simulator roles.

## References

- [ADR 0003](0003-single-active-simulator-constraint.md)
- [ADR 0007](0007-protected-http-transport.md)
