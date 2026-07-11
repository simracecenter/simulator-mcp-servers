# Contributing to Sim RaceCenter — Simulator MCP Servers

Thanks for helping build Race Control. This document covers the mechanics of contributing;
[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md) covers how we treat each other.

## Ground rules

- Discuss non-trivial changes first. The [project board](https://github.com/orgs/simracecenter/projects/1)
  is where product/planning captures design and feature-development work; it is not a substitute
  for an issue. Before writing code, open a **GitHub issue** as the engineering artifact for that
  work (reference the relevant project card/ADR if one exists) — your PR should reference and close
  that issue.
  Architecture-level decisions are recorded as ADRs under [docs/adr](docs/adr/) (see
  [ADR 0001](docs/adr/0001-project-layout.md) for the current one); propose new ADRs for anything
  that changes the shape of the workspace,
  the launcher's process model, or a simulator adapter's public contract.
- Keep pull requests focused. Small, reviewable PRs move faster than large ones.
- All contributions are licensed under **GPL-3.0-or-later** (see [LICENSE](LICENSE)). New source
  files should carry an SPDX header:

  ```rust
  // SPDX-License-Identifier: GPL-3.0-or-later
  ```

## Developer Certificate of Origin (DCO)

We use the [DCO](https://developercertificate.org/) instead of a CLA. Every commit must be signed
off, certifying you wrote it (or have the right to submit it) and agree to license it under this
project's license:

```sh
git commit -s -m "Add camera-focus verification timeout"
```

This adds a `Signed-off-by: Your Name <you@example.com>` trailer. PRs with unsigned commits will be
asked to amend (`git commit --amend -s` / `git rebase --exec 'git commit --amend --no-edit -s'`).

## Development environment

Use the provided dev container (Linux, cross-compiles to Windows via MinGW):

```sh
cargo build --workspace
cargo test --workspace
cargo build --workspace --target x86_64-pc-windows-gnu
```

Before opening a PR:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

CI runs the same checks (see [.github/workflows/ci.yml](.github/workflows/ci.yml)) and will fail
the build on formatting or lint issues.

## Coding standards

- Follow the adapter pattern established by `mcp-core`/`iracing-mcp`: a trait (port) for each
  simulator SDK, a real implementation, and a `Stub` implementation so logic is testable without a
  live sim or Windows host. See [ADR 0001 D1](docs/adr/0001-project-layout.md).
- Mutating MCP tools that send simulator control commands must verify their effect against
  telemetry rather than trusting the command was applied — don't add a "fire-and-forget" tool
  without a verification loop.
- New simulator crates depend on `mcp-core` for transport/JSON-RPC/config plumbing rather than
  reimplementing it.
- User-facing strings (CLI help, UI copy, log/error messages a Driver or Broadcast Agent sees)
  follow the voice and vocabulary in [README.md § Naming & voice](README.md#naming--voice).

## Commit messages

Use clear, imperative-mood summaries (`Add`, `Fix`, `Refactor`, not `Added`/`Fixes`). Reference the
relevant project card or ADR section when applicable, e.g.:

```
Add replay_seek_frame tolerance handling

Ref: ADR 0001 D2
```

## Pull requests

Use the PR template checklist. In short: tests pass, `fmt`/`clippy` are clean, commits are signed
off, and any architecture-affecting change updates the relevant ADR.

## Reporting bugs / requesting features

Use the issue templates under `.github/ISSUE_TEMPLATE/`. For security issues, do **not** open a
public issue — see [SECURITY.md](SECURITY.md).
