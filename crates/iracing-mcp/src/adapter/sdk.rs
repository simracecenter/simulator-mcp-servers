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
use std::sync::OnceLock;
#[cfg(any(windows, test))]
use std::sync::{Arc, Mutex};
#[cfg(windows)]
use tracing::{debug, warn};

#[cfg(windows)]
use iracing_broadcast::{BroadcastMessage, Client as BroadcastClient};

#[cfg(windows)]
use std::{ffi::OsStr, os::windows::ffi::OsStrExt, slice};

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
/// Bit 0 (`irsdk_stConnected`) of `IrsdkHeaderPrefix::status`. iRacing's
/// background `iRacingService` keeps the shared-memory mapping open (with the
/// last-known telemetry frozen) even after the sim itself has fully exited,
/// so successfully opening/mapping the file is not sufficient to detect a
/// live connection - this bit must be checked too.
const IRSDK_STATUS_CONNECTED: i32 = 1;

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

#[cfg(any(windows, test))]
#[derive(Clone)]
struct ParsedSessionCache<T> {
    session_info_update: i32,
    document: Arc<T>,
}

#[cfg(windows)]
static SESSION_YAML_CACHE: OnceLock<Mutex<Option<ParsedSessionCache<YamlValue>>>> = OnceLock::new();

#[cfg(windows)]
struct SdkConnection {
    connection: iracing::Connection,
    mapping: winapi::um::winnt::HANDLE,
    view: *mut std::ffi::c_void,
}

#[cfg(windows)]
// `iracing::Connection` stores a raw pointer to its mapped view, so it does
// not provide the auto Send/Sync guarantees needed here. SdkConnection is
// accessed only while SDK_CONNECTION's Mutex is held; its mapping handle and
// view remain owned for its lifetime and are released in Drop.
unsafe impl Send for SdkConnection {}

#[cfg(windows)]
unsafe impl Sync for SdkConnection {}

#[cfg(windows)]
impl Drop for SdkConnection {
    fn drop(&mut self) {
        unsafe {
            UnmapViewOfFile(self.view);
            CloseHandle(self.mapping);
        }
    }
}

#[cfg(windows)]
static SDK_CONNECTION: OnceLock<Mutex<Option<SdkConnection>>> = OnceLock::new();

#[derive(Debug, Default)]
pub struct SdkAdapter;

#[cfg(any(windows, test))]
fn cache_parsed_document<T, F>(
    cache: &Mutex<Option<ParsedSessionCache<T>>>,
    session_info_update: i32,
    session_yaml: &str,
    parse: F,
) -> Result<Arc<T>, AdapterError>
where
    T: Send + Sync + 'static,
    F: FnOnce(&str) -> Result<T, AdapterError>,
{
    {
        let guard = cache.lock().map_err(|_| {
            AdapterError::SessionInfo("session YAML cache lock poisoned".to_string())
        })?;
        if let Some(cached) = guard.as_ref() {
            if cached.session_info_update == session_info_update {
                return Ok(Arc::clone(&cached.document));
            }
        }
    }

    let document = Arc::new(parse(session_yaml)?);
    let mut guard = cache
        .lock()
        .map_err(|_| AdapterError::SessionInfo("session YAML cache lock poisoned".to_string()))?;

    if let Some(cached) = guard.as_ref() {
        if cached.session_info_update == session_info_update {
            return Ok(Arc::clone(&cached.document));
        }
    }

    *guard = Some(ParsedSessionCache {
        session_info_update,
        document: Arc::clone(&document),
    });
    Ok(document)
}

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

#[cfg(all(test, not(windows)))]
mod tests {
    use super::*;

    fn assert_not_available<T>(result: Result<T, AdapterError>) {
        match result {
            Err(AdapterError::NotConnected(message)) => {
                assert_eq!(message, "the iRacing SDK is only available on Windows");
            }
            _ => panic!("expected the non-Windows SDK error"),
        }
    }

    #[tokio::test]
    async fn non_windows_adapter_reports_disconnected_for_every_operation() {
        let adapter = SdkAdapter;
        let overview = adapter.get_session_overview().await;

        assert!(!overview.connected);
        assert!(!overview.is_replay);
        assert!(!overview.is_in_car);
        assert_eq!(overview.session_name, "Disconnected");
        assert_eq!(overview.track_name, "Disconnected");

        assert_not_available(adapter.get_session_data().await);
        assert_not_available(adapter.get_replay_state().await);
        assert_not_available(adapter.set_replay_playback(1, false).await);
        assert_not_available(adapter.replay_seek_session_time(0, 0).await);
        assert_not_available(
            adapter
                .replay_seek_frame(ReplaySeekFrameMode::Begin, 0)
                .await,
        );
        assert_not_available(adapter.replay_search_event(ReplaySearchMode::ToStart).await);
        assert_not_available(adapter.camera_set_state(0).await);
        assert_not_available(adapter.camera_focus(0, None, None).await);
        assert_not_available(adapter.get_weekend_info().await);
        assert_not_available(adapter.get_roster(false, false).await);
        assert_not_available(adapter.get_camera_groups().await);
        assert_not_available(adapter.get_standings(None).await);
        assert_not_available(adapter.get_relatives().await);
        assert_not_available(adapter.resolve_driver("driver", 1).await);
    }
}

#[cfg(test)]
mod parsed_cache_tests {
    use super::{cache_parsed_document, ParsedSessionCache};
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn unchanged_session_info_update_reuses_parsed_document() {
        let cache = Mutex::new(None::<ParsedSessionCache<Value>>);
        let parse_count = AtomicUsize::new(0);

        let first = cache_parsed_document(&cache, 7, r#"{"track":"first"}"#, |yaml| {
            parse_count.fetch_add(1, Ordering::SeqCst);
            serde_json::from_str(yaml)
                .map_err(|error| super::AdapterError::SessionInfo(error.to_string()))
        })
        .expect("first document should parse");
        let second = cache_parsed_document(&cache, 7, r#"{"track":"ignored"}"#, |yaml| {
            parse_count.fetch_add(1, Ordering::SeqCst);
            serde_json::from_str(yaml)
                .map_err(|error| super::AdapterError::SessionInfo(error.to_string()))
        })
        .expect("cached document should be returned");

        assert_eq!(parse_count.load(Ordering::SeqCst), 1);
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first["track"], "first");
    }

    #[test]
    fn changed_session_info_update_reparses_document() {
        let cache = Mutex::new(None::<ParsedSessionCache<Value>>);
        let parse_count = AtomicUsize::new(0);

        let first = cache_parsed_document(&cache, 7, r#"{"track":"first"}"#, |yaml| {
            parse_count.fetch_add(1, Ordering::SeqCst);
            serde_json::from_str(yaml)
                .map_err(|error| super::AdapterError::SessionInfo(error.to_string()))
        })
        .expect("first document should parse");
        let second = cache_parsed_document(&cache, 8, r#"{"track":"second"}"#, |yaml| {
            parse_count.fetch_add(1, Ordering::SeqCst);
            serde_json::from_str(yaml)
                .map_err(|error| super::AdapterError::SessionInfo(error.to_string()))
        })
        .expect("changed document should parse");

        assert_eq!(parse_count.load(Ordering::SeqCst), 2);
        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second["track"], "second");
    }
}

#[cfg(test)]
mod track_surface_tests {
    use super::{track_surface_details, track_surface_for_car};

    #[test]
    fn maps_all_known_track_surface_values() {
        assert_eq!(
            track_surface_details(-1),
            (Some("NotInWorld".to_string()), Some(false))
        );
        assert_eq!(
            track_surface_details(0),
            (Some("OffTrack".to_string()), Some(true))
        );
        assert_eq!(
            track_surface_details(1),
            (Some("InPitStall".to_string()), Some(true))
        );
        assert_eq!(
            track_surface_details(2),
            (Some("AproachingPits".to_string()), Some(true))
        );
        assert_eq!(
            track_surface_details(3),
            (Some("OnTrack".to_string()), Some(true))
        );
    }

    #[test]
    fn unknown_track_surface_values_are_unavailable() {
        assert_eq!(track_surface_details(99), (None, None));
    }

    #[test]
    fn absent_track_surface_variable_is_unavailable() {
        assert_eq!(track_surface_for_car(None, 0), (None, None));
        assert_eq!(track_surface_for_car(Some(&[]), 0), (None, None));
    }
}

#[cfg(windows)]
#[async_trait]
impl IracingAdapter for SdkAdapter {
    async fn get_session_overview(&self) -> SessionOverview {
        match run_blocking(|| Ok(SdkAdapter.get_session_overview_sync())).await {
            Ok(overview) => overview,
            Err(error) => {
                warn!(%error, "get_session_overview: blocking task unavailable");
                SessionOverview {
                    connected: false,
                    is_replay: false,
                    is_in_car: false,
                    session_name: "Disconnected".to_string(),
                    track_name: "Disconnected".to_string(),
                }
            }
        }
    }

    async fn get_session_data(&self) -> Result<SessionData, AdapterError> {
        run_blocking(|| SdkAdapter.session_data_sync()).await
    }

    async fn get_replay_state(&self) -> Result<ReplayState, AdapterError> {
        run_blocking(|| SdkAdapter.replay_state_sync()).await
    }

    async fn set_replay_playback(&self, speed: i32, slow_motion: bool) -> Result<(), AdapterError> {
        run_blocking(move || SdkAdapter.set_replay_playback_sync(speed, slow_motion)).await
    }

    async fn replay_seek_session_time(
        &self,
        session_num: i32,
        session_time_ms: i32,
    ) -> Result<(), AdapterError> {
        run_blocking(move || SdkAdapter.replay_seek_session_time_sync(session_num, session_time_ms))
            .await
    }

    async fn replay_seek_frame(
        &self,
        mode: ReplaySeekFrameMode,
        frame: i32,
    ) -> Result<(), AdapterError> {
        run_blocking(move || SdkAdapter.replay_seek_frame_sync(mode, frame)).await
    }

    async fn replay_search_event(&self, mode: ReplaySearchMode) -> Result<(), AdapterError> {
        run_blocking(move || SdkAdapter.replay_search_event_sync(mode)).await
    }

    async fn camera_set_state(&self, state_bits: i32) -> Result<(), AdapterError> {
        run_blocking(move || SdkAdapter.camera_set_state_sync(state_bits)).await
    }

    async fn camera_focus(
        &self,
        car_idx: i32,
        group_number: Option<i32>,
        camera_number: Option<i32>,
    ) -> Result<(), AdapterError> {
        run_blocking(move || SdkAdapter.camera_focus_sync(car_idx, group_number, camera_number))
            .await
    }

    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError> {
        run_blocking(|| SdkAdapter.get_weekend_info_sync()).await
    }

    async fn get_roster(
        &self,
        include_spectators: bool,
        include_pace_car: bool,
    ) -> Result<Roster, AdapterError> {
        run_blocking(move || SdkAdapter.get_roster_sync(include_spectators, include_pace_car)).await
    }

    async fn get_camera_groups(&self) -> Result<CameraGroupList, AdapterError> {
        run_blocking(|| SdkAdapter.get_camera_groups_sync()).await
    }

    async fn get_standings(&self, session_num: Option<i32>) -> Result<Standings, AdapterError> {
        run_blocking(move || SdkAdapter.get_standings_sync(session_num)).await
    }

    async fn get_relatives(&self) -> Result<Relatives, AdapterError> {
        run_blocking(|| SdkAdapter.get_relatives_sync()).await
    }

    async fn resolve_driver(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<ResolveDriverResult, AdapterError> {
        let query = query.to_string();
        run_blocking(move || SdkAdapter.resolve_driver_sync(&query, limit)).await
    }
}

#[cfg(windows)]
impl SdkAdapter {
    fn get_session_overview_sync(&self) -> SessionOverview {
        // This tool never fails — a disconnected sim is a valid overview, not
        // an error — but the underlying reads can fail for reasons worth
        // knowing (unparseable session YAML, a missing telemetry var). Log
        // those at `warn` before collapsing them into `None` so they aren't
        // silently swallowed and left indistinguishable from "sim not running".
        let session_data = match self.session_data_sync() {
            Ok(data) => Some(data),
            Err(error) => {
                warn!(%error, "get_session_overview: session data unavailable");
                None
            }
        };
        let replay_state = match self.replay_state_sync() {
            Ok(state) => Some(state),
            Err(error) => {
                warn!(%error, "get_session_overview: replay state unavailable");
                None
            }
        };

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

    fn session_data_sync(&self) -> Result<SessionData, AdapterError> {
        let current_session_num = with_sdk_connection(|connection| {
            let telemetry = connection
                .connection
                .telemetry()
                .map_err(|error| AdapterError::NotConnected(error.to_string()))?;
            read_i32(&telemetry, "SessionNum")
        })?;
        let session_yaml = read_session_yaml()?;

        parse_session_data(&session_yaml, current_session_num)
    }

    fn replay_state_sync(&self) -> Result<ReplayState, AdapterError> {
        let state = with_sdk_connection(|connection| {
            let sample = connection
                .connection
                .telemetry()
                .map_err(|error| AdapterError::NotConnected(error.to_string()))?;

            Ok(ReplayState {
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
            })
        })?;
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

        ensure_sdk_connection()?;

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

        ensure_sdk_connection()?;

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
        ensure_sdk_connection()?;

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
        ensure_sdk_connection()?;

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

        ensure_sdk_connection()?;

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

    fn get_weekend_info_sync(&self) -> Result<WeekendInfo, AdapterError> {
        let root = read_session_yaml()?;
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

    fn get_roster_sync(
        &self,
        include_spectators: bool,
        include_pace_car: bool,
    ) -> Result<Roster, AdapterError> {
        let root = read_session_yaml()?;
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

    fn get_camera_groups_sync(&self) -> Result<CameraGroupList, AdapterError> {
        let root = read_session_yaml()?;
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

    fn get_standings_sync(&self, session_num: Option<i32>) -> Result<Standings, AdapterError> {
        let root = read_session_yaml()?;
        let sessions = yaml_seq_at(&root, &["SessionInfo", "Sessions"])?;

        // Determine which session to use
        let target_session_num = match session_num {
            Some(n) => n,
            None => {
                // fall back to telemetry SessionNum
                with_sdk_connection(|connection| {
                    let sample = connection
                        .connection
                        .telemetry()
                        .map_err(|e| AdapterError::NotConnected(e.to_string()))?;
                    Ok(read_i32(&sample, "SessionNum").unwrap_or(0))
                })?
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
                    .map(|p| {
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
                        SessionPosition {
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
                        }
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

    fn get_relatives_sync(&self) -> Result<Relatives, AdapterError> {
        // Ensure session data is available (and any related connection issues
        // surface as an error) before reading telemetry arrays below.
        let _session_data = self.session_data_sync()?;
        let roster = self.get_roster_sync(false, false)?;
        let telemetry = with_sdk_connection(|connection| {
            connection
                .connection
                .telemetry()
                .map_err(|error| AdapterError::NotConnected(error.to_string()))
        })?;

        let session_num = read_i32(&telemetry, "SessionNum")?;
        let class_positions = read_i32_vec(&telemetry, "CarIdxClassPosition")?;
        let laps = read_i32_vec(&telemetry, "CarIdxLap")?;
        let lap_dist_pcts = read_f32_vec(&telemetry, "CarIdxLapDistPct")?;
        let on_pit_road = read_bool_vec(&telemetry, "CarIdxOnPitRoad")?;
        let est_times = read_f32_vec(&telemetry, "CarIdxEstTime")?;
        let f2_times = read_f32_vec(&telemetry, "CarIdxF2Time")?;
        let track_surfaces = read_optional_i32_vec(&telemetry, "CarIdxTrackSurface")?;

        #[derive(Clone)]
        struct RawRelative {
            class_position: i32,
            car_idx: i32,
            car_number: String,
            display_name: String,
            lap: i32,
            lap_dist_pct: Option<f64>,
            is_in_pit: bool,
            track_surface: Option<String>,
            in_world: Option<bool>,
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
                let (track_surface, in_world) =
                    track_surface_for_car(track_surfaces.as_deref(), car_idx);
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
                    track_surface,
                    in_world,
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
                track_surface: current.track_surface.clone(),
                in_world: current.in_world,
            });
        }

        Ok(Relatives {
            basis: "track".to_string(),
            session_num,
            entries,
            count: raw_entries.len(),
        })
    }

    fn resolve_driver_sync(
        &self,
        query: &str,
        limit: usize,
    ) -> Result<ResolveDriverResult, AdapterError> {
        let roster = self.get_roster_sync(false, false)?;
        let q = query.to_lowercase();
        let mut scored: Vec<DriverMatch> = roster
            .entries
            .iter()
            .filter_map(|e| score_driver(e, &q))
            .collect();
        scored.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
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
fn read_optional_i32_vec(
    sample: &iracing::telemetry::Sample,
    name: &'static str,
) -> Result<Option<Vec<i32>>, AdapterError> {
    match sample.get(name) {
        Err(_) => Ok(None),
        Ok(Value::IntVec(values)) => Ok(Some(values)),
        Ok(Value::INT(value)) => Ok(Some(vec![value])),
        Ok(_) => Err(AdapterError::InvalidTelemetryType(name)),
    }
}

#[cfg(any(windows, test))]
fn track_surface_for_car(
    track_surfaces: Option<&[i32]>,
    car_idx: usize,
) -> (Option<String>, Option<bool>) {
    track_surfaces
        .and_then(|values| values.get(car_idx).copied())
        .map(track_surface_details)
        .unwrap_or((None, None))
}

#[cfg(any(windows, test))]
fn track_surface_details(raw_value: i32) -> (Option<String>, Option<bool>) {
    match raw_value {
        -1 => (Some("NotInWorld".to_string()), Some(false)),
        0 => (Some("OffTrack".to_string()), Some(true)),
        1 => (Some("InPitStall".to_string()), Some(true)),
        2 => (Some("AproachingPits".to_string()), Some(true)),
        3 => (Some("OnTrack".to_string()), Some(true)),
        value => {
            #[cfg(windows)]
            tracing::debug!(raw_value = value, "unknown iRacing track surface value");
            #[cfg(not(windows))]
            let _ = value;
            (None, None)
        }
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
    root: &YamlValue,
    current_session_num: i32,
) -> Result<SessionData, AdapterError> {
    let track_display_name = yaml_str_at(root, &["WeekendInfo", "TrackDisplayName"])?.to_string();
    let sessions = yaml_seq_at(root, &["SessionInfo", "Sessions"])?;
    let driver_count = yaml_seq_at(root, &["DriverInfo", "Drivers"])?.len();

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
fn find_car_number_for_car_idx(root: &YamlValue, car_idx: i32) -> Result<String, AdapterError> {
    let drivers = yaml_seq_at(root, &["DriverInfo", "Drivers"])?;

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
    send_broadcast_message_2(message, var1, (var2 & 0xFFFF) | ((var3 & 0xFFFF) << 16))
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
async fn run_blocking<T, F>(operation: F) -> Result<T, AdapterError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, AdapterError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|error| AdapterError::Internal(format!("blocking SDK task failed: {error}")))?
}

#[cfg(windows)]
fn with_sdk_connection<T, F>(operation: F) -> Result<T, AdapterError>
where
    F: FnOnce(&SdkConnection) -> Result<T, AdapterError>,
{
    let state = SDK_CONNECTION.get_or_init(|| Mutex::new(None));
    let mut guard = state.lock().map_err(|_| {
        AdapterError::NotConnected("iRacing SDK connection lock poisoned".to_string())
    })?;
    if guard.is_none() {
        *guard = Some(SdkConnection::new()?);
    }

    let status = guard
        .as_ref()
        .ok_or_else(|| {
            AdapterError::NotConnected("iRacing SDK connection is unavailable".to_string())
        })?
        .ensure_connected();
    if let Err(error) = status {
        *guard = None;
        return Err(error);
    }

    let result = {
        let sdk = guard.as_ref().ok_or_else(|| {
            AdapterError::NotConnected("iRacing SDK connection is unavailable".to_string())
        })?;
        operation(sdk)
    };
    if result.is_err() {
        *guard = None;
    }
    result
}

#[cfg(windows)]
fn ensure_sdk_connection() -> Result<(), AdapterError> {
    with_sdk_connection(|_| Ok(()))
}

#[cfg(windows)]
fn read_session_yaml() -> Result<Arc<YamlValue>, AdapterError> {
    let cache = SESSION_YAML_CACHE.get_or_init(|| Mutex::new(None));

    let (session_info_update, session_yaml) =
        with_sdk_connection(|sdk| unsafe { read_session_yaml_from_view(sdk.view) })?;
    cache_parsed_document(
        cache,
        session_info_update,
        &session_yaml,
        parse_session_root,
    )
}

#[cfg(windows)]
impl SdkConnection {
    fn new() -> Result<Self, AdapterError> {
        let connection = iracing::Connection::new()
            .map_err(|error| AdapterError::NotConnected(error.to_string()))?;
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

            Ok(Self {
                connection,
                mapping,
                view,
            })
        }
    }

    fn ensure_connected(&self) -> Result<(), AdapterError> {
        let status = unsafe { (*(self.view as *const IrsdkHeaderPrefix)).status };
        if status & IRSDK_STATUS_CONNECTED == 0 {
            return Err(AdapterError::NotConnected(
                "iRacing SDK shared memory is mapped but reports the simulator is not connected"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
unsafe fn read_session_yaml_from_view(
    view: *mut std::ffi::c_void,
) -> Result<(i32, String), AdapterError> {
    if view.is_null() {
        return Err(AdapterError::NotConnected(
            "shared-memory view pointer was null".to_string(),
        ));
    }

    let header = &*(view as *const IrsdkHeaderPrefix);
    if header.status & IRSDK_STATUS_CONNECTED == 0 {
        return Err(AdapterError::NotConnected(
            "iRacing SDK shared memory is mapped but reports the simulator is not connected"
                .to_string(),
        ));
    }

    let start = (view as usize + header.session_info_offset as usize) as *const u8;
    let bytes = slice::from_raw_parts(start, header.session_info_len as usize);

    Ok((
        header.session_info_update,
        String::from_utf8_lossy(bytes)
            .trim_end_matches('\0')
            .to_string(),
    ))
}
