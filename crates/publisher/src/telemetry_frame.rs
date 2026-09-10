// SPDX-License-Identifier: GPL-3.0-or-later

/// A single decoded telemetry frame from the iRacing session stream.
#[derive(serde::Deserialize, Clone, Debug)]
#[serde(rename_all = "snake_case")]
pub struct TelemetryFrame {
    pub lap: u8,
    pub session_time: f32,
    pub lap_dist_pct: f32,
    pub player_car_idx: u8,
    pub player_car_position: u8,
    pub on_pit_road: bool,
    pub session_flags: u32,
    /// Indexed by car_idx. Values < -0.5 are iRacing sentinels (inactive slot).
    pub car_idx_lap_dist_pct: Vec<f32>,
    /// Indexed by car_idx. 0 = inactive.
    pub car_idx_position: Vec<u8>,
    /// Indexed by car_idx.
    pub car_idx_on_pit_road: Vec<bool>,
    /// Indexed by car_idx.
    #[serde(default)]
    pub car_idx_track_surface: Vec<i32>,
    /// Elapsed time of the last completed lap in seconds. 0.0 until lap 1 is done.
    /// Used for anchor-count bootstrap only; absent from JSONL fixtures (defaults to 0.0).
    #[serde(default)]
    pub lap_last_lap_time: f32,
    /// iRacing `SessionInfoUpdate` monotonic counter from the mmap header.
    /// Increments whenever the `SessionInfo` YAML blob changes.
    /// Used by `RosterCache::needs_update()`.
    /// Absent from JSONL fixtures; defaults to 0.
    #[serde(default)]
    pub session_info_update: u32,
    /// iRacing `SessionTick` — sim step counter (~16ms resolution).
    /// Used as the deduplication key on Race Control: first writer wins per
    /// `(raceSessionId, session_tick, event_type)`. Absent from JSONL fixtures.
    #[serde(default)]
    pub session_tick: i64,
    /// iRacing `SessionState` enum.
    /// Values: Invalid=0, GetInCar=1, Warmup=2, ParadeLaps=3, Racing=4,
    ///         Checkered=5, CoolDown=6.
    /// Needed for flag/session lifecycle events. Absent from JSONL fixtures.
    #[serde(default)]
    pub session_state: i32,
    /// iRacing `SessionNum` — which sub-session is active.
    /// Typical values: practice=0, qualifying=1, race=2.
    /// Absent from JSONL fixtures; defaults to 0.
    #[serde(default)]
    pub session_num: i32,
    /// iRacing `SessionTimeRemain` — seconds left in the current session.
    /// `None` when the variable is absent or the session is untimed
    /// (iRacing reports a sentinel of one week for unlimited sessions).
    #[serde(default)]
    pub session_time_remain: Option<f64>,
    /// iRacing `SessionLapsRemainEx` — laps left in the current session.
    /// `None` when the variable is absent or the session is not lap-limited.
    #[serde(default)]
    pub session_laps_remain: Option<i32>,
    /// Player incident count from iRacing (`PlayerCarMyIncidentCount`).
    /// Absent from some fixtures/mocks; defaults to 0.
    #[serde(default)]
    pub player_incident_count: i32,
    /// Laps completed per car, indexed by car_idx.
    /// Used to provide `leaderLap` context in `PublisherEventContext`.
    /// Absent from JSONL fixtures; defaults to empty vec.
    #[serde(default)]
    pub car_idx_lap_completed: Vec<i32>,
    #[serde(default)]
    pub lf_temp_m: f32,
    #[serde(default)]
    pub rf_temp_m: f32,
    #[serde(default)]
    pub lr_temp_m: f32,
    #[serde(default)]
    pub rr_temp_m: f32,
    #[serde(default)]
    pub fuel_level: f32,
    #[serde(default)]
    pub throttle: f32,
    #[serde(default)]
    pub brake: f32,
    #[serde(default)]
    pub speed: f32,
}
