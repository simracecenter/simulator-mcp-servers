# AGENTS.md

Instructions for any AI coding agent (GitHub Copilot, Codex, or otherwise) working in this
repository. Read the files below **before** making changes — don't guess at conventions this repo
has already documented.

## Start here

1. **[.github/copilot-instructions.md](.github/copilot-instructions.md)** — the canonical repo
   instructions: workspace shape, where architectural decisions live, how planning (project board)
   relates to engineering (GitHub issues), and the custom agent workflow. Read this first; it links
   to everything else you need.
2. **[docs/adr/README.md](docs/adr/README.md)** — index of every Architecture Decision Record.
   Consult the relevant ADR (e.g. [docs/adr/0001-project-layout.md](docs/adr/0001-project-layout.md))
   before proposing changes to workspace shape, the launcher's process model, or a simulator
   adapter's public contract. Record a new ADR (or amend an existing one) when you make a decision
   like that, not just the code. In particular, read
   **[docs/adr/0003-single-active-simulator-constraint.md](docs/adr/0003-single-active-simulator-constraint.md)**
   before adding anything that smells like multi-server support (a handler registry, per-sim
   dynamic ports, merged capabilities across sims, etc.) — this repo only ever runs **one**
   simulator MCP server at a time, by hard design constraint, not as a v1 simplification.
3. **[CONTRIBUTING.md](CONTRIBUTING.md)** — ground rules (issue-before-code, DCO sign-off on every
   commit, SPDX headers on new source files), development environment/build commands, required
   pre-PR checks (`cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace`), and coding standards.
4. **[CODE_OF_CONDUCT.md](CODE_OF_CONDUCT.md)** and **[SECURITY.md](SECURITY.md)** — how we treat
   each other, and how to report vulnerabilities (this project's MCP transports are unauthenticated
   by design; see SECURITY.md's trust model before touching transport code).
5. **[README.md](README.md)** — project overview and crate layout.

## Custom agent workflows

Check **[.github/agents/](.github/agents/)** before improvising a multi-step workflow that already
has a dedicated agent:

- **Get Next Card** ([.github/agents/get-next-card.agent.md](.github/agents/get-next-card.agent.md))
  — refines a project-board card into a recorded decision and a linked GitHub issue.
- **Issue Planner** ([.github/agents/issue-planner.agent.md](.github/agents/issue-planner.agent.md))
  — turns a GitHub issue into a concrete, file-by-file implementation plan. Writes no code.
- **Implement Issue** ([.github/agents/implement-issue.agent.md](.github/agents/implement-issue.agent.md))
  — executes an issue's recorded implementation plan end-to-end (branch, code, `fmt`/`clippy`/
  `test`, DCO-signed commit, push, PR).

These hand off to each other via GitHub (card → issue → PR), not via conversation context, so pick
up the workflow at whichever stage the work is already at.

## Planning vs. engineering

Product/planning work lives on the [project board](https://github.com/orgs/simracecenter/projects/1);
actual engineering work is tracked as **GitHub issues**, which PRs reference and close. A project
card can exist without an issue (design-only), but writing code always requires an issue per
CONTRIBUTING.md. Don't conflate the two, and link a card and its issue back to each other once both
exist.
