//! Shared MCP server plumbing for every `<sim>-mcp` crate in this workspace.
//!
//! Per ADR 0001 (D1), this crate owns everything that doesn't need to know
//! which simulator it's talking to: JSON-RPC request/response types, the
//! [`McpHandler`] trait each `<sim>-mcp` crate implements, the stdio/HTTP
//! transports, and the config-merge and send-poll-verify-loop helpers
//! ported/promoted from `margic/iracing-mcp` and `iracing-mcp`.

pub mod capabilities;
pub mod config;
pub mod jsonrpc;
pub mod transport;
pub mod verify;

pub use capabilities::{CapabilityStatus, ToolCapability};
pub use jsonrpc::{JsonRpcError, JsonRpcRequest, JsonRpcResponse, McpHandler};
