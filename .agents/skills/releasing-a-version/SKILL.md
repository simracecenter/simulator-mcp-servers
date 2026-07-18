---
name: releasing-a-version
description: How to cut a new release of simulator-mcp-servers — bump the version, tag it, and let the release pipeline build and publish the Windows Director Console exe to GitHub Releases. Use when asked to release a version, publish a build, or change the release workflow.
---

# Releasing a version

Releases are produced by [`.github/workflows/release.yml`](../../../.github/workflows/release.yml).
On a pushed `v*` tag (or manual `workflow_dispatch`) it builds the Director Console launcher on a
native Windows runner and publishes it to a
[GitHub Release](https://github.com/simracecenter/simulator-mcp-servers/releases).

## What ships

The one distributable binary is `crates/launcher` → `simracecenter-launcher.exe` (the Director
Console: tray UI + settings web UI). It only runs meaningfully on Windows. Each release attaches:

- `simracecenter-launcher-<tag>-x86_64-pc-windows-msvc.exe` — raw executable
- `simracecenter-launcher-<tag>-x86_64-pc-windows-msvc.zip` — exe + `LICENSE` + `README.md`
- `simracecenter-launcher-<tag>-x86_64-pc-windows-msvc.sha256` — SHA256 checksums

## How to cut a release

1. Bump `version` under `[workspace.package]` in the root `Cargo.toml`, commit **DCO-signed**
   (`git commit -s`), and merge to `main` (issue-before-code + PR per CONTRIBUTING.md).
2. Tag the release commit with a matching `v`-prefixed semver tag and push:
   ```sh
   git tag v0.1.0
   git push origin v0.1.0
   ```
3. The workflow builds the exe and creates the Release. Pre-release tags (containing `-`, e.g.
   `v0.1.0-rc.1`) publish as GitHub pre-releases.

**Version guard:** the tag minus its leading `v` MUST equal the `Cargo.toml` version, or the run
fails fast. So always bump `Cargo.toml` first, then tag the same number.

To re-build an existing tag, use the workflow's manual `workflow_dispatch` (takes a `tag` input).

## Key facts about the pipeline

- Runs on `windows-latest` with the **MSVC** target `x86_64-pc-windows-msvc` (chosen over the
  MinGW `x86_64-pc-windows-gnu` cross-compile that PR CI uses — MSVC is the canonical end-user
  Windows target). MSVC + the target are provided via `dtolnay/rust-toolchain`.
- Builds only the launcher: `cargo build --release -p launcher --target x86_64-pc-windows-msvc`.
- Publishes with `softprops/action-gh-release@v2`, `generate_release_notes: true`,
  `permissions: contents: write`. Uses the built-in `GITHUB_TOKEN` — **no extra secrets needed**
  for the current unsigned flow.

## Gotchas / conventions

- The exe is currently **unsigned** → Windows SmartScreen warns on first run. Authenticode signing
  is planned in issue #26 (recommends Azure Trusted Signing). If/when added, sign the exe *after*
  `cargo build` but *before* staging/zipping so the raw exe AND the zipped copy are both signed and
  match the SHA256 checksums.
- Keep the PR-CI cross-compile (`ci.yml`, MinGW/`windows-gnu`) as-is — it's the fast PR check; the
  release job is the only thing that needs the native MSVC build.
- Validate workflow edits with `actionlint`. You can't exercise the MSVC build on Linux; as a proxy
  in the dev container run `cargo build --release -p launcher --target x86_64-pc-windows-gnu`
  (the `windows-gnu` target is preinstalled). Real end-to-end verification = pushing a `v*` tag.
- The launcher embeds `crates/launcher/assets/logo.ico` via `include_bytes!`, so the release build
  needs no extra asset-copy step.
- Every commit needs a DCO `Signed-off-by` trailer; new source files need an SPDX header. See
  [CONTRIBUTING.md § Releasing](../../../CONTRIBUTING.md#releasing).
