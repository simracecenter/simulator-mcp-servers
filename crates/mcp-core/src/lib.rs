//! Shared MCP server plumbing for every `<sim>-mcp` crate in this workspace.
//!
//! Per ADR 0001 (D1), this crate owns everything that doesn't need to know
//! which simulator it's talking to: JSON-RPC request/response types, the
//! [`McpHandler`] trait each `<sim>-mcp` crate implements, the stdio/HTTP
//! transports, and (later) the config-merge and telemetry-verification-loop
//! helpers ported from `margic/iracing-mcp`.

pub mod config;
pub mod jsonrpc;
pub mod transport;

pub use jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpHandler};
