use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

        let mut encoded = serde_json::to_string(&response).unwrap_or_default();
        encoded.push('\n');
        stdout.write_all(encoded.as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}
