# Sim RaceCenter — Simulator MCP Servers

<p align="center">
  <img src="docs/assets/logo_black.png" alt="Sim RaceCenter logo" width="120">
</p>

[![Build status](https://img.shields.io/github/actions/workflow/status/simracecenter/simulator-mcp-servers/ci.yml?branch=main)](https://github.com/simracecenter/simulator-mcp-servers/actions/workflows/ci.yml)
[![License](https://img.shields.io/github/license/simracecenter/simulator-mcp-servers?color=FF5F1F)](LICENSE)

**Data-driven precision for cinematic race broadcasts.**

Sim RaceCenter's Broadcast Agent orchestrates live sim-racing broadcasts: it interrogates session
telemetry and directs replay/camera systems on the Driver's behalf, through natural-language intent
resolved into deterministic, verifiable commands. This backend repository is Sim RaceCenter's
open source MCP servers for racing simulators exposing each supported simulator's
telemetry and control surface as [Model Context Protocol (MCP)](https://modelcontextprotocol.io)
servers, plus the Director Console that runs them on the Rig.

Status: **early development**. Design decisions live in [docs/adr](docs/adr/) (start with
[ADR 0001](docs/adr/0001-project-layout.md)) and the
[project board](https://github.com/orgs/simracecenter/projects/1).

## What's here

```
simulator-mcp-servers/
├── crates/
│   ├── mcp-core/      # Shared MCP JSON-RPC layer, transports, verification-loop
│   │                  # helpers, and config load/merge — used by every simulator crate.
│   ├── iracing-mcp/   # iRacing telemetry + replay/camera control MCP server.
│   ├── lmu-mcp/       # Le Mans Ultimate MCP server.
│   └── launcher/      # The Director Console: CLI + singleton runner + settings web UI,
│                       # hosts exactly one active simulator's MCP server at a time.
└── e2e/               # Playwright end-to-end tests for the launcher's settings UI.
```



## Getting started

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

The Director Console (`launcher`) only runs meaningfully on Windows, next to a running simulator —
its SDK adapters use local shared memory and Win32 broadcast messages that don't exist on Linux.
Linux/dev-container work verifies logic against stub adapters and cross-compilation, and the
`e2e/` Playwright suite drives the settings UI headless on Linux; end-to-end verification with a
live sim still happens on a Windows Rig.

## Documentation

| Document | Purpose |
| --- | --- |
| [docs/adr](docs/adr/) | Architectural Decision Records (start with [ADR 0001](docs/adr/0001-project-layout.md)) |
| [docs/iracing-mcp-server.md](docs/iracing-mcp-server.md) | The `iracing-mcp` server: why it exists, its full tool reference, and the technical approach — also the reference pattern for new `<sim>-mcp` adapters |
| [CONTRIBUTING.md](CONTRIBUTING.md) | How to propose changes, coding standards, DCO sign-off |
| [CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) | Community expectations |
| [SECURITY.md](SECURITY.md) | How to report a vulnerability |

## Naming & voice

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
