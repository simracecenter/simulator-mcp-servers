// SPDX-License-Identifier: GPL-3.0-or-later
//! `LmuAdapter` trait + domain types, per
//! [ADR 0002 D3](../../../../docs/adr/0002-lmu-adapter-design.md#d3--draft-lmuadapter-trait-shape).
//!
//! Structurally mirrors `crates/iracing-mcp/src/adapter/mod.rs`: one method
//! per capability, domain-typed returns, a shared `AdapterError` enum — no
//! MCP/JSON-RPC or raw shared-memory types leak into this layer. `Sdk` and
//! `Stub` implementations live in sibling modules.

use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod sdk;
pub mod stub;

pub use sdk::SdkAdapter;
pub use stub::StubAdapter;

pub type LmuAdapterRef = Arc<dyn LmuAdapter>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionOverview {
    pub connected: bool,
    pub is_replay: bool,
    pub is_in_car: bool,
    pub session_name: String,
    pub track_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionData {
    pub track_name: String,
    pub session_type: String,
    pub game_phase: String,
    pub current_et_sec: f64,
    pub end_et_sec: f64,
    pub max_laps: i32,
    pub driver_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeekendInfo {
    pub track_name: String,
    pub session_type: String,
    pub max_laps: i32,
    pub end_et_sec: f64,
    pub ambient_temp_c: f64,
    pub track_temp_c: f64,
    pub raining: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterEntry {
    pub id: i32,
    pub driver_name: String,
    pub vehicle_name: String,
    pub vehicle_class: String,
    pub is_player: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    pub entries: Vec<RosterEntry>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StandingsEntry {
    pub place: i32,
    pub id: i32,
    pub driver_name: String,
    pub vehicle_name: String,
    pub laps_completed: i32,
    pub sector: i32,
    pub best_lap_time_sec: f64,
    pub last_lap_time_sec: f64,
    pub time_behind_leader_sec: f64,
    pub laps_behind_leader: i32,
    pub in_pits: bool,
    pub finish_status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Standings {
    pub session_type: String,
    pub positions: Vec<StandingsEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelativeEntry {
    pub id: i32,
    pub driver_name: String,
    pub place: i32,
    pub laps_completed: i32,
    pub time_behind_next_sec: f64,
    pub laps_behind_next: i32,
    pub in_pits: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Relatives {
    pub entries: Vec<RelativeEntry>,
    pub count: usize,
}

/// `rF2Weather` — 1 FPS per ADR 0002's research table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeatherState {
    pub ambient_temp_c: f64,
    pub track_temp_c: f64,
    pub raining: f64,
    pub cloudiness: f64,
    pub wind_speed_ms: f64,
}

/// `rF2PitInfo` — 100 FPS per ADR 0002's research table.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PitInfoState {
    pub in_pits: bool,
    pub pit_state: String,
    pub num_pitstops: i32,
    pub num_penalties: i32,
}

/// A single `rF2HWControl` write. `control_name`/`value` are deliberately
/// generic (ADR 0002 D3: "exact fields are deferred to implementation") —
/// the specific control names this maps to are only confirmed by the
/// manual live-verification done criterion, not decided here.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HwControlCommand {
    pub control_name: String,
    pub value: f64,
}

/// An `rF2WeatherControl` write. `raining` is the only field verified via
/// the read path (`rF2Weather`) today; `cloudiness`/`ambient_temp_c` are
/// accepted but not independently verified in v1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeatherControl {
    pub raining: f64,
    #[serde(default)]
    pub cloudiness: Option<f64>,
    #[serde(default)]
    pub ambient_temp_c: Option<f64>,
}

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("LMU shared memory is not connected: {0}")]
    NotConnected(String),
    #[error("failed to read LMU shared memory: {0}")]
    SharedMemory(String),
    #[error("target not found: {0}")]
    TargetNotFound(String),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// `camera_focus`/`replay_seek_session_time` return this unconditionally
    /// in v1 — no known `rF2` input buffer exists for either today (ADR 0002
    /// D2, Open follow-ups). Tracked further in #9.
    #[error("{0} is not supported by the LMU adapter yet (see issue #9)")]
    NotSupported(&'static str),
}

#[async_trait]
pub trait LmuAdapter: Send + Sync {
    // Read path — same polling shape as IracingAdapter, backed by
    // rF2Scoring/rF2Telemetry.
    async fn get_session_overview(&self) -> SessionOverview;
    async fn get_session_data(&self) -> Result<SessionData, AdapterError>;
    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError>;
    async fn get_roster(&self, include_spectators: bool) -> Result<Roster, AdapterError>;
    async fn get_standings(&self, session_num: Option<i32>) -> Result<Standings, AdapterError>;
    async fn get_relatives(&self) -> Result<Relatives, AdapterError>;
    async fn get_weather(&self) -> Result<WeatherState, AdapterError>;
    async fn get_pit_info(&self) -> Result<PitInfoState, AdapterError>;

    // Command path — input buffers, verified via the read path above (no
    // broadcast gap to bridge, per ADR 0002 D2).
    async fn pit_menu_command(&self, control: HwControlCommand) -> Result<(), AdapterError>;
    async fn set_weather(&self, weather: WeatherControl) -> Result<(), AdapterError>;

    // Unconfirmed — no known input buffer for these; both implementations
    // return `AdapterError::NotSupported` unconditionally in v1.
    async fn camera_focus(&self, car_idx: i32) -> Result<(), AdapterError>;
    async fn replay_seek_session_time(&self, session_time_ms: i32) -> Result<(), AdapterError>;
}
