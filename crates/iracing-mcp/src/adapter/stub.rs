// SPDX-License-Identifier: GPL-3.0-or-later
//! `StubAdapter`, ported from `margic/iracing-mcp`
//! (`crates/iracing-mcp-server/src/adapter/stub.rs`, ADR 0001 D5).
//!
//! Test-only: exercised by `crates/iracing-mcp`'s own test suite (unit tests
//! and `tests/http_transport.rs`), never constructed by
//! `crates/launcher/src/runner.rs`.

use std::{
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use mcp_core::{Read, SnapshotMeta};

use super::{
    AdapterError, CameraEntry, CameraGroup, CameraGroupList, DriverMatch, IracingAdapter,
    RelativeEntry, Relatives, ReplaySearchMode, ReplaySeekFrameMode, ReplayState,
    ResolveDriverResult, Roster, RosterEntry, SessionData, SessionOverview, SessionPosition,
    Standings, WeekendInfo,
};

#[derive(Debug)]
pub struct StubAdapter {
    replay_state: Mutex<ReplayState>,
}

fn read_with_meta<T>(data: T, state: &ReplayState) -> Read<T> {
    Read {
        data,
        meta: SnapshotMeta {
            session_tick: Some(state.replay_frame_num),
            session_time: Some(state.replay_session_time),
            captured_at_unix_ms: Some(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64,
            ),
            age_ms: Some(0),
            stale: Some(false),
            session_key: Some("stub:300:0".to_string()),
            session_revision: Some(0),
            server_elapsed_ms: 0,
        },
    }
}

impl Default for StubAdapter {
    fn default() -> Self {
        Self {
            replay_state: Mutex::new(ReplayState {
                connected: true,
                is_on_track: false,
                is_in_garage: false,
                is_replay_playing: true,
                replay_play_speed: 1,
                replay_play_slow_motion: false,
                replay_frame_num: 12_345,
                replay_frame_num_end: 67_890,
                replay_session_num: 0,
                replay_session_time: 205.25,
                cam_car_idx: 7,
                cam_group_number: 1,
                cam_camera_number: 0,
                cam_camera_state: 0,
            }),
        }
    }
}

#[async_trait]
impl IracingAdapter for StubAdapter {
    async fn get_session_overview(&self) -> Read<SessionOverview> {
        let rs = self.replay_state.lock().expect("not poisoned").clone();
        read_with_meta(
            SessionOverview {
                connected: rs.connected,
                is_replay: rs.is_replay(),
                is_in_car: rs.is_on_track || rs.is_in_garage,
                session_name: "Practice".to_string(),
                track_name: "Stub Track".to_string(),
            },
            &rs,
        )
    }

    async fn get_session_data(&self) -> Result<Read<SessionData>, AdapterError> {
        let rs = self.replay_state.lock().expect("not poisoned").clone();
        Ok(read_with_meta(
            SessionData {
                track_display_name: "Stub Track".to_string(),
                current_session_type: "Practice".to_string(),
                driver_count: 2,
                session_count: 1,
            },
            &rs,
        ))
    }

    async fn get_replay_state(&self) -> Result<Read<ReplayState>, AdapterError> {
        let state = self.replay_state.lock().expect("not poisoned").clone();
        Ok(read_with_meta(state.clone(), &state))
    }

    async fn set_replay_playback(&self, speed: i32, slow_motion: bool) -> Result<(), AdapterError> {
        if !(0..=255).contains(&speed) {
            return Err(AdapterError::UnsupportedReplaySpeed(speed));
        }
        let mut s = self.replay_state.lock().expect("not poisoned");
        s.replay_play_speed = speed;
        s.replay_play_slow_motion = slow_motion;
        s.is_replay_playing = speed != 0;
        Ok(())
    }

    async fn replay_seek_session_time(
        &self,
        session_num: i32,
        session_time_ms: i32,
    ) -> Result<(), AdapterError> {
        if session_num < 0 {
            return Err(AdapterError::InvalidArgument(
                "session_num must be non-negative".to_string(),
            ));
        }
        let mut s = self.replay_state.lock().expect("not poisoned");
        s.replay_session_num = session_num;
        s.replay_session_time = session_time_ms as f64 / 1000.0;
        s.replay_frame_num = (s.replay_session_time * 60.0) as i32;
        Ok(())
    }

    async fn replay_seek_frame(
        &self,
        mode: ReplaySeekFrameMode,
        frame: i32,
    ) -> Result<(), AdapterError> {
        let mut s = self.replay_state.lock().expect("not poisoned");
        let target = match mode {
            ReplaySeekFrameMode::Begin => frame,
            ReplaySeekFrameMode::Current => s.replay_frame_num.saturating_add(frame),
            ReplaySeekFrameMode::End => s.replay_frame_num_end.saturating_sub(frame),
        };
        s.replay_frame_num = target.clamp(0, s.replay_frame_num_end);
        s.replay_session_time = s.replay_frame_num as f64 / 60.0;
        Ok(())
    }

    async fn replay_search_event(&self, mode: ReplaySearchMode) -> Result<(), AdapterError> {
        let mut s = self.replay_state.lock().expect("not poisoned");
        let mut frame = s.replay_frame_num;
        frame = match mode {
            ReplaySearchMode::ToStart => 0,
            ReplaySearchMode::ToEnd => s.replay_frame_num_end,
            ReplaySearchMode::PrevSession => frame.saturating_sub(3600),
            ReplaySearchMode::NextSession => frame.saturating_add(3600),
            ReplaySearchMode::PrevLap => frame.saturating_sub(600),
            ReplaySearchMode::NextLap => frame.saturating_add(600),
            ReplaySearchMode::PrevFrame => frame.saturating_sub(1),
            ReplaySearchMode::NextFrame => frame.saturating_add(1),
            ReplaySearchMode::PrevIncident => frame.saturating_sub(300),
            ReplaySearchMode::NextIncident => frame.saturating_add(300),
        };
        s.replay_frame_num = frame.clamp(0, s.replay_frame_num_end);
        s.replay_session_time = s.replay_frame_num as f64 / 60.0;
        Ok(())
    }

    async fn camera_set_state(&self, state_bits: i32) -> Result<(), AdapterError> {
        if !(0..=0xFFFF).contains(&state_bits) {
            return Err(AdapterError::InvalidArgument(
                "state_bits must be in 0..=65535".to_string(),
            ));
        }
        let mut s = self.replay_state.lock().expect("not poisoned");
        s.cam_camera_state = state_bits;
        Ok(())
    }

    async fn camera_focus(
        &self,
        car_idx: i32,
        group_number: Option<i32>,
        camera_number: Option<i32>,
    ) -> Result<(), AdapterError> {
        if car_idx < 0 {
            return Err(AdapterError::InvalidArgument(
                "car_idx must be non-negative".to_string(),
            ));
        }
        let mut s = self.replay_state.lock().expect("not poisoned");
        s.cam_car_idx = car_idx;
        if let Some(g) = group_number {
            s.cam_group_number = g;
        }
        if let Some(c) = camera_number {
            s.cam_camera_number = c;
        }
        Ok(())
    }

    async fn get_weekend_info(&self) -> Result<Read<WeekendInfo>, AdapterError> {
        let rs = self.replay_state.lock().expect("not poisoned").clone();
        Ok(read_with_meta(
            WeekendInfo {
                track_name: "stub_track".to_string(),
                track_id: 1,
                track_display_name: "Stub Track".to_string(),
                track_config_name: "Full".to_string(),
                track_length_km: 5.0,
                track_city: "Stubville".to_string(),
                track_country: "Stubland".to_string(),
                track_num_turns: 12,
                track_pit_speed_limit_kph: 60.0,
                track_type: "Road Course".to_string(),
                series_id: 100,
                season_id: 200,
                session_id: 300,
                sub_session_id: 400,
                official: false,
                event_type: "Practice".to_string(),
                category: "Road".to_string(),
                sim_mode: "Full".to_string(),
                team_racing: false,
                weather_type: "Constant".to_string(),
                skies: "Clear".to_string(),
                surface_temp_c: 35.0,
                air_temp_c: 25.0,
                wind_vel_ms: 2.0,
            },
            &rs,
        ))
    }

    async fn get_roster(
        &self,
        _include_spectators: bool,
        _include_pace_car: bool,
    ) -> Result<Read<Roster>, AdapterError> {
        let entries = vec![
            RosterEntry {
                car_idx: 0,
                user_name: "Alice Driver".to_string(),
                abbrev_name: "Driver, A".to_string(),
                initials: "AD".to_string(),
                user_id: 1001,
                team_name: String::new(),
                car_number: "4".to_string(),
                car_number_raw: 4,
                car_id: 67,
                car_screen_name: "Stub GTE".to_string(),
                car_class_id: 1,
                car_class_short_name: "GTE".to_string(),
                irating: 3500,
                lic_string: "A 4.50".to_string(),
                is_spectator: false,
            },
            RosterEntry {
                car_idx: 7,
                user_name: "Bob Racer".to_string(),
                abbrev_name: "Racer, B".to_string(),
                initials: "BR".to_string(),
                user_id: 1002,
                team_name: String::new(),
                car_number: "7".to_string(),
                car_number_raw: 7,
                car_id: 67,
                car_screen_name: "Stub GTE".to_string(),
                car_class_id: 1,
                car_class_short_name: "GTE".to_string(),
                irating: 4200,
                lic_string: "A 4.99".to_string(),
                is_spectator: false,
            },
        ];
        let count = entries.len();
        let rs = self.replay_state.lock().expect("not poisoned").clone();
        Ok(read_with_meta(Roster { entries, count }, &rs))
    }

    async fn get_camera_groups(&self) -> Result<Read<CameraGroupList>, AdapterError> {
        let groups = vec![
            CameraGroup {
                group_num: 1,
                group_name: "TV1".to_string(),
                is_scenic: false,
                cameras: vec![
                    CameraEntry {
                        camera_num: 0,
                        camera_name: "Chase".to_string(),
                    },
                    CameraEntry {
                        camera_num: 1,
                        camera_name: "Cockpit".to_string(),
                    },
                ],
            },
            CameraGroup {
                group_num: 2,
                group_name: "Scenic".to_string(),
                is_scenic: true,
                cameras: vec![CameraEntry {
                    camera_num: 0,
                    camera_name: "Blimp".to_string(),
                }],
            },
        ];
        let count = groups.len();
        let rs = self.replay_state.lock().expect("not poisoned").clone();
        Ok(read_with_meta(CameraGroupList { groups, count }, &rs))
    }

    async fn get_standings(
        &self,
        _session_num: Option<i32>,
    ) -> Result<Read<Standings>, AdapterError> {
        let positions = vec![
            SessionPosition {
                position: 1,
                class_position: 1,
                car_idx: 7,
                lap: 5,
                laps_complete: 4,
                fastest_lap: 3,
                fastest_time: 92.5,
                last_time: 93.1,
                incidents: 0,
                reason_out: "Running".to_string(),
            },
            SessionPosition {
                position: 2,
                class_position: 2,
                car_idx: 0,
                lap: 5,
                laps_complete: 4,
                fastest_lap: 2,
                fastest_time: 93.2,
                last_time: 94.0,
                incidents: 2,
                reason_out: "Running".to_string(),
            },
        ];
        let rs = self.replay_state.lock().expect("not poisoned").clone();
        Ok(read_with_meta(
            Standings {
                session_num: 0,
                session_type: "Practice".to_string(),
                positions,
            },
            &rs,
        ))
    }

    async fn get_relatives(&self) -> Result<Read<Relatives>, AdapterError> {
        let entries = vec![
            RelativeEntry {
                position: 1,
                class_position: 1,
                car_idx: 7,
                car_number: "7".to_string(),
                display_name: "Bob Racer".to_string(),
                lap: 5,
                lap_dist_pct: Some(0.82),
                is_in_pit: false,
                gap_ahead_sec: None,
                gap_behind_sec: Some(0.842),
                delta_laps: 0,
                estimated_time_sec: Some(93.1),
                f2_time_sec: Some(0.0),
                track_surface: Some("OnTrack".to_string()),
                in_world: Some(true),
            },
            RelativeEntry {
                position: 2,
                class_position: 2,
                car_idx: 0,
                car_number: "4".to_string(),
                display_name: "Alice Driver".to_string(),
                lap: 5,
                lap_dist_pct: Some(0.77),
                is_in_pit: false,
                gap_ahead_sec: Some(0.842),
                gap_behind_sec: None,
                delta_laps: 0,
                estimated_time_sec: Some(94.0),
                f2_time_sec: Some(0.842),
                track_surface: Some("InPitStall".to_string()),
                in_world: Some(true),
            },
        ];

        let rs = self.replay_state.lock().expect("not poisoned").clone();
        Ok(read_with_meta(
            Relatives {
                basis: "track".to_string(),
                session_num: 0,
                entries: entries.clone(),
                count: entries.len(),
            },
            &rs,
        ))
    }

    async fn resolve_driver(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<Read<ResolveDriverResult>, AdapterError> {
        let roster = self.get_roster(false, false).await?.data;
        let q = query.to_lowercase();
        let mut scored: Vec<DriverMatch> = roster
            .entries
            .iter()
            .filter_map(|e| {
                let name_lower = e.user_name.to_lowercase();
                let (confidence, reason) = if name_lower == q {
                    (1.0, "exact")
                } else if name_lower.starts_with(&q) {
                    (0.9, "name_prefix")
                } else if name_lower.contains(&q) {
                    (0.6, "substring")
                } else if e.car_number == query {
                    (0.85, "car_number")
                } else {
                    return None;
                };
                Some(DriverMatch {
                    car_idx: e.car_idx,
                    display_name: e.user_name.clone(),
                    car_number: e.car_number.clone(),
                    confidence,
                    match_reason: reason.to_string(),
                })
            })
            .collect();
        scored.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        scored.truncate(limit);
        let best_match = scored.first().cloned();
        let rs = self.replay_state.lock().expect("not poisoned").clone();
        Ok(read_with_meta(
            ResolveDriverResult {
                best_match,
                candidates: scored,
            },
            &rs,
        ))
    }

    async fn snapshot_meta(&self) -> SnapshotMeta {
        let state = self.replay_state.lock().expect("not poisoned").clone();
        read_with_meta((), &state).meta
    }
}
