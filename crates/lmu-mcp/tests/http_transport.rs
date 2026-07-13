// SPDX-License-Identifier: GPL-3.0-or-later
//! Mirrors `crates/iracing-mcp/tests/http_transport.rs`. Exercises the real
//! `LmuMcpHandler` (backed by `StubAdapter`) through
//! `mcp_core::transport::http::build_router`. No live-LMU test is possible
//! in CI — that gap is covered by this issue's manual done criterion, not
//! an automated test.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use lmu_mcp::{adapter, LmuMcpHandler};
use mcp_core::transport::http::build_router;
use serde_json::{json, Value};
use tower::ServiceExt;

fn build_app() -> axum::Router {
    let handler = Arc::new(LmuMcpHandler::new(
        Arc::new(adapter::StubAdapter::default()),
    ));
    build_router(handler)
}

async fn mcp_call(name: &str, arguments: Value) -> Value {
    let app = build_app();
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let res = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        json["result"]["content"][0]["type"],
        Value::String("text".into())
    );
    assert!(json["result"]["content"][0]["text"].is_string());
    json["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn http_healthz_works() {
    let app = build_app();

    let response = app
        .oneshot(
            Request::builder()
                .uri("/healthz")
                .method("GET")
                .body(Body::empty())
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn http_mcp_initialize_and_tools_list_work() {
    let app = build_app();

    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {}
    });

    let init_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(init_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(init_res.status(), StatusCode::OK);
    let init_body = to_bytes(init_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let init_json: Value = serde_json::from_slice(&init_body).expect("json body");
    assert_eq!(
        init_json["result"]["serverInfo"]["name"],
        Value::String("lmu-mcp".to_string())
    );

    let list_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/list",
        "params": {}
    });

    let list_res = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(list_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(list_res.status(), StatusCode::OK);
    let list_body = to_bytes(list_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let list_json: Value = serde_json::from_slice(&list_body).expect("json body");
    let tools = list_json["result"]["tools"]
        .as_array()
        .expect("tools array");
    assert_eq!(tools.len(), 12);
}

#[tokio::test]
async fn http_mcp_get_session_overview_reports_connected() {
    let data = mcp_call("get_session_overview", json!({})).await;
    assert_eq!(data["ok"], Value::Bool(true));
    assert_eq!(data["data"]["connected"], Value::Bool(true));
}

#[tokio::test]
async fn http_mcp_get_standings_returns_positions() {
    let data = mcp_call("get_standings", json!({})).await;
    assert_eq!(data["ok"], Value::Bool(true));
    assert!(data["data"]["positions"].as_array().unwrap().len() >= 2);
}

#[tokio::test]
async fn http_mcp_set_weather_verifies() {
    let data = mcp_call("set_weather", json!({ "raining": 0.3 })).await;
    assert_eq!(data["ok"], Value::Bool(true));
    assert_eq!(data["data"]["verified"], Value::Bool(true));
}

#[tokio::test]
async fn http_mcp_camera_focus_returns_not_supported() {
    let app = build_app();
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "camera_focus", "arguments": { "carIdx": 0 } }
    });
    let res = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = to_bytes(res.into_body(), 1024 * 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["result"]["isError"], Value::Bool(true));
    assert_eq!(
        json["result"]["structuredContent"]["error"]["code"],
        Value::String("not_supported".to_string())
    );
}
