use async_trait::async_trait;
use mcp_core::{JsonRpcRequest, JsonRpcResponse, McpHandler};
use serde_json::json;

/// Placeholder [`McpHandler`] for iRacing.
///
/// Answers `initialize`/`tools/list` so a client can connect, but every real
/// tool call currently returns a "not implemented yet" error until the
/// adapter/tool migration (ADR 0001 D5) lands.
#[derive(Default)]
pub struct IracingMcpHandler;

#[async_trait]
impl McpHandler for IracingMcpHandler {
    async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        match request.method.as_str() {
            "initialize" => JsonRpcResponse::ok(
                request.id,
                json!({
                    "protocolVersion": "2025-06-18",
                    "serverInfo": { "name": "iracing-mcp", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "tools": { "listChanged": true } }
                }),
            ),
            "tools/list" => JsonRpcResponse::ok(request.id, json!({ "tools": [] })),
            _ => JsonRpcResponse::err(
                request.id,
                -32601,
                "iracing-mcp tool set not yet migrated from margic/iracing-mcp (ADR 0001 D5)",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[tokio::test]
    async fn tools_list_is_empty_until_migration_lands() {
        let handler = IracingMcpHandler;
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/list".to_string(),
            params: Value::Null,
        };

        let response = handler.handle(request).await;

        assert_eq!(response.result.unwrap()["tools"], json!([]));
    }
}
