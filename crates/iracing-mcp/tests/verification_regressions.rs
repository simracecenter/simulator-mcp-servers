// SPDX-License-Identifier: GPL-3.0-or-later
//! Ported from `margic/iracing-mcp`
//! (`crates/iracing-mcp-server/tests/verification_regressions.rs`, ADR 0001 D5).

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use iracing_mcp::{adapter, IracingMcpHandler};
use mcp_core::transport::http::build_router;
use serde_json::{json, Value};
use tower::ServiceExt;

#[derive(Debug)]
struct ScriptedAdapter {
    inner: Mutex<ScriptedAdapterInner>,
}

#[derive(Debug)]
struct ScriptedAdapterInner {
    current: adapter::ReplayState,
    active: Option<VecDeque<adapter::ReplayState>>,
    scripts: HashMap<&'static str, VecDeque<VecDeque<adapter::ReplayState>>>,
}

impl ScriptedAdapter {
    fn new(current: adapter::ReplayState) -> Self {
        Self {
            inner: Mutex::new(ScriptedAdapterInner {
                current,
                active: None,
                scripts: HashMap::new(),
            }),
        }
    }

    fn with_script(self, command: &'static str, sequence: Vec<adapter::ReplayState>) -> Self {
        let mut inner = self.inner.lock().expect("not poisoned");
        inner
            .scripts
            .entry(command)
            .or_default()
            .push_back(sequence.into_iter().collect());
        drop(inner);
        self
    }

    fn activate_script(&self, command: &'static str) {
        let mut inner = self.inner.lock().expect("not poisoned");
        inner.active = inner.scripts.get_mut(command).and_then(VecDeque::pop_front);
    }
}

#[async_trait]
impl adapter::IracingAdapter for ScriptedAdapter {
    async fn get_session_overview(&self) -> adapter::SessionOverview {
        let current = self.get_replay_state().await.expect("replay state");
        adapter::SessionOverview {
            connected: current.connected,
            is_replay: current.is_replay_playing || current.replay_frame_num > 0,
            is_in_car: current.is_on_track || current.is_in_garage,
            session_name: "Scripted".to_string(),
            track_name: "Scripted Track".to_string(),
        }
    }

    async fn get_session_data(&self) -> Result<adapter::SessionData, adapter::AdapterError> {
        Ok(adapter::SessionData {
            track_display_name: "Scripted Track".to_string(),
            current_session_type: "Race".to_string(),
            driver_count: 2,
            session_count: 1,
        })
    }

    async fn get_replay_state(&self) -> Result<adapter::ReplayState, adapter::AdapterError> {
        let mut inner = self.inner.lock().expect("not poisoned");

        if let Some(next) = inner.active.as_mut().and_then(VecDeque::pop_front) {
            inner.current = next.clone();
            if inner.active.as_ref().is_some_and(VecDeque::is_empty) {
                inner.active = None;
            }
            return Ok(next);
        }

        if inner.active.as_ref().is_some_and(VecDeque::is_empty) {
            inner.active = None;
        }

        Ok(inner.current.clone())
    }

    async fn set_replay_playback(
        &self,
        speed: i32,
        slow_motion: bool,
    ) -> Result<(), adapter::AdapterError> {
        self.activate_script("set_replay_playback");

        let mut inner = self.inner.lock().expect("not poisoned");
        if inner.active.is_none() {
            inner.current.replay_play_speed = speed;
            inner.current.replay_play_slow_motion = slow_motion;
            inner.current.is_replay_playing = speed != 0;
        }
        Ok(())
    }

    async fn replay_seek_session_time(
        &self,
        session_num: i32,
        session_time_ms: i32,
    ) -> Result<(), adapter::AdapterError> {
        self.activate_script("replay_seek_session_time");

        let mut inner = self.inner.lock().expect("not poisoned");
        if inner.active.is_none() {
            inner.current.replay_session_num = session_num;
            inner.current.replay_session_time = session_time_ms as f64 / 1000.0;
            inner.current.replay_frame_num = (inner.current.replay_session_time * 60.0) as i32;
        }
        Ok(())
    }

    async fn replay_seek_frame(
        &self,
        mode: adapter::ReplaySeekFrameMode,
        frame: i32,
    ) -> Result<(), adapter::AdapterError> {
        let mut inner = self.inner.lock().expect("not poisoned");
        let target = match mode {
            adapter::ReplaySeekFrameMode::Begin => frame,
            adapter::ReplaySeekFrameMode::Current => {
                inner.current.replay_frame_num.saturating_add(frame)
            }
            adapter::ReplaySeekFrameMode::End => {
                inner.current.replay_frame_num_end.saturating_sub(frame)
            }
        };
        inner.current.replay_frame_num = target.clamp(0, inner.current.replay_frame_num_end);
        inner.current.replay_session_time = inner.current.replay_frame_num as f64 / 60.0;
        Ok(())
    }

    async fn replay_search_event(
        &self,
        mode: adapter::ReplaySearchMode,
    ) -> Result<(), adapter::AdapterError> {
        self.activate_script("replay_search_event");

        let mut inner = self.inner.lock().expect("not poisoned");
        if inner.active.is_none() {
            if let adapter::ReplaySearchMode::ToEnd = mode {
                inner.current.replay_frame_num = inner.current.replay_frame_num_end;
            }
        }
        Ok(())
    }

    async fn camera_set_state(&self, state_bits: i32) -> Result<(), adapter::AdapterError> {
        let mut inner = self.inner.lock().expect("not poisoned");
        inner.current.cam_camera_state = state_bits;
        Ok(())
    }

    async fn camera_focus(
        &self,
        car_idx: i32,
        group_number: Option<i32>,
        camera_number: Option<i32>,
    ) -> Result<(), adapter::AdapterError> {
        self.activate_script("camera_focus");

        let mut inner = self.inner.lock().expect("not poisoned");
        if inner.active.is_none() {
            inner.current.cam_car_idx = car_idx;
            if let Some(group_number) = group_number {
                inner.current.cam_group_number = group_number;
            }
            if let Some(camera_number) = camera_number {
                inner.current.cam_camera_number = camera_number;
            }
        }
        Ok(())
    }

    async fn get_weekend_info(&self) -> Result<adapter::WeekendInfo, adapter::AdapterError> {
        Ok(adapter::WeekendInfo {
            track_name: "scripted_track".to_string(),
            track_id: 1,
            track_display_name: "Scripted Track".to_string(),
            track_config_name: "Full".to_string(),
            track_length_km: 5.0,
            track_city: "Scripted City".to_string(),
            track_country: "Scripted Country".to_string(),
            track_num_turns: 8,
            track_pit_speed_limit_kph: 60.0,
            track_type: "Road".to_string(),
            series_id: 1,
            season_id: 1,
            session_id: 1,
            sub_session_id: 1,
            official: false,
            event_type: "Race".to_string(),
            category: "Road".to_string(),
            sim_mode: "Full".to_string(),
            team_racing: false,
            weather_type: "Constant".to_string(),
            skies: "Clear".to_string(),
            surface_temp_c: 30.0,
            air_temp_c: 20.0,
            wind_vel_ms: 1.0,
        })
    }

    async fn get_roster(
        &self,
        _include_spectators: bool,
        _include_pace_car: bool,
    ) -> Result<adapter::Roster, adapter::AdapterError> {
        Ok(adapter::Roster {
            entries: vec![],
            count: 0,
        })
    }

    async fn get_camera_groups(&self) -> Result<adapter::CameraGroupList, adapter::AdapterError> {
        Ok(adapter::CameraGroupList {
            groups: vec![],
            count: 0,
        })
    }

    async fn get_standings(
        &self,
        _session_num: Option<i32>,
    ) -> Result<adapter::Standings, adapter::AdapterError> {
        Ok(adapter::Standings {
            session_num: 0,
            session_type: "Race".to_string(),
            positions: vec![],
        })
    }

    async fn get_relatives(&self) -> Result<adapter::Relatives, adapter::AdapterError> {
        Ok(adapter::Relatives {
            basis: "track".to_string(),
            session_num: 0,
            entries: vec![],
            count: 0,
        })
    }

    async fn resolve_driver(
        &self,
        _query: &str,
        _limit: usize,
    ) -> Result<adapter::ResolveDriverResult, adapter::AdapterError> {
        Ok(adapter::ResolveDriverResult {
            best_match: None,
            candidates: vec![],
        })
    }
}

fn base_replay_state() -> adapter::ReplayState {
    adapter::ReplayState {
        connected: true,
        is_on_track: false,
        is_in_garage: false,
        is_replay_playing: true,
        replay_play_speed: 1,
        replay_play_slow_motion: false,
        replay_frame_num: 6_000,
        replay_frame_num_end: 12_000,
        replay_session_num: 0,
        replay_session_time: 100.0,
        cam_car_idx: 2,
        cam_group_number: 1,
        cam_camera_number: 0,
        cam_camera_state: 0,
    }
}

fn build_app(adapter: Arc<dyn adapter::IracingAdapter>) -> axum::Router {
    let handler = Arc::new(IracingMcpHandler::new(adapter));
    build_router(handler)
}

async fn call_tool_payload(app: axum::Router, name: &str, arguments: Value) -> Value {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "tools/call",
        "params": { "name": name, "arguments": arguments }
    });
    let response = app
        .oneshot(
            Request::builder()
                .uri("/mcp")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("valid request"),
        )
        .await
        .expect("router response");

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    let json: Value = serde_json::from_slice(&bytes).expect("json body");
    json["result"]["structuredContent"].clone()
}

#[tokio::test]
async fn replay_search_to_end_verifies_after_delayed_polls() {
    let initial = base_replay_state();
    let adapter: Arc<dyn adapter::IracingAdapter> =
        Arc::new(ScriptedAdapter::new(initial.clone()).with_script(
            "replay_search_event",
            vec![
                adapter::ReplayState {
                    replay_frame_num: 6_120,
                    replay_frame_num_end: 12_000,
                    replay_session_time: 102.0,
                    ..initial.clone()
                },
                adapter::ReplayState {
                    replay_frame_num: 11_997,
                    replay_frame_num_end: 1,
                    replay_session_time: 199.95,
                    ..initial
                },
            ],
        ));
    let app = build_app(adapter);

    let payload = call_tool_payload(app, "replay_search_event", json!({ "mode": "to_end" })).await;

    assert_eq!(payload["ok"], Value::Bool(true));
    assert_eq!(payload["data"]["verified"], Value::Bool(true));
    assert_eq!(
        payload["data"]["observed"]["replayFrameNum"],
        Value::from(11_997)
    );
}

#[tokio::test]
async fn replay_show_window_timeout_reports_step_level_verification() {
    let initial = base_replay_state();
    let paused_state = adapter::ReplayState {
        replay_play_speed: 0,
        is_replay_playing: false,
        ..initial.clone()
    };
    let seek_state = adapter::ReplayState {
        replay_frame_num: 5_400,
        replay_session_time: 90.0,
        replay_play_speed: 0,
        is_replay_playing: false,
        ..initial.clone()
    };
    let focus_state = adapter::ReplayState {
        cam_car_idx: 7,
        cam_group_number: 3,
        replay_frame_num: 5_400,
        replay_session_time: 90.0,
        replay_play_speed: 0,
        is_replay_playing: false,
        ..initial.clone()
    };
    let playback_state = adapter::ReplayState {
        cam_car_idx: 7,
        cam_group_number: 3,
        replay_frame_num: 5_405,
        replay_session_time: 90.083,
        is_replay_playing: true,
        replay_play_speed: 1,
        ..initial.clone()
    };
    let adapter: Arc<dyn adapter::IracingAdapter> = Arc::new(
        ScriptedAdapter::new(initial)
            .with_script("set_replay_playback", vec![paused_state])
            .with_script("replay_seek_session_time", vec![seek_state])
            .with_script("camera_focus", vec![focus_state])
            .with_script("set_replay_playback", vec![playback_state]),
    );
    let app = build_app(adapter);

    let payload = call_tool_payload(
        app,
        "replay_show_window",
        json!({
            "sessionNum": 0,
            "startTimeMs": 90000,
            "endTimeMs": 120000,
            "focusCarIdx": 7,
            "cameraGroupNum": 3,
            "speed": 1,
            "timeoutMs": 150
        }),
    )
    .await;

    assert_eq!(payload["ok"], Value::Bool(false));
    assert_eq!(
        payload["error"]["code"],
        Value::String("timeout".to_string())
    );
    assert_eq!(payload["data"]["verified"], Value::Bool(false));
    assert_eq!(
        payload["data"]["steps"][0]["tool"],
        Value::String("replay_seek_session_time".to_string())
    );
    assert_eq!(payload["data"]["steps"][0]["verified"], Value::Bool(true));
    assert_eq!(
        payload["data"]["steps"][1]["tool"],
        Value::String("camera_focus".to_string())
    );
    assert_eq!(payload["data"]["steps"][1]["verified"], Value::Bool(true));
    assert_eq!(
        payload["data"]["steps"][2]["tool"],
        Value::String("replay_set_playback".to_string())
    );
    assert_eq!(payload["data"]["steps"][2]["verified"], Value::Bool(true));
    assert_eq!(
        payload["data"]["steps"][3]["tool"],
        Value::String("replay_pause_at_end".to_string())
    );
    assert_eq!(payload["data"]["steps"][3]["verified"], Value::Bool(false));
    assert_eq!(
        payload["data"]["steps"][3]["reachedEnd"],
        Value::Bool(false)
    );
}

#[tokio::test]
async fn replay_commands_return_wrong_mode_when_in_car() {
    let adapter: Arc<dyn adapter::IracingAdapter> =
        Arc::new(ScriptedAdapter::new(adapter::ReplayState {
            is_on_track: true,
            ..base_replay_state()
        }));
    let app = build_app(adapter);

    let payload = call_tool_payload(
        app,
        "replay_set_playback",
        json!({ "speed": 0, "slowMotion": false }),
    )
    .await;

    assert_eq!(payload["ok"], Value::Bool(false));
    assert_eq!(payload["data"], Value::Null);
    assert_eq!(
        payload["error"]["code"],
        Value::String("wrong_mode".to_string())
    );
}

#[tokio::test]
async fn camera_focus_verifies_car_target_even_if_camera_slot_changes() {
    let initial = base_replay_state();
    let adapter: Arc<dyn adapter::IracingAdapter> = Arc::new(ScriptedAdapter::new(initial));
    let app = build_app(adapter);

    let payload = call_tool_payload(
        app,
        "camera_focus",
        json!({
            "carIdx": 7,
            "groupNumber": 3,
            "cameraNumber": 2
        }),
    )
    .await;

    assert_eq!(payload["ok"], Value::Bool(true));
    assert_eq!(payload["data"]["verified"], Value::Bool(true));
    assert_eq!(payload["data"]["observed"]["camCarIdx"], Value::from(7));
}

#[tokio::test]
async fn camera_set_state_verifies_requested_bits() {
    let initial = base_replay_state();
    let adapter: Arc<dyn adapter::IracingAdapter> = Arc::new(ScriptedAdapter::new(initial));
    let app = build_app(adapter);

    let payload = call_tool_payload(
        app,
        "camera_set_state",
        json!({
            "camToolActive": true,
            "uiHidden": true
        }),
    )
    .await;

    assert_eq!(payload["ok"], Value::Bool(true));
    assert_eq!(payload["data"]["verified"], Value::Bool(true));
    let observed_state = payload["data"]["observed"]["camCameraState"]
        .as_i64()
        .expect("camCameraState as i64") as i32;
    assert_eq!(observed_state & 0x0C, 0x0C);
}
