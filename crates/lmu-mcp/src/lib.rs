//! Le Mans Ultimate (LMU) MCP server.
//!
//! Implements `LmuAdapter`/`SdkAdapter`/`StubAdapter` per
//! [ADR 0002](../../../docs/adr/0002-lmu-adapter-design.md), following the
//! exact shape proven by `crates/iracing-mcp` (ADR 0001 D1). [`LmuMcpHandler`]
//! implements [`mcp_core::McpHandler`], holding `Arc<dyn adapter::LmuAdapter>`
//! as internal state; production code (`crates/launcher/src/runner.rs`)
//! constructs it with [`adapter::SdkAdapter`], tests use [`adapter::StubAdapter`].

pub mod adapter;
pub mod handler;

pub use handler::LmuMcpHandler;
