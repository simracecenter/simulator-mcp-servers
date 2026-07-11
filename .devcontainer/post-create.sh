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

echo "Dev container setup complete."
