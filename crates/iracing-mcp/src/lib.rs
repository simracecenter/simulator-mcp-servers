//! iRacing MCP server.
//!
//! **Status:** skeleton only. The real `IracingAdapter`/`SdkAdapter`/
//! `StubAdapter` port from `margic/iracing-mcp`, plus its tool set, is
//! tracked as its own follow-up (ADR 0001 D5 — see the "Port iracing-mcp
//! adapter/tool code into crates/iracing-mcp" project card). This crate
//! exists so the workspace shape matches ADR 0001 D1 ahead of that work.

pub mod handler;

pub use handler::IracingMcpHandler;
