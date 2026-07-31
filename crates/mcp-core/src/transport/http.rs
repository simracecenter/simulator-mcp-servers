use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::Bytes,
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use tokio_stream::{wrappers::ReceiverStream, StreamExt};

use crate::jsonrpc::{JsonRpcRequest, JsonRpcResponse, McpHandler};
use crate::transport::session::{SessionRegistry, StreamError};

/// Header carrying the MCP Streamable HTTP session id in both directions.
const SESSION_HEADER: &str = "mcp-session-id";
/// Keeps idle SSE streams alive through proxies and client read timeouts.
const SSE_KEEPALIVE: Duration = Duration::from_secs(15);

struct AppState<H> {
    handler: Arc<H>,
    sessions: Arc<SessionRegistry>,
}

// Derived `Clone` would require `H: Clone`; the handler is behind an `Arc`.
impl<H> Clone for AppState<H> {
    fn clone(&self) -> Self {
        Self {
            handler: Arc::clone(&self.handler),
            sessions: Arc::clone(&self.sessions),
        }
    }
}

/// Builds the MCP Streamable HTTP router (`POST`/`GET`/`DELETE /mcp` plus
/// `GET /healthz`) backed by `handler`.
///
/// `GET /mcp` serves the server-to-client SSE stream the MCP Streamable HTTP
/// transport requires; without it clients such as `mcp`'s `streamable_http`
/// (used by `google-adk`) terminate the session after their first exchange.
///
/// Exposed separately from [`run_http`] so tests and other transports can
/// exercise the router directly (e.g. via `tower::ServiceExt::oneshot`)
/// without binding a real TCP listener.
pub fn build_router<H: McpHandler>(handler: Arc<H>) -> Router {
    let state = AppState {
        handler,
        sessions: Arc::new(SessionRegistry::new()),
    };

    Router::new()
        .route("/healthz", get(healthz))
        .route(
            "/mcp",
            get(open_stream::<H>)
                .post(handle_request::<H>)
                .delete(close_session::<H>),
        )
        .with_state(state)
}

/// Serves MCP Streamable HTTP on `/mcp` and `GET /healthz`, backed by
/// `handler`.
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
///
/// A request without an `id` is a notification or a client response: the
/// Streamable HTTP spec requires an empty `202 Accepted` for those rather than
/// a JSON-RPC envelope with a null id.
async fn handle_request<H: McpHandler>(
    State(state): State<AppState<H>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if let Some(session) = session_id(&headers) {
        if !state.sessions.contains(&session) {
            return unknown_session();
        }
    }

    let request = match serde_json::from_slice::<JsonRpcRequest>(&body) {
        Ok(request) => request,
        Err(error) => {
            return Json(JsonRpcResponse::err(
                None,
                -32700,
                format!("parse error: {error}"),
            ))
            .into_response()
        }
    };

    if request.id.is_none() {
        state.handler.handle(request).await;
        return StatusCode::ACCEPTED.into_response();
    }

    let issue_session = request.method == "initialize";
    let response = Json(state.handler.handle(request).await);

    if issue_session {
        let session = state.sessions.create();
        return ([(SESSION_HEADER, session)], response).into_response();
    }

    response.into_response()
}

/// Serves the server-to-client half of the transport: an SSE stream that stays
/// open for the life of the session.
async fn open_stream<H: McpHandler>(
    State(state): State<AppState<H>>,
    headers: HeaderMap,
) -> Response {
    if !accepts_event_stream(&headers) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "GET /mcp requires Accept: text/event-stream",
        )
            .into_response();
    }

    // Clients may open the stream before they hold a session id; give them an
    // anonymous stream rather than failing the session outright.
    let session = match session_id(&headers) {
        Some(session) => session,
        None => state.sessions.create(),
    };

    let receiver = match state.sessions.take_stream(&session) {
        Ok(receiver) => receiver,
        Err(StreamError::UnknownSession) => return unknown_session(),
        Err(StreamError::AlreadyStreaming) => {
            return (
                StatusCode::CONFLICT,
                "session already has an open event stream",
            )
                .into_response()
        }
    };

    let events = ReceiverStream::new(receiver).map(|message| {
        Ok::<Event, Infallible>(Event::default().event("message").data(message.to_string()))
    });

    (
        [(SESSION_HEADER, session)],
        Sse::new(events).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE)),
    )
        .into_response()
}

/// Explicit session teardown (`DELETE /mcp`), ending any open SSE stream.
async fn close_session<H: McpHandler>(
    State(state): State<AppState<H>>,
    headers: HeaderMap,
) -> Response {
    match session_id(&headers) {
        Some(session) if state.sessions.remove(&session) => StatusCode::NO_CONTENT.into_response(),
        _ => unknown_session(),
    }
}

fn session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(SESSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn accepts_event_stream(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.contains("text/event-stream"))
}

/// `404` tells the client its session is gone so it re-`initialize`s, instead
/// of retrying forever against a session the server has forgotten.
fn unknown_session() -> Response {
    (StatusCode::NOT_FOUND, "unknown mcp session").into_response()
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

    fn router() -> Router {
        build_router(Arc::new(PingHandler))
    }

    fn post_json(body: Value) -> Request<Body> {
        Request::post("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    async fn initialize(app: &Router) -> String {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize"});
        let response = app.clone().oneshot(post_json(body)).await.unwrap();

        response
            .headers()
            .get(SESSION_HEADER)
            .expect("session header")
            .to_str()
            .unwrap()
            .to_owned()
    }

    #[tokio::test]
    async fn http_transport_round_trips_a_request() {
        let body = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"});

        let response = router().oneshot(post_json(body)).await.unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let parsed: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(parsed["result"], Value::from("pong"));
    }

    #[tokio::test]
    async fn initialize_issues_a_session_id() {
        let app = router();
        let session = initialize(&app).await;

        assert!(!session.is_empty());

        // A follow-up call on that session is accepted.
        let request = Request::post("/mcp")
            .header("content-type", "application/json")
            .header(SESSION_HEADER, &session)
            .body(Body::from(
                serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}).to_string(),
            ))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_serves_an_event_stream_for_a_session() {
        let app = router();
        let session = initialize(&app).await;

        let request = Request::get("/mcp")
            .header(header::ACCEPT, "text/event-stream")
            .header(SESSION_HEADER, &session)
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/event-stream"));
    }

    #[tokio::test]
    async fn get_without_event_stream_accept_is_not_acceptable() {
        let request = Request::get("/mcp")
            .header(header::ACCEPT, "application/json")
            .body(Body::empty())
            .unwrap();

        let response = router().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_ACCEPTABLE);
    }

    #[tokio::test]
    async fn unknown_session_is_rejected() {
        let app = router();

        let get = Request::get("/mcp")
            .header(header::ACCEPT, "text/event-stream")
            .header(SESSION_HEADER, "nope")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(get).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );

        let post = Request::post("/mcp")
            .header("content-type", "application/json")
            .header(SESSION_HEADER, "nope")
            .body(Body::from(
                serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}).to_string(),
            ))
            .unwrap();
        assert_eq!(
            app.oneshot(post).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn notifications_are_accepted_without_a_body() {
        let body = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});

        let response = router().oneshot(post_json(body)).await.unwrap();

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.is_empty());
    }

    #[tokio::test]
    async fn delete_ends_the_session() {
        let app = router();
        let session = initialize(&app).await;

        let delete = Request::delete("/mcp")
            .header(SESSION_HEADER, &session)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(delete).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );

        let get = Request::get("/mcp")
            .header(header::ACCEPT, "text/event-stream")
            .header(SESSION_HEADER, &session)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(get).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn malformed_body_returns_jsonrpc_parse_error() {
        let request = Request::post("/mcp")
            .header("content-type", "application/json")
            .body(Body::from("{ not valid json"))
            .unwrap();

        let response = router().oneshot(request).await.unwrap();
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
