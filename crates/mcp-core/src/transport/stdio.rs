use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tracing::error;

use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse, McpHandler};

/// Reads newline-delimited JSON-RPC requests from stdin and writes
/// newline-delimited responses to stdout, one line per message.
pub async fn run_stdio<H: McpHandler>(handler: Arc<H>) -> std::io::Result<()> {
    run_stdio_with_io(tokio::io::stdin(), tokio::io::stdout(), handler).await
}

async fn run_stdio_with_io<R, W, H>(
    reader: R,
    mut writer: W,
    handler: Arc<H>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    H: McpHandler,
{
    let mut lines = BufReader::new(reader).lines();
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
        writer.write_all(encoded.as_bytes()).await?;
        writer.flush().await?;
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
    use async_trait::async_trait;
    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct PingHandler;

    #[async_trait]
    impl McpHandler for PingHandler {
        async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
            JsonRpcResponse::ok(request.id, json!("pong"))
        }
    }

    #[tokio::test]
    async fn stdio_handles_requests_blank_lines_and_parse_errors() {
        let (client, server) = tokio::io::duplex(4096);
        let (server_reader, server_writer) = tokio::io::split(server);
        let task = tokio::spawn(run_stdio_with_io(
            server_reader,
            server_writer,
            Arc::new(PingHandler),
        ));
        let (mut client_reader, mut client_writer) = tokio::io::split(client);

        client_writer
            .write_all(b"\n{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"ping\"}\nnot-json\n")
            .await
            .unwrap();
        client_writer.shutdown().await.unwrap();

        let mut output = String::new();
        client_reader.read_to_string(&mut output).await.unwrap();
        task.await.unwrap().unwrap();

        let responses: Vec<Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], 7);
        assert_eq!(responses[0]["result"], "pong");
        assert_eq!(responses[1]["id"], Value::Null);
        assert_eq!(responses[1]["error"]["code"], -32700);
        assert!(responses[1]["error"]["message"]
            .as_str()
            .unwrap()
            .starts_with("parse error:"));
    }

    #[test]
    fn serialize_response_round_trips_a_normal_response() {
        let response = JsonRpcResponse::ok(Some(Value::from(7)), serde_json::json!("pong"));
        let encoded = serialize_response(response);
        let parsed: Value = serde_json::from_str(&encoded).expect("valid JSON");

        assert_eq!(parsed["id"], Value::from(7));
        assert_eq!(parsed["result"], Value::from("pong"));
    }
}
