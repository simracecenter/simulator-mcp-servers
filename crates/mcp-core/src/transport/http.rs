use std::sync::Arc;

use axum::{extract::State, routing::post, Json, Router};

use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse, McpHandler};

/// Serves a single `POST /` JSON-RPC endpoint backed by `handler`.
pub async fn run_http<H: McpHandler>(bind: &str, handler: Arc<H>) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", post(handle_request::<H>))
        .with_state(handler);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))
}

async fn handle_request<H: McpHandler>(
    State(handler): State<Arc<H>>,
    Json(request): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    Json(handler.handle(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::JsonRpcResponse;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    struct PingHandler;

    #[async_trait]
    impl McpHandler for PingHandler {
        async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
            JsonRpcResponse::ok(request.id, serde_json::json!("pong"))
        }
    }

    #[tokio::test]
    async fn http_transport_round_trips_a_request() {
        let app = Router::new()
            .route("/", post(handle_request::<PingHandler>))
            .with_state(Arc::new(PingHandler));

        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});
        let request = Request::post("/")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(parsed["result"], Value::from("pong"));
    }
}
