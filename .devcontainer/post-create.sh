#!/usr/bin/env bash
set -euo pipefail

echo "Running post-create setup for Rust MCP Servers dev container..."

# Verify required tooling
command -v rustc >/dev/null 2>&1 && echo "rustc: $(rustc --version)"
command -v cargo >/dev/null 2>&1 && echo "cargo: $(cargo --version)"
command -v gh >/dev/null 2>&1 && echo "gh: $(gh --version | head -n 1)"
command -v docker >/dev/null 2>&1 && echo "docker: $(docker --version)"

# Install/update Rust Analyzer if not already present
if ! command -v rust-analyzer >/dev/null 2>&1; then
    echo "Installing rust-analyzer..."
    rustup component add rust-analyzer
fi

# Pre-fetch dependencies for the workspace to warm up the build cache
if [ -f "Cargo.toml" ]; then
    echo "Fetching Cargo dependencies..."
    cargo fetch
fi

# Remind about Windows cross-compilation
if rustup target list --installed | grep -q "x86_64-pc-windows-gnu"; then
    echo "Windows GNU target is installed for cross-compilation."
fi

# Set recommended git aliases for the project
if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    git config --local advice.detachedHead false 2>/dev/null || true
fi

# Wire `gh` to use a GH_PROJECTS_TOKEN Codespaces secret, if one is configured.
#
# The default Codespaces GITHUB_TOKEN is repo-scoped and cannot read/write org-level
# Project v2 boards even for org owners. If you've added a personal access token
# (scopes: repo, project, read:org) as a Codespaces secret named GH_PROJECTS_TOKEN,
# this makes `gh project ...` / `gh api graphql ...` use it automatically by
# exporting it as GH_TOKEN, which `gh` prefers over GITHUB_TOKEN.
#
# See docs/adr/README.md or ask the "Get Next Card" agent if `gh project` commands
# still fail with a permissions error after this.
GH_PROJECTS_SNIPPET_MARKER="# >>> GH_PROJECTS_TOKEN wiring >>>"
for rc_file in "$HOME/.bashrc" "$HOME/.zshrc"; do
    [ -f "$rc_file" ] || continue
    if ! grep -qF "$GH_PROJECTS_SNIPPET_MARKER" "$rc_file"; then
        cat >>"$rc_file" <<EOF

$GH_PROJECTS_SNIPPET_MARKER
if [ -n "\${GH_PROJECTS_TOKEN:-}" ]; then
    export GH_TOKEN="\$GH_PROJECTS_TOKEN"
fi
# <<< GH_PROJECTS_TOKEN wiring <<<
EOF
    fi
done

if [ -n "${GH_PROJECTS_TOKEN:-}" ]; then
    echo "GH_PROJECTS_TOKEN found — gh will use it (as GH_TOKEN) for org Project v2 access."
else
    echo "GH_PROJECTS_TOKEN not set — 'gh project' commands against the org board may fail."
    echo "Add a Codespaces secret named GH_PROJECTS_TOKEN (a classic PAT with repo, project," \
         "read:org scopes) and rebuild the codespace to enable org Project v2 access."
fi

echo "Dev container setup complete."
