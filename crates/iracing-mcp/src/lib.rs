//! iRacing MCP server.
//!
//! Ports the `IracingAdapter`/`SdkAdapter`/`StubAdapter` and full tool set
//! from `margic/iracing-mcp` (ADR 0001 D5 — see the "Port iracing-mcp
//! adapter/tool code into crates/iracing-mcp" project card). [`IracingMcpHandler`]
//! implements [`mcp_core::McpHandler`], holding `Arc<dyn adapter::IracingAdapter>`
//! as internal state; production code (`crates/launcher/src/runner.rs`)
//! constructs it with [`adapter::SdkAdapter`], tests use [`adapter::StubAdapter`].

pub mod adapter;
pub mod handler;

pub use handler::IracingMcpHandler;
