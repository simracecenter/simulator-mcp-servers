// SPDX-License-Identifier: GPL-3.0-or-later
//! Ported from `margic/iracing-mcp` (`crates/iracing-mcp-server/tests/live_mcp_suite.rs`,
//! ADR 0001 D5). Requires live iRacing replay/spectator mode — kept `#[ignore]`,
//! not run in CI.

use std::sync::Arc;

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use iracing_mcp::{adapter, IracingMcpHandler};
use mcp_core::transport::http::build_router;
use serde_json::{json, Value};
use tower::ServiceExt;

#[tokio::test]
#[ignore = "requires live iRacing replay/spectator mode"]
async fn live_mcp_camera_focus_switches_target_and_camera() {
    let app = build_live_app();
    let before = call_tool(app.clone(), "replay_get_state", json!({})).await;
    assert!(!before["isOnTrack"].as_bool().unwrap_or(true));
    assert!(!before["isInGarage"].as_bool().unwrap_or(true));

    let current_car_idx = before["camCarIdx"].as_i64().expect("camCarIdx as i64") as i32;
    let target_car_idx = choose_alternate_car_idx(&app, current_car_idx)
        .await
        .expect("an alternate focus car should exist");

    let focused_car = call_tool(
        app.clone(),
        "camera_focus",
        json!({
            "carIdx": target_car_idx,
        }),
    )
    .await;
    assert_eq!(focused_car["verified"], Value::Bool(true));
    assert_eq!(
        focused_car["observed"]["camCarIdx"],
        Value::from(target_car_idx)
    );

    let restored = call_tool(
        app,
        "camera_focus",
        json!({
            "carIdx": current_car_idx,
        }),
    )
    .await;
    assert_eq!(restored["verified"], Value::Bool(true));
    assert_eq!(
        restored["observed"]["camCarIdx"],
        Value::from(current_car_idx)
    );
}

#[tokio::test]
#[ignore = "requires live iRacing replay/spectator mode"]
async fn live_mcp_replay_seek_session_time_verifies_and_restores() {
    let app = build_live_app();
    let before = call_tool(app.clone(), "replay_get_state", json!({})).await;
    assert!(!before["isOnTrack"].as_bool().unwrap_or(true));
    assert!(!before["isInGarage"].as_bool().unwrap_or(true));

    let session_num = before["replaySessionNum"]
        .as_i64()
        .expect("replaySessionNum as i64") as i32;
    let original_time_ms = (before["replaySessionTime"]
        .as_f64()
        .expect("replaySessionTime as f64")
        * 1000.0)
        .round() as i32;
    let target_time_ms = if original_time_ms > 2_000 {
        original_time_ms - 2_000
    } else {
        original_time_ms + 2_000
    };

    let seek = call_tool(
        app.clone(),
        "replay_seek_session_time",
        json!({
            "sessionNum": session_num,
            "sessionTimeMs": target_time_ms,
            "toleranceMs": 2000
        }),
    )
    .await;
    assert_eq!(seek["verified"], Value::Bool(true));

    let restored = call_tool(
        app,
        "replay_seek_session_time",
        json!({
            "sessionNum": session_num,
            "sessionTimeMs": original_time_ms,
            "toleranceMs": 2000
        }),
    )
    .await;
    assert_eq!(restored["verified"], Value::Bool(true));
}

#[tokio::test]
#[ignore = "requires live iRacing replay/spectator mode"]
async fn live_mcp_camera_set_state_verifies_and_restores() {
    let app = build_live_app();
    let before = call_tool(app.clone(), "replay_get_state", json!({})).await;
    assert!(!before["isOnTrack"].as_bool().unwrap_or(true));
    assert!(!before["isInGarage"].as_bool().unwrap_or(true));

    let before_state = before["camCameraState"]
        .as_i64()
        .expect("camCameraState as i64") as i32;
    let set = call_tool(
        app.clone(),
        "camera_set_state",
        json!({
            "camToolActive": true,
            "uiHidden": true
        }),
    )
    .await;
    assert_eq!(set["verified"], Value::Bool(true));
    let observed_state = set["observed"]["camCameraState"]
        .as_i64()
        .expect("camCameraState as i64") as i32;
    assert_eq!(observed_state & 0x0C, 0x0C);

    let restore = call_tool(
        app,
        "camera_set_state",
        json!({
            "camToolActive": (before_state & 0x04) != 0,
            "uiHidden": (before_state & 0x08) != 0,
        }),
    )
    .await;
    assert_eq!(restore["verified"], Value::Bool(true));
}

#[tokio::test]
#[ignore = "requires live iRacing replay/spectator mode"]
async fn live_mcp_replay_search_to_end_verifies_after_seek_back() {
    let app = build_live_app();
    let before = call_tool(app.clone(), "replay_get_state", json!({})).await;
    assert!(!before["isOnTrack"].as_bool().unwrap_or(true));
    assert!(!before["isInGarage"].as_bool().unwrap_or(true));

    let session_num = before["replaySessionNum"]
        .as_i64()
        .expect("replaySessionNum as i64") as i32;
    let current_time_ms = (before["replaySessionTime"]
        .as_f64()
        .expect("replaySessionTime as f64")
        * 1000.0)
        .round() as i32;
    let target_time_ms = current_time_ms.saturating_sub(2_000);

    let seek = call_tool(
        app.clone(),
        "replay_seek_session_time",
        json!({
            "sessionNum": session_num,
            "sessionTimeMs": target_time_ms,
            "toleranceMs": 2000
        }),
    )
    .await;
    assert_eq!(seek["verified"], Value::Bool(true));

    let payload = call_tool_payload(
        app.clone(),
        "replay_search_event",
        json!({ "mode": "to_end" }),
    )
    .await;
    assert_eq!(
        payload["ok"],
        Value::Bool(true),
        "unexpected payload: {payload}"
    );
    assert_eq!(
        payload["data"]["verified"],
        Value::Bool(true),
        "unexpected payload: {payload}"
    );
}

#[tokio::test]
#[ignore = "requires live iRacing replay/spectator mode"]
async fn live_mcp_replay_set_playback_verifies_and_restores() {
    let app = build_live_app();
    let before = call_tool(app.clone(), "replay_get_state", json!({})).await;
    assert!(!before["isOnTrack"].as_bool().unwrap_or(true));
    assert!(!before["isInGarage"].as_bool().unwrap_or(true));

    let original_speed = before["replayPlaySpeed"]
        .as_i64()
        .expect("replayPlaySpeed as i64") as i32;
    let original_slow_motion = before["replayPlaySlowMotion"]
        .as_bool()
        .expect("replayPlaySlowMotion as bool");
    let session_num = before["replaySessionNum"]
        .as_i64()
        .expect("replaySessionNum as i64") as i32;
    let current_time_ms = (before["replaySessionTime"]
        .as_f64()
        .expect("replaySessionTime as f64")
        * 1000.0)
        .round() as i32;
    let arm_time_ms = current_time_ms.saturating_sub(2_000);

    let arm_seek = call_tool(
        app.clone(),
        "replay_seek_session_time",
        json!({
            "sessionNum": session_num,
            "sessionTimeMs": arm_time_ms,
            "toleranceMs": 2000
        }),
    )
    .await;
    assert_eq!(arm_seek["verified"], Value::Bool(true));

    let changed = call_tool(
        app.clone(),
        "replay_set_playback",
        json!({
            "speed": 0,
            "slowMotion": false
        }),
    )
    .await;
    assert_eq!(changed["verified"], Value::Bool(true));
    assert_eq!(changed["observed"]["replayPlaySpeed"], Value::from(0));

    let resumed = call_tool(
        app,
        "replay_set_playback",
        json!({
            "speed": if original_speed == 0 { 1 } else { original_speed },
            "slowMotion": original_slow_motion
        }),
    )
    .await;
    assert_eq!(resumed["verified"], Value::Bool(true));
    assert_eq!(
        resumed["observed"]["replayPlaySpeed"],
        Value::from(if original_speed == 0 {
            1
        } else {
            original_speed
        })
    );
}

#[tokio::test]
#[ignore = "requires live iRacing replay/spectator mode"]
async fn live_mcp_replay_seek_frame_verifies_and_restores() {
    let app = build_live_app();
    let before = call_tool(app.clone(), "replay_get_state", json!({})).await;
    assert!(!before["isOnTrack"].as_bool().unwrap_or(true));
    assert!(!before["isInGarage"].as_bool().unwrap_or(true));

    let session_num = before["replaySessionNum"]
        .as_i64()
        .expect("replaySessionNum as i64") as i32;
    let current_time_ms = (before["replaySessionTime"]
        .as_f64()
        .expect("replaySessionTime as f64")
        * 1000.0)
        .round() as i32;
    let arm_time_ms = current_time_ms.saturating_sub(2_000);

    let arm_seek = call_tool(
        app.clone(),
        "replay_seek_session_time",
        json!({
            "sessionNum": session_num,
            "sessionTimeMs": arm_time_ms,
            "toleranceMs": 2000
        }),
    )
    .await;
    assert_eq!(arm_seek["verified"], Value::Bool(true));

    let pause = call_tool(
        app.clone(),
        "replay_set_playback",
        json!({
            "speed": 0,
            "slowMotion": false
        }),
    )
    .await;
    assert_eq!(pause["verified"], Value::Bool(true));

    let seek_back = call_tool(
        app.clone(),
        "replay_seek_frame",
        json!({
            "mode": "current",
            "frame": -120,
            "toleranceFrames": 600
        }),
    )
    .await;
    assert_eq!(seek_back["verified"], Value::Bool(true));

    let seek_forward = call_tool(
        app.clone(),
        "replay_seek_frame",
        json!({
            "mode": "current",
            "frame": 120,
            "toleranceFrames": 600
        }),
    )
    .await;
    assert_eq!(seek_forward["verified"], Value::Bool(true));

    let resume = call_tool(
        app,
        "replay_set_playback",
        json!({
            "speed": 1,
            "slowMotion": false
        }),
    )
    .await;
    assert_eq!(resume["verified"], Value::Bool(true));
}

fn build_live_app() -> axum::Router {
    let handler = Arc::new(IracingMcpHandler::new(Arc::new(adapter::SdkAdapter)));
    build_router(handler)
}

async fn call_tool(app: axum::Router, name: &str, arguments: Value) -> Value {
    let payload = call_tool_payload(app, name, arguments).await;
    let ok = payload.get("ok").and_then(Value::as_bool).unwrap_or(false);
    if !ok {
        let error = payload.get("error").cloned().unwrap_or(Value::Null);
        panic!("tool {name} returned MCP error: {error}; full payload: {payload}");
    }

    payload
        .get("data")
        .cloned()
        .unwrap_or_else(|| panic!("tool {name} returned ok=true but missing data field: {payload}"))
}

async fn call_tool_payload(app: axum::Router, name: &str, arguments: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": {
            "name": name,
            "arguments": arguments
        }
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(request.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let json: Value = serde_json::from_slice(&body).expect("json body");
    json.get("result")
        .and_then(|v| v.get("structuredContent"))
        .cloned()
        .unwrap_or_else(|| panic!("tool {name} returned unexpected response envelope: {json}"))
}

async fn choose_alternate_car_idx(app: &axum::Router, current_car_idx: i32) -> Option<i32> {
    let relatives = call_tool(app.clone(), "get_relatives", json!({})).await;
    let entries = relatives.get("entries")?.as_array()?;

    entries
        .iter()
        .filter_map(|entry| {
            entry
                .get("carIdx")
                .and_then(Value::as_i64)
                .map(|v| v as i32)
        })
        .find(|&car_idx| car_idx != current_car_idx && car_idx >= 0)
}
