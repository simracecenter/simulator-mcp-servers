---
name: testing-launcher-transport
description: Smoke-test launcher MCP/settings transports, Windows native builds, Publisher HTTPS pairing and protected sessions. Use for launcher main.rs, runner.rs, pairing or mcp-core HTTP transport changes.
---

# Testing the launcher's transports (headless)

The launcher (`crates/launcher`) hosts the MCP transport (`--bind`, default
`0.0.0.0:8765`, routes `GET /healthz` + `GET`/`POST`/`DELETE /mcp`) and settings
server (`--settings-bind`, default `127.0.0.1:8766`, routes `GET /healthz`,
`/api/status`, `POST /api/sim`). Simulator roles use plain HTTP. Publisher
uses HTTPS and additionally exposes `POST /pair`; settings remains loopback HTTP.
The tray UI only builds on Windows, so on Linux/CI use `--headless`.

Transport-only shell tests need text evidence, not recording. If testing settings
UI, record native card/button interactions and collect protocol responses separately.

## How to run

```sh
cargo build -p launcher
RUST_LOG=info,warn ./target/debug/simracecenter-launcher --headless > /tmp/launcher.log 2>&1 &
```

- Use no transport/bind flags when testing defaults.
- `RUST_LOG=info,warn` (or at least `warn`) is needed for log lines; the subscriber
  uses `EnvFilter::from_default_env()`.
- Kill the PID when done.

## Proving LAN reachability vs loopback

Loopback succeeds for both `127.0.0.1` and `0.0.0.0` binds. Probe the machine's
non-loopback, non-Docker IP to distinguish them:

```sh
hostname -I
curl -s -w '\nHTTP=%{http_code}\n' http://<LAN_IP>:8765/healthz
curl -s --max-time 5 http://<LAN_IP>:8766/healthz; echo exit=$?
```

Loopback-only settings refuses the non-loopback connection (curl exit 7, HTTP 000);
LAN MCP responds 200 `{"ok":true}`. Check the `WARN ... reachable off-host ...`
startup message when using a non-loopback MCP bind.

Simulator tools can be inspected with:

```sh
curl -s -X POST http://<IP>:8765/mcp -H 'content-type: application/json' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

iRacing exposes about 16 tools including `get_session_overview`. Off-rig the SDK
reports not connected; live control requires Windows with the simulator running.

## e2e Playwright suite

`e2e/tests/settings.spec.ts` drives the headless settings server. `spawnLauncher`
defaults to `--transport stdio` to avoid port collisions. Explicitly pass HTTP and
a free bind when testing network behavior.

## Windows boxes

MSVC Rust alone is insufficient without Visual Studio linker prerequisites.
Prefer MSVC when available. Otherwise native GNU builds can use:

```powershell
choco install mingw rustup.install -y --no-progress
$env:Path="$env:USERPROFILE\.cargo\bin;C:\ProgramData\mingw64\mingw64\bin;$env:Path"
rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal
cargo +stable-x86_64-pc-windows-gnu build -p launcher
Start-Process .\target\debug\simracecenter-launcher.exe -ArgumentList `
  "--headless","--transport","http","--bind","127.0.0.1:8765","--settings-bind","127.0.0.1:8766"
```

The package is `launcher`; the executable is `simracecenter-launcher.exe`.
It takes the `SimRaceCenterLauncher` single-instance mutex: stop the existing test
instance before another, or it refuses startup. In Windows PTY-driven Python
probes, submit commands with CR; LF alone may echo without processing.

After a release, inspect the MSVC binary's imports with
`objdump -p <exe> | grep -i "DLL Name\|<none>"` and run `<exe> --help` on a Windows
box. GNU-native testing does not reproduce MSVC import-library differences.

## Streamable HTTP with a real MCP client

Bundled `C:\devin\python` may lack pip. If an SDK is needed:

```sh
curl -sSL -o get-pip.py https://bootstrap.pypa.io/get-pip.py
python get-pip.py
python -m pip install mcp requests
```

In `mcp >= 2.0`, use `streamable_http_client(url, http_client=...)` rather than
`streamablehttp_client`. It yields `(read, write)`, not a session getter.
Use `create_mcp_http_client()` event hooks to observe `mcp-session-id` and status.
SDK fields are snake_case (`server_info`, `is_error`).

Raw unprotected simulator transport checks:
- GET without `Accept: text/event-stream`: 406.
- Bogus session on GET/POST/DELETE: 404.
- Id-less POST: 202 empty.
- Second concurrent GET on one session: 409.
- DELETE: 204, then 404.
- Malformed body: 200 with JSON-RPC -32700.
- SSE keep-alive comments every 15 seconds. Read bytes in a background thread for
  at least 35 seconds; a blocking stream iterator does not enforce a timed break.
- Unprotected simulator POST allows session-less back-compat. **Do not apply this
  assumption to Publisher's protected router.**

## Publisher pairing and protected sessions

Read ADR 0008 and Director `docs/12-rig-pairing.md`/`test/support/mcp-fixture.ts`
at the intended revision. Test with isolated launcher config; on Windows it is
`%APPDATA%\SimRaceCenter`. Preserve existing state before testing and remove only
freshly created test files afterwards.

- Open `http://127.0.0.1:8766` and click Publisher. RIG PAIRING should show six digits,
  colon-separated SHA256 fingerprint and paired status. It is hidden for iRacing/LMU.
- `/api/status` JSON keys are **camelCase** (`pairingCode`, `certFingerprint`,
  `directorName`, `deviceId`, `toolNames`), despite snake_case Rust fields.
- Compare TLS peer hash with settings and `rig-cert.pem`; verify `rig-key.pem` and
  config `device_id`. OpenSSL from Git for Windows can inspect the peer:
  `openssl s_client -connect 127.0.0.1:8765 -servername localhost` piped to
  `openssl x509 -noout -fingerprint -sha256 -subject -issuer`.
- `/pair` payload uses `pairingCode` and `director: {name, ingestUrl, ingestFingerprint}`.
  Five valid-format wrong codes produce 403; sixth and correct-code attempts during
  lockout produce 429 with Retry-After. Honor the advertised delay before pairing.
  Origin is 403, >16 KiB is 413, missing schema fields 422, already paired 409.
- Hold minted bearer credentials only in process memory. Redact before writing
  response transcripts. Verify stored `credential_sha256` against the bearer hash;
  scan app config, logs, worktree and evidence for plaintext without writing the
  search secret into a script or command line. State the scan scope explicitly.
- Protected `/mcp`: bearer initialize without session issues `Mcp-Session-Id`;
  subsequent calls need it. Missing bearer 401, Origin 403, unknown session 404,
  missing post-initialize session 400. Reinitialize after process restart; do not
  expect old in-memory session IDs to survive.
- Check tool discovery separately from authorization: a listed tool may not be
  in the pairing credential's publisher-only scope. Record any listed-but-denied
  `get_capabilities` result rather than treating all discovery as permission proof.
- Configure with `ingest_url`, `token`, `cert_fingerprint` (64 hex); verify no token
  echo, DPAPI token file has no plaintext, and configured:true survives restart.
- Off-rig start can report RUNNING with `waiting_for_iracing`; this is not proof of
  telemetry ingestion. Record exact status and stop afterward.
- UI Unpair must remove `[pairing]`, rotate code and make the old bearer return 401.
  Role switching must rebind the same port from HTTPS to plain HTTP and back;
  verify actual protocols and tools, not only the selected card. Re-pair afterward.

## Devin Secrets Needed

None for local transport/pairing tests. Real telemetry ingestion requires a
running simulator and a real Director ingest endpoint/token, outside this smoke test.
