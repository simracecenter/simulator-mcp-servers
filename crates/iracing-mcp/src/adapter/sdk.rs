// SPDX-License-Identifier: GPL-3.0-or-later
//! `SdkAdapter`, ported from `margic/iracing-mcp`
//! (`crates/iracing-mcp-server/src/adapter/sdk_live.rs`, ADR 0001 D5).
//!
//! Upstream's own `adapter/sdk.rs` is dropped entirely; `sdk_live.rs` is
//! canonical and becomes this file.

use async_trait::async_trait;

#[cfg(windows)]
use iracing::telemetry::Value;
#[cfg(windows)]
use serde_yaml::Value as YamlValue;
#[cfg(windows)]
use std::sync::{Mutex, OnceLock};
#[cfg(windows)]
use tracing::debug;

#[cfg(windows)]
use iracing_broadcast::{BroadcastMessage, Client as BroadcastClient};

#[cfg(windows)]
use std::{ffi::OsStr, os::windows::ffi::OsStrExt, ptr::null_mut, slice};

#[cfg(windows)]
use winapi::{
    shared::minwindef::FALSE,
    um::{
        errhandlingapi::GetLastError,
        handleapi::CloseHandle,
        memoryapi::{MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ},
        winuser::{RegisterWindowMessageW, SendNotifyMessageW, HWND_BROADCAST},
    },
};

use super::{
    AdapterError, CameraGroupList, IracingAdapter, Relatives, ReplaySearchMode,
    ReplaySeekFrameMode, ReplayState, ResolveDriverResult, Roster, SessionData, SessionOverview,
    Standings, WeekendInfo,
};

#[cfg(windows)]
use super::{CameraEntry, CameraGroup, DriverMatch, RelativeEntry, RosterEntry, SessionPosition};

#[cfg(windows)]
const IRSDK_MEMMAPFILENAME: &str = "Local\\IRSDKMemMapFileName";
#[cfg(windows)]
const IRSDK_BROADCASTMSGNAME: &str = "IRSDK_BROADCASTMSG";
#[cfg(windows)]
const BROADCAST_CAM_SWITCH_NUM: i32 = 1;
#[cfg(windows)]
const BROADCAST_CAM_SET_STATE: i32 = 2;
#[cfg(windows)]
const BROADCAST_REPLAY_SET_PLAY_POSITION: i32 = 4;
#[cfg(windows)]
const BROADCAST_REPLAY_SEARCH: i32 = 5;
#[cfg(windows)]
const BROADCAST_REPLAY_SEARCH_SESSION_TIME: i32 = 12;

#[cfg(windows)]
#[repr(C)]
struct IrsdkHeaderPrefix {
    ver: i32,
    status: i32,
    tick_rate: i32,
    session_info_update: i32,
    session_info_len: i32,
    session_info_offset: i32,
}

#[cfg(windows)]
#[derive(Clone)]
struct SessionYamlCache {
    session_info_update: i32,
    yaml: String,
}

#[cfg(windows)]
static SESSION_YAML_CACHE: OnceLock<Mutex<Option<SessionYamlCache>>> = OnceLock::new();

#[derive(Debug, Default)]
pub struct SdkAdapter;

/// The iRacing SDK's shared-memory telemetry map and broadcast-message API
/// are only available on Windows (the `iracing`/`iracing-broadcast` crates
/// gate their entire public surface behind `target_os = "windows"`). On any
/// other target, every method reports the adapter as disconnected/unavailable
/// so the crate still compiles — and its stub-backed tests still run — on
/// Linux.
#[cfg(not(windows))]
#[async_trait]
impl IracingAdapter for SdkAdapter {
    async fn get_session_overview(&self) -> SessionOverview {
        SessionOverview {
            connected: false,
            is_replay: false,
            is_in_car: false,
            session_name: "Disconnected".to_string(),
            track_name: "Disconnected".to_string(),
        }
    }

    async fn get_session_data(&self) -> Result<SessionData, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_replay_state(&self) -> Result<ReplayState, AdapterError> {
        Err(Self::not_available())
    }

    async fn set_replay_playback(
        &self,
        _speed: i32,
        _slow_motion: bool,
    ) -> Result<(), AdapterError> {
        Err(Self::not_available())
    }

    async fn replay_seek_session_time(
        &self,
        _session_num: i32,
        _session_time_ms: i32,
    ) -> Result<(), AdapterError> {
        Err(Self::not_available())
    }

    async fn replay_seek_frame(
        &self,
        _mode: ReplaySeekFrameMode,
        _frame: i32,
    ) -> Result<(), AdapterError> {
        Err(Self::not_available())
    }

    async fn replay_search_event(&self, _mode: ReplaySearchMode) -> Result<(), AdapterError> {
        Err(Self::not_available())
    }

    async fn camera_set_state(&self, _state_bits: i32) -> Result<(), AdapterError> {
        Err(Self::not_available())
    }

    async fn camera_focus(
        &self,
        _car_idx: i32,
        _group_number: Option<i32>,
        _camera_number: Option<i32>,
    ) -> Result<(), AdapterError> {
        Err(Self::not_available())
    }

    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_roster(
        &self,
        _include_spectators: bool,
        _include_pace_car: bool,
    ) -> Result<Roster, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_camera_groups(&self) -> Result<CameraGroupList, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_standings(&self, _session_num: Option<i32>) -> Result<Standings, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_relatives(&self) -> Result<Relatives, AdapterError> {
        Err(Self::not_available())
    }

    async fn resolve_driver(
        &self,
        _query: &str,
        _limit: usize,
    ) -> Result<ResolveDriverResult, AdapterError> {
        Err(Self::not_available())
    }
}

#[cfg(not(windows))]
impl SdkAdapter {
    fn not_available() -> AdapterError {
        AdapterError::NotConnected("the iRacing SDK is only available on Windows".to_string())
    }
}

#[cfg(windows)]
impl SdkAdapter {
    fn session_data_sync(&self) -> Result<SessionData, AdapterError> {
        let connection = iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;
        let telemetry = connection
            .telemetry()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;
        let current_session_num = read_i32(&telemetry, "SessionNum")?;
        let session_yaml = read_session_yaml()?;

        parse_session_data(&session_yaml, current_session_num)
    }

    fn replay_state_sync(&self) -> Result<ReplayState, AdapterError> {
        let connection = iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;
        let sample = connection
            .telemetry()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;

        let state = ReplayState {
            connected: true,
            is_on_track: read_bool(&sample, "IsOnTrack")?,
            is_in_garage: read_bool(&sample, "IsInGarage")?,
            is_replay_playing: read_bool(&sample, "IsReplayPlaying")?,
            replay_play_speed: read_i32(&sample, "ReplayPlaySpeed")?,
            replay_play_slow_motion: read_bool(&sample, "ReplayPlaySlowMotion")?,
            replay_frame_num: read_i32(&sample, "ReplayFrameNum")?,
            replay_frame_num_end: read_i32(&sample, "ReplayFrameNumEnd")?,
            replay_session_num: read_i32(&sample, "ReplaySessionNum")?,
            replay_session_time: read_f64(&sample, "ReplaySessionTime")?,
            cam_car_idx: read_i32(&sample, "CamCarIdx")?,
            cam_group_number: read_i32(&sample, "CamGroupNumber")?,
            cam_camera_number: read_i32(&sample, "CamCameraNumber")?,
            cam_camera_state: read_i32(&sample, "CamCameraState")?,
        };
        debug!(
            "replay_state_sync: speed={} playing={} slow={} frame={} session_num={} session_time={:.3} cam_car={} cam_group={} cam_camera={} on_track={} in_garage={}",
            state.replay_play_speed, state.is_replay_playing, state.replay_play_slow_motion,
            state.replay_frame_num, state.replay_session_num, state.replay_session_time,
            state.cam_car_idx, state.cam_group_number, state.cam_camera_number,
            state.is_on_track, state.is_in_garage
        );
        Ok(state)
    }

    fn set_replay_playback_sync(&self, speed: i32, slow_motion: bool) -> Result<(), AdapterError> {
        if !(0..=255).contains(&speed) {
            return Err(AdapterError::UnsupportedReplaySpeed(speed));
        }

        iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;

        debug!(
            "set_replay_playback: sending ReplaySetPlaySpeed speed={} slow_motion={}",
            speed, slow_motion
        );
        let result = send_replay_set_play_speed(speed, slow_motion);
        debug!("set_replay_playback: broadcast result={:?}", result);
        result
    }

    fn replay_seek_session_time_sync(
        &self,
        session_num: i32,
        session_time_ms: i32,
    ) -> Result<(), AdapterError> {
        if session_num < 0 {
            return Err(AdapterError::InvalidArgument(
                "session_num must be non-negative".to_string(),
            ));
        }

        if session_time_ms < 0 {
            return Err(AdapterError::InvalidArgument(
                "session_time_ms must be non-negative".to_string(),
            ));
        }

        iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;

        let session_time = session_time_ms as u32;
        let time_lo = (session_time & 0xFFFF) as i32;
        let time_hi = ((session_time >> 16) & 0xFFFF) as i32;
        debug!(
            "replay_seek_session_time: sending ReplaySearchSessionTime session_num={} session_time_ms={} time_lo=0x{:04X} time_hi=0x{:04X}",
            session_num, session_time_ms, time_lo, time_hi
        );
        let result = send_broadcast_message_3(
            BROADCAST_REPLAY_SEARCH_SESSION_TIME,
            session_num,
            time_lo,
            time_hi,
        );
        debug!("replay_seek_session_time: broadcast result={:?}", result);
        result
    }

    fn replay_seek_frame_sync(
        &self,
        mode: ReplaySeekFrameMode,
        frame: i32,
    ) -> Result<(), AdapterError> {
        iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;

        let mode_code = match mode {
            ReplaySeekFrameMode::Begin => 0,
            ReplaySeekFrameMode::Current => 1,
            ReplaySeekFrameMode::End => 2,
        };
        debug!(
            "replay_seek_frame: sending ReplaySetPlayPosition mode={:?} frame={}",
            mode, frame
        );
        let result = send_broadcast_message_2(BROADCAST_REPLAY_SET_PLAY_POSITION, mode_code, frame);
        debug!("replay_seek_frame: broadcast result={:?}", result);
        result
    }

    fn replay_search_event_sync(&self, mode: ReplaySearchMode) -> Result<(), AdapterError> {
        iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;

        let mode_code = match mode {
            ReplaySearchMode::ToStart => 0,
            ReplaySearchMode::ToEnd => 1,
            ReplaySearchMode::PrevSession => 2,
            ReplaySearchMode::NextSession => 3,
            ReplaySearchMode::PrevLap => 4,
            ReplaySearchMode::NextLap => 5,
            ReplaySearchMode::PrevFrame => 6,
            ReplaySearchMode::NextFrame => 7,
            ReplaySearchMode::PrevIncident => 8,
            ReplaySearchMode::NextIncident => 9,
        };
        debug!(
            "replay_search_event: sending ReplaySearch mode={:?} code={}",
            mode, mode_code
        );
        let result = send_broadcast_message_2(BROADCAST_REPLAY_SEARCH, mode_code, 0);
        debug!("replay_search_event: broadcast result={:?}", result);
        result
    }

    fn camera_set_state_sync(&self, state_bits: i32) -> Result<(), AdapterError> {
        if !(0..=0xFFFF).contains(&state_bits) {
            return Err(AdapterError::InvalidArgument(
                "state_bits must be in 0..=65535".to_string(),
            ));
        }

        iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;

        debug!(
            "camera_set_state: sending CamSetState state_bits={}",
            state_bits
        );
        let result = send_broadcast_message_2(BROADCAST_CAM_SET_STATE, state_bits, 0);
        debug!("camera_set_state: broadcast result={:?}", result);
        result
    }

    fn camera_focus_sync(
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

        let replay_state = self.replay_state_sync()?;
        let group_number = group_number.unwrap_or(replay_state.cam_group_number);
        let camera_number = camera_number.unwrap_or(replay_state.cam_camera_number);
        let session_yaml = read_session_yaml()?;
        let car_number = find_car_number_for_car_idx(&session_yaml, car_idx)?;
        let padded_car_number = i32::from(pad_car_number(&car_number));

        debug!(
            "camera_focus: sending CamSwitchNum car_idx={} car_number={:?} padded={} group={} camera={}",
            car_idx, car_number, padded_car_number, group_number, camera_number
        );
        let result = send_broadcast_message_3(
            BROADCAST_CAM_SWITCH_NUM,
            padded_car_number,
            normalize_u16(group_number, "group_number")?,
            normalize_u16(camera_number, "camera_number")?,
        );
        debug!("camera_focus: broadcast result={:?}", result);
        result
    }
}

#[cfg(windows)]
#[async_trait]
impl IracingAdapter for SdkAdapter {
    async fn get_session_overview(&self) -> SessionOverview {
        let session_data = self.session_data_sync().ok();
        let replay_state = self.replay_state_sync().ok();

        SessionOverview {
            connected: replay_state.is_some(),
            is_replay: replay_state
                .as_ref()
                .map(|state| {
                    state.is_replay_playing
                        || state.replay_frame_num > 0
                        || state.replay_session_time > 0.0
                })
                .unwrap_or(false),
            is_in_car: replay_state
                .as_ref()
                .map(|state| state.is_on_track || state.is_in_garage)
                .unwrap_or(false),
            session_name: session_data
                .as_ref()
                .map(|session| session.current_session_type.clone())
                .unwrap_or_else(|| "Disconnected".to_string()),
            track_name: session_data
                .as_ref()
                .map(|session| session.track_display_name.clone())
                .unwrap_or_else(|| "Disconnected".to_string()),
        }
    }

    async fn get_session_data(&self) -> Result<SessionData, AdapterError> {
        self.session_data_sync()
    }

    async fn get_replay_state(&self) -> Result<ReplayState, AdapterError> {
        self.replay_state_sync()
    }

    async fn set_replay_playback(&self, speed: i32, slow_motion: bool) -> Result<(), AdapterError> {
        self.set_replay_playback_sync(speed, slow_motion)
    }

    async fn replay_seek_session_time(
        &self,
        session_num: i32,
        session_time_ms: i32,
    ) -> Result<(), AdapterError> {
        self.replay_seek_session_time_sync(session_num, session_time_ms)
    }

    async fn replay_seek_frame(
        &self,
        mode: ReplaySeekFrameMode,
        frame: i32,
    ) -> Result<(), AdapterError> {
        self.replay_seek_frame_sync(mode, frame)
    }

    async fn replay_search_event(&self, mode: ReplaySearchMode) -> Result<(), AdapterError> {
        self.replay_search_event_sync(mode)
    }

    async fn camera_set_state(&self, state_bits: i32) -> Result<(), AdapterError> {
        self.camera_set_state_sync(state_bits)
    }

    async fn camera_focus(
        &self,
        car_idx: i32,
        group_number: Option<i32>,
        camera_number: Option<i32>,
    ) -> Result<(), AdapterError> {
        self.camera_focus_sync(car_idx, group_number, camera_number)
    }

    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError> {
        let yaml = read_session_yaml()?;
        let root = parse_session_root(&yaml)?;
        let wi = |k: &str| -> String {
            yaml_str_at(&root, &["WeekendInfo", k])
                .unwrap_or_default()
                .to_string()
        };
        let wi_i = |k: &str| -> i32 {
            root.get("WeekendInfo")
                .and_then(|v| v.get(k))
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32
        };
        let wi_f = |k: &str| -> f64 {
            root.get("WeekendInfo")
                .and_then(|v| v.get(k))
                .and_then(|v| {
                    v.as_f64().or_else(|| {
                        v.as_str()
                            .and_then(|s| s.trim_end_matches(" km").parse().ok())
                    })
                })
                .unwrap_or(0.0)
        };
        let wi_b = |k: &str| -> bool {
            root.get("WeekendInfo")
                .and_then(|v| v.get(k))
                .and_then(|v| v.as_i64())
                .map(|n| n != 0)
                .unwrap_or(false)
        };
        let weather = |k: &str| -> String {
            yaml_str_at(&root, &["WeekendInfo", "WeatherParams", k])
                .or_else(|_| yaml_str_at(&root, &["WeekendInfo", k]))
                .unwrap_or_default()
                .to_string()
        };
        let weather_f = |k: &str| -> f64 {
            root.get("WeekendInfo")
                .and_then(|v| v.get("WeatherParams").or(Some(v)))
                .and_then(|v| v.get(k))
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
        };
        Ok(WeekendInfo {
            track_name: wi("TrackName"),
            track_id: wi_i("TrackID"),
            track_display_name: wi("TrackDisplayName"),
            track_config_name: wi("TrackConfigName"),
            track_length_km: wi_f("TrackLength"),
            track_city: wi("TrackCity"),
            track_country: wi("TrackCountry"),
            track_num_turns: wi_i("TrackNumTurns"),
            track_pit_speed_limit_kph: wi_f("TrackPitSpeedLimit"),
            track_type: wi("TrackType"),
            series_id: wi_i("SeriesID"),
            season_id: wi_i("SeasonID"),
            session_id: wi_i("SessionID"),
            sub_session_id: wi_i("SubSessionID"),
            official: wi_b("Official"),
            event_type: wi("EventType"),
            category: wi("Category"),
            sim_mode: wi("SimMode"),
            team_racing: wi_b("TeamRacing"),
            weather_type: weather("WeatherType"),
            skies: weather("Skies"),
            surface_temp_c: weather_f("TempTrack"),
            air_temp_c: weather_f("TempAir"),
            wind_vel_ms: weather_f("WindVel"),
        })
    }

    async fn get_roster(
        &self,
        include_spectators: bool,
        include_pace_car: bool,
    ) -> Result<Roster, AdapterError> {
        let yaml = read_session_yaml()?;
        let root = parse_session_root(&yaml)?;
        let drivers = yaml_seq_at(&root, &["DriverInfo", "Drivers"])?;

        let str_field = |d: &YamlValue, k: &str| -> String {
            d.get(k)
                .and_then(|v| {
                    v.as_str()
                        .map(String::from)
                        .or_else(|| v.as_i64().map(|n| n.to_string()))
                })
                .unwrap_or_default()
        };
        let i_field = |d: &YamlValue, k: &str| -> i32 {
            d.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as i32
        };
        let i64_field =
            |d: &YamlValue, k: &str| -> i64 { d.get(k).and_then(|v| v.as_i64()).unwrap_or(0) };
        let b_field = |d: &YamlValue, k: &str| -> bool {
            d.get(k)
                .and_then(|v| v.as_i64())
                .map(|n| n != 0)
                .unwrap_or(false)
        };

        let mut entries: Vec<RosterEntry> = drivers
            .iter()
            .filter_map(|d| {
                let car_idx = i_field(d, "CarIdx");
                let is_spectator = b_field(d, "IsSpectator");
                let is_pace_car = i_field(d, "CarIsPaceCar") != 0
                    || str_field(d, "CarNumber") == "0"
                        && str_field(d, "UserName").to_lowercase().contains("pace");
                if !include_spectators && is_spectator {
                    return None;
                }
                if !include_pace_car && is_pace_car {
                    return None;
                }
                Some(RosterEntry {
                    car_idx,
                    user_name: str_field(d, "UserName"),
                    abbrev_name: str_field(d, "AbbrevName"),
                    initials: str_field(d, "Initials"),
                    user_id: i64_field(d, "UserID"),
                    team_name: str_field(d, "TeamName"),
                    car_number: str_field(d, "CarNumber"),
                    car_number_raw: i_field(d, "CarNumberRaw"),
                    car_id: i_field(d, "CarID"),
                    car_screen_name: str_field(d, "CarScreenName"),
                    car_class_id: i_field(d, "CarClassID"),
                    car_class_short_name: str_field(d, "CarClassShortName"),
                    irating: i_field(d, "IRating"),
                    lic_string: str_field(d, "LicString"),
                    is_spectator,
                })
            })
            .collect();

        entries.sort_by_key(|e| e.car_idx);
        let count = entries.len();
        Ok(Roster { entries, count })
    }

    async fn get_camera_groups(&self) -> Result<CameraGroupList, AdapterError> {
        let yaml = read_session_yaml()?;
        let root = parse_session_root(&yaml)?;
        let groups_yaml = yaml_seq_at(&root, &["CameraInfo", "Groups"])?;

        let mut groups: Vec<CameraGroup> = groups_yaml
            .iter()
            .filter_map(|g| {
                let group_num = g.get("GroupNum")?.as_i64()? as i32;
                let group_name = g.get("GroupName")?.as_str()?.to_string();
                let is_scenic = g.get("IsScenic").and_then(|v| v.as_bool()).unwrap_or(false);
                let cameras = g
                    .get("Cameras")
                    .and_then(|v| v.as_sequence())
                    .map(|cams| {
                        cams.iter()
                            .filter_map(|c| {
                                let camera_num = c.get("CameraNum")?.as_i64()? as i32;
                                let camera_name = c.get("CameraName")?.as_str()?.to_string();
                                Some(CameraEntry {
                                    camera_num,
                                    camera_name,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Some(CameraGroup {
                    group_num,
                    group_name,
                    is_scenic,
                    cameras,
                })
            })
            .collect();

        groups.sort_by_key(|g| g.group_num);
        let count = groups.len();
        Ok(CameraGroupList { groups, count })
    }

    async fn get_standings(&self, session_num: Option<i32>) -> Result<Standings, AdapterError> {
        let yaml = read_session_yaml()?;
        let root = parse_session_root(&yaml)?;
        let sessions = yaml_seq_at(&root, &["SessionInfo", "Sessions"])?;

        // Determine which session to use
        let target_session_num = match session_num {
            Some(n) => n,
            None => {
                // fall back to telemetry SessionNum
                let connection = iracing::Connection::new()
                    .map_err(|e| AdapterError::NotConnected(e.to_string()))?;
                let sample = connection
                    .telemetry()
                    .map_err(|e| AdapterError::NotConnected(e.to_string()))?;
                read_i32(&sample, "SessionNum").unwrap_or(0)
            }
        };

        let session = sessions
            .iter()
            .find(|s| {
                s.get("SessionNum")
                    .and_then(|v| v.as_i64())
                    .map(|n| n as i32 == target_session_num)
                    .unwrap_or(false)
            })
            .or_else(|| sessions.last())
            .ok_or_else(|| AdapterError::SessionInfo("no sessions in YAML".to_string()))?;

        let session_type = session
            .get("SessionType")
            .and_then(|v| v.as_str())
            .unwrap_or("Unknown")
            .to_string();

        let positions: Vec<SessionPosition> = session
            .get("ResultsPositions")
            .and_then(|v| v.as_sequence())
            .map(|pos| {
                pos.iter()
                    .filter_map(|p| {
                        let str_f = |k: &str| -> String {
                            p.get(k)
                                .and_then(|v| v.as_str().map(String::from))
                                .unwrap_or_default()
                        };
                        let i_f = |k: &str| -> i32 {
                            p.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as i32
                        };
                        let f_f =
                            |k: &str| -> f64 { p.get(k).and_then(|v| v.as_f64()).unwrap_or(-1.0) };
                        Some(SessionPosition {
                            position: i_f("Position"),
                            class_position: i_f("ClassPosition"),
                            car_idx: i_f("CarIdx"),
                            lap: i_f("Lap"),
                            laps_complete: i_f("LapsComplete"),
                            fastest_lap: i_f("FastestLap"),
                            fastest_time: f_f("FastestTime"),
                            last_time: f_f("LastTime"),
                            incidents: i_f("Incidents"),
                            reason_out: str_f("ReasonOut"),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        Ok(Standings {
            session_num: target_session_num,
            session_type,
            positions,
        })
    }

    async fn get_relatives(&self) -> Result<Relatives, AdapterError> {
        // Ensure session data is available (and any related connection issues
        // surface as an error) before reading telemetry arrays below.
        let _session_data = self.session_data_sync()?;
        let roster = self.get_roster(false, false).await?;
        let connection = iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;
        let sample = connection
            .telemetry()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;

        let session_num = read_i32(&sample, "SessionNum")?;
        let class_positions = read_i32_vec(&sample, "CarIdxClassPosition")?;
        let laps = read_i32_vec(&sample, "CarIdxLap")?;
        let lap_dist_pcts = read_f32_vec(&sample, "CarIdxLapDistPct")?;
        let on_pit_road = read_bool_vec(&sample, "CarIdxOnPitRoad")?;
        let est_times = read_f32_vec(&sample, "CarIdxEstTime")?;
        let f2_times = read_f32_vec(&sample, "CarIdxF2Time")?;

        #[derive(Clone)]
        struct RawRelative {
            class_position: i32,
            car_idx: i32,
            car_number: String,
            display_name: String,
            lap: i32,
            lap_dist_pct: Option<f64>,
            is_in_pit: bool,
            track_coord_sec: f64,
            estimated_time_sec: Option<f64>,
            f2_time_sec: Option<f64>,
        }

        let mut raw_entries: Vec<RawRelative> = roster
            .entries
            .iter()
            .map(|entry| {
                let car_idx = entry.car_idx.max(0) as usize;
                let class_position = class_positions.get(car_idx).copied().unwrap_or(0);
                let lap = laps.get(car_idx).copied().unwrap_or(0);
                let lap_dist_pct = lap_dist_pcts
                    .get(car_idx)
                    .copied()
                    .map(|value| value as f64);
                let is_in_pit = on_pit_road.get(car_idx).copied().unwrap_or(false);
                let estimated_time_sec = est_times.get(car_idx).copied().map(|value| value as f64);
                let f2_time_sec = f2_times.get(car_idx).copied().map(|value| value as f64);
                // For true on-track relatives, prefer the current track coordinate estimate.
                let track_coord_sec = estimated_time_sec
                    .filter(|value| value.is_finite() && *value >= 0.0)
                    .or(f2_time_sec.filter(|value| value.is_finite() && *value >= 0.0))
                    .unwrap_or(-1.0);

                RawRelative {
                    class_position,
                    car_idx: entry.car_idx,
                    car_number: entry.car_number.clone(),
                    display_name: entry.user_name.clone(),
                    lap,
                    lap_dist_pct,
                    is_in_pit,
                    track_coord_sec,
                    estimated_time_sec,
                    f2_time_sec,
                }
            })
            .collect();

        raw_entries.sort_by(|left, right| {
            right
                .track_coord_sec
                .total_cmp(&left.track_coord_sec)
                .then_with(|| right.lap.cmp(&left.lap))
                .then_with(|| {
                    right
                        .lap_dist_pct
                        .unwrap_or(f64::NEG_INFINITY)
                        .partial_cmp(&left.lap_dist_pct.unwrap_or(f64::NEG_INFINITY))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
                .then_with(|| left.is_in_pit.cmp(&right.is_in_pit))
                .then_with(|| left.class_position.cmp(&right.class_position))
                .then_with(|| left.car_idx.cmp(&right.car_idx))
        });

        let leader_lap = raw_entries.first().map(|entry| entry.lap).unwrap_or(0);
        let mut entries: Vec<RelativeEntry> = Vec::with_capacity(raw_entries.len());
        for (index, current) in raw_entries.iter().enumerate() {
            let previous = if index > 0 {
                raw_entries.get(index - 1)
            } else {
                None
            };
            let next = raw_entries.get(index + 1);

            let gap_ahead_sec = match previous {
                Some(previous_entry) => {
                    let delta = previous_entry.track_coord_sec - current.track_coord_sec;
                    if delta >= 0.0 {
                        Some(delta)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            let gap_behind_sec = match next {
                Some(next_entry) => {
                    let delta = current.track_coord_sec - next_entry.track_coord_sec;
                    if delta >= 0.0 {
                        Some(delta)
                    } else {
                        None
                    }
                }
                _ => None,
            };

            entries.push(RelativeEntry {
                position: (index + 1) as i32,
                class_position: if current.class_position > 0 {
                    current.class_position
                } else {
                    (index + 1) as i32
                },
                car_idx: current.car_idx,
                car_number: current.car_number.clone(),
                display_name: current.display_name.clone(),
                lap: current.lap,
                lap_dist_pct: current.lap_dist_pct,
                is_in_pit: current.is_in_pit,
                gap_ahead_sec,
                gap_behind_sec,
                delta_laps: leader_lap.saturating_sub(current.lap),
                estimated_time_sec: current.estimated_time_sec,
                f2_time_sec: current.f2_time_sec,
            });
        }

        Ok(Relatives {
            basis: "track".to_string(),
            session_num,
            entries,
            count: raw_entries.len(),
        })
    }

    async fn resolve_driver(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<ResolveDriverResult, AdapterError> {
        let roster = self.get_roster(false, false).await?;
        let q = query.to_lowercase();
        let mut scored: Vec<DriverMatch> = roster
            .entries
            .iter()
            .filter_map(|e| score_driver(e, &q))
            .collect();
        scored.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap());
        scored.truncate(limit);
        let best_match = scored.first().cloned();
        Ok(ResolveDriverResult {
            best_match,
            candidates: scored,
        })
    }
}

#[cfg(windows)]
fn score_driver(entry: &super::RosterEntry, q: &str) -> Option<super::DriverMatch> {
    let name_lower = entry.user_name.to_lowercase();
    let abbrev_lower = entry.abbrev_name.to_lowercase();
    let car_num = entry.car_number.trim_start_matches('0').to_string();

    let (confidence, reason) = if name_lower == q {
        (1.0, "exact")
    } else if name_lower.starts_with(q) {
        (0.9, "name_prefix")
    } else if abbrev_lower.contains(q) {
        (0.8, "abbrev")
    } else if name_lower.split_whitespace().any(|w| w.starts_with(q)) {
        (0.75, "given_name_or_surname_prefix")
    } else if car_num == q || entry.car_number == q {
        (0.85, "car_number")
    } else if name_lower.contains(q) {
        (0.6, "substring")
    } else {
        return None;
    };

    Some(super::DriverMatch {
        car_idx: entry.car_idx,
        display_name: entry.user_name.clone(),
        car_number: entry.car_number.clone(),
        confidence,
        match_reason: reason.to_string(),
    })
}

#[cfg(windows)]
fn read_bool(
    sample: &iracing::telemetry::Sample,
    name: &'static str,
) -> Result<bool, AdapterError> {
    match sample
        .get(name)
        .map_err(|_| AdapterError::MissingTelemetryVar(name))?
    {
        Value::BOOL(value) => Ok(value),
        _ => Err(AdapterError::InvalidTelemetryType(name)),
    }
}

#[cfg(windows)]
fn read_i32(sample: &iracing::telemetry::Sample, name: &'static str) -> Result<i32, AdapterError> {
    match sample
        .get(name)
        .map_err(|_| AdapterError::MissingTelemetryVar(name))?
    {
        Value::INT(value) => Ok(value),
        Value::BITS(value) => Ok(value as i32),
        _ => Err(AdapterError::InvalidTelemetryType(name)),
    }
}

#[cfg(windows)]
fn read_f64(sample: &iracing::telemetry::Sample, name: &'static str) -> Result<f64, AdapterError> {
    match sample
        .get(name)
        .map_err(|_| AdapterError::MissingTelemetryVar(name))?
    {
        Value::DOUBLE(value) => Ok(value),
        Value::FLOAT(value) => Ok(value as f64),
        _ => Err(AdapterError::InvalidTelemetryType(name)),
    }
}

#[cfg(windows)]
fn read_i32_vec(
    sample: &iracing::telemetry::Sample,
    name: &'static str,
) -> Result<Vec<i32>, AdapterError> {
    match sample
        .get(name)
        .map_err(|_| AdapterError::MissingTelemetryVar(name))?
    {
        Value::IntVec(values) => Ok(values),
        Value::INT(value) => Ok(vec![value]),
        _ => Err(AdapterError::InvalidTelemetryType(name)),
    }
}

#[cfg(windows)]
fn read_f32_vec(
    sample: &iracing::telemetry::Sample,
    name: &'static str,
) -> Result<Vec<f32>, AdapterError> {
    match sample
        .get(name)
        .map_err(|_| AdapterError::MissingTelemetryVar(name))?
    {
        Value::FloatVec(values) => Ok(values),
        Value::FLOAT(value) => Ok(vec![value]),
        _ => Err(AdapterError::InvalidTelemetryType(name)),
    }
}

#[cfg(windows)]
fn read_bool_vec(
    sample: &iracing::telemetry::Sample,
    name: &'static str,
) -> Result<Vec<bool>, AdapterError> {
    match sample
        .get(name)
        .map_err(|_| AdapterError::MissingTelemetryVar(name))?
    {
        Value::BoolVec(values) => Ok(values),
        Value::BOOL(value) => Ok(vec![value]),
        _ => Err(AdapterError::InvalidTelemetryType(name)),
    }
}

#[cfg(windows)]
fn parse_session_data(
    session_yaml: &str,
    current_session_num: i32,
) -> Result<SessionData, AdapterError> {
    let root = parse_session_root(session_yaml)?;

    let track_display_name = yaml_str_at(&root, &["WeekendInfo", "TrackDisplayName"])?.to_string();
    let sessions = yaml_seq_at(&root, &["SessionInfo", "Sessions"])?;
    let driver_count = yaml_seq_at(&root, &["DriverInfo", "Drivers"])?.len();

    let current_session_type = sessions
        .iter()
        .find(|session| {
            session.get("SessionNum").and_then(YamlValue::as_i64)
                == Some(current_session_num as i64)
        })
        .or_else(|| sessions.first())
        .and_then(|session| session.get("SessionType"))
        .and_then(YamlValue::as_str)
        .unwrap_or("Unknown")
        .to_string();

    Ok(SessionData {
        track_display_name,
        current_session_type,
        driver_count,
        session_count: sessions.len(),
    })
}

#[cfg(windows)]
fn parse_session_root(session_yaml: &str) -> Result<YamlValue, AdapterError> {
    serde_yaml::from_str(session_yaml).map_err(|error| AdapterError::SessionInfo(error.to_string()))
}

#[cfg(windows)]
fn find_car_number_for_car_idx(session_yaml: &str, car_idx: i32) -> Result<String, AdapterError> {
    let root = parse_session_root(session_yaml)?;
    let drivers = yaml_seq_at(&root, &["DriverInfo", "Drivers"])?;

    let driver = drivers
        .iter()
        .find(|driver| driver.get("CarIdx").and_then(YamlValue::as_i64) == Some(car_idx as i64))
        .ok_or_else(|| AdapterError::TargetNotFound(format!("car_idx={car_idx}")))?;

    driver
        .get("CarNumber")
        .and_then(YamlValue::as_str)
        .map(|value| value.to_string())
        .ok_or_else(|| {
            AdapterError::SessionInfo("DriverInfo.Drivers[].CarNumber missing".to_string())
        })
}

#[cfg(windows)]
fn yaml_str_at<'a>(root: &'a YamlValue, path: &[&str]) -> Result<&'a str, AdapterError> {
    let value = yaml_value_at(root, path)?;
    value
        .as_str()
        .ok_or_else(|| AdapterError::SessionInfo(format!("{} is not a string", path.join("."))))
}

#[cfg(windows)]
fn yaml_seq_at<'a>(root: &'a YamlValue, path: &[&str]) -> Result<&'a Vec<YamlValue>, AdapterError> {
    let value = yaml_value_at(root, path)?;
    value
        .as_sequence()
        .ok_or_else(|| AdapterError::SessionInfo(format!("{} is not a sequence", path.join("."))))
}

#[cfg(windows)]
fn yaml_value_at<'a>(root: &'a YamlValue, path: &[&str]) -> Result<&'a YamlValue, AdapterError> {
    let mut current = root;

    for segment in path {
        current = current
            .get(*segment)
            .ok_or_else(|| AdapterError::SessionInfo(format!("missing {}", path.join("."))))?;
    }

    Ok(current)
}

#[cfg(windows)]
fn pad_car_number(car_number: &str) -> u16 {
    let bytes = car_number.as_bytes();
    let mut zeros = 0usize;
    for &byte in bytes {
        if byte == b'0' {
            zeros += 1;
        } else {
            break;
        }
    }

    if zeros > 0 && zeros == bytes.len() {
        zeros -= 1;
    }

    let number: u16 = car_number.parse().unwrap_or(0);

    if zeros > 0 {
        let num_place = if number > 99 {
            3
        } else if number > 9 {
            2
        } else {
            1
        };

        number + 1000 * (num_place + zeros as u16)
    } else {
        number
    }
}

#[cfg(windows)]
fn normalize_u16(value: i32, field: &str) -> Result<i32, AdapterError> {
    if !(0..=u16::MAX as i32).contains(&value) {
        return Err(AdapterError::InvalidArgument(format!(
            "{field} must be in 0..={}.",
            u16::MAX
        )));
    }

    Ok(value)
}

#[cfg(windows)]
fn send_replay_set_play_speed(speed: i32, slow_motion: bool) -> Result<(), AdapterError> {
    let client =
        BroadcastClient::new().map_err(|error| AdapterError::Broadcast(error.to_string()))?;
    client
        .send_message(BroadcastMessage::ReplaySetPlaySpeed(
            speed as u8,
            slow_motion,
        ))
        .map_err(|error| AdapterError::Broadcast(error.to_string()))
}

#[cfg(windows)]
fn send_broadcast_message_3(
    message: i32,
    var1: i32,
    var2: i32,
    var3: i32,
) -> Result<(), AdapterError> {
    send_broadcast_message_2(
        message,
        var1,
        ((var2 & 0xFFFF) | ((var3 & 0xFFFF) << 16)) as i32,
    )
}

#[cfg(windows)]
fn send_broadcast_message_2(message: i32, var1: i32, var2: i32) -> Result<(), AdapterError> {
    let broadcast_message = wide_string(IRSDK_BROADCASTMSGNAME);
    let wparam = ((message & 0xFFFF) | ((var1 & 0xFFFF) << 16)) as usize;
    let lparam = var2 as isize;
    debug!(
        "send_broadcast_message_2: message={} var1={} var2={} wparam=0x{:08X} lparam=0x{:08X}",
        message, var1, var2, wparam, lparam as usize
    );

    unsafe {
        let message_id = RegisterWindowMessageW(broadcast_message.as_ptr());
        if message_id == 0 {
            return Err(AdapterError::Broadcast(
                std::io::Error::from_raw_os_error(GetLastError() as i32).to_string(),
            ));
        }
        debug!(
            "send_broadcast_message_2: registered message_id={}",
            message_id
        );

        let success = SendNotifyMessageW(HWND_BROADCAST, message_id, wparam, lparam);

        if success == 0 {
            return Err(AdapterError::Broadcast(
                std::io::Error::from_raw_os_error(GetLastError() as i32).to_string(),
            ));
        }
        debug!("send_broadcast_message_2: SendNotifyMessageW returned success");
    }

    Ok(())
}

#[cfg(windows)]
fn wide_string(value: &str) -> Vec<u16> {
    OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(windows)]
fn read_session_yaml() -> Result<String, AdapterError> {
    let path = wide_string(IRSDK_MEMMAPFILENAME);

    unsafe {
        let mapping = OpenFileMappingW(FILE_MAP_READ, FALSE, path.as_ptr());
        if mapping.is_null() {
            return Err(AdapterError::NotConnected(
                std::io::Error::from_raw_os_error(GetLastError() as i32).to_string(),
            ));
        }

        let view = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
        if view.is_null() {
            let error = std::io::Error::from_raw_os_error(GetLastError() as i32).to_string();
            CloseHandle(mapping);
            return Err(AdapterError::NotConnected(error));
        }

        let result = read_session_yaml_from_view(view);

        UnmapViewOfFile(view);
        CloseHandle(mapping);

        let (session_info_update, yaml) = result?;
        let cache = SESSION_YAML_CACHE.get_or_init(|| Mutex::new(None));
        let mut guard = cache.lock().map_err(|_| {
            AdapterError::SessionInfo("session YAML cache lock poisoned".to_string())
        })?;

        if let Some(cached) = guard.as_ref() {
            if cached.session_info_update == session_info_update {
                return Ok(cached.yaml.clone());
            }
        }

        *guard = Some(SessionYamlCache {
            session_info_update,
            yaml: yaml.clone(),
        });

        Ok(yaml)
    }
}

#[cfg(windows)]
unsafe fn read_session_yaml_from_view(
    view: *mut std::ffi::c_void,
) -> Result<(i32, String), AdapterError> {
    if view == null_mut() {
        return Err(AdapterError::NotConnected(
            "shared-memory view pointer was null".to_string(),
        ));
    }

    let header = &*(view as *const IrsdkHeaderPrefix);
    let start = (view as usize + header.session_info_offset as usize) as *const u8;
    let bytes = slice::from_raw_parts(start, header.session_info_len as usize);

    Ok((
        header.session_info_update,
        String::from_utf8_lossy(bytes)
            .trim_end_matches('\0')
            .to_string(),
    ))
}
