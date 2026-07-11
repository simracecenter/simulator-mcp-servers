//! Transports that carry MCP JSON-RPC traffic to/from an [`McpHandler`].
//!
//! Both transports are generic over the handler trait, matching the
//! `margic/iracing-mcp` precedent (stdio for local agent runners, HTTP for
//! remote/LAN agent hosts — ADR 0001 §2.1).

pub mod http;
pub mod stdio;
