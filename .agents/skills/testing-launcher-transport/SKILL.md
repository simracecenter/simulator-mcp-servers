---
name: testing-launcher-transport
description: Smoke-test the Director Console launcher's MCP + settings HTTP transports on Linux (bind defaults, LAN vs loopback reachability, startup warnings). Use when verifying launcher transport/bind behavior or changes to crates/launcher/src/main.rs, runner.rs, or mcp-core transports.
---

# Testing the launcher's transports (Linux, headless)

The launcher (`crates/launcher`) hosts two HTTP servers: the **MCP transport** (`--bind`, default
`0.0.0.0:8765`, routes `GET /healthz` + `POST /mcp`) and the **settings server** (`--settings-bind`,
default `127.0.0.1:8766`, routes `GET /healthz`, `/api/status`, `POST /api/sim`). The tray UI only
builds on Windows, so on Linux/CI you must run **`--headless`** to exercise the transports.

This is a **shell/server** test — do NOT record it (no GUI). Capture curl output as text evidence.

## How to run

```sh
cargo build -p launcher
RUST_LOG=info,warn ./target/debug/simracecenter-launcher --headless > /tmp/launcher.log 2>&1 &
```

- Run with **no** `--transport`/`--bind` flags to test the *defaults* (that's usually the point).
- `RUST_LOG=info,warn` (or at least `warn`) is required to see log lines — the subscriber uses
  `EnvFilter::from_default_env()`, so with `RUST_LOG` unset you get no output.
- Kill it with `kill <pid>` when done.

## Proving LAN reachability vs loopback (the key adversarial trick)

Loopback (`127.0.0.1`) requests succeed for *both* a loopback bind and a `0.0.0.0` bind, so they
can't tell the two apart. To prove a bind is actually LAN-reachable, hit the machine's
**non-loopback** IP:

```sh
hostname -I            # e.g. 172.16.26.2 (eth0); pick the non-docker global IP
curl -s -w '\nHTTP=%{http_code}\n' http://<LAN_IP>:8765/healthz          # 200 {"ok":true} if LAN-reachable
curl -s --max-time 5 http://<LAN_IP>:8766/healthz; echo exit=$?          # exit 7 / HTTP 000 if loopback-only
```

A loopback-only server refuses the non-loopback connection (`curl` exit 7, `HTTP=000`). A
`0.0.0.0` server answers on both. Use this to distinguish "http default" from "stdio default" and
"LAN bind" from "loopback bind" in one shot.

## Useful checks

- MCP tool surface: `curl -s -X POST http://<IP>:8765/mcp -H 'content-type: application/json' -d
  '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'` → JSON-RPC `result.tools` array (iRacing stub
  returns ~16 tools incl. `get_session_overview`). On Linux the stub adapter reports the sim as
  "not connected" — that's expected; live-sim control needs a Windows Rig.
- Non-loopback bind warning: the launcher logs a `WARN ... reachable off-host ... (see SECURITY.md)
  bind=<addr>` at startup when the MCP bind is not loopback. Grep `/tmp/launcher.log` for it.

## e2e Playwright suite

`e2e/tests/settings.spec.ts` drives the settings server headless. `spawnLauncher` defaults to
`--transport stdio` (unless a test sets `--transport`) to avoid fixed-port `0.0.0.0:8765` collisions
between parallel workers — keep that in mind if you add tests that need the MCP HTTP transport (pass
an explicit `--bind <free-port>`).

## Devin Secrets Needed
None — everything runs locally on the box.
