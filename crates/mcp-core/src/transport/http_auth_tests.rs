use super::*;
use crate::transport::http::access::CredentialRegistry;
use async_trait::async_trait;
use axum::{body::Body, http::Request};
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;
use uuid::Uuid;

struct CountHandler(Arc<AtomicUsize>);

#[async_trait]
impl McpHandler for CountHandler {
    async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        self.0.fetch_add(1, Ordering::SeqCst);
        JsonRpcResponse::ok(request.id, json!({"ok": true}))
    }
}

fn protected() -> (Router, Arc<CredentialRegistry>, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let credentials = Arc::new(CredentialRegistry::new());
    let router = build_protected_router(Arc::new(CountHandler(calls.clone())), credentials.clone());
    (router, credentials, calls)
}

fn post(token: &str, body: Value) -> Request<Body> {
    Request::post("/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {token}"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn session(app: &Router, token: &str) -> String {
    let response = app
        .clone()
        .oneshot(post(
            token,
            json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    response.headers()[SESSION_HEADER]
        .to_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn unauthorized_requests_never_reach_handler_or_allocate_sessions() {
    let (app, _, calls) = protected();
    for method in ["POST", "GET", "DELETE"] {
        let request = Request::builder()
            .method(method)
            .uri("/mcp")
            .body(Body::from("not json"))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(SESSION_HEADER).is_none());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn tool_access_is_explicit_and_credentials_can_be_revoked() {
    let (app, registry, calls) = protected();
    let credential = registry
        .issue(["get_session_overview"], Duration::from_secs(60))
        .unwrap();
    let session = session(&app, credential.token()).await;
    for (tool, expected) in [
        ("camera_focus", StatusCode::FORBIDDEN),
        ("unknown", StatusCode::FORBIDDEN),
        ("get_session_overview", StatusCode::OK),
    ] {
        let mut request = post(
            credential.token(),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":tool}}),
        );
        request
            .headers_mut()
            .insert(SESSION_HEADER, session.parse().unwrap());
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            expected
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    registry.revoke(credential.id());
    let mut request = post(
        credential.token(),
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
    );
    request
        .headers_mut()
        .insert(SESSION_HEADER, session.parse().unwrap());
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn sessions_are_bound_to_the_issuing_credential() {
    let (app, registry, _) = protected();
    let owner = registry
        .issue(["get_session_overview"], Duration::from_secs(60))
        .unwrap();
    let other = registry
        .issue(["get_session_overview"], Duration::from_secs(60))
        .unwrap();
    let session = session(&app, owner.token()).await;
    for method in ["POST", "GET", "DELETE"] {
        let request = Request::builder()
            .method(method)
            .uri("/mcp")
            .header(header::AUTHORIZATION, format!("Bearer {}", other.token()))
            .header(header::ACCEPT, "text/event-stream")
            .header(SESSION_HEADER, &session)
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            ))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
    let request = Request::delete("/mcp")
        .header(header::AUTHORIZATION, format!("Bearer {}", owner.token()))
        .header(SESSION_HEADER, session)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::NO_CONTENT
    );
}

#[tokio::test]
async fn origin_and_ambiguous_authorization_are_denied() {
    let (app, registry, calls) = protected();
    let credential = registry
        .issue(["get_session_overview"], Duration::from_secs(60))
        .unwrap();
    let mut browser = post(
        credential.token(),
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    );
    browser
        .headers_mut()
        .insert(header::ORIGIN, "https://untrusted.example".parse().unwrap());
    assert_eq!(
        app.clone().oneshot(browser).await.unwrap().status(),
        StatusCode::FORBIDDEN
    );
    let mut duplicate = post(
        credential.token(),
        json!({"jsonrpc":"2.0","id":1,"method":"initialize"}),
    );
    duplicate
        .headers_mut()
        .append(header::AUTHORIZATION, "Bearer other".parse().unwrap());
    assert_eq!(
        app.oneshot(duplicate).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn expired_credentials_are_rejected_on_all_verbs() {
    let (app, registry, calls) = protected();
    let credential = registry
        .issue(["get_session_overview"], Duration::from_secs(1))
        .unwrap();
    let session = session(&app, credential.token()).await;
    tokio::time::advance(Duration::from_secs(2)).await;
    for method in ["POST", "GET", "DELETE"] {
        let request = Request::builder()
            .method(method)
            .uri("/mcp")
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", credential.token()),
            )
            .header(SESSION_HEADER, &session)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unclassified_methods_and_tool_notifications_are_denied() {
    let (app, registry, calls) = protected();
    let credential = registry
        .issue(["get_session_overview"], Duration::from_secs(60))
        .unwrap();
    let session = session(&app, credential.token()).await;
    for body in [
        json!({"jsonrpc":"2.0","id":2,"method":"resources/read"}),
        json!({"jsonrpc":"2.0","id":2,"method":"future_mutation"}),
        json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"get_session_overview"}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{}}),
    ] {
        let mut request = post(credential.token(), body);
        request
            .headers_mut()
            .insert(SESSION_HEADER, session.parse().unwrap());
        assert_eq!(
            app.clone().oneshot(request).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }
    let mut initialized = post(
        credential.token(),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    );
    initialized
        .headers_mut()
        .insert(SESSION_HEADER, session.parse().unwrap());
    assert_eq!(
        app.oneshot(initialized).await.unwrap().status(),
        StatusCode::ACCEPTED
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn protected_transport_requires_initialization_and_limits_bodies() {
    let (app, registry, calls) = protected();
    let credential = registry
        .issue(["get_session_overview"], Duration::from_secs(60))
        .unwrap();
    let request = post(
        credential.token(),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
    );
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let request = Request::get("/mcp")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", credential.token()),
        )
        .header(header::ACCEPT, "text/event-stream")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(request).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let request = Request::post("/mcp")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {}", credential.token()),
        )
        .body(Body::from(vec![b' '; 1024 * 1024 + 1]))
        .unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unauthorized_body_is_not_polled() {
    let (app, _, _) = protected();
    let body = Body::from_stream(tokio_stream::once(()).map(
        |_| -> Result<Bytes, std::io::Error> { panic!("unauthorized request body was read") },
    ));
    let request = Request::post("/mcp").body(body).unwrap();
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}

#[test]
fn credential_issuance_is_bounded_and_has_no_wildcard() {
    let registry = CredentialRegistry::new();
    assert!(registry.issue(["*"], Duration::from_secs(60)).is_err());
    assert!(registry.issue(["read"], Duration::ZERO).is_err());
    assert!(registry
        .issue(["read"], Duration::from_secs(86401))
        .is_err());
    let first = registry.issue(["read"], Duration::from_secs(60)).unwrap();
    for _ in 1..64 {
        registry.issue(["read"], Duration::from_secs(60)).unwrap();
    }
    assert!(registry.issue(["read"], Duration::from_secs(60)).is_err());
    registry.revoke(first.id());
    assert!(registry.issue(["read"], Duration::from_secs(60)).is_ok());
}

#[tokio::test]
async fn persistent_grants_have_no_expiry() {
    let registry = CredentialRegistry::new();
    let credential = registry.issue_persistent(["read"]).unwrap();
    let app = build_protected_router(
        Arc::new(CountHandler(Arc::new(AtomicUsize::new(0)))),
        Arc::new(registry),
    );
    assert!(!session(&app, credential.token()).await.is_empty());
}

#[tokio::test]
async fn restored_credentials_authenticate_and_revoke() {
    let registry = Arc::new(CredentialRegistry::new());
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let digest: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let id = Uuid::new_v4();
    registry.restore(id, digest, ["publisher_status"]).unwrap();
    let app = build_protected_router(
        Arc::new(CountHandler(Arc::new(AtomicUsize::new(0)))),
        registry.clone(),
    );
    let session_id = session(&app, &token).await;
    registry.revoke(id);
    let mut request = post(
        &token,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    );
    request
        .headers_mut()
        .insert(SESSION_HEADER, session_id.parse().unwrap());
    assert_eq!(
        app.oneshot(request).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
}
