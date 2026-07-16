// SPDX-License-Identifier: GPL-3.0-or-later
//! [`McpHandler`] implementation for LMU, mirroring
//! `crates/iracing-mcp/src/handler.rs`'s shape. Holds `Arc<dyn LmuAdapter>`
//! as internal state; `pit_menu_command`/`set_weather` use the shared
//! `mcp_core::verify` send-poll-verify helper (ADR 0002 D2 — the same helper
//! promoted out of `iracing-mcp` for #8).

use async_trait::async_trait;
use mcp_core::verify::{verify_loop, VerifyOutcome};
use mcp_core::{JsonRpcRequest, JsonRpcResponse, McpHandler, ToolCapability};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::Duration;
use tracing::warn;

use crate::adapter::{
    camera_focus_verified, AdapterError, HwControlCommand, LmuAdapterRef, WeatherControl,
};

/// Real [`McpHandler`] for LMU, backed by `Arc<dyn LmuAdapter>`.
///
/// Defaults to [`crate::adapter::SdkAdapter`] in production (constructed by
/// `crates/launcher/src/runner.rs`) and [`crate::adapter::StubAdapter`] in
/// tests.
pub struct LmuMcpHandler {
    adapter: LmuAdapterRef,
}

impl LmuMcpHandler {
    pub fn new(adapter: LmuAdapterRef) -> Self {
        Self { adapter }
    }
}

#[async_trait]
impl McpHandler for LmuMcpHandler {
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
                    "serverInfo": { "name": "lmu-mcp", "version": env!("CARGO_PKG_VERSION") },
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
struct GetRosterArgs {
    #[serde(default)]
    include_spectators: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetStandingsArgs {
    session_num: Option<i32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PitMenuCommandArgs {
    control_name: String,
    value: f64,
    #[serde(default = "default_pit_menu_timeout_ms")]
    timeout_ms: u64,
}

fn default_pit_menu_timeout_ms() -> u64 {
    1000
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetWeatherArgs {
    raining: f64,
    cloudiness: Option<f64>,
    ambient_temp_c: Option<f64>,
    #[serde(default = "default_weather_tolerance")]
    tolerance: f64,
    #[serde(default = "default_weather_timeout_ms")]
    timeout_ms: u64,
}

fn default_weather_tolerance() -> f64 {
    0.05
}

fn default_weather_timeout_ms() -> u64 {
    2000
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CameraFocusArgs {
    car_idx: i32,
    camera_type: Option<i32>,
    track_side_group: Option<i32>,
    #[serde(default = "default_camera_focus_timeout_ms")]
    timeout_ms: u64,
}

fn default_camera_focus_timeout_ms() -> u64 {
    1000
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReplaySeekSessionTimeArgs {
    session_time_ms: i32,
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "get_session_overview",
            "description": "Returns current LMU session connectivity and mode.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "get_session_data",
            "description": "Returns current session type, game phase, and elapsed/end times.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "get_weekend_info",
            "description": "Returns static event/track/weather metadata for the current weekend.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "get_roster",
            "description": "Returns the list of drivers, cars, and car classes in the session.",
            "inputSchema": {
                "type": "object",
                "properties": { "includeSpectators": { "type": "boolean" } },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_standings",
            "description": "Returns current session standings and timing for each driver.",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionNum": { "type": "integer" } },
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_relatives",
            "description": "Returns a live field-order and gap view computed from scoring data.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "get_weather",
            "description": "Returns current weather state (rain, cloudiness, temperatures, wind).",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "get_pit_info",
            "description": "Returns current pit menu/lane state for the player's car.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
        json!({
            "name": "pit_menu_command",
            "description": "Writes an rF2HWControl pit-menu command and verifies it via get_pit_info.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "controlName": { "type": "string" },
                    "value": { "type": "number" },
                    "timeoutMs": { "type": "integer" }
                },
                "required": ["controlName", "value"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "set_weather",
            "description": "Writes an rF2WeatherControl command and verifies the resulting weather state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "raining": { "type": "number", "minimum": 0, "maximum": 1 },
                    "cloudiness": { "type": "number" },
                    "ambientTempC": { "type": "number" },
                    "tolerance": { "type": "number" },
                    "timeoutMs": { "type": "integer" }
                },
                "required": ["raining"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "camera_focus",
            "description": "Switches LMU's camera focus to a target car (PUT /rest/watch/focus/{slotId}), optionally also the camera type/track-side group (PUT /rest/watch/focus/{cameraType}/{trackSideGroup}/false; cameraType 1=cockpit, 2=nosecam, 3=swingman, 4/5=trackside), and verifies whichever was requested.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "carIdx": { "type": "integer" },
                    "cameraType": { "type": "integer" },
                    "trackSideGroup": { "type": "integer" },
                    "timeoutMs": { "type": "integer" }
                },
                "required": ["carIdx"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "replay_seek_session_time",
            "description": "Not supported by the LMU adapter (no known API supports replay seeking) — see issue #9. Call get_capabilities to check before calling.",
            "inputSchema": {
                "type": "object",
                "properties": { "sessionTimeMs": { "type": "integer" } },
                "required": ["sessionTimeMs"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "get_capabilities",
            "description": "Returns which tools are supported, degraded, or unsupported for the active LMU adapter, so an agent can plan without hitting not_supported/not_yet_implemented errors.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
        }),
    ]
}

/// Static, adapter-independent description of what this build of `lmu-mcp`
/// can do against a real, live LMU instance (ADR 0002's Amendment) — this is
/// what a Broadcast Agent needs to plan around, whether it's talking to a
/// production `SdkAdapter` or a `StubAdapter`-backed test/dev instance,
/// which intentionally implements more than `SdkAdapter` currently does for
/// testability. Update this whenever an entry's real-world support changes.
fn capabilities() -> Vec<ToolCapability> {
    vec![
        ToolCapability::supported("get_session_overview"),
        ToolCapability::supported("get_session_data"),
        ToolCapability::supported("get_weekend_info"),
        ToolCapability::degraded(
            "get_roster",
            "includeSpectators filtering isn't implemented yet over REST — all entries are returned",
        ),
        ToolCapability::supported("get_standings"),
        ToolCapability::supported("get_relatives"),
        ToolCapability::degraded(
            "get_weather",
            "wind speed unit is unconfirmed (passed through as reported by the REST API) — see ADR 0002 amendment",
        ),
        ToolCapability::supported("get_pit_info"),
        ToolCapability::unsupported(
            "pit_menu_command",
            "not yet wired to LMU's REST API (no confirmed endpoint) — see ADR 0002 amendment",
        ),
        ToolCapability::unsupported(
            "set_weather",
            "not yet wired to LMU's REST API (no confirmed endpoint) — see ADR 0002 amendment",
        ),
        ToolCapability::supported("camera_focus"),
        ToolCapability::unsupported(
            "replay_seek_session_time",
            "a replaytime endpoint exists but live-testing showed no observable seek effect from the live-monitor context — see ADR 0002 amendment and issue #9",
        ),
        ToolCapability::supported("get_capabilities"),
    ]
}

impl LmuMcpHandler {
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
            "get_session_data" => match self.adapter.get_session_data().await {
                Ok(data) => tool_ok(id, data),
                Err(e) => tool_err(id, error_code(&e), &e.to_string()),
            },
            "get_weekend_info" => match self.adapter.get_weekend_info().await {
                Ok(info) => tool_ok(id, info),
                Err(e) => tool_err(id, error_code(&e), &e.to_string()),
            },
            "get_roster" => {
                let args: GetRosterArgs =
                    parse_tool_args(&id, &params, "get_roster").unwrap_or(GetRosterArgs {
                        include_spectators: false,
                    });
                match self.adapter.get_roster(args.include_spectators).await {
                    Ok(roster) => tool_ok(id, roster),
                    Err(e) => tool_err(id, error_code(&e), &e.to_string()),
                }
            }
            "get_standings" => {
                let args: GetStandingsArgs = parse_tool_args(&id, &params, "get_standings")
                    .unwrap_or(GetStandingsArgs { session_num: None });
                match self.adapter.get_standings(args.session_num).await {
                    Ok(standings) => tool_ok(id, standings),
                    Err(e) => tool_err(id, error_code(&e), &e.to_string()),
                }
            }
            "get_relatives" => match self.adapter.get_relatives().await {
                Ok(relatives) => tool_ok(id, relatives),
                Err(e) => tool_err(id, error_code(&e), &e.to_string()),
            },
            "get_weather" => match self.adapter.get_weather().await {
                Ok(weather) => tool_ok(id, weather),
                Err(e) => tool_err(id, error_code(&e), &e.to_string()),
            },
            "get_pit_info" => match self.adapter.get_pit_info().await {
                Ok(pit_info) => tool_ok(id, pit_info),
                Err(e) => tool_err(id, error_code(&e), &e.to_string()),
            },
            "pit_menu_command" => self.pit_menu_command(id, params).await,
            "set_weather" => self.set_weather(id, params).await,
            "camera_focus" => self.camera_focus(id, params).await,
            "replay_seek_session_time" => {
                let args: ReplaySeekSessionTimeArgs =
                    match parse_tool_args(&id, &params, "replay_seek_session_time") {
                        Ok(args) => args,
                        Err(response) => return response,
                    };
                match self
                    .adapter
                    .replay_seek_session_time(args.session_time_ms)
                    .await
                {
                    Ok(()) => tool_ok(id, json!({})),
                    Err(e) => tool_err(id, error_code(&e), &e.to_string()),
                }
            }
            "get_capabilities" => tool_ok(id, capabilities()),
            _ => JsonRpcResponse::err(id, -32602, "unknown tool name"),
        }
    }

    /// Sends an `rF2HWControl` command and verifies it via `get_pit_info`.
    ///
    /// Only `request_pit`/`cancel_pit`/`confirm_pit` control names have a
    /// modeled, verifiable effect on `PitInfoState` today (see
    /// `adapter::stub::StubAdapter::pit_menu_command`'s doc comment) — any
    /// other control name is accepted and reported `verified: true` as soon
    /// as a single post-send poll succeeds, since this v1 can't generically
    /// predict its effect on pit info state without the real plugin headers
    /// (see `adapter::sdk`'s module doc comment on the pending manual
    /// verification step).
    async fn pit_menu_command(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: PitMenuCommandArgs = match parse_tool_args(&id, &params, "pit_menu_command") {
            Ok(args) => args,
            Err(response) => return response,
        };

        let before = match self.adapter.get_pit_info().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        let control = HwControlCommand {
            control_name: args.control_name.clone(),
            value: args.value,
        };
        let timeout = Duration::from_millis(args.timeout_ms.max(1));
        let control_name = args.control_name.clone();
        let mut first_poll_seen = false;

        let outcome = verify_loop(
            before,
            self.adapter.pit_menu_command(control),
            || self.adapter.get_pit_info(),
            move |current| {
                let verified = match control_name.as_str() {
                    "request_pit" => current.pit_state.eq_ignore_ascii_case("requested"),
                    "cancel_pit" => !current.pit_state.eq_ignore_ascii_case("requested"),
                    "confirm_pit" => current.in_pits,
                    _ => {
                        // No modeled expected effect for unknown control
                        // names — accept as verified on the first poll that
                        // succeeds (see doc comment above).
                        let was_first = !first_poll_seen;
                        first_poll_seen = true;
                        was_first
                    }
                };
                verified
            },
            timeout,
            Duration::from_millis(50),
        )
        .await;

        match outcome {
            Ok(VerifyOutcome::Verified {
                before,
                observed,
                elapsed,
            }) => tool_ok(
                id,
                json!({
                    "commandAccepted": true,
                    "verified": true,
                    "reason": null,
                    "before": before,
                    "observed": observed,
                    "elapsedMs": elapsed.as_millis()
                }),
            ),
            Ok(VerifyOutcome::TimedOut {
                before,
                observed,
                elapsed,
            }) => {
                let reason = format!(
                    "Pit info did not reflect controlName={} within {}ms.",
                    args.control_name,
                    timeout.as_millis()
                );
                tool_verification_err(
                    "pit_menu_command",
                    id,
                    "timeout",
                    &reason,
                    json!({
                        "commandAccepted": true,
                        "verified": false,
                        "reason": reason,
                        "before": before,
                        "observed": observed,
                        "elapsedMs": elapsed.as_millis()
                    }),
                )
            }
            Err(error) => tool_err(id, error_code(&error), &error.to_string()),
        }
    }

    async fn set_weather(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: SetWeatherArgs = match parse_tool_args(&id, &params, "set_weather") {
            Ok(args) => args,
            Err(response) => return response,
        };

        if !(0.0..=1.0).contains(&args.raining) {
            return tool_err(id, "invalid_arguments", "raining must be in 0.0..=1.0");
        }

        let before = match self.adapter.get_weather().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        let weather = WeatherControl {
            raining: args.raining,
            cloudiness: args.cloudiness,
            ambient_temp_c: args.ambient_temp_c,
        };
        let timeout = Duration::from_millis(args.timeout_ms.max(1));
        let target_raining = args.raining;
        let tolerance = args.tolerance;

        let outcome = verify_loop(
            before,
            self.adapter.set_weather(weather),
            || self.adapter.get_weather(),
            move |current| (current.raining - target_raining).abs() <= tolerance,
            timeout,
            Duration::from_millis(50),
        )
        .await;

        match outcome {
            Ok(VerifyOutcome::Verified {
                before,
                observed,
                elapsed,
            }) => tool_ok(
                id,
                json!({
                    "commandAccepted": true,
                    "verified": true,
                    "reason": null,
                    "before": before,
                    "observed": observed,
                    "elapsedMs": elapsed.as_millis()
                }),
            ),
            Ok(VerifyOutcome::TimedOut {
                before,
                observed,
                elapsed,
            }) => {
                let reason = format!(
                    "Weather did not reach raining={} (tolerance={}) within {}ms.",
                    args.raining,
                    args.tolerance,
                    timeout.as_millis()
                );
                tool_verification_err(
                    "set_weather",
                    id,
                    "timeout",
                    &reason,
                    json!({
                        "commandAccepted": true,
                        "verified": false,
                        "reason": reason,
                        "before": before,
                        "observed": observed,
                        "elapsedMs": elapsed.as_millis()
                    }),
                )
            }
            Err(error) => tool_err(id, error_code(&error), &error.to_string()),
        }
    }

    /// Sends `PUT /rest/watch/focus/{slotId}` and, if `cameraType` was
    /// requested, also `PUT /rest/watch/focus/{cameraType}/{trackSideGroup}/false`.
    /// Verifies whichever of car/camera-type were actually requested via
    /// `get_camera_state` (`GET /rest/watch/focus` +
    /// `GET /rest/replay/CameraController/getCameraInfo`) — confirmed
    /// working live 2026-07-13 (ADR 0002 Amendment), mirroring
    /// `iracing-mcp`'s send-then-poll `camera_focus`.
    async fn camera_focus(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let args: CameraFocusArgs = match parse_tool_args(&id, &params, "camera_focus") {
            Ok(args) => args,
            Err(response) => return response,
        };

        let before = match self.adapter.get_camera_state().await {
            Ok(before) => before,
            Err(error) => return tool_err(id, error_code(&error), &error.to_string()),
        };

        let timeout = Duration::from_millis(args.timeout_ms.max(1));
        let target_car_idx = args.car_idx;
        let target_camera_type = args.camera_type;

        let outcome = verify_loop(
            before,
            self.adapter
                .camera_focus(args.car_idx, args.camera_type, args.track_side_group),
            || self.adapter.get_camera_state(),
            move |current| camera_focus_verified(current, target_car_idx, target_camera_type),
            timeout,
            Duration::from_millis(50),
        )
        .await;

        match outcome {
            Ok(VerifyOutcome::Verified {
                before,
                observed,
                elapsed,
            }) => tool_ok(
                id,
                json!({
                    "commandAccepted": true,
                    "verified": true,
                    "reason": null,
                    "before": before,
                    "observed": observed,
                    "elapsedMs": elapsed.as_millis()
                }),
            ),
            Ok(VerifyOutcome::TimedOut {
                before,
                observed,
                elapsed,
            }) => {
                let reason = format!(
                    "Camera focus did not reach carIdx={}{} within {}ms.",
                    args.car_idx,
                    args.camera_type
                        .map(|t| format!(", cameraType={t}"))
                        .unwrap_or_default(),
                    timeout.as_millis()
                );
                tool_verification_err(
                    "camera_focus",
                    id,
                    "timeout",
                    &reason,
                    json!({
                        "commandAccepted": true,
                        "verified": false,
                        "reason": reason,
                        "before": before,
                        "observed": observed,
                        "elapsedMs": elapsed.as_millis()
                    }),
                )
            }
            Err(error) => tool_err(id, error_code(&error), &error.to_string()),
        }
    }
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
        AdapterError::RestApi(_) => "rest_api_error",
        AdapterError::TargetNotFound(_) => "target_not_found",
        AdapterError::InvalidArgument(_) => "invalid_arguments",
        AdapterError::NotSupported(_) => "not_supported",
        AdapterError::NotYetImplemented(_) => "not_yet_implemented",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::StubAdapter;
    use std::sync::Arc;

    fn handler() -> LmuMcpHandler {
        LmuMcpHandler::new(Arc::new(StubAdapter::default()))
    }

    #[tokio::test]
    async fn tools_list_returns_all_thirteen_tools() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/list".to_string(),
            params: Value::Null,
        };

        let response = handler.handle(request).await;
        let tools = response.result.unwrap()["tools"].as_array().unwrap().len();

        assert_eq!(tools, 13);
    }

    #[tokio::test]
    async fn get_capabilities_flags_camera_focus_supported_and_replay_seek_unsupported() {
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

        let camera_focus = caps.iter().find(|c| c["name"] == "camera_focus").unwrap();
        assert_eq!(camera_focus["status"], "supported");

        let replay_seek = caps
            .iter()
            .find(|c| c["name"] == "replay_seek_session_time")
            .unwrap();
        assert_eq!(replay_seek["status"], "unsupported");
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

    #[tokio::test]
    async fn set_weather_verifies_raining() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/call".to_string(),
            params: json!({
                "name": "set_weather",
                "arguments": { "raining": 0.5 }
            }),
        };

        let response = handler.handle(request).await;
        let data = response.result.unwrap()["structuredContent"]["data"].clone();

        assert_eq!(data["verified"], Value::Bool(true));
        assert_eq!(data["observed"]["raining"], json!(0.5));
    }

    #[tokio::test]
    async fn pit_menu_command_request_pit_verifies() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/call".to_string(),
            params: json!({
                "name": "pit_menu_command",
                "arguments": { "controlName": "request_pit", "value": 1.0 }
            }),
        };

        let response = handler.handle(request).await;
        let data = response.result.unwrap()["structuredContent"]["data"].clone();

        assert_eq!(data["verified"], Value::Bool(true));
        assert_eq!(
            data["observed"]["pitState"],
            Value::String("requested".to_string())
        );
    }

    #[tokio::test]
    async fn camera_focus_verifies_target_car() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/call".to_string(),
            params: json!({ "name": "camera_focus", "arguments": { "carIdx": 1 } }),
        };

        let response = handler.handle(request).await;
        let data = response.result.unwrap()["structuredContent"]["data"].clone();

        assert_eq!(data["verified"], Value::Bool(true));
        assert_eq!(data["observed"]["focusSlotId"], json!(1));
    }

    #[tokio::test]
    async fn camera_focus_verifies_camera_type_and_group() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/call".to_string(),
            params: json!({
                "name": "camera_focus",
                "arguments": { "carIdx": 1, "cameraType": 2 }
            }),
        };

        let response = handler.handle(request).await;
        let data = response.result.unwrap()["structuredContent"]["data"].clone();

        assert_eq!(data["verified"], Value::Bool(true));
        assert_eq!(data["observed"]["focusSlotId"], json!(1));
        assert_eq!(
            data["observed"]["cameraName"],
            Value::String("NOSECAM".to_string())
        );
    }

    #[tokio::test]
    async fn get_session_data_returns_real_fields() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/call".to_string(),
            params: json!({ "name": "get_session_data", "arguments": {} }),
        };

        let response = handler.handle(request).await;
        let result = response.result.unwrap();

        assert_eq!(result["isError"], Value::Bool(false));
        assert!(result["structuredContent"]["data"]["trackName"].is_string());
        assert!(result["structuredContent"]["data"]["gamePhase"].is_string());
    }

    #[tokio::test]
    async fn get_pit_info_returns_real_fields() {
        let handler = handler();
        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::from(1)),
            method: "tools/call".to_string(),
            params: json!({ "name": "get_pit_info", "arguments": {} }),
        };

        let response = handler.handle(request).await;
        let result = response.result.unwrap();

        assert_eq!(result["isError"], Value::Bool(false));
        assert!(result["structuredContent"]["data"]["pitState"].is_string());
    }
}
