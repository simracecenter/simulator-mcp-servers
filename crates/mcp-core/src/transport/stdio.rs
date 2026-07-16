use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

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

        let mut encoded = serde_json::to_string(&response).unwrap_or_default();
        encoded.push('\n');
        writer.write_all(encoded.as_bytes()).await?;
        writer.flush().await?;
    }

    Ok(())
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
}
