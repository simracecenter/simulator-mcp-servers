// SPDX-License-Identifier: GPL-3.0-or-later
//! `StubAdapter` — in-memory fixture for Linux CI/tests, mirroring
//! `crates/iracing-mcp/src/adapter/stub.rs`'s shape.

use std::sync::Mutex;

use async_trait::async_trait;

use super::CameraFocusState;
use super::{
    AdapterError, HwControlCommand, LmuAdapter, PitInfoState, RelativeEntry, Relatives, Roster,
    RosterEntry, SessionData, SessionOverview, Standings, StandingsEntry, WeatherControl,
    WeatherState, WeekendInfo,
};

#[derive(Debug, Clone)]
struct StubState {
    connected: bool,
    in_pits: bool,
    pit_state: String,
    num_pitstops: i32,
    raining: f64,
    cloudiness: f64,
    ambient_temp_c: f64,
    track_temp_c: f64,
    /// Current camera-focus slot id, mirroring LMU's REST `GET`/`PUT
    /// /rest/watch/focus[/{slotId}]` (ADR 0002 Amendment). Defaults to `0`
    /// to match the player's own car in this fixture's roster/standings.
    focus: i32,
    camera_name: String,
    camera_group: String,
}

impl Default for StubState {
    fn default() -> Self {
        Self {
            connected: true,
            in_pits: false,
            pit_state: "none".to_string(),
            num_pitstops: 0,
            raining: 0.0,
            cloudiness: 0.2,
            ambient_temp_c: 25.0,
            track_temp_c: 32.0,
            focus: 0,
            camera_name: "COCKPIT".to_string(),
            camera_group: "Driving".to_string(),
        }
    }
}

#[derive(Debug)]
pub struct StubAdapter {
    state: Mutex<StubState>,
}

impl Default for StubAdapter {
    fn default() -> Self {
        Self {
            state: Mutex::new(StubState::default()),
        }
    }
}

#[async_trait]
impl LmuAdapter for StubAdapter {
    async fn get_session_overview(&self) -> SessionOverview {
        let state = self.state.lock().expect("not poisoned").clone();
        SessionOverview {
            connected: state.connected,
            is_replay: false,
            is_in_car: !state.in_pits,
            session_name: "Practice".to_string(),
            track_name: "Stub Circuit".to_string(),
        }
    }

    async fn get_session_data(&self) -> Result<SessionData, AdapterError> {
        Ok(SessionData {
            track_name: "Stub Circuit".to_string(),
            session_type: "Practice".to_string(),
            game_phase: "GreenFlag".to_string(),
            current_et_sec: 305.5,
            end_et_sec: 3600.0,
            max_laps: 0,
            driver_count: 2,
        })
    }

    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError> {
        let state = self.state.lock().expect("not poisoned").clone();
        Ok(WeekendInfo {
            track_name: "Stub Circuit".to_string(),
            session_type: "Practice".to_string(),
            max_laps: 0,
            end_et_sec: 3600.0,
            ambient_temp_c: state.ambient_temp_c,
            track_temp_c: state.track_temp_c,
            raining: state.raining,
        })
    }

    async fn get_roster(&self, _include_spectators: bool) -> Result<Roster, AdapterError> {
        let entries = vec![
            RosterEntry {
                id: 0,
                driver_name: "Alice Driver".to_string(),
                vehicle_name: "Stub Hypercar".to_string(),
                vehicle_class: "Hypercar".to_string(),
                is_player: true,
            },
            RosterEntry {
                id: 1,
                driver_name: "Bob Racer".to_string(),
                vehicle_name: "Stub Hypercar".to_string(),
                vehicle_class: "Hypercar".to_string(),
                is_player: false,
            },
        ];
        let count = entries.len();
        Ok(Roster { entries, count })
    }

    async fn get_standings(&self, _session_num: Option<i32>) -> Result<Standings, AdapterError> {
        let state = self.state.lock().expect("not poisoned").clone();
        let positions = vec![
            StandingsEntry {
                place: 1,
                id: 1,
                driver_name: "Bob Racer".to_string(),
                vehicle_name: "Stub Hypercar".to_string(),
                laps_completed: 4,
                sector: 2,
                best_lap_time_sec: 92.5,
                last_lap_time_sec: 93.1,
                time_behind_leader_sec: 0.0,
                laps_behind_leader: 0,
                in_pits: false,
                finish_status: "none".to_string(),
            },
            StandingsEntry {
                place: 2,
                id: 0,
                driver_name: "Alice Driver".to_string(),
                vehicle_name: "Stub Hypercar".to_string(),
                laps_completed: 4,
                sector: 1,
                best_lap_time_sec: 93.2,
                last_lap_time_sec: 94.0,
                time_behind_leader_sec: 0.842,
                laps_behind_leader: 0,
                in_pits: state.in_pits,
                finish_status: "none".to_string(),
            },
        ];
        Ok(Standings {
            session_type: "Practice".to_string(),
            positions,
        })
    }

    async fn get_relatives(&self) -> Result<Relatives, AdapterError> {
        let entries = vec![
            RelativeEntry {
                id: 1,
                driver_name: "Bob Racer".to_string(),
                place: 1,
                laps_completed: 4,
                time_behind_next_sec: 0.0,
                laps_behind_next: 0,
                in_pits: false,
            },
            RelativeEntry {
                id: 0,
                driver_name: "Alice Driver".to_string(),
                place: 2,
                laps_completed: 4,
                time_behind_next_sec: 0.842,
                laps_behind_next: 0,
                in_pits: false,
            },
        ];
        let count = entries.len();
        Ok(Relatives { entries, count })
    }

    async fn get_weather(&self) -> Result<WeatherState, AdapterError> {
        let state = self.state.lock().expect("not poisoned").clone();
        Ok(WeatherState {
            ambient_temp_c: state.ambient_temp_c,
            track_temp_c: state.track_temp_c,
            raining: state.raining,
            cloudiness: state.cloudiness,
            wind_speed_ms: 2.0,
        })
    }

    async fn get_pit_info(&self) -> Result<PitInfoState, AdapterError> {
        let state = self.state.lock().expect("not poisoned").clone();
        Ok(PitInfoState {
            in_pits: state.in_pits,
            pit_state: state.pit_state,
            num_pitstops: state.num_pitstops,
            num_penalties: 0,
        })
    }

    async fn pit_menu_command(&self, control: HwControlCommand) -> Result<(), AdapterError> {
        let mut state = self.state.lock().expect("not poisoned");
        match control.control_name.as_str() {
            "request_pit" if control.value != 0.0 => {
                state.pit_state = "requested".to_string();
            }
            "cancel_pit" => {
                state.pit_state = "none".to_string();
            }
            "confirm_pit" if control.value != 0.0 => {
                state.in_pits = true;
                state.pit_state = "stopped".to_string();
                state.num_pitstops += 1;
            }
            _ => {
                // Unknown control names are accepted but have no modeled
                // effect on Stub state — see AdapterError::NotSupported's
                // doc comment: the exact rF2HWControl surface isn't pinned
                // down yet (requires the manual live-verification step).
            }
        }
        Ok(())
    }

    async fn set_weather(&self, weather: WeatherControl) -> Result<(), AdapterError> {
        if !(0.0..=1.0).contains(&weather.raining) {
            return Err(AdapterError::InvalidArgument(
                "raining must be in 0.0..=1.0".to_string(),
            ));
        }
        let mut state = self.state.lock().expect("not poisoned");
        state.raining = weather.raining;
        if let Some(cloudiness) = weather.cloudiness {
            state.cloudiness = cloudiness;
        }
        if let Some(ambient_temp_c) = weather.ambient_temp_c {
            state.ambient_temp_c = ambient_temp_c;
        }
        Ok(())
    }

    async fn camera_focus(
        &self,
        car_idx: i32,
        camera_type: Option<i32>,
        track_side_group: Option<i32>,
    ) -> Result<(), AdapterError> {
        let mut state = self.state.lock().expect("not poisoned");
        state.focus = car_idx;
        if let Some(camera_type) = camera_type {
            let _ = track_side_group; // unused in this fixture's simple mapping
            let (name, group) = match camera_type {
                0 | 1 => ("COCKPIT", "Driving"),
                2 => ("NOSECAM", "Driving"),
                3 => ("SWINGMAN", "Driving"),
                _ => ("TRACKING000", "Trackside"),
            };
            state.camera_name = name.to_string();
            state.camera_group = group.to_string();
        }
        Ok(())
    }

    async fn get_camera_state(&self) -> Result<CameraFocusState, AdapterError> {
        let state = self.state.lock().expect("not poisoned").clone();
        Ok(CameraFocusState {
            focus_slot_id: state.focus,
            camera_name: state.camera_name,
            camera_group: state.camera_group,
        })
    }

    async fn replay_seek_session_time(&self, _session_time_ms: i32) -> Result<(), AdapterError> {
        Err(AdapterError::NotSupported("replay_seek_session_time"))
    }
}
