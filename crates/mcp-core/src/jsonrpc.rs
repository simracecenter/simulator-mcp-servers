//! Minimal MCP JSON-RPC types and the per-simulator handler trait.
//!
//! This mirrors the hand-rolled JSON-RPC layer in `margic/iracing-mcp`
//! (`initialize` / `tools/list` / `tools/call`), generalized so any
//! `<sim>-mcp` crate can plug in its own tool set without re-implementing
//! request/response plumbing.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
}

impl JsonRpcResponse {
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: "2.0",
            id: id.unwrap_or(Value::Null),
            result: Some(result),
            error: None,
        }
    }

    pub fn err(id: Option<Value>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: "2.0",
            id: id.unwrap_or(Value::Null),
            result: None,
            error: Some(JsonRpcError {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Implemented once per simulator crate (`iracing-mcp`, `lmu-mcp`, ...).
///
/// Transports in [`crate::transport`] are generic over this trait, so adding
/// a new simulator never requires touching the stdio/HTTP plumbing.
#[async_trait]
pub trait McpHandler: Send + Sync + 'static {
    async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smallest possible handler, used to prove the JSON-RPC scaffold works
    /// end-to-end without depending on any real simulator adapter.
    struct PingHandler;

    #[async_trait]
    impl McpHandler for PingHandler {
        async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
            match request.method.as_str() {
                "ping" => JsonRpcResponse::ok(request.id, serde_json::json!("pong")),
                _ => JsonRpcResponse::err(request.id, -32601, "method not found"),
            }
        }
    }

    #[tokio::test]
    async fn ping_handler_returns_pong() {
        let handler = PingHandler;
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "ping".to_string(),
            params: Value::Null,
        };

        let response = handler.handle(request).await;

        assert_eq!(response.result, Some(Value::from("pong")));
        assert!(response.error.is_none());
    }
}
