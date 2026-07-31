---
name: testing-launcher-transport
description: Smoke-test the Director Console launcher's MCP + settings HTTP transports (bind defaults, LAN vs loopback reachability, Streamable HTTP sessions/SSE, startup warnings). Use when verifying launcher transport/bind behavior or changes to crates/launcher/src/main.rs, runner.rs, or mcp-core transports.
---

# Testing the launcher's transports (headless)

The launcher (`crates/launcher`) hosts two HTTP servers: the **MCP transport** (`--bind`, default
`0.0.0.0:8765`, routes `GET /healthz` + `GET`/`POST`/`DELETE /mcp`) and the **settings server**
(`--settings-bind`, default `127.0.0.1:8766`, routes `GET /healthz`, `/api/status`, `POST /api/sim`).
The tray UI only builds on Windows, so on Linux/CI you must run **`--headless`** to exercise the
transports.

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
  returns ~16 tools incl. `get_session_overview`). Off-rig the stub adapter reports the sim as
  "not connected" — that's expected; live-sim control needs a Windows Rig with the sim running.
- Non-loopback bind warning: the launcher logs a `WARN ... reachable off-host ... (see SECURITY.md)
  bind=<addr>` at startup when the MCP bind is not loopback. Grep `/tmp/launcher.log` for it.

## e2e Playwright suite

`e2e/tests/settings.spec.ts` drives the settings server headless. `spawnLauncher` defaults to
`--transport stdio` (unless a test sets `--transport`) to avoid fixed-port `0.0.0.0:8765` collisions
between parallel workers — keep that in mind if you add tests that need the MCP HTTP transport (pass
an explicit `--bind <free-port>`).

## Windows boxes (no Rust preinstalled)

The msvc host toolchain usually has no linker on a fresh Windows Devin box. Install rustup + mingw
(`choco install mingw`) and build with the gnu toolchain:

```powershell
$env:Path="$env:USERPROFILE\.cargo\bin;C:\ProgramData\mingw64\mingw64\bin;$env:Path"
cargo +stable-x86_64-pc-windows-gnu build -p launcher
Start-Process .\target\debug\simracecenter-launcher.exe -ArgumentList `
  "--headless","--transport","http","--bind","127.0.0.1:8765","--settings-bind","127.0.0.1:8766"
```

The launcher takes a **single-instance mutex** (`SimRaceCenterLauncher`) — stop the HTTP instance
(`Stop-Process -Name simracecenter-launcher`) before starting a stdio one, or it exits with
"refusing to start a second instance".

## Testing the Streamable HTTP transport with a real MCP client

The bundled `C:\devin\python` may ship without pip: bootstrap with
`curl -sSL -o get-pip.py https://bootstrap.pypa.io/get-pip.py; python get-pip.py`, then
`python -m pip install mcp requests`.

In **`mcp` >= 2.0** the API changed: use `streamable_http_client(url, http_client=...)` (not
`streamablehttp_client`), it yields only `(read, write)` — no `get_session_id`. To observe the session
id / status codes, pass `create_mcp_http_client()` with `event_hooks={"request":[...],"response":[...]}`
and log `resp.headers["mcp-session-id"]`. Field names are snake_case (`server_info`, `is_error`).

Useful raw checks against `/mcp` (all verified working): GET without `Accept: text/event-stream` → 406;
bogus `Mcp-Session-Id` on GET/POST/DELETE → 404; id-less POST → 202 empty; second concurrent GET on one
session → 409; DELETE → 204 then 404; malformed body → 200 with JSON-RPC `-32700`. SSE keep-alive `:`
comments arrive every 15 s on an idle stream — read raw bytes for ≥35 s in a background thread and assert
no EOF (a `for ... in r.raw.stream(1)` loop blocks until the next byte, so don't judge liveness by a
timed `break`). Note POST without any session header is still accepted (POST-only back-compat), so
"session enforcement" is intentionally not testable.

## Devin Secrets Needed
None — everything runs locally on the box.
