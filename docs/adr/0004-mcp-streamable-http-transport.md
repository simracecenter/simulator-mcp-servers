# 0004 — MCP Streamable HTTP Transport (Hand-Rolled, Session-Scoped)

- **Status:** Accepted
- **Date:** 2026-07-31
- **Deciders:** Sim RaceCenter maintainers

## Context

`crates/mcp-core`'s HTTP transport was a plain JSON-RPC-over-`POST` endpoint: `POST /mcp` handled
every request and nothing else on that path was routed. That is enough for a client that only ever
POSTs, and it is how every tool in `iracing-mcp` and `lmu-mcp` was verified.

It is *not* the MCP **Streamable HTTP** transport (spec revisions 2025-03-26 / 2025-06-18), which
also requires a server-to-client `GET` SSE stream, a session identifier, and a session-teardown
verb. Real clients depend on all three. `mcp`'s `streamable_http` client (used by `google-adk`, and
therefore by the Sim RaceCenter Broadcast Agent) opens `GET <url>` with
`Accept: text/event-stream` immediately after `initialize`; our `405` made it declare
`Session terminated`, so the auto-broadcast agent executed exactly one cycle and then stalled —
[issue #34](https://github.com/simracecenter/simulator-mcp-servers/issues/34). The tools themselves
were fine: a direct `camera_focus` POST verified in ~52 ms.

## Decision

### 1. Implement Streamable HTTP by hand in `mcp-core`, rather than adopting an MCP SDK

`rmcp` (the official Rust SDK) would replace our `McpHandler` trait, JSON-RPC types, tool
registration, and both transports at once — a workspace-wide rewrite touching every adapter's public
contract (ADR 0001, ADR 0002) to fix a transport gap. We keep the hand-rolled layer and extend it;
the transport surface is small (three verbs, one header) and fully covered by unit tests.

*Rejected:* depending on `rmcp`; keeping POST-only and asking clients to use the deprecated
HTTP+SSE transport (ADK does not offer that choice).

### 2. All three verbs live on the single `/mcp` path

`POST` (client→server JSON-RPC), `GET` (server→client SSE stream), `DELETE` (session teardown), plus
the unrelated `GET /healthz`. Streamable HTTP mandates one endpoint; splitting the stream onto a
second path would break spec-compliant clients.

### 3. Sessions are in-process, opaque, and non-resumable across restarts

`initialize` mints a UUID and returns it as `Mcp-Session-Id`; the transport keeps a
`SessionRegistry` (`transport/session.rs`) mapping that id to a bounded mpsc queue of
server-to-client messages, drained by the session's SSE stream. An unknown session id on `POST`,
`GET`, or `DELETE` yields `404`, which is the client's signal to re-`initialize` rather than retry
forever.

No persistence and no `Last-Event-ID` replay buffer: a Rig runs exactly one simulator MCP server
(ADR 0003) with one Broadcast Agent, and restarting the server restarts the simulator session
anyway, so a re-`initialize` is both cheap and correct.

*Rejected:* a signed/stateless session token (nothing to protect — the transport is unauthenticated
by design, see SECURITY.md); durable session storage (no value for a single-rig, single-agent
deployment).

### 4. The stream exists to hold the session open; server-initiated messages are a seam

Nothing in `iracing-mcp` or `lmu-mcp` pushes notifications today. The registry still exposes a
`sender()` so future work (session/telemetry events, progress notifications) is a handler change,
not another transport change. Streams get a 15 s keep-alive so idle sessions survive proxy and
client read timeouts, and a second concurrent `GET` for one session is refused with `409` rather
than silently splitting the message flow.

### 5. Notifications get `202 Accepted`

A JSON-RPC message without an `id` (e.g. `notifications/initialized`) is dispatched to the handler
but answered with an empty `202`, per spec, instead of the previous `200` envelope with `id: null`.
Malformed bodies keep returning a `-32700` envelope with `200` (unchanged, and deliberately unlike
axum's opaque `400`).

## Consequences

- ADK/`mcp` clients keep one MCP session across broadcast cycles; issue #34's repro
  (`live_broadcast_smoke.py --cycles 3`) no longer dies after cycle 1.
- Every `<sim>-mcp` crate inherits the fix — the change is entirely in `mcp-core`; no adapter, tool,
  or launcher code changed.
- Sessions accumulate if a client neither `DELETE`s nor disconnects cleanly. Each is a small map
  entry plus a 64-slot queue, and the process is a single-user Rig binary, so this is accepted for
  now; if it ever matters, evict on `last_seen`.
- The stdio transport is unaffected and remains the local-child-process option.

## Open follow-ups

- Push real server-initiated notifications (session change, flag events) over the new stream instead
  of leaving clients to poll.
