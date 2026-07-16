// SPDX-License-Identifier: GPL-3.0-or-later
//! Ported from `margic/iracing-mcp` (`crates/iracing-mcp-server/tests/http_transport.rs`,
//! ADR 0001 D5). Exercises the real `IracingMcpHandler` (backed by
//! `StubAdapter`) through `mcp_core::transport::http::build_router`.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use iracing_mcp::{adapter, IracingMcpHandler};
use mcp_core::transport::http::build_router;
use serde_json::{json, Value};
use tower::ServiceExt;

fn build_app() -> axum::Router {
    let handler = Arc::new(IracingMcpHandler::new(Arc::new(
        adapter::StubAdapter::default(),
    )));
    build_router(handler)
}

// ── helper ──────────────────────────────────────────────────────────────────
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
    assert_eq!(json["result"]["isError"], Value::Bool(false));
    json["result"]["structuredContent"]["data"].clone()
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
async fn http_mcp_initialize_and_tools_call_work() {
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
        init_json["result"]["protocolVersion"],
        Value::String("2025-06-18".to_string())
    );

    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "get_session_overview",
            "arguments": {}
        }
    });

    let call_res = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(call_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(call_res.status(), StatusCode::OK);
    let call_body = to_bytes(call_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let call_json: Value = serde_json::from_slice(&call_body).expect("json body");

    assert_eq!(call_json["result"]["content"][0]["type"], "text");
    assert_eq!(
        call_json["result"]["structuredContent"]["data"]["connected"],
        Value::Bool(true)
    );
}

#[tokio::test]
async fn http_mcp_malformed_body_returns_jsonrpc_parse_error() {
    let app = build_app();

    let res = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from("{ this is not valid json"))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    // The malformed body is surfaced as a JSON-RPC parse error inside a 200
    // envelope, not an opaque transport-level 400 the client can't parse.
    assert_eq!(res.status(), StatusCode::OK);
    let body = to_bytes(res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let json: Value = serde_json::from_slice(&body).expect("json body");

    assert_eq!(json["error"]["code"], Value::from(-32700));
    assert_eq!(json["id"], Value::Null);
    assert!(json["result"].is_null());
}

#[tokio::test]
async fn http_mcp_replay_tools_work() {
    let app = build_app();

    let state_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": "replay_get_state",
            "arguments": {}
        }
    });

    let state_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(state_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(state_res.status(), StatusCode::OK);
    let state_body = to_bytes(state_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let state_json: Value = serde_json::from_slice(&state_body).expect("json body");
    assert_eq!(
        state_json["result"]["structuredContent"]["data"]["replayPlaySpeed"],
        Value::from(1)
    );

    let playback_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "replay_set_playback",
            "arguments": {
                "speed": 0,
                "slowMotion": false
            }
        }
    });

    let playback_res = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(playback_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(playback_res.status(), StatusCode::OK);
    let playback_body = to_bytes(playback_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let playback_json: Value = serde_json::from_slice(&playback_body).expect("json body");

    assert_eq!(
        playback_json["result"]["structuredContent"]["data"]["verified"],
        Value::Bool(true)
    );
    assert_eq!(
        playback_json["result"]["structuredContent"]["data"]["observed"]["replayPlaySpeed"],
        Value::from(0)
    );
    assert_eq!(
        playback_json["result"]["structuredContent"]["data"]["observed"]["isReplayPlaying"],
        Value::Bool(false)
    );
}

#[tokio::test]
async fn http_mcp_camera_and_timeline_tools_work() {
    let app = build_app();

    let camera_req = json!({
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "camera_focus",
            "arguments": {
                "carIdx": 12,
                "groupNumber": 3,
                "cameraNumber": 2
            }
        }
    });

    let camera_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(camera_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(camera_res.status(), StatusCode::OK);
    let camera_body = to_bytes(camera_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let camera_json: Value = serde_json::from_slice(&camera_body).expect("json body");
    assert_eq!(
        camera_json["result"]["structuredContent"]["data"]["verified"],
        Value::Bool(true)
    );
    assert_eq!(
        camera_json["result"]["structuredContent"]["data"]["observed"]["camCarIdx"],
        Value::from(12)
    );
    assert_eq!(
        camera_json["result"]["structuredContent"]["data"]["observed"]["camGroupNumber"],
        Value::from(3)
    );
    assert_eq!(
        camera_json["result"]["structuredContent"]["data"]["observed"]["camCameraNumber"],
        Value::from(2)
    );

    let seek_req = json!({
        "jsonrpc": "2.0",
        "id": 4,
        "method": "tools/call",
        "params": {
            "name": "replay_seek_session_time",
            "arguments": {
                "sessionNum": 0,
                "sessionTimeMs": 120000,
                "toleranceMs": 100
            }
        }
    });

    let seek_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(seek_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(seek_res.status(), StatusCode::OK);
    let seek_body = to_bytes(seek_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let seek_json: Value = serde_json::from_slice(&seek_body).expect("json body");
    assert_eq!(
        seek_json["result"]["structuredContent"]["data"]["verified"],
        Value::Bool(true)
    );
    assert_eq!(
        seek_json["result"]["structuredContent"]["data"]["observed"]["replaySessionNum"],
        Value::from(0)
    );

    let seek_frame_req = json!({
        "jsonrpc": "2.0",
        "id": 5,
        "method": "tools/call",
        "params": {
            "name": "replay_seek_frame",
            "arguments": {
                "mode": "current",
                "frame": 120,
                "toleranceFrames": 1
            }
        }
    });

    let seek_frame_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(seek_frame_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(seek_frame_res.status(), StatusCode::OK);
    let seek_frame_body = to_bytes(seek_frame_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let seek_frame_json: Value = serde_json::from_slice(&seek_frame_body).expect("json body");
    assert_eq!(
        seek_frame_json["result"]["structuredContent"]["data"]["verified"],
        Value::Bool(true)
    );

    let search_event_req = json!({
        "jsonrpc": "2.0",
        "id": 6,
        "method": "tools/call",
        "params": {
            "name": "replay_search_event",
            "arguments": {
                "mode": "next_frame"
            }
        }
    });

    let search_event_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(search_event_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(search_event_res.status(), StatusCode::OK);
    let search_event_body = to_bytes(search_event_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let search_event_json: Value = serde_json::from_slice(&search_event_body).expect("json body");
    assert_eq!(
        search_event_json["result"]["structuredContent"]["data"]["verified"],
        Value::Bool(true)
    );

    let set_state_req = json!({
        "jsonrpc": "2.0",
        "id": 7,
        "method": "tools/call",
        "params": {
            "name": "camera_set_state",
            "arguments": {
                "camToolActive": true,
                "uiHidden": true
            }
        }
    });

    let set_state_res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(set_state_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(set_state_res.status(), StatusCode::OK);
    let set_state_body = to_bytes(set_state_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let set_state_json: Value = serde_json::from_slice(&set_state_body).expect("json body");
    assert_eq!(
        set_state_json["result"]["structuredContent"]["data"]["verified"],
        Value::Bool(true)
    );
    assert_eq!(
        set_state_json["result"]["structuredContent"]["data"]["observed"]["camCameraState"],
        Value::from(12)
    );

    let show_window_req = json!({
        "jsonrpc": "2.0",
        "id": 8,
        "method": "tools/call",
        "params": {
            "name": "replay_show_window",
            "arguments": {
                "sessionNum": 0,
                "startTimeMs": 300000,
                "focusCarIdx": 7,
                "cameraGroupNum": 2,
                "speed": 1,
                "timeoutMs": 1000
            }
        }
    });

    let show_window_res = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(show_window_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(show_window_res.status(), StatusCode::OK);
    let show_window_body = to_bytes(show_window_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let show_window_json: Value = serde_json::from_slice(&show_window_body).expect("json body");
    assert_eq!(
        show_window_json["result"]["structuredContent"]["data"]["verified"],
        Value::Bool(true)
    );
    assert_eq!(
        show_window_json["result"]["structuredContent"]["data"]["finalState"]["camCarIdx"],
        Value::from(7)
    );
}

#[tokio::test]
async fn http_mcp_m1_read_tools_work() {
    // get_weekend_info
    let wi = mcp_call("get_weekend_info", json!({})).await;
    assert_eq!(wi["trackDisplayName"], Value::String("Stub Track".into()));
    assert_eq!(
        wi["trackLengthKm"],
        Value::Number(serde_json::Number::from_f64(5.0).unwrap())
    );

    // get_roster
    let roster = mcp_call("get_roster", json!({})).await;
    assert_eq!(roster["count"], Value::Number(2.into()));
    assert_eq!(
        roster["entries"][0]["userName"],
        Value::String("Alice Driver".into())
    );
    assert_eq!(roster["entries"][1]["carIdx"], Value::Number(7.into()));

    // get_camera_groups
    let cg = mcp_call("get_camera_groups", json!({})).await;
    assert_eq!(cg["count"], Value::Number(2.into()));
    assert_eq!(cg["groups"][0]["groupName"], Value::String("TV1".into()));
    assert_eq!(cg["groups"][1]["isScenic"], Value::Bool(true));

    // get_standings
    let st = mcp_call("get_standings", json!({})).await;
    assert_eq!(st["sessionType"], Value::String("Practice".into()));
    assert_eq!(st["positions"][0]["carIdx"], Value::Number(7.into()));
    assert_eq!(st["positions"][0]["position"], Value::Number(1.into()));

    // get_relatives
    let rel = mcp_call("get_relatives", json!({})).await;
    assert_eq!(rel["basis"], Value::String("track".into()));
    assert_eq!(rel["count"], Value::Number(2.into()));
    assert_eq!(rel["entries"][0]["carIdx"], Value::Number(7.into()));
    assert_eq!(rel["entries"][0]["position"], Value::Number(1.into()));

    // resolve_driver — exact match
    let rd = mcp_call("resolve_driver", json!({ "query": "alice driver" })).await;
    assert_eq!(rd["bestMatch"]["carIdx"], Value::Number(0.into()));
    assert_eq!(
        rd["bestMatch"]["matchReason"],
        Value::String("exact".into())
    );

    // resolve_driver — prefix match
    let rd2 = mcp_call("resolve_driver", json!({ "query": "bob" })).await;
    assert_eq!(rd2["bestMatch"]["carIdx"], Value::Number(7.into()));

    // resolve_driver — car number
    let rd3 = mcp_call("resolve_driver", json!({ "query": "7" })).await;
    assert_eq!(rd3["bestMatch"]["carIdx"], Value::Number(7.into()));
}

#[tokio::test]
async fn http_mcp_tool_errors_use_text_and_structured_content() {
    let app = build_app();

    let bad_req = json!({
        "jsonrpc": "2.0",
        "id": 9,
        "method": "tools/call",
        "params": {
            "name": "camera_focus",
            "arguments": {}
        }
    });

    let bad_res = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(bad_req.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(bad_res.status(), StatusCode::OK);
    let bad_body = to_bytes(bad_res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let bad_json: Value = serde_json::from_slice(&bad_body).expect("json body");

    assert_eq!(
        bad_json["result"]["content"][0]["type"],
        Value::String("text".into())
    );
    assert!(bad_json["result"]["content"][0]["text"].is_string());
    assert_eq!(bad_json["result"]["isError"], Value::Bool(true));
    assert_eq!(
        bad_json["result"]["structuredContent"]["ok"],
        Value::Bool(false)
    );
    assert_eq!(
        bad_json["result"]["structuredContent"]["error"]["code"],
        Value::String("invalid_arguments".into())
    );
}
