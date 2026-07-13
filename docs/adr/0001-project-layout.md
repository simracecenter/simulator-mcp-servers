# ADR 0001: Repository Layout & Launcher Architecture for `simulator-mcp-servers`

- **Status:** Accepted
- **Date:** 2026-07-09
- **Deciders:** repo owner (margic), simracecenter org

## Context

This repo is being prepared for open-sourcing under the `simracecenter` GitHub org as the home
for **all** simulator MCP servers (iRacing today, Le Mans Ultimate next, more later). The existing
[margic/iracing-mcp](https://github.com/margic/iracing-mcp) prototype established a useful pattern
worth carrying forward: a trait-based SDK adapter (`IracingAdapter` with a real `SdkAdapter` and an
in-memory `StubAdapter`), a hand-rolled MCP JSON-RPC layer, and dual stdio/HTTP transports — all
testable on Linux without a live sim. We are **not** moving that repo; instead we're deciding how to
restructure this one to host multiple servers behind a single, low-footprint launcher.

Constraints driving this decision:

1. Must be friendly to launch on a Windows gaming rig: a UI, plus a scriptable CLI entry point
   (PowerShell, Stream Deck, etc.).
2. Configurable via UI or a settings file, with CLI flags overriding at launch time for automation.
3. Resource usage must be very low — these machines are running the simulator itself; anything the
   launcher/servers consume is competing with the sim.
4. Modular: new simulators (LMU next) should be easy to add without reworking shared plumbing.
5. Everything lives in one repo, but the internal structure needs to make trade-offs explicit.
6. Development and testing happen on Linux in dev containers; the shipped artifact targets Windows
   (`x86_64-pc-windows-gnu`, per the existing devcontainer).

## Decision

### D1 — Workspace shape: shared core crate + one crate per simulator + a launcher crate

```
simulator-mcp-servers/
├── Cargo.toml                    # workspace root
└── crates/
    ├── mcp-core/                 # shared: JSON-RPC/MCP layer, stdio+HTTP transports,
    │                             # verification-loop helpers, typed AdapterError, config I/O
    ├── iracing-mcp/              # IracingAdapter (Sdk + Stub), iRacing-specific tool set
    ├── lmu-mcp/                  # future: LmuAdapter (Sdk + Stub), LMU-specific tool set
    └── launcher/                 # binary crate: CLI, singleton runner, tray UI
```

Each `<sim>-mcp` crate depends on `mcp-core` and implements the same shape iracing-mcp already
proved out: a `XAdapter` trait, a real SDK-backed implementation, and a `Stub` implementation for
CI on Linux. `mcp-core` owns everything that shouldn't be duplicated per simulator: the JSON-RPC
plumbing, transport implementations, the "snapshot → send → poll → verify" loop, and config
loading/merging.

**Rejected alternatives:**
- *One self-contained crate per simulator, no shared core* — fastest to bootstrap per-sim but
  duplicates the transport/verification/config code that has nothing sim-specific about it, and
  that duplication will drift as servers are added.
- *Independent Cargo workspace per simulator inside the monorepo* — avoids workspace-wide rebuild
  coupling but forces an internal-crate publishing/vendoring step just to share `mcp-core`, adding
  friction for little benefit at this scale.

### D2 — Launcher owns process lifecycle; UI is a swappable adapter behind a port

The launcher is a **single binary** with a **singleton runner**: at any moment it hosts at most one
active simulator's MCP server, because in practice you are never playing iRacing and LMU at the
same time, and running two servers' telemetry polling loops concurrently would waste the resource
budget for no real use case. The active simulator is chosen by config default + CLI override
(`--sim iracing|lmu`).

The UI is deliberately **not** load-bearing for that logic. We define a narrow `LauncherUi` port
(trait) that the runner talks to for status/config display; the runner, config loading, singleton
enforcement, and MCP hosting have **no dependency** on any specific UI toolkit:

```
launcher/
├── src/
│   ├── main.rs        # CLI parsing, config load, singleton guard, dispatch
│   ├── runner.rs       # owns the active XAdapter + mcp-core server; UI-agnostic
│   ├── config.rs       # TOML load/save + CLI-override merge
│   └── ui/
│       ├── mod.rs      # `LauncherUi` trait (port)
│       └── tray.rs      # first adapter: system tray icon + minimal native window
```

v1 UI implementation is a **system tray icon with a minimal native window** for status/settings
(no Electron/Tauri/WebView2), keeping baseline memory/CPU low, per constraint #3. Because it sits
behind the `LauncherUi` port, a fuller dashboard or a different UI toolkit can replace it later
without touching the runner, config, or MCP hosting code.

**Update (2026-07-10):** the concrete crate for this v1 UI is **`native-windows-gui` (nwg)**.
It wraps native Win32 controls directly (no imposed rendering engine, no extra windowing
abstraction like `tray-icon` would need paired with `winit`), provides a real tray icon + window
out of the box, and a throwaway spike confirmed it cross-compiles cleanly for
`x86_64-pc-windows-gnu` (release exe ~1.97 MB, ~30-crate dependency tree) with
`#![windows_subsystem = "windows"]` suppressing the console window. Full rationale recorded on
the [project card](https://github.com/orgs/simracecenter/projects/1/views/2?pane=issue&itemId=210617424).

Scripted automation (PowerShell, Stream Deck) uses the same binary in headless mode
(`--sim iracing --headless` or similar), skipping the tray/window entirely but going through the
identical runner/config/singleton path — one code path for both interactive and scripted launch.

**Rejected alternatives:**
- *Separate binary per server, launcher spawns children* — better crash isolation, but adds a
  supervision layer (respawn policy, IPC for status) for a scenario (two sims running at once)
  that doesn't occur in practice, and costs more baseline memory (N processes vs 1).
- *Tauri/webview UI* — directly conflicts with the low-resource-usage constraint (bundles a
  WebView2 runtime and JS engine).

### D3 — Singleton enforcement

The launcher takes a named OS-level lock (e.g., a Windows named mutex) on startup so a second
launch (double-clicked tray icon, a Stream Deck button fired twice, etc.) fails fast with a clear
message instead of starting a competing MCP server. This is orthogonal to, and in addition to, the
"only one sim active" rule in D2 — it protects against accidentally running two copies of the same
sim's server.

### D4 — Configuration: TOML file, UI-editable, CLI-overridable

- Location: `%APPDATA%\SimRaceCenter\config.toml`.
- The tray UI reads/writes this file directly.
- CLI flags passed at launch override the corresponding config values for that run only (they are
  not persisted back to the file unless the user explicitly saves from the UI).
- Each `<sim>-mcp` crate defines its own config section/struct; `mcp-core` provides the merge
  (file → CLI override) logic once, generically, so per-sim crates don't reimplement it.

### D5 — Migration of the existing iracing-mcp code

Scaffold the structure above first (empty/skeleton `mcp-core`, `iracing-mcp`, `launcher` crates,
workspace wiring, devcontainer, CI) as its own piece of work. Porting the actual iracing-mcp
adapter/tool code into `crates/iracing-mcp` (and extracting the shared parts into `mcp-core`) is a
**separate follow-up task**, tracked once the skeleton lands, rather than a big-bang import.

### D6 — Release & versioning: single unified version

One version number for the whole repo; one installer/zip artifact bundles the launcher and all
`<sim>-mcp` crates together. Matches the "one repo" goal and is simplest for end users (one
download, one changelog, no cross-crate version-compatibility matrix to reason about).

**Rejected alternative:** independent per-crate versioning/release artifacts — more flexible, but
adds real CI/release complexity (compatibility matrix between launcher and server versions) that
isn't justified yet at two simulators.

### D7 — License: GPL-3.0

⚠️ **Flag before finalizing:** the current iracing-mcp repo vendors iRacing's own C++/C# SDK
reference sources under `iracing/`, which "retain their original iRacing.com Motorsport Simulations
license" (per its README) — those files were **not** under an open-source license themselves. If
that reference material is carried into this repo, it should stay excluded from the GPL-3.0 grant
(kept clearly attributed/segregated, e.g. `third_party/` with its own `LICENSE` header, as
iracing-mcp already did by marking its project license "TBD" separately from the vendored SDK).
Worth a quick check that GPL-3.0 for the launcher/servers doesn't create friction with any
SDK/EULA terms iRacing imposes on redistributing their headers or interfacing with their telemetry
API before this is finalized.

## Consequences

**Positive**
- One shared `mcp-core` means transport, verification-loop, and config-merge logic is written and
  tested once; adding LMU (or the next sim) is "implement the adapter trait + tools," not "rebuild
  the plumbing."
- Single active-sim-at-a-time runner keeps the resource footprint close to "one adapter + one tray
  icon," matching the real usage pattern and constraint #3.
- The `LauncherUi` port means the v1 tray UI is not a dead end — a richer UI can be swapped in
  later without touching runner/config/MCP-hosting code.
- Single version/artifact keeps distribution and user-facing docs simple.

**Negative / trade-offs**
- Workspace-wide compilation coupling: a change to `mcp-core` touches every `<sim>-mcp` crate's
  build; mitigated by keeping `mcp-core`'s surface area deliberately small and stable.
- Singleton, single-active-sim design means there's no supported path to running two sims' servers
  simultaneously later without revisiting D2/D3 — acceptable given today's stated use case, but
  worth remembering if that assumption changes.
- Deferring the iracing-mcp migration (D5) means the skeleton will initially ship without a fully
  working sim adapter; needs a tracked follow-up so it doesn't stall indefinitely.

## Open follow-ups

Tracked on the [Simulator MCP Servers project board](https://github.com/orgs/simracecenter/projects/1)
(created via `gh project create`/`gh project item-create` on 2026-07-09; draft cards, not yet
linked to GitHub issues).

- [x] Concrete crate choice for the tray icon + minimal window — **decided: `native-windows-gui`
      (nwg)**, see D2 update above.
      [Project card](https://github.com/orgs/simracecenter/projects/1/views/2?pane=issue&itemId=210617424)
      (status: Done)
- [ ] Follow-up task: port iracing-mcp's adapter/tool code into `crates/iracing-mcp` and extract
      shared pieces into `mcp-core` (D5). **Migration plan decided 2026-07-12 — see project card
      for the full breakdown. Key decisions:**
      - Source: `margic/iracing-mcp` @ `main`; all dependencies are on crates.io
        (`iracing = "0.4.1"`, `iracing-broadcast = "0.1.0"`, `serde_yaml = "0.8"`,
        `winapi = "0.3"` under `[target.'cfg(windows)'.dependencies]`).
      - `adapter/sdk_live.rs` → `crates/iracing-mcp/src/adapter/sdk.rs` (drop older `sdk.rs`).
      - `adapter/stub.rs` → `crates/iracing-mcp/src/adapter/stub.rs`.
      - `adapter/mod.rs` (trait + domain types) → `crates/iracing-mcp/src/adapter/mod.rs`.
      - `mcp/mod.rs` (tool dispatch + verification loop) → `crates/iracing-mcp/src/handler.rs`
        (replaces stub), adapted to implement `mcp_core::McpHandler`.
      - `crates/mcp-core/src/transport/http.rs` needs `POST /mcp` route + `GET /healthz`
        (currently only serves `POST /`).
      - `crates/launcher/src/runner.rs` needs to wire `Arc<IracingMcpHandler>` to the
        configured transport.
      - Three upstream test files port to `crates/iracing-mcp/tests/` (`http_transport.rs`,
        `verification_regressions.rs`, `live_mcp_suite.rs` — last one kept `#[ignore]`).
      - Done when: `cargo test -p iracing-mcp` passes on Linux, `cargo build --target
        x86_64-pc-windows-gnu -p iracing-mcp` compiles, `tools/list` returns the real
        14-tool set, clippy clean.
      [Project card](https://github.com/orgs/simracecenter/projects/1/views/2?pane=issue&itemId=210617431)
- [ ] Confirm GPL-3.0 compatibility with any vendored iRacing SDK reference material before it's
      added to this repo (D7).
      [Project card](https://github.com/orgs/simracecenter/projects/1/views/2?pane=issue&itemId=210617434)
- [ ] Design headless CLI flag surface (`--sim`, `--headless`, `--transport`, config-file path
      override) for PowerShell/Stream Deck scripting.
      [Project card](https://github.com/orgs/simracecenter/projects/1/views/2?pane=issue&itemId=210617441)
- [x] LMU adapter research (SDK shape, whether it also uses a fire-and-forget broadcast channel
      like iRacing, or something else) — **researched 2026-07-13, see
      [ADR 0002](0002-lmu-adapter-design.md)**: LMU (rFactor 2 engine lineage) uses a
      shared-memory-mapped-file model for both telemetry reads and commands (input buffers), not
      an OS broadcast channel like iRacing. Camera/replay control parity is unconfirmed pending
      live verification once implementation starts.
      [Project card](https://github.com/orgs/simracecenter/projects/1/views/2?pane=issue&itemId=210617448)
