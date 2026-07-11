# Rust MCP Servers Dev Container
# Linux-based development environment for building Rust MCP servers.
# Pre-configured for cross-compilation to Windows and container-based testing.

FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# Install system dependencies for cross-compilation, testing, and general tooling.
# mingw-w64 enables Linux-hosted builds for x86_64-pc-windows-gnu.
RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get install -y --no-install-recommends \
        mingw-w64 \
        libssl-dev \
        pkg-config \
        valgrind \
        lldb \
        jq \
        ca-certificates \
        curl \
        gnupg \
        lsb-release \
    && apt-get clean \
    && rm -rf /var/lib/apt/lists/*

# Add Windows targets for Rust to support builds that ultimately run on Windows machines.
# The x86_64-pc-windows-gnu target uses the installed MinGW toolchain and can be linked
# directly from this Linux container. The MSVC target is omitted here because linking
# MSVC binaries requires the Windows Visual C++ toolchain.
RUN rustup target add x86_64-pc-windows-gnu \
    && rustup component add rustfmt clippy rust-src

# Ensure the vscode user owns the cargo registry volume mount point
RUN mkdir -p /usr/local/cargo/registry && chown -R vscode:vscode /usr/local/cargo

# Set the default shell
ENV SHELL=/bin/zsh
