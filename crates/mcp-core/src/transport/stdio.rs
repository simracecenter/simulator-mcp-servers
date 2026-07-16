use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::error;

use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse, McpHandler};

/// Reads newline-delimited JSON-RPC requests from stdin and writes
/// newline-delimited responses to stdout, one line per message.
pub async fn run_stdio<H: McpHandler>(handler: Arc<H>) -> std::io::Result<()> {
    let stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut lines = BufReader::new(stdin).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(request) => handler.handle(request).await,
            Err(error) => JsonRpcResponse::err(None, -32700, format!("parse error: {error}")),
        };

        // Never drop a response into a silent blank line: if the handler's
        // response can't be serialized, emit a well-formed JSON-RPC internal
        // error (preserving the request id) so the client sees a failure
        // rather than an empty message.
        let mut encoded = serialize_response(response);
        encoded.push('\n');
        stdout.write_all(encoded.as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}

/// Serializes a response to a JSON string, falling back to a `-32603`
/// internal-error envelope (with the original id) if the response itself
/// can't be serialized, and finally to a constant error string if even that
/// fails. Either fallback is logged so the failure isn't silent.
fn serialize_response(response: JsonRpcResponse) -> String {
    match serde_json::to_string(&response) {
        Ok(encoded) => encoded,
        Err(error) => {
            error!(%error, "failed to serialize JSON-RPC response");
            let fallback = JsonRpcResponse::err(
                Some(response.id),
                -32603,
                "internal error: failed to serialize response",
            );
            serde_json::to_string(&fallback).unwrap_or_else(|error| {
                error!(%error, "failed to serialize JSON-RPC error fallback");
                r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"internal error: failed to serialize response"}}"#
                    .to_string()
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    #[test]
    fn serialize_response_round_trips_a_normal_response() {
        let response = JsonRpcResponse::ok(Some(Value::from(7)), serde_json::json!("pong"));
        let encoded = serialize_response(response);
        let parsed: Value = serde_json::from_str(&encoded).expect("valid JSON");

        assert_eq!(parsed["id"], Value::from(7));
        assert_eq!(parsed["result"], Value::from("pong"));
    }
}
