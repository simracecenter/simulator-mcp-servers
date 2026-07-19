# Security Policy

## Supported versions

This project is in early development (pre-1.0). Security fixes are only provided for the latest
commit on `main`; there is no long-term-support branch yet.

## Reporting a vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities. Instead, use
[GitHub's private vulnerability reporting](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing/privately-reporting-a-security-vulnerability)
for this repository (Security tab → "Report a vulnerability"), or contact the maintainers directly
if that's unavailable to you.

Please include:
- A description of the issue and its potential impact.
- Steps to reproduce, or a proof of concept.
- The affected component (e.g. `mcp-core` transport, an `<sim>-mcp` adapter, the launcher).

We'll acknowledge reports as quickly as we can and keep you updated as a fix is developed.

## Known trust-model considerations

- The Director Console (`launcher`) and each simulator's MCP server run on the Rig, next to the
  simulator, trusted by a single Driver. They are not designed to be exposed directly to the public
  internet.
- The MCP transport **defaults to HTTP bound to `0.0.0.0:8765`**, i.e. reachable from other hosts on
  the LAN. This is intentional: the common topology runs the server on the Rig while the Broadcast
  Agent runs on separate hardware, so a loopback-only default would be unreachable by the agent that
  needs it. The transport is **unauthenticated**, so treat "on the network" as "able to invoke every
  tool": run it only on a trusted network interface/segment and never port-forward it to the
  internet. To restrict it to same-machine clients, launch with a loopback bind
  (`--bind 127.0.0.1:8765`) or use `--transport stdio`. The launcher logs a warning at startup when
  the MCP transport is bound to a non-loopback address.
- The settings HTTP server (Director Console UI) is separate and still binds loopback
  (`127.0.0.1:8766`) by default, because it only needs to be driven from the Rig itself.
- Mutating MCP tools translate into simulator SDK broadcast messages and are verified against
  telemetry, but that verification is a correctness mechanism, not an authorization mechanism —
  anything that can reach the transport can invoke any tool it exposes.

If you believe any of these assumptions are insufficient for your deployment, please open an issue
(non-sensitive) or a private report (sensitive) so we can track hardening work.
