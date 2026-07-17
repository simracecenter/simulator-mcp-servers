# ADR 0003: Single Active Simulator Constraint — No Multi-Instance Support

- **Status:** Accepted
- **Date:** 2026-07-16
- **Deciders:** repo owner (margic), simracecenter org

## Context

[ADR 0001 D2](0001-project-layout.md#d2--launcher-owns-process-lifecycle-ui-is-a-swappable-adapter-behind-a-port)
already decided the launcher hosts "at most one active simulator's MCP server" and D3 added a
singleton OS lock so a second launch of the *same* sim fails fast. In practice, though, this
constraint keeps needing to be re-explained/re-discovered: as more `<sim>-mcp` crates land (LMU in
ADR 0002, more to come), it's a recurring temptation — for a human or an agent working a feature —
to add "just in case" abstractions for running several simulator servers concurrently: a
multi-handler registry, per-sim dynamic port/endpoint allocation, a tool-namespacing scheme to
merge multiple sims' tool lists behind one transport, a pool of adapters keyed by sim name, etc.

This ADR exists to state, as a **first-class, standalone decision** (not a side effect buried in
ADR 0001), exactly what this repo's MCP servers are *for*, and why concurrent multi-sim support is
explicitly out of scope — so that temptation has a clear, citable answer instead of being
re-litigated (or silently designed around) on every new feature.

### What this project's MCP servers are for

Each `<sim>-mcp` crate exposes one racing simulator's live telemetry (session/standings/weather/
pit state, etc.) and a narrow set of verified control actions (camera focus, replay seek/playback,
pit commands, weather) to an MCP client — an AI agent such as **Race Control** — over stdio or
HTTP, per [ADR 0001](0001-project-layout.md). The whole point is to let that agent *reason about
and act on whatever simulator the user is actually running right now*, the same way the user
themselves is limited to driving/watching one simulator at a time on that machine.

### Why concurrent multi-sim support has no real use case here

1. **A racing rig runs one simulator at a time, by construction.** iRacing and LMU (and any future
   sim) are full-screen/VR, GPU- and input-device-exclusive applications. A single physical rig —
   one GPU, one set of wheel/pedals/VR headset, one set of driver attention — cannot be "driving"
   two simulators simultaneously. There is no user story where an agent needs live telemetry from
   two different sims' MCP servers at the same wall-clock moment.
2. **Resource budget is already spoken for.** Per ADR 0001 constraint #3, this software's entire
   resource budget is "whatever the sim isn't using" — these are gaming rigs, often mid-race, often
   in VR. Every additional concurrently-running adapter means another live telemetry-polling loop
   (5–50 Hz shared-memory/HTTP reads per ADR 0002), another transport listener, another set of
   in-memory buffers — all for a server whose sim isn't even running. That's pure waste, not
   headroom held in reserve for a plausible future need.
3. **It would multiply implementation cost for zero product value.** Supporting N concurrent
   servers means solving problems this project does not otherwise have: routing an MCP client's
   `tools/call` to the right sim's handler (name collisions between e.g. two sims both exposing
   `get_session_overview`), merging `get_capabilities` output across sims, per-sim
   port/endpoint allocation and discovery, and lifecycle/crash-isolation for each concurrently
   running adapter. None of that complexity buys the user anything, because they can't act on two
   sims at once anyway.

## Decision

### D1 — Exactly one simulator MCP server is active at any time; this is a hard constraint, not a v1 simplification

The launcher hosts **one** `<sim>-mcp` handler behind **one** transport listener at a time, chosen
by config default + `--sim` CLI override (already decided in ADR 0001 D2). This ADR upgrades that
from "current design" to an explicit, durable constraint: **do not build abstractions to support
running more than one simulator server concurrently** in this repo. That includes (non-exhaustive):

- A registry/pool/map of multiple live adapters or handlers keyed by sim name.
- Tool-name namespacing or routing logic to disambiguate which sim a `tools/call` targets.
- Merging multiple sims' `get_capabilities`/`tools/list` output into one response.
- Dynamic per-sim port allocation, service discovery, or a "list of running servers" endpoint.
- Spawning/supervising N child server processes instead of the current single-process runner.

If a concrete, evidenced use case for concurrent multi-sim support ever emerges (e.g., a future
product direction genuinely requires it), that is a decision significant enough to warrant
superseding this ADR explicitly — not something to route around piecemeal inside an unrelated
feature PR.

### D2 — Implementation choices this constraint licenses (and why)

Because "exactly one active sim" is a hard constraint and not a temporary limitation, the following
simplifications in the existing implementation are **intentional, not technical debt**:

- **Single `Arc<dyn McpHandler>` per transport, no dispatch table.** `mcp-core`'s transports
  (`transport/http.rs`, `transport/stdio.rs`) are wired to exactly one handler instance for the
  process lifetime. There is deliberately no concept in `mcp-core` of registering more than one
  handler — adding one would be solving a routing problem this project doesn't have.
- **One fixed listener/endpoint, not per-sim ports.** The HTTP transport binds a single
  configured address (`127.0.0.1:8765` by default, per the 2026-07-15 loopback-binding hardening);
  there's no per-sim port table or discovery mechanism to maintain.
- **Config selects *a* sim, not a set.** `launcher/src/config.rs`'s `Sim` selection
  (`--sim iracing|lmu`) is a single enum value, not a list — switching sims means restarting the
  launcher against a different config/flag, not toggling membership in a running set.
- **Singleton OS-level lock stays scoped to "don't double-launch the same sim" (ADR 0001 D3),** not
  generalized into a multi-instance coordinator. It exists to fail fast on an accidental double
  launch, not to arbitrate between multiple *different* concurrently-desired sims.
- **Each `<sim>-mcp` crate can reuse tool names freely** (`get_session_overview`, `camera_focus`,
  etc. mean "for whichever sim is currently hosted") without cross-sim collision handling, because
  only one crate's handler is ever live in a given process.

## Consequences

**Positive**
- Removes an entire class of speculative complexity (multi-handler routing, capability merging,
  per-sim service discovery) from the design space permanently, keeping `mcp-core` and the launcher
  small and easy to reason about, per ADR 0001's stated goals.
- Gives contributors and agents a citable answer when tempted to add "supports running multiple
  sims at once" as a generalization while building an unrelated feature: don't — see this ADR.
- Matches the actual, physically-constrained usage pattern (one rig, one sim, one driver) exactly,
  so no capability is being left on the table for real users.

**Negative / trade-offs**
- If a genuine future need for concurrent multi-sim support appears (unclear what that would be
  today), it requires a real design effort and an ADR superseding this one and touching ADR 0001
  D2/D3 — it is not a drop-in extension of the current shape.

## Open follow-ups

None. This ADR formalizes an existing constraint rather than opening new work; revisit only if a
concrete concurrent multi-sim use case is proposed, at which point supersede this ADR rather than
amending around it.
