use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::State,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse, McpHandler};

/// Builds the `POST /mcp` + `GET /healthz` router backed by `handler`.
///
/// Exposed separately from [`run_http`] so tests and other transports can
/// exercise the router directly (e.g. via `tower::ServiceExt::oneshot`)
/// without binding a real TCP listener.
pub fn build_router<H: McpHandler>(handler: Arc<H>) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/mcp", post(handle_request::<H>))
        .with_state(handler)
}

/// Serves `POST /mcp` (JSON-RPC) and `GET /healthz` backed by `handler`.
pub async fn run_http<H: McpHandler>(bind: &str, handler: Arc<H>) -> std::io::Result<()> {
    let app = build_router(handler);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))
}

async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
}

/// Takes the raw request body rather than an `axum::Json<JsonRpcRequest>`
/// extractor so a malformed body returns a JSON-RPC `-32700` parse error in
/// a `200` envelope — matching the stdio transport — instead of axum's
/// opaque `400` with a plain-text body the JSON-RPC client can't interpret.
async fn handle_request<H: McpHandler>(
    State(handler): State<Arc<H>>,
    body: Bytes,
) -> Json<JsonRpcResponse> {
    match serde_json::from_slice::<JsonRpcRequest>(&body) {
        Ok(request) => Json(handler.handle(request).await),
        Err(error) => Json(JsonRpcResponse::err(
            None,
            -32700,
            format!("parse error: {error}"),
        )),
    }
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

    #[tokio::test]
    async fn malformed_body_returns_jsonrpc_parse_error() {
        let app = Router::new()
            .route("/", post(handle_request::<PingHandler>))
            .with_state(Arc::new(PingHandler));

        let request = Request::post("/")
            .header("content-type", "application/json")
            .body(Body::from("{ not valid json"))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        // A malformed body is reported inside a 200 JSON-RPC envelope rather
        // than an opaque transport-level 400.
        assert_eq!(response.status(), axum::http::StatusCode::OK);

        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(parsed["error"]["code"], Value::from(-32700));
        assert_eq!(parsed["id"], Value::Null);
        assert!(parsed["result"].is_null());
    }
}
