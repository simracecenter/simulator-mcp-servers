# ADR 0008: Director Telemetry Publisher Role

- Status: Proposed
- Date: 2026-09-10
- Engineering issue: https://github.com/simracecenter/simulator-mcp-servers/issues/57

## Context

Director needs to manage the telemetry publisher on a Rig over MCP. The
publisher is a separate Windows artifact from
`margic/director-narrative-core`, bundled next to the launcher by the
installer.

## Proposed Decision

`publisher` is a third exclusive launcher role behind the same single
`SwappableHandler`. ADR 0003 D1 is preserved: one role runs per host,
publisher is not a simulator, and it never runs alongside one. The handler
supervises one external `publisher.exe`; its path can only be overridden by
`config.toml` under `[publisher] exe_path`.

The publisher receives configuration through environment variables, never
argv. The ingest URL and certificate fingerprint live in `[publisher]`; the
token is stored in the Windows current-user DPAPI as `publisher.token`.
There is no auto-restart. Switching roles stops the child.

The mutating tools `publisher_configure`, `publisher_start`, and
`publisher_stop` are gated by ADR 0007's exact-name allowlists, exactly like
camera tools.

## Limits

This slice adds no TLS, pairing, installer, autostart, heartbeat, or version
protocol. The default listener is unchanged.

## References

- [ADR 0003](0003-single-active-simulator-constraint.md)
- [ADR 0007](0007-protected-http-transport.md)
