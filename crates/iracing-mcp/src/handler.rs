// SPDX-License-Identifier: GPL-3.0-or-later
//! [`McpHandler`] implementation for iRacing, ported from `margic/iracing-mcp`
//! (`crates/iracing-mcp-server/src/mcp/mod.rs`, ADR 0001 D5).
//!
//! Holds `Arc<dyn IracingAdapter>` as internal state and implements all 15
//! tools upstream registers in `tools/list`. The replay/camera tools'
//! send-poll-verify loop now lives in [`mcp_core::verify`]; this module only
//! shapes each tool's send/poll/verify closures and turns the resulting
//! [`mcp_core::verify::VerifyOutcome`] into a `tools/call` response.

use async_trait::async_trait;
use mcp_core::verify::{verify_loop, VerifyOutcome};
use mcp_core::{JsonRpcRequest, JsonRpcResponse, McpHandler, ToolCapability};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::{sleep, Duration, Instant};
use tracing::warn;

use crate::adapter::{
    AdapterError, AdapterRef, ReplaySearchMode, ReplaySeekFrameMode, ReplayState,
};

/// Real [`McpHandler`] for iRacing, backed by `Arc<dyn IracingAdapter>`.
///
/// Defaults to [`crate::adapter::SdkAdapter`] in production (constructed by
/// `crates/launcher/src/runner.rs`) and [`crate::adapter::StubAdapter`] in
/// tests.
pub struct IracingMcpHandler {
    adapter: AdapterRef,
}

impl IracingMcpHandler {
    pub fn new(adapter: AdapterRef) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl McpHandler for IracingMcpHandler {
    async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        if request.jsonrpc != "2.0" {
            return JsonRpcResponse::err(
                request.id,
                -32600,
                "invalid request: jsonrpc must be 2.0",
            );
        }

        match request.method.as_str() {
            "initialize" => JsonRpcResponse::ok(
                request.id,
                json!({
                    "protocolVersion": "2025-06-18",
                    "serverInfo": { "name": "iracing-mcp", "version": env!("CARGO_PKG_VERSION") },
                    "capabilities": { "tools": { "listChanged": true } }
                }),
            ),
            "tools/list" => JsonRpcResponse::ok(request.id, json!({ "tools": tool_descriptors() })),
            "tools/call" => self.tools_call(request.id, request.params).await,
            _ => JsonRpcResponse::err(request.id, -32601, "method not found"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaySetPlaybackArgs {
    speed: i32,
    #[serde(default)]
    slow_motion: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaySeekSessionTimeArgs {
    session_num: i32,
    session_time_ms: i32,
    #[serde(default = "default_seek_tolerance_ms")]
    tolerance_ms: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaySeekFrameArgs {
    #[serde(default = "default_seek_mode")]
    mode: ReplaySeekFrameMode,
    frame: i32,
    #[serde(default = "default_seek_frame_tolerance")]
    tolerance_frames: i32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaySearchEventArgs {
    mode: ReplaySearchMode,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplayShowWindowArgs {
    session_num: i32,
    start_time_ms: i32,
    end_time_ms: Option<i32>,
    focus_car_idx: i32,
    camera_group_num: Option<i32>,
    #[serde(default = "default_show_window_speed")]
    speed: i32,
    #[serde(default = "default_show_window_timeout_ms")]
    timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CameraFocusArgs {
    car_idx: i32,
    group_number: Option<i32>,
    camera_number: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CameraSetStateArgs {
    cam_tool_active: Option<bool>,
    ui_hidden: Option<bool>,
    use_auto_shot_selection: Option<bool>,
    use_temporary_edits: Option<bool>,
    use_key_acceleration: Option<bool>,
    use_key_10x_acceleration: Option<bool>,
    use_mouse_aim_mode: Option<bool>,
}

fn default_seek_tolerance_ms() -> i32 {
    500
}

fn default_seek_mode() -> ReplaySeekFrameMode {
    ReplaySeekFrameMode::Begin
}

fn default_seek_frame_tolerance() -> i32 {
    4
}

fn default_show_window_speed() -> i32 {
    1
}

fn default_show_window_timeout_ms() -> u64 {
    2000
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetRosterArgs {
    #[serde(default)]
    include_spectators: bool,
    #[serde(default)]
    include_pace_car: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetStandingsArgs {
    session_num: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetRelativesArgs {}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveDriverArgs {
    query: String,
    #[serde(default = "default_resolve_limit")]
    limit: usize,
}

fn default_resolve_limit() -> usize {
    3
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "get_session_overview",
            "description": "Returns current iRacing session connectivity and mode.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "replay_get_state",
            "description": "Returns live replay and camera telemetry used for replay verification.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "replay_set_playback",
            "description": "Sets replay playback speed and verifies the resulting replay telemetry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "speed": { "type": "integer" },
                    "slowMotion": { "type": "boolean" }
                },
                "required": ["speed"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "replay_seek_session_time",
            "description": "Seeks the replay timeline to a session-relative time and verifies the observed replay position.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionNum": { "type": "integer" },
                    "sessionTimeMs": { "type": "integer" },
                    "toleranceMs": { "type": "integer" }
                },
                "required": ["sessionNum", "sessionTimeMs"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "replay_seek_frame",
            "description": "Seeks the replay timeline to an absolute or relative frame and verifies the observed frame.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": { "type": "string", "enum": ["begin", "current", "end"] },
                    "frame": { "type": "integer" },
                    "toleranceFrames": { "type": "integer" }
                },
                "required": ["frame"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "replay_search_event",
            "description": "Performs a semantic replay jump (lap, frame, incident, session, start/end) and verifies movement.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mode": {
                        "type": "string",
                        "enum": [
                            "to_start", "to_end", "prev_session", "next_session",
                            "prev_lap", "next_lap", "prev_frame", "next_frame",
                            "prev_incident", "next_incident"
                        ]
                    }
                },
                "required": ["mode"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "replay_show_window",
            "description": "Composite convenience tool: seek to time, focus camera, set playback speed, and optionally pause at endTimeMs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionNum": { "type": "integer", "minimum": 0 },
                    "startTimeMs": { "type": "integer", "minimum": 0 },
                    "endTimeMs": { "type": "integer", "minimum": 0 },
                    "focusCarIdx": { "type": "integer", "minimum": 0 },
                    "cameraGroupNum": { "type": "integer", "minimum": 0 },
                    "speed": { "type": "integer" },
                    "timeoutMs": { "type": "integer", "minimum": 0 }
                },
                "required": ["sessionNum", "startTimeMs", "focusCarIdx"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "camera_focus",
            "description": "Focuses the active camera on a target car and optionally switches group/camera with telemetry verification.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "carIdx": { "type": "integer" },
                    "groupNumber": { "type": ["integer", "null"] },
                    "cameraNumber": { "type": ["integer", "null"] }
                },
                "required": ["carIdx"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "camera_set_state",
            "description": "Sets camera state bits (UI/cam tool behavior) and verifies CamCameraState telemetry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "camToolActive": { "type": "boolean" },
                    "uiHidden": { "type": "boolean" },
                    "useAutoShotSelection": { "type": "boolean" },
                    "useTemporaryEdits": { "type": "boolean" },
                    "useKeyAcceleration": { "type": "boolean" },
                    "useKey10xAcceleration": { "type": "boolean" },
                    "useMouseAimMode": { "type": "boolean" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_weekend_info",
            "description": "Returns static event and weather metadata for the current weekend.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_roster",
            "description": "Returns the list of drivers, cars, and car classes in the session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "includeSpectators": { "type": "boolean" },
                    "includePaceCar": { "type": "boolean" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_camera_groups",
            "description": "Returns all available camera groups and their cameras for the current session.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_standings",
            "description": "Returns current session standings and timing for each driver.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "sessionNum": { "type": "integer" }
                },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_relatives",
            "description": "Returns a live field-order and gap view computed from telemetry arrays.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        json!({
            "name": "resolve_driver",
            "description": "Maps a spoken or typed name, initials, or car number to a stable carIdx.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer" }
                },
                "required": ["query"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_capabilities",
            "description": "Returns which tools are supported, degraded, or unsupported for the active adapter, so an agent can plan without hitting not_supported errors.",
            "inputSchema": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
    ]
}

/// All 15 tools above are fully implemented against `IracingAdapter` — this
/// server is the mature reference implementation (ADR 0001 D5), so every
/// entry is `Supported`. Kept as an explicit list (rather than a loop over
/// `tool_descriptors()`) so a newly added tool that's *not* yet fully wired
/// up has to be deliberately added here too.
fn capabilities() -> Vec<ToolCapability> {
    vec![
        ToolCapability::supported("get_session_overview"),
        ToolCapability::supported("replay_get_state"),
        ToolCapability::supported("replay_set_playback"),
        ToolCapability::supported("replay_seek_session_time"),
        ToolCapability::supported("replay_seek_frame"),
        ToolCapability::supported("replay_search_event"),
        ToolCapability::supported("replay_show_window"),
        ToolCapability::supported("camera_focus"),
        ToolCapability::supported("camera_set_state"),
        ToolCapability::supported("get_weekend_info"),
        ToolCapability::supported("get_roster"),
        ToolCapability::supported("get_camera_groups"),
        ToolCapability::supported("get_standings"),
        ToolCapability::supported("get_relatives"),
        ToolCapability::supported("resolve_driver"),
        ToolCapability::supported("get_capabilities"),
    ]
}

impl IracingMcpHandler {
    async fn tools_call(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match name {
            "get_session_overview" => {
                let overview = self.adapter.get_session_overview().await;
                tool_ok(id, overview)
            }
            "replay_get_state" => match self.adapter.get_replay_state().await {
                Ok(replay_state) => tool_ok(id, replay_state),
                Err(error) => tool_err(id, error_code(&error), &error.to_string()),
            },
            "replay_set_playback" => self.replay_set_playback(id, params).await,
            "replay_seek_session_time" => self.replay_seek_session_time(id, params).await,
            "replay_seek_frame" => self.replay_seek_frame(id, params).await,
            "replay_search_event" => self.replay_search_event(id, params).await,
            "replay_show_window" => self.replay_show_window(id, params).await,
            "camera_focus" => self.camera_focus(id, params).await,
            "camera_set_state" => self.camera_set_state(id, params).await,
            "get_weekend_info" => match self.adapter.get_weekend_info().await {
                Ok(info) => tool_ok(id, info),
                Err(e) => tool_err(id, error_code(&e), &e.to_string()),
            },
            "get_roster" => {
                let args: GetRosterArgs =
                    parse_tool_args(&id, &params, "get_roster").unwrap_or(GetRosterArgs {
                        include_spectators: false,
                        include_pace_car: false,
                    });
                match self
                    .adapter
                    .get_roster(args.include_spectators, args.include_pace_car)
                    .await
                {
                    Ok(roster) => tool_ok(id, roster),
                    Err(e) => tool_err(id, error_code(&e), &e.to_string()),
                }
            }
            "get_camera_groups" => match self.adapter.get_camera_groups().await {
                Ok(groups) => tool_ok(id, groups),
                Err(e) => tool_err(id, error_code(&e), &e.to_string()),
            },
            "get_standings" => {
                let args: GetStandingsArgs = parse_tool_args(&id, &params, "get_standings")
                    .unwrap_or(GetStandingsArgs { session_num: None });
                match self.adapter.get_standings(args.session_num).await {
                    Ok(standings) => tool_ok(id, standings),
                    Err(e) => tool_err(id, error_code(&e), &e.to_string()),
                }
            }
            "get_relatives" => {
                let _: GetRelativesArgs = match parse_tool_args(&id, &params, "get_relatives") {
                    Ok(args) => args,
                    Err(response) => return response,
                };
                match self.adapter.get_relatives().await {
                    Ok(relatives) => tool_ok(id, relatives),
                    Err(e) => tool_err(id, error_code(&e), &e.to_string()),
                }
            }
            "resolve_driver" => {
                let args: ResolveDriverArgs = match parse_tool_args(&id, &params, "resolve_driver")
                {
                    Ok(a) => a,
                    Err(r) => return r,
                };
                match self.adapter.resolve_driver(&args.query, args.limit).await {
                    Ok(result) => tool_ok(id, result),
                    Err(e) => tool_err(id, error_code(&e), &e.to_string()),
                }
            }
            "get_capabilities" => tool_ok(id, capabilities()),
            _ => JsonRpcResponse::err(id, -32602, "unknown tool name"),
        }
    }

    async fn replay_set_playback(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: ReplaySetPlaybackArgs = match parse_tool_args(&id, &params, "replay_set_playback")
        {
            Ok(args) => args,
            Err(response) => return response,
        };

        let before = match self.adapter.get_replay_state().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        if let Err(response) = ensure_out_of_car(id.clone(), &before) {
            return response;
        }

        let timeout = if args.speed == 0 {
            Duration::from_millis(5000)
        } else {
            Duration::from_millis(1000)
        };
        let mut pause_candidate_frame = None;

        let outcome = verify_loop(
            before,
            self.adapter
                .set_replay_playback(args.speed, args.slow_motion),
            || self.adapter.get_replay_state(),
            |current| verify_playback_state(current, &args, &mut pause_candidate_frame),
            timeout,
            Duration::from_millis(50),
        )
        .await;

        respond_verify_outcome(
            id,
            "replay_set_playback",
            outcome,
            json!({}),
            json!({}),
            || {
                format!(
                    "Replay playback telemetry did not reach speed={} slowMotion={} within {}ms.",
                    args.speed,
                    args.slow_motion,
                    timeout.as_millis()
                )
            },
        )
    }

    async fn replay_seek_session_time(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: ReplaySeekSessionTimeArgs =
            match parse_tool_args(&id, &params, "replay_seek_session_time") {
                Ok(args) => args,
                Err(response) => return response,
            };

        let before = match self.adapter.get_replay_state().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        if let Err(response) = ensure_out_of_car(id.clone(), &before) {
            return response;
        }

        let timeout = Duration::from_millis(5000);

        let outcome = verify_loop(
            before,
            self.adapter
                .replay_seek_session_time(args.session_num, args.session_time_ms),
            || self.adapter.get_replay_state(),
            |current: &ReplayState| {
                let observed_time_ms = (current.replay_session_time * 1000.0).round() as i32;
                current.replay_session_num == args.session_num
                    && (observed_time_ms - args.session_time_ms).abs() <= args.tolerance_ms
            },
            timeout,
            Duration::from_millis(50),
        )
        .await;

        respond_verify_outcome(
            id,
            "replay_seek_session_time",
            outcome,
            json!({}),
            json!({}),
            || {
                format!(
                    "Replay session time did not reach sessionNum={} sessionTimeMs={} within {}ms.",
                    args.session_num,
                    args.session_time_ms,
                    timeout.as_millis()
                )
            },
        )
    }

    async fn camera_focus(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: CameraFocusArgs = match parse_tool_args(&id, &params, "camera_focus") {
            Ok(args) => args,
            Err(response) => return response,
        };

        let before = match self.adapter.get_replay_state().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        if let Err(response) = ensure_out_of_car(id.clone(), &before) {
            return response;
        }

        let expected_group = args.group_number.unwrap_or(before.cam_group_number);
        let expected_camera = args.camera_number.unwrap_or(before.cam_camera_number);
        let verify_group = args.group_number.is_some();
        let verify_camera = args.camera_number.is_some();
        let timeout = Duration::from_millis(1500);

        let outcome = verify_loop(
            before,
            self.adapter
                .camera_focus(args.car_idx, args.group_number, args.camera_number),
            || self.adapter.get_replay_state(),
            |current: &ReplayState| {
                current.cam_car_idx == args.car_idx
                    && (!verify_group || current.cam_group_number == expected_group)
                    && (!verify_camera || current.cam_camera_number == expected_camera)
            },
            timeout,
            Duration::from_millis(50),
        )
        .await;

        let timeout_extra = json!({
            "requested": {
                "carIdx": args.car_idx,
                "groupNumber": args.group_number,
                "cameraNumber": args.camera_number
            }
        });

        respond_verify_outcome(
            id,
            "camera_focus",
            outcome,
            json!({}),
            timeout_extra,
            || {
                let expected_parts = [
                    Some(format!("carIdx={}", args.car_idx)),
                    verify_group.then(|| format!("groupNumber={}", expected_group)),
                    verify_camera.then(|| format!("cameraNumber={}", expected_camera)),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");

                format!(
                    "Camera did not reach expected {} within {}ms.",
                    expected_parts,
                    timeout.as_millis()
                )
            },
        )
    }

    async fn replay_seek_frame(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: ReplaySeekFrameArgs = match parse_tool_args(&id, &params, "replay_seek_frame") {
            Ok(args) => args,
            Err(response) => return response,
        };

        let before = match self.adapter.get_replay_state().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        if let Err(response) = ensure_out_of_car(id.clone(), &before) {
            return response;
        }

        let target_frame = match args.mode {
            ReplaySeekFrameMode::Begin => args.frame,
            ReplaySeekFrameMode::Current => before.replay_frame_num.saturating_add(args.frame),
            ReplaySeekFrameMode::End => before.replay_frame_num_end.saturating_sub(args.frame),
        }
        .clamp(0, before.replay_frame_num_end.max(before.replay_frame_num));

        let timeout = Duration::from_millis(1000);

        let outcome = verify_loop(
            before,
            self.adapter.replay_seek_frame(args.mode, args.frame),
            || self.adapter.get_replay_state(),
            |current: &ReplayState| {
                let delta = (current.replay_frame_num - target_frame).abs();
                delta <= args.tolerance_frames
            },
            timeout,
            Duration::from_millis(50),
        )
        .await;

        let target_frame_extra = json!({ "targetFrame": target_frame });

        respond_verify_outcome(
            id,
            "replay_seek_frame",
            outcome,
            target_frame_extra.clone(),
            target_frame_extra,
            || {
                format!(
                    "Replay frame did not reach targetFrame={} within {}ms.",
                    target_frame,
                    timeout.as_millis()
                )
            },
        )
    }

    async fn replay_search_event(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: ReplaySearchEventArgs = match parse_tool_args(&id, &params, "replay_search_event")
        {
            Ok(args) => args,
            Err(response) => return response,
        };

        let before = match self.adapter.get_replay_state().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        if let Err(response) = ensure_out_of_car(id.clone(), &before) {
            return response;
        }

        let mode = args.mode;
        let before_for_verify = before.clone();
        let timeout = Duration::from_millis(1000);

        let outcome = verify_loop(
            before,
            self.adapter.replay_search_event(mode),
            || self.adapter.get_replay_state(),
            |current: &ReplayState| verify_search_event_state(mode, &before_for_verify, current),
            timeout,
            Duration::from_millis(50),
        )
        .await;

        respond_verify_outcome(
            id,
            "replay_search_event",
            outcome,
            json!({}),
            json!({}),
            || {
                format!(
                    "Replay search mode={:?} did not produce expected movement within {}ms.",
                    mode,
                    timeout.as_millis()
                )
            },
        )
    }

    async fn replay_show_window(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: ReplayShowWindowArgs = match parse_tool_args(&id, &params, "replay_show_window") {
            Ok(args) => args,
            Err(response) => return response,
        };

        if args.start_time_ms < 0 || args.focus_car_idx < 0 || args.session_num < 0 {
            return tool_err(
                id,
                "invalid_arguments",
                "sessionNum, startTimeMs, and focusCarIdx must be >= 0",
            );
        }

        if let Some(end_time_ms) = args.end_time_ms {
            if end_time_ms < args.start_time_ms {
                return tool_err(id, "invalid_arguments", "endTimeMs must be >= startTimeMs");
            }
        }

        let before = match self.adapter.get_replay_state().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        if let Err(response) = ensure_out_of_car(id.clone(), &before) {
            return response;
        }

        let started_at = Instant::now();
        let step_timeout = Duration::from_millis(args.timeout_ms.max(1));
        let mut steps: Vec<Value> = Vec::new();

        if args.speed != 0 {
            if let Err(error) = self.adapter.set_replay_playback(0, false).await {
                return tool_err(id, error_code(&error), &error.to_string());
            }

            let mut paused = false;
            let deadline = Instant::now() + step_timeout;
            while Instant::now() < deadline {
                match self.adapter.get_replay_state().await {
                    Ok(current) => {
                        if current.replay_play_speed == 0 {
                            paused = true;
                            break;
                        }
                    }
                    Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
                }
                sleep(Duration::from_millis(50)).await;
            }

            if !paused {
                return tool_verification_err(
                    "replay_show_window",
                    id,
                    "timeout",
                    "Replay did not pause before show_window seek verification could start.",
                    json!({
                        "commandAccepted": true,
                        "verified": false,
                        "reason": "Replay did not pause before show_window seek verification could start.",
                        "before": before,
                        "steps": steps,
                        "finalState": self.adapter.get_replay_state().await.unwrap_or(before.clone()),
                        "elapsedMs": started_at.elapsed().as_millis()
                    }),
                );
            }
        }

        // Step 1: seek session time
        if let Err(error) = self
            .adapter
            .replay_seek_session_time(args.session_num, args.start_time_ms)
            .await
        {
            return tool_err(id, error_code(&error), &error.to_string());
        }

        let mut seek_verified = false;
        let mut seek_observed = before.clone();
        let deadline = Instant::now() + step_timeout;
        while Instant::now() < deadline {
            match self.adapter.get_replay_state().await {
                Ok(current) => {
                    let observed_time_ms = (current.replay_session_time * 1000.0).round() as i32;
                    if current.replay_session_num == args.session_num
                        && (observed_time_ms - args.start_time_ms).abs() <= 300
                    {
                        seek_verified = true;
                        seek_observed = current;
                        break;
                    }
                    seek_observed = current;
                }
                Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
            }
            sleep(Duration::from_millis(50)).await;
        }
        steps.push(json!({
            "tool": "replay_seek_session_time",
            "verified": seek_verified,
            "observed": seek_observed
        }));

        // Step 2: focus camera on target car
        if let Err(error) = self
            .adapter
            .camera_focus(args.focus_car_idx, args.camera_group_num, None)
            .await
        {
            return tool_err(id, error_code(&error), &error.to_string());
        }

        let mut focus_verified = false;
        let mut focus_observed = seek_observed.clone();
        let deadline = Instant::now() + step_timeout;
        while Instant::now() < deadline {
            match self.adapter.get_replay_state().await {
                Ok(current) => {
                    if current.cam_car_idx == args.focus_car_idx {
                        focus_verified = true;
                        focus_observed = current;
                        break;
                    }
                    focus_observed = current;
                }
                Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
            }
            sleep(Duration::from_millis(50)).await;
        }
        steps.push(json!({
            "tool": "camera_focus",
            "verified": focus_verified,
            "observed": focus_observed
        }));

        // Step 3: set playback speed
        if let Err(error) = self.adapter.set_replay_playback(args.speed, false).await {
            return tool_err(id, error_code(&error), &error.to_string());
        }

        let mut playback_verified = false;
        let mut playback_observed = focus_observed.clone();
        let deadline = Instant::now() + step_timeout;
        while Instant::now() < deadline {
            match self.adapter.get_replay_state().await {
                Ok(current) => {
                    let speed_ok = current.replay_play_speed == args.speed;
                    let play_ok = if args.speed == 0 {
                        !current.is_replay_playing
                    } else {
                        current.is_replay_playing
                    };
                    if speed_ok && play_ok {
                        playback_verified = true;
                        playback_observed = current;
                        break;
                    }
                    playback_observed = current;
                }
                Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
            }
            sleep(Duration::from_millis(50)).await;
        }
        steps.push(json!({
            "tool": "replay_set_playback",
            "verified": playback_verified,
            "observed": playback_observed
        }));

        // Optional pause step after reaching endTimeMs.
        if let Some(end_time_ms) = args.end_time_ms {
            let mut reached_end = args.speed == 0;
            let mut end_observed = playback_observed.clone();
            let deadline = Instant::now() + step_timeout;
            while Instant::now() < deadline {
                match self.adapter.get_replay_state().await {
                    Ok(current) => {
                        end_observed = current.clone();
                        let observed_time_ms =
                            (current.replay_session_time * 1000.0).round() as i32;
                        if observed_time_ms >= end_time_ms {
                            reached_end = true;
                            break;
                        }
                    }
                    Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
                }
                sleep(Duration::from_millis(50)).await;
            }

            if reached_end {
                if let Err(error) = self.adapter.set_replay_playback(0, false).await {
                    return tool_err(id, error_code(&error), &error.to_string());
                }
            }

            let mut paused_verified = false;
            let mut pause_observed = end_observed;
            let deadline = Instant::now() + step_timeout;
            while Instant::now() < deadline {
                match self.adapter.get_replay_state().await {
                    Ok(current) => {
                        pause_observed = current.clone();
                        if reached_end && current.replay_play_speed == 0 {
                            paused_verified = true;
                            break;
                        }

                        if !reached_end {
                            break;
                        }
                    }
                    Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
                }
                sleep(Duration::from_millis(50)).await;
            }

            steps.push(json!({
                "tool": "replay_pause_at_end",
                "verified": reached_end && paused_verified,
                "reachedEnd": reached_end,
                "targetEndTimeMs": end_time_ms,
                "observed": pause_observed
            }));
        }

        let final_state = match self.adapter.get_replay_state().await {
            Ok(current) => current,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };
        let all_verified = steps.iter().all(|step| {
            step.get("verified")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        });

        if all_verified {
            tool_ok(
                id,
                json!({
                    "commandAccepted": true,
                    "verified": true,
                    "reason": Value::Null,
                    "before": before,
                    "steps": steps,
                    "finalState": final_state,
                    "elapsedMs": started_at.elapsed().as_millis()
                }),
            )
        } else {
            tool_verification_err(
                "replay_show_window",
                id,
                "timeout",
                "One or more replay_show_window steps did not verify before timeout.",
                json!({
                    "commandAccepted": true,
                    "verified": false,
                    "reason": "One or more replay_show_window steps did not verify before timeout.",
                    "before": before,
                    "steps": steps,
                    "finalState": final_state,
                    "elapsedMs": started_at.elapsed().as_millis()
                }),
            )
        }
    }

    async fn camera_set_state(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: CameraSetStateArgs = match parse_tool_args(&id, &params, "camera_set_state") {
            Ok(args) => args,
            Err(response) => return response,
        };

        let before = match self.adapter.get_replay_state().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        if let Err(response) = ensure_out_of_car(id.clone(), &before) {
            return response;
        }

        let expected_state = apply_camera_state_updates(before.cam_camera_state, &args);
        let requested_mask = camera_state_requested_mask(&args);
        let expected_masked_state = expected_state & requested_mask;

        let timeout = Duration::from_millis(750);

        let outcome = verify_loop(
            before,
            self.adapter.camera_set_state(expected_state),
            || self.adapter.get_replay_state(),
            |current: &ReplayState| {
                (current.cam_camera_state & requested_mask) == expected_masked_state
            },
            timeout,
            Duration::from_millis(50),
        )
        .await;

        let extra = json!({
            "requestedMask": requested_mask,
            "expectedMaskedState": expected_masked_state,
            "expectedState": expected_state
        });

        respond_verify_outcome(
            id,
            "camera_set_state",
            outcome,
            extra.clone(),
            extra,
            || {
                format!(
                "Camera state did not reach expectedMask={} expectedMaskedState={} within {}ms.",
                requested_mask,
                expected_masked_state,
                timeout.as_millis()
            )
            },
        )
    }
}

/// Shapes a [`verify_loop`] result over [`ReplayState`] into the shared
/// `commandAccepted`/`verified`/`before`/`observed`/`elapsedMs` `tools/call`
/// payload used by every verifying command tool.
///
/// `verified_extra` / `timeout_extra` are merged into their respective
/// payloads (each tool decides which of `targetFrame`, `requested`, ... it
/// wants where); the timeout `reason` is only computed (via `timeout_reason`)
/// on the timed-out path.
fn respond_verify_outcome(
    id: Option<Value>,
    tool_name: &str,
    outcome: Result<VerifyOutcome<ReplayState>, AdapterError>,
    verified_extra: Value,
    timeout_extra: Value,
    timeout_reason: impl FnOnce() -> String,
) -> JsonRpcResponse {
    match outcome {
        Ok(VerifyOutcome::Verified {
            before,
            observed,
            elapsed,
        }) => {
            let mut payload = json!({
                "commandAccepted": true,
                "verified": true,
                "reason": Value::Null,
                "before": before,
                "observed": observed,
                "elapsedMs": elapsed.as_millis()
            });
            merge_object(&mut payload, verified_extra);
            tool_ok(id, payload)
        }
        Ok(VerifyOutcome::TimedOut {
            before,
            observed,
            elapsed,
        }) => {
            let reason = timeout_reason();
            let mut payload = json!({
                "commandAccepted": true,
                "verified": false,
                "reason": reason,
                "before": before,
                "observed": observed,
                "elapsedMs": elapsed.as_millis()
            });
            merge_object(&mut payload, timeout_extra);
            tool_verification_err(tool_name, id, "timeout", &reason, payload)
        }
        Err(error) => tool_err(id, error_code(&error), &error.to_string()),
    }
}

/// Shallow-merges the fields of `extra` (when it is a JSON object) into
/// `target` (when it is a JSON object), overwriting on key collision.
fn merge_object(target: &mut Value, extra: Value) {
    if let (Some(target_map), Value::Object(extra_map)) = (target.as_object_mut(), extra) {
        target_map.extend(extra_map);
    }
}

fn verify_search_event_state(
    mode: ReplaySearchMode,
    before: &ReplayState,
    current: &ReplayState,
) -> bool {
    match mode {
        ReplaySearchMode::ToStart => current.replay_frame_num <= 4,
        ReplaySearchMode::ToEnd => {
            let near_end = current.replay_frame_num_end > 0
                && current.replay_frame_num >= current.replay_frame_num_end.saturating_sub(4);
            let jumped_far = (current.replay_frame_num - before.replay_frame_num).abs() >= 1000;
            let end_changed = current.replay_frame_num_end != before.replay_frame_num_end;
            near_end || jumped_far || end_changed
        }
        ReplaySearchMode::PrevSession
        | ReplaySearchMode::PrevLap
        | ReplaySearchMode::PrevFrame
        | ReplaySearchMode::PrevIncident => current.replay_frame_num < before.replay_frame_num,
        ReplaySearchMode::NextSession
        | ReplaySearchMode::NextLap
        | ReplaySearchMode::NextFrame
        | ReplaySearchMode::NextIncident => current.replay_frame_num > before.replay_frame_num,
    }
}

/// A `CamCameraState` bit paired with the [`CameraSetStateArgs`] field that
/// requests it.
type CameraStateField = (i32, fn(&CameraSetStateArgs) -> Option<bool>);

/// The `CamCameraState` bit for each [`CameraSetStateArgs`] field, in the
/// order upstream documents them. Both the "apply requested updates" and
/// "which bits were requested" passes iterate this single table so the bit
/// values live in exactly one place.
const CAMERA_STATE_FIELDS: &[CameraStateField] = &[
    (0x04, |args| args.cam_tool_active),
    (0x08, |args| args.ui_hidden),
    (0x10, |args| args.use_auto_shot_selection),
    (0x20, |args| args.use_temporary_edits),
    (0x40, |args| args.use_key_acceleration),
    (0x80, |args| args.use_key_10x_acceleration),
    (0x100, |args| args.use_mouse_aim_mode),
];

fn apply_camera_state_updates(base: i32, args: &CameraSetStateArgs) -> i32 {
    let mut state = base;
    for (bit, field) in CAMERA_STATE_FIELDS {
        if let Some(enabled) = field(args) {
            if enabled {
                state |= bit;
            } else {
                state &= !bit;
            }
        }
    }
    state
}

fn camera_state_requested_mask(args: &CameraSetStateArgs) -> i32 {
    let mut mask = 0;
    for (bit, field) in CAMERA_STATE_FIELDS {
        if field(args).is_some() {
            mask |= bit;
        }
    }
    mask
}

fn verify_playback_state(
    current: &ReplayState,
    args: &ReplaySetPlaybackArgs,
    pause_candidate_frame: &mut Option<i32>,
) -> bool {
    if current.replay_play_speed != args.speed
        || current.replay_play_slow_motion != args.slow_motion
    {
        return false;
    }

    if args.speed != 0 {
        return current.is_replay_playing;
    }

    match pause_candidate_frame {
        Some(candidate_frame) if current.replay_frame_num == *candidate_frame => true,
        Some(candidate_frame) => {
            *candidate_frame = current.replay_frame_num;
            false
        }
        None => {
            *pause_candidate_frame = Some(current.replay_frame_num);
            false
        }
    }
}

fn ensure_out_of_car(id: Option<Value>, replay_state: &ReplayState) -> Result<(), JsonRpcResponse> {
    if replay_state.is_on_track || replay_state.is_in_garage {
        return Err(tool_err(
            id,
            "wrong_mode",
            "camera and replay commands only work when you are out of the car",
        ));
    }

    Ok(())
}

fn parse_tool_args<T: for<'de> Deserialize<'de>>(
    id: &Option<Value>,
    params: &Value,
    tool_name: &str,
) -> Result<T, JsonRpcResponse> {
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
    serde_json::from_value(arguments).map_err(|error| {
        tool_err(
            id.clone(),
            "invalid_arguments",
            &format!("invalid {tool_name} arguments: {error}"),
        )
    })
}

fn tool_ok(id: Option<Value>, data: impl Serialize) -> JsonRpcResponse {
    build_tool_result(
        id,
        json!({
            "ok": true,
            "data": data,
            "warnings": [],
            "error": null
        }),
        false,
    )
}

fn tool_err(id: Option<Value>, code: &str, message: &str) -> JsonRpcResponse {
    build_tool_result(
        id,
        json!({
            "ok": false,
            "data": null,
            "warnings": [],
            "error": {
                "code": code,
                "message": message
            }
        }),
        true,
    )
}

fn tool_verification_err(
    tool_name: &str,
    id: Option<Value>,
    code: &str,
    message: &str,
    data: Value,
) -> JsonRpcResponse {
    if code == "timeout" {
        warn!(tool = %tool_name, message = %message, "tool verification timeout");
    }

    build_tool_result(
        id,
        json!({
            "ok": false,
            "data": data,
            "warnings": [],
            "error": {
                "code": code,
                "message": message
            }
        }),
        true,
    )
}

fn build_tool_result(id: Option<Value>, payload: Value, is_error: bool) -> JsonRpcResponse {
    let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
    JsonRpcResponse::ok(
        id,
        json!({
            "content": [
                {
                    "type": "text",
                    "text": text
                }
            ],
            "structuredContent": payload,
            "isError": is_error
        }),
    )
}

fn error_code(error: &AdapterError) -> &'static str {
    match error {
        AdapterError::NotConnected(_) => "not_connected",
        AdapterError::WrongMode => "wrong_mode",
        AdapterError::SessionInfo(_) => "session_info_error",
        AdapterError::TargetNotFound(_) => "target_not_found",
        AdapterError::MissingTelemetryVar(_) => "missing_telemetry_var",
        AdapterError::InvalidTelemetryType(_) => "invalid_telemetry_type",
        AdapterError::Broadcast(_) => "broadcast_error",
        AdapterError::UnsupportedReplaySpeed(_) => "invalid_arguments",
        AdapterError::InvalidArgument(_) => "invalid_arguments",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::StubAdapter;
    use std::sync::Arc;

    fn handler() -> IracingMcpHandler {
        IracingMcpHandler::new(Arc::new(StubAdapter::default()))
    }

    #[tokio::test]
    async fn tools_list_returns_all_sixteen_tools() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/list".to_string(),
            params: Value::Null,
        };

        let response = handler.handle(request).await;
        let tools = response.result.unwrap()["tools"].as_array().unwrap().len();

        assert_eq!(tools, 16);
    }

    #[tokio::test]
    async fn get_capabilities_reports_all_tools_supported() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/call".to_string(),
            params: json!({ "name": "get_capabilities", "arguments": {} }),
        };

        let response = handler.handle(request).await;
        let data = response.result.unwrap()["structuredContent"]["data"].clone();
        let caps = data.as_array().unwrap();

        assert_eq!(caps.len(), 16);
        assert!(caps.iter().all(|c| c["status"] == "supported"));
    }

    #[tokio::test]
    async fn get_session_overview_reports_connected() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/call".to_string(),
            params: json!({ "name": "get_session_overview", "arguments": {} }),
        };

        let response = handler.handle(request).await;
        let data = response.result.unwrap()["structuredContent"]["data"].clone();

        assert_eq!(data["connected"], Value::Bool(true));
    }
}
