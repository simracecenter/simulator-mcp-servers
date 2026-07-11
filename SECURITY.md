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

- The Director Console (`launcher`) and each simulator's MCP server are designed to run on the same
  machine as the simulator, trusted by a single Driver/Rig. They are not designed to be exposed
  directly to the public internet.
- The optional HTTP transport (as opposed to stdio) is intended for LAN use by a trusted Broadcast
  Agent host. If you run it, bind it to a trusted network interface/segment and do not port-forward
  it to the internet.
- Mutating MCP tools translate into simulator SDK broadcast messages and are verified against
  telemetry, but that verification is a correctness mechanism, not an authorization mechanism —
  anything that can reach the transport can invoke any tool it exposes.

If you believe any of these assumptions are insufficient for your deployment, please open an issue
(non-sensitive) or a private report (sensitive) so we can track hardening work.
