// SPDX-License-Identifier: GPL-3.0-or-later
//! `IracingAdapter` trait + domain types, ported from `margic/iracing-mcp`
//! (`crates/iracing-mcp-server/src/adapter/mod.rs`, ADR 0001 D5).

use std::sync::Arc;

use async_trait::async_trait;
use mcp_core::{Read, SnapshotMeta};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod sdk;
pub mod stub;

pub use sdk::SdkAdapter;
pub use stub::StubAdapter;

pub type AdapterRef = Arc<dyn IracingAdapter>;

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
    pub track_display_name: String,
    pub current_session_type: String,
    pub driver_count: usize,
    pub session_count: usize,
}

// ── M1 read types ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WeekendInfo {
    pub track_name: String,
    pub track_id: i32,
    pub track_display_name: String,
    pub track_config_name: String,
    pub track_length_km: f64,
    pub track_city: String,
    pub track_country: String,
    pub track_num_turns: i32,
    pub track_pit_speed_limit_kph: f64,
    pub track_type: String,
    pub series_id: i32,
    pub season_id: i32,
    pub session_id: i32,
    pub sub_session_id: i32,
    pub official: bool,
    pub event_type: String,
    pub category: String,
    pub sim_mode: String,
    pub team_racing: bool,
    pub weather_type: String,
    pub skies: String,
    pub surface_temp_c: f64,
    pub air_temp_c: f64,
    pub wind_vel_ms: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterEntry {
    pub car_idx: i32,
    pub user_name: String,
    pub abbrev_name: String,
    pub initials: String,
    pub user_id: i64,
    pub team_name: String,
    pub car_number: String,
    pub car_number_raw: i32,
    pub car_id: i32,
    pub car_screen_name: String,
    pub car_class_id: i32,
    pub car_class_short_name: String,
    pub irating: i32,
    pub lic_string: String,
    pub is_spectator: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Roster {
    pub entries: Vec<RosterEntry>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraEntry {
    pub camera_num: i32,
    pub camera_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraGroup {
    pub group_num: i32,
    pub group_name: String,
    pub is_scenic: bool,
    pub cameras: Vec<CameraEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CameraGroupList {
    pub groups: Vec<CameraGroup>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionPosition {
    pub position: i32,
    pub class_position: i32,
    pub car_idx: i32,
    pub lap: i32,
    pub laps_complete: i32,
    pub fastest_lap: i32,
    pub fastest_time: f64,
    pub last_time: f64,
    pub incidents: i32,
    pub reason_out: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Standings {
    pub session_num: i32,
    pub session_type: String,
    pub positions: Vec<SessionPosition>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelativeEntry {
    pub position: i32,
    pub class_position: i32,
    pub car_idx: i32,
    pub car_number: String,
    pub display_name: String,
    pub lap: i32,
    pub lap_dist_pct: Option<f64>,
    pub is_in_pit: bool,
    pub gap_ahead_sec: Option<f64>,
    pub gap_behind_sec: Option<f64>,
    pub delta_laps: i32,
    pub estimated_time_sec: Option<f64>,
    pub f2_time_sec: Option<f64>,
    pub track_surface: Option<String>,
    pub in_world: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Relatives {
    pub basis: String,
    pub session_num: i32,
    pub entries: Vec<RelativeEntry>,
    pub count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriverMatch {
    pub car_idx: i32,
    pub display_name: String,
    pub car_number: String,
    pub confidence: f64,
    pub match_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveDriverResult {
    pub best_match: Option<DriverMatch>,
    pub candidates: Vec<DriverMatch>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReplaySeekFrameMode {
    Begin,
    Current,
    End,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaySearchMode {
    ToStart,
    ToEnd,
    PrevSession,
    NextSession,
    PrevLap,
    NextLap,
    PrevFrame,
    NextFrame,
    PrevIncident,
    NextIncident,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayState {
    pub connected: bool,
    pub is_on_track: bool,
    pub is_in_garage: bool,
    pub is_replay_playing: bool,
    pub replay_play_speed: i32,
    pub replay_play_slow_motion: bool,
    pub replay_frame_num: i32,
    pub replay_frame_num_end: i32,
    pub replay_session_num: i32,
    pub replay_session_time: f64,
    pub cam_car_idx: i32,
    pub cam_group_number: i32,
    pub cam_camera_number: i32,
    pub cam_camera_state: i32,
}

pub const REPLAY_LIVE_EDGE_TOLERANCE_FRAMES: i32 = 60;

impl ReplayState {
    pub fn frames_behind_live(&self) -> i32 {
        self.replay_frame_num_end
    }

    pub fn at_live_edge(&self) -> bool {
        self.frames_behind_live() <= REPLAY_LIVE_EDGE_TOLERANCE_FRAMES
    }

    pub fn is_replay(&self) -> bool {
        !self.at_live_edge() || self.replay_play_speed == 0 || self.replay_play_slow_motion
    }

    pub fn tool_data(&self) -> serde_json::Value {
        let mut data = serde_json::to_value(self).expect("ReplayState should serialize");
        let object = data
            .as_object_mut()
            .expect("serialized ReplayState should be an object");
        object.insert(
            "atLiveEdge".to_string(),
            serde_json::json!(self.at_live_edge()),
        );
        object.insert(
            "framesBehindLive".to_string(),
            serde_json::json!(self.frames_behind_live()),
        );
        data
    }
}

#[derive(Debug, Error)]
pub enum AdapterError {
    #[error("iRacing SDK is not connected: {0}")]
    NotConnected(String),
    #[error("operation requires replay or spectator mode")]
    WrongMode,
    #[error("failed to read session info: {0}")]
    SessionInfo(String),
    #[error("target not found: {0}")]
    TargetNotFound(String),
    #[error("missing telemetry variable {0}")]
    MissingTelemetryVar(&'static str),
    #[error("invalid telemetry type for {0}")]
    InvalidTelemetryType(&'static str),
    #[error("failed to send broadcast message: {0}")]
    Broadcast(String),
    #[error("unsupported replay speed {0}; expected 0..=255 for current SDK wrapper")]
    UnsupportedReplaySpeed(i32),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("internal error: {0}")]
    Internal(String),
}

#[async_trait]
pub trait IracingAdapter: Send + Sync {
    async fn get_session_overview(&self) -> Read<SessionOverview>;
    async fn get_session_data(&self) -> Result<Read<SessionData>, AdapterError>;
    async fn get_replay_state(&self) -> Result<Read<ReplayState>, AdapterError>;
    async fn snapshot_meta(&self) -> SnapshotMeta;
    async fn set_replay_playback(&self, speed: i32, slow_motion: bool) -> Result<(), AdapterError>;
    async fn replay_seek_session_time(
        &self,
        session_num: i32,
        session_time_ms: i32,
    ) -> Result<(), AdapterError>;
    async fn replay_seek_frame(
        &self,
        mode: ReplaySeekFrameMode,
        frame: i32,
    ) -> Result<(), AdapterError>;
    async fn replay_search_event(&self, mode: ReplaySearchMode) -> Result<(), AdapterError>;
    async fn camera_set_state(&self, state_bits: i32) -> Result<(), AdapterError>;
    async fn camera_focus(
        &self,
        car_idx: i32,
        group_number: Option<i32>,
        camera_number: Option<i32>,
    ) -> Result<(), AdapterError>;
    async fn get_weekend_info(&self) -> Result<Read<WeekendInfo>, AdapterError>;
    async fn get_roster(
        &self,
        include_spectators: bool,
        include_pace_car: bool,
    ) -> Result<Read<Roster>, AdapterError>;
    async fn get_camera_groups(&self) -> Result<Read<CameraGroupList>, AdapterError>;
    async fn get_standings(
        &self,
        session_num: Option<i32>,
    ) -> Result<Read<Standings>, AdapterError>;
    async fn get_relatives(&self) -> Result<Read<Relatives>, AdapterError>;
    async fn resolve_driver(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Read<ResolveDriverResult>, AdapterError>;
}

#[cfg(test)]
mod replay_state_tests {
    use super::{ReplayState, REPLAY_LIVE_EDGE_TOLERANCE_FRAMES};

    fn state(frames_behind_live: i32, speed: i32, slow_motion: bool) -> ReplayState {
        ReplayState {
            connected: true,
            is_on_track: false,
            is_in_garage: false,
            is_replay_playing: true,
            replay_play_speed: speed,
            replay_play_slow_motion: slow_motion,
            replay_frame_num: 0,
            replay_frame_num_end: frames_behind_live,
            replay_session_num: 0,
            replay_session_time: 0.0,
            cam_car_idx: 0,
            cam_group_number: 0,
            cam_camera_number: 0,
            cam_camera_state: 0,
        }
    }

    #[test]
    fn live_edge_evidence_is_not_replay() {
        let state = state(1, 1, false);

        assert!(!state.is_replay());
        assert!(state.at_live_edge());
        assert_eq!(state.frames_behind_live(), 1);
    }

    #[test]
    fn scrubbed_evidence_is_replay() {
        let state = state(35_848, 1, false);

        assert!(state.is_replay());
        assert!(!state.at_live_edge());
    }

    #[test]
    fn paused_playback_is_replay_even_at_live_edge() {
        assert!(state(1, 0, false).is_replay());
    }

    #[test]
    fn slow_motion_is_replay_even_at_live_edge() {
        assert!(state(1, 1, true).is_replay());
    }

    #[test]
    fn tolerance_boundary_is_inclusive_for_live_edge() {
        assert!(!state(REPLAY_LIVE_EDGE_TOLERANCE_FRAMES, 1, false).is_replay());
        assert!(state(REPLAY_LIVE_EDGE_TOLERANCE_FRAMES + 1, 1, false).is_replay());
    }

    #[test]
    fn tool_data_adds_live_edge_fields_without_removing_state() {
        let data = state(35_848, 1, false).tool_data();

        assert_eq!(data["framesBehindLive"], 35_848);
        assert_eq!(data["atLiveEdge"], false);
        assert_eq!(data["replayFrameNum"], 0);
    }
}
