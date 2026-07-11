## What this changes

<!-- Crisp summary. Reference the relevant issue/project card/ADR section if applicable. -->

## Why

<!-- What problem does this solve, or what capability does it add? -->

## Checklist

- [ ] `cargo fmt --all` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace` passes
- [ ] Cross-compiles for `x86_64-pc-windows-gnu` (if this touches `launcher` or a `<sim>-mcp` crate)
- [ ] Commits are signed off (`git commit -s`) per the [DCO](https://developercertificate.org/)
- [ ] Updated [ADR 0001](../docs/adr/0001-project-layout.md) or opened a new ADR, if this changes
      workspace shape, the launcher's process model, or a simulator adapter's public contract
- [ ] Updated docs (README/CONTRIBUTING) if user-facing behavior changed

## Notes for reviewers (optional)
