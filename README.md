# Sim RaceCenter — Simulator MCP Servers

<p align="center">
  <img src="docs/assets/logo_black.png" alt="Sim RaceCenter logo" width="120">
</p>

[![Build status](https://img.shields.io/github/actions/workflow/status/simracecenter/simulator-mcp-servers/ci.yml?branch=main)](https://github.com/simracecenter/simulator-mcp-servers/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/simracecenter/simulator-mcp-servers?color=FF5F1F)](LICENSE)

**Open-source Race Control for AI-powered sim-racing broadcasts.**

[Sim RaceCenter](https://www.simracecenter.com/) is an ecosystem for turning live simulator
telemetry into cinematic race broadcasts. The hosted product brings together the Broadcast Agent,
Director Console, overlays, replay workflows, and race-intelligence services; this repository is
the open-source Race Control layer that lets those systems talk to racing simulators through
[Model Context Protocol (MCP)](https://modelcontextprotocol.io).

These MCP servers can run as part of the larger Sim RaceCenter platform, or standalone for anyone
building their own local tools, agents, automations, overlays, or broadcast workflows. Each
supported simulator exposes telemetry and control capabilities through a deterministic MCP API,
with verification loops for mutating commands where the simulator makes that possible.

Status: **early development**. Learn more about the product at
[simracecenter.com](https://www.simracecenter.com/). Design decisions live in
[docs/adr](docs/adr/) (start with [ADR 0001](docs/adr/0001-project-layout.md)) and the
[project board](https://github.com/orgs/simracecenter/projects/1).

## What's Here

```
simulator-mcp-servers/
├── crates/
│   ├── mcp-core/      # Shared MCP JSON-RPC layer, transports, verification-loop
│   │                  # helpers, and config load/merge — used by every simulator crate.
│   ├── iracing-mcp/   # iRacing telemetry + replay/camera control MCP server.
│   ├── lmu-mcp/       # Le Mans Ultimate MCP server.
│   └── launcher/      # The Director Console: CLI + singleton runner + settings web UI,
│                       # hosts exactly one active simulator MCP server at a time.
└── e2e/               # Playwright end-to-end tests for the launcher's settings UI.
```

## How It Fits

Sim RaceCenter has three layers:

- **Race Control** — this repository: local MCP servers, simulator adapters, verification logic,
  and the Windows Director Console that runs on the Rig.
- **Broadcast Agent** — the AI orchestrator that consumes Race Control tools and decides what the
  audience should see next.
- **Product experience** — the web, overlay, replay, and account surfaces described at
  [simracecenter.com](https://www.simracecenter.com/).

You do not need the hosted product to use this code. Run a simulator MCP server locally, connect an
MCP-capable client or agent, and build against the same tool surface that powers Sim RaceCenter.

## Getting Started

Pre-built Windows builds of the Director Console (`simracecenter-launcher.exe`) are attached to
each [GitHub Release](https://github.com/simracecenter/simulator-mcp-servers/releases). See
[CONTRIBUTING.md § Releasing](CONTRIBUTING.md#releasing) for how releases are cut. To build from
source instead:

Development happens in the provided dev container (Linux, cross-compiling to Windows):

```sh
# Build and test everything on Linux
cargo build --workspace
cargo test --workspace

# Cross-compile the launcher for the target platform (Windows, MinGW toolchain)
cargo build --workspace --target x86_64-pc-windows-gnu

# Formatting and lints (required before opening a PR)
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings

# End-to-end browser tests for the launcher's settings UI
cd e2e
npm ci
npx playwright test
```

### Connecting a remote Broadcast Agent

The simulator MCP server must run on the Rig (it needs local SDK / shared-memory / broadcast-message
access), but the Broadcast Agent that consumes its tools usually runs on separate hardware. To
support that topology out of the box, `simracecenter-launcher.exe` **defaults to the HTTP transport
bound to `0.0.0.0:8765`** — reachable from other hosts on the LAN — so a Driver can just run the exe
on the Rig and point the agent at `http://<rig-lan-ip>:8765/mcp`. No hidden `--transport http --bind
0.0.0.0:8765` flags are required.

That default trades same-machine-only exposure for LAN reachability, and the transport is
**unauthenticated** — anything that can reach it can invoke any tool. Run it only on a trusted
network segment and **never port-forward it to the internet** (see [SECURITY.md](SECURITY.md)). To
restrict the server to the Rig itself, launch with `--bind 127.0.0.1:8765`, or use `--transport
stdio` when an MCP client spawns the server as a local child process.

The Director Console (`launcher`) only runs meaningfully on Windows, next to a running simulator.
Its SDK adapters use local shared memory and Win32 broadcast messages that do not exist on Linux.
Linux/dev-container work verifies logic against stub adapters and cross-compilation, and the
`e2e/` Playwright suite drives the settings UI headless on Linux. End-to-end verification with a
live simulator still happens on a Windows Rig.

## Documentation

| Document | Purpose |
| --- | --- |
| [simracecenter.com](https://www.simracecenter.com/) | Product overview for the broader Sim RaceCenter ecosystem |
| [How It Works](https://www.simracecenter.com/how-it-works) | Conceptual product flow and hosted experience |
| [docs/adr](docs/adr/) | Architectural Decision Records (start with [ADR 0001](docs/adr/0001-project-layout.md)) |
| [docs/iracing-mcp-server.md](docs/iracing-mcp-server.md) | The `iracing-mcp` server: why it exists, its full tool reference, and the technical approach — also the reference pattern for new `<sim>-mcp` adapters |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to propose changes, coding standards, DCO sign-off |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Community expectations |
| [SECURITY.md](SECURITY.md) | How to report a vulnerability |

## Naming & Voice

User-facing copy (UI text, CLI help, log/error messages, docs) follows Sim RaceCenter's brand
protocol: professional, technical, crisp — data, not drama. In that copy, use:

| Term | Refers to |
| --- | --- |
| **Director Console** | The launcher's tray icon + settings window (never "Admin Panel") |
| **Race Control** | This backend/API (the MCP servers) |
| **Driver** | The user/competitor (never "Gamer") |
| **Rig** | The physical simulator cockpit/PC |
| **Broadcast Agent** | The AI orchestrator consuming these MCP tools |
| **Telemetry** | The raw simulator data stream |

This applies to strings a Driver or Broadcast Agent actually sees — it does not require renaming
Rust crates, modules, or internal identifiers.

## License

GPL-3.0-or-later — see [LICENSE](LICENSE). Any vendored third-party SDK reference material (e.g.
from a simulator vendor) is kept under its own original license, segregated from this grant — see
[ADR 0001 D7](docs/adr/0001-project-layout.md).
