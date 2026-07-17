# Architectural Decision Records

This folder tracks the architectural decisions for `simulator-mcp-servers`, in the order they were
made. Each ADR captures the context, the decision, and its trade-offs so future contributors (human
or agent) can see *why* the code is shaped the way it is without re-deriving it from scratch.

| # | Title | Status |
| --- | --- | --- |
| [0001](0001-project-layout.md) | Repository Layout & Launcher Architecture for `simulator-mcp-servers` | Accepted |
| [0002](0002-lmu-adapter-design.md) | LMU Telemetry Access Model & `LmuAdapter` Design | Accepted |
| [0003](0003-single-active-simulator-constraint.md) | Single Active Simulator Constraint — No Multi-Instance Support | Accepted |

## Adding a new ADR

1. Copy the format of an existing ADR: `Status` / `Date` / `Deciders`, `Context`, `Decision`
   (numbered sub-decisions, each with rejected alternatives), `Consequences`, and, if applicable,
   `Open follow-ups` linking to project board cards.
2. Name the file `NNNN-short-title.md`, incrementing from the highest existing number.
3. Add a row to the table above.
4. Link to it from [../../README.md](../../README.md) if it documents a decision contributors
   should know about up front.

Status values: `Proposed`, `Accepted`, `Superseded by NNNN`, `Rejected`.
