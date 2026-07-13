// SPDX-License-Identifier: GPL-3.0-or-later
//! `SdkAdapter` — real LMU adapter backed by
//! [`rF2SharedMemoryMapPlugin`](https://github.com/TheIronWolfModding/rF2SharedMemoryMapPlugin)'s
//! named shared-memory buffers, per
//! [ADR 0002](../../../../docs/adr/0002-lmu-adapter-design.md).
//!
//! Mirrors `crates/iracing-mcp/src/adapter/sdk.rs`'s shape: real body is
//! `#[cfg(windows)]` (the plugin, like the iRacing SDK, is Windows/shared-
//! memory-only); on any other target every method reports the adapter as
//! disconnected/unavailable so the crate still compiles and its Stub-backed
//! tests still run on Linux.
//!
//! ## Plugin version pin
//!
//! **Not pinned to a specific tag/commit in this change.** ADR 0002's own
//! implementation task asks the implementing engineer to record "the latest
//! tagged release of `rF2SharedMemoryMapPlugin` at implementation time" here.
//! This code was written in a Linux dev container with no access to the
//! plugin's actual `Include/rF2State.h`/`rF2Data.hpp` headers or release
//! history, so the struct layouts below are a best-effort reconstruction
//! from the buffer/field names documented in
//! [ADR 0002](../../../../docs/adr/0002-lmu-adapter-design.md)'s research
//! section, **not** verified byte-for-byte against a real header.
//!
//! Before trusting this against a live LMU instance: pull the actual tagged
//! release's headers on Windows, diff the struct definitions below against
//! them, fix any mismatches, and replace this comment with the exact
//! version/commit used — this is required by this issue's Done criteria
//! (manual live verification) and should happen together with it, not be
//! skipped.

use async_trait::async_trait;

#[cfg(windows)]
use std::{ffi::OsStr, os::windows::ffi::OsStrExt, mem::MaybeUninit, ptr};

#[cfg(windows)]
use winapi::{
    ctypes::c_void,
    shared::minwindef::FALSE,
    um::{
        handleapi::CloseHandle,
        memoryapi::{
            MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, VirtualQuery, FILE_MAP_READ,
        },
        winnt::MEMORY_BASIC_INFORMATION,
    },
};

use super::{
    AdapterError, HwControlCommand, LmuAdapter, PitInfoState, RelativeEntry, Relatives, Roster,
    SessionData, SessionOverview, Standings, WeatherControl, WeatherState, WeekendInfo,
};

#[cfg(windows)]
use super::{RosterEntry, StandingsEntry};

/// Shared-memory-mapped-file names used by `rF2SharedMemoryMapPlugin`.
#[cfg(windows)]
const SCORING_MAP_NAME: &str = "$rF2SMMP_Scoring$";
#[cfg(windows)]
const WEATHER_MAP_NAME: &str = "$rF2SMMP_Weather$";
#[cfg(windows)]
const PIT_INFO_MAP_NAME: &str = "$rF2SMMP_PitInfo$";
#[cfg(windows)]
const HW_CONTROL_MAP_NAME: &str = "$rF2SMMP_HWControl$";
#[cfg(windows)]
const WEATHER_CONTROL_MAP_NAME: &str = "$rF2SMMP_WeatherControl$";

/// Max vehicles in the fixed-size `mVehicles` array — mirrors the plugin's
/// own `rF2MappedBufferHeader::MAX_MAPPED_VEHICLES` (best-effort recollection,
/// needs verifying against the real header — see module doc comment).
#[cfg(windows)]
const MAX_MAPPED_VEHICLES: usize = 128;

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct RawVehicleScoring {
    id: i32,
    driver_name: [u8; 32],
    vehicle_name: [u8; 64],
    vehicle_class: [u8; 32],
    total_laps: i32,
    sector: i8,
    finish_status: i8,
    in_pits: u8,
    place: i8,
    best_lap_time_sec: f64,
    last_lap_time_sec: f64,
    time_behind_next_sec: f64,
    laps_behind_next: i32,
    time_behind_leader_sec: f64,
    laps_behind_leader: i32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct RawScoring {
    version_update_begin: u32,
    track_name: [u8; 64],
    session: i32,
    game_phase: u8,
    current_et_sec: f64,
    end_et_sec: f64,
    max_laps: i32,
    num_vehicles: i32,
    vehicles: [RawVehicleScoring; MAX_MAPPED_VEHICLES],
    version_update_end: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct RawWeather {
    version_update_begin: u32,
    ambient_temp_c: f64,
    track_temp_c: f64,
    raining: f64,
    cloudiness: f64,
    wind_speed_ms: f64,
    version_update_end: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct RawPitInfo {
    version_update_begin: u32,
    in_pits: u8,
    pit_state: u8,
    num_pitstops: i32,
    num_penalties: i32,
    version_update_end: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct RawHwControl {
    version_update_begin: u32,
    control_name: [u8; 32],
    value: f64,
    version_update_end: u32,
}

#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
struct RawWeatherControl {
    version_update_begin: u32,
    raining: f64,
    cloudiness: f64,
    ambient_temp_c: f64,
    version_update_end: u32,
}

#[cfg(windows)]
fn cstr_bytes_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(windows)]
fn open_mapping(name: &str) -> Result<*mut c_void, AdapterError> {
    let wide: Vec<u16> = OsStr::new(name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is a valid, NUL-terminated UTF-16 string for the
    // duration of this call.
    let handle = unsafe {
        OpenFileMappingW(
            FILE_MAP_READ | 0x0002, /* FILE_MAP_WRITE */
            FALSE,
            wide.as_ptr(),
        )
    };
    if handle.is_null() {
        return Err(AdapterError::NotConnected(format!(
            "shared-memory mapping '{name}' not found — is rF2SharedMemoryMapPlugin installed and LMU running?"
        )));
    }
    Ok(handle)
}

/// Maps `name`, validates the mapped region is at least `size_of::<T>()`
/// bytes (so a struct-layout mismatch is a clean error rather than an
/// out-of-bounds read), copies it out via an unaligned read, then unmaps —
/// mirroring `crates/iracing-mcp/src/adapter/sdk.rs`'s raw
/// `OpenFileMappingW`/`MapViewOfFile`/`UnmapViewOfFile` pattern.
#[cfg(windows)]
fn read_shared_buffer<T: Copy>(name: &str) -> Result<T, AdapterError> {
    let handle = open_mapping(name)?;

    // SAFETY: `handle` was just successfully opened above.
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ, 0, 0, 0) };
    if view.is_null() {
        // SAFETY: `handle` is a valid handle owned by this function.
        unsafe { CloseHandle(handle) };
        return Err(AdapterError::SharedMemory(format!(
            "MapViewOfFile failed for '{name}'"
        )));
    }

    // SAFETY: `MEMORY_BASIC_INFORMATION` is a plain-old-data Win32 struct;
    // `VirtualQuery` below fully populates it before it's read.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { MaybeUninit::zeroed().assume_init() };
    // SAFETY: `view` is a valid mapped pointer and `info` is large enough
    // for `VirtualQuery`'s output.
    let queried = unsafe {
        VirtualQuery(
            view,
            &mut info,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };

    let mapped_size = if queried == 0 { 0 } else { info.RegionSize };

    let result = if mapped_size < std::mem::size_of::<T>() {
        Err(AdapterError::SharedMemory(format!(
            "'{name}' mapped region ({mapped_size} bytes) is smaller than the expected struct \
             ({expected} bytes) — the struct layout in sdk.rs is out of date with the installed \
             plugin version; see this file's module doc comment",
            expected = std::mem::size_of::<T>()
        )))
    } else {
        // SAFETY: `view` points to at least `size_of::<T>()` readable bytes,
        // checked above. `ptr::read_unaligned` tolerates any alignment.
        Ok(unsafe { ptr::read_unaligned(view as *const T) })
    };

    // SAFETY: `view`/`handle` are valid and owned by this function.
    unsafe {
        UnmapViewOfFile(view);
        CloseHandle(handle);
    }

    result
}

/// Writes `value` into the start of the named input-buffer mapping.
/// Best-effort mirror of the read path above — see module doc comment.
#[cfg(windows)]
fn write_shared_buffer<T: Copy>(name: &str, value: T) -> Result<(), AdapterError> {
    let handle = open_mapping(name)?;

    // SAFETY: `handle` was just successfully opened above; requesting
    // FILE_MAP_WRITE for an input buffer that this plugin exposes as
    // read/write.
    let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ | 0x0002, 0, 0, 0) };
    if view.is_null() {
        // SAFETY: `handle` is a valid handle owned by this function.
        unsafe { CloseHandle(handle) };
        return Err(AdapterError::SharedMemory(format!(
            "MapViewOfFile (write) failed for '{name}'"
        )));
    }

    // SAFETY: `MEMORY_BASIC_INFORMATION` is a plain-old-data Win32 struct;
    // `VirtualQuery` below fully populates it before it's read.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { MaybeUninit::zeroed().assume_init() };
    // SAFETY: `view` is a valid mapped pointer and `info` is large enough.
    let queried = unsafe {
        VirtualQuery(
            view,
            &mut info,
            std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    let mapped_size = if queried == 0 { 0 } else { info.RegionSize };

    let result = if mapped_size < std::mem::size_of::<T>() {
        Err(AdapterError::SharedMemory(format!(
            "'{name}' mapped region ({mapped_size} bytes) is smaller than the expected struct \
             ({expected} bytes)",
            expected = std::mem::size_of::<T>()
        )))
    } else {
        // SAFETY: `view` points to at least `size_of::<T>()` writable bytes,
        // checked above.
        unsafe { ptr::write_unaligned(view as *mut T, value) };
        Ok(())
    };

    // SAFETY: `view`/`handle` are valid and owned by this function.
    unsafe {
        UnmapViewOfFile(view);
        CloseHandle(handle);
    }

    result
}

#[derive(Debug, Default)]
pub struct SdkAdapter;

#[cfg(not(windows))]
#[async_trait]
impl LmuAdapter for SdkAdapter {
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

    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_roster(&self, _include_spectators: bool) -> Result<Roster, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_standings(&self, _session_num: Option<i32>) -> Result<Standings, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_relatives(&self) -> Result<Relatives, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_weather(&self) -> Result<WeatherState, AdapterError> {
        Err(Self::not_available())
    }

    async fn get_pit_info(&self) -> Result<PitInfoState, AdapterError> {
        Err(Self::not_available())
    }

    async fn pit_menu_command(&self, _control: HwControlCommand) -> Result<(), AdapterError> {
        Err(Self::not_available())
    }

    async fn set_weather(&self, _weather: WeatherControl) -> Result<(), AdapterError> {
        Err(Self::not_available())
    }

    async fn camera_focus(&self, _car_idx: i32) -> Result<(), AdapterError> {
        Err(AdapterError::NotSupported("camera_focus"))
    }

    async fn replay_seek_session_time(&self, _session_time_ms: i32) -> Result<(), AdapterError> {
        Err(AdapterError::NotSupported("replay_seek_session_time"))
    }
}

#[cfg(not(windows))]
impl SdkAdapter {
    fn not_available() -> AdapterError {
        AdapterError::NotConnected(
            "the rF2SharedMemoryMapPlugin shared memory is only available on Windows".to_string(),
        )
    }
}

#[cfg(windows)]
impl SdkAdapter {
    fn scoring_sync(&self) -> Result<RawScoring, AdapterError> {
        read_shared_buffer::<RawScoring>(SCORING_MAP_NAME)
    }

    fn weather_sync(&self) -> Result<RawWeather, AdapterError> {
        read_shared_buffer::<RawWeather>(WEATHER_MAP_NAME)
    }

    fn pit_info_sync(&self) -> Result<RawPitInfo, AdapterError> {
        read_shared_buffer::<RawPitInfo>(PIT_INFO_MAP_NAME)
    }

    fn vehicles(scoring: &RawScoring) -> Vec<&RawVehicleScoring> {
        let count = (scoring.num_vehicles.max(0) as usize).min(MAX_MAPPED_VEHICLES);
        scoring.vehicles[..count].iter().collect()
    }
}

#[cfg(windows)]
#[async_trait]
impl LmuAdapter for SdkAdapter {
    async fn get_session_overview(&self) -> SessionOverview {
        match self.scoring_sync() {
            Ok(scoring) => SessionOverview {
                connected: true,
                is_replay: false,
                is_in_car: Self::vehicles(&scoring).iter().any(|v| v.in_pits == 0),
                session_name: format!("session={}", scoring.session),
                track_name: cstr_bytes_to_string(&scoring.track_name),
            },
            Err(_) => SessionOverview {
                connected: false,
                is_replay: false,
                is_in_car: false,
                session_name: "Disconnected".to_string(),
                track_name: "Disconnected".to_string(),
            },
        }
    }

    async fn get_session_data(&self) -> Result<SessionData, AdapterError> {
        let scoring = self.scoring_sync()?;
        Ok(SessionData {
            track_name: cstr_bytes_to_string(&scoring.track_name),
            session_type: format!("session={}", scoring.session),
            game_phase: format!("phase={}", scoring.game_phase),
            current_et_sec: scoring.current_et_sec,
            end_et_sec: scoring.end_et_sec,
            max_laps: scoring.max_laps,
            driver_count: Self::vehicles(&scoring).len(),
        })
    }

    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError> {
        let scoring = self.scoring_sync()?;
        let weather = self.weather_sync()?;
        Ok(WeekendInfo {
            track_name: cstr_bytes_to_string(&scoring.track_name),
            session_type: format!("session={}", scoring.session),
            max_laps: scoring.max_laps,
            end_et_sec: scoring.end_et_sec,
            ambient_temp_c: weather.ambient_temp_c,
            track_temp_c: weather.track_temp_c,
            raining: weather.raining,
        })
    }

    async fn get_roster(&self, _include_spectators: bool) -> Result<Roster, AdapterError> {
        let scoring = self.scoring_sync()?;
        let entries: Vec<RosterEntry> = Self::vehicles(&scoring)
            .iter()
            .map(|v| RosterEntry {
                id: v.id,
                driver_name: cstr_bytes_to_string(&v.driver_name),
                vehicle_name: cstr_bytes_to_string(&v.vehicle_name),
                vehicle_class: cstr_bytes_to_string(&v.vehicle_class),
                is_player: false,
            })
            .collect();
        let count = entries.len();
        Ok(Roster { entries, count })
    }

    async fn get_standings(&self, _session_num: Option<i32>) -> Result<Standings, AdapterError> {
        let scoring = self.scoring_sync()?;
        let positions: Vec<StandingsEntry> = Self::vehicles(&scoring)
            .iter()
            .map(|v| StandingsEntry {
                place: v.place as i32,
                id: v.id,
                driver_name: cstr_bytes_to_string(&v.driver_name),
                vehicle_name: cstr_bytes_to_string(&v.vehicle_name),
                laps_completed: v.total_laps,
                sector: v.sector as i32,
                best_lap_time_sec: v.best_lap_time_sec,
                last_lap_time_sec: v.last_lap_time_sec,
                time_behind_leader_sec: v.time_behind_leader_sec,
                laps_behind_leader: v.laps_behind_leader,
                in_pits: v.in_pits != 0,
                finish_status: format!("status={}", v.finish_status),
            })
            .collect();
        Ok(Standings {
            session_type: format!("session={}", scoring.session),
            positions,
        })
    }

    async fn get_relatives(&self) -> Result<Relatives, AdapterError> {
        let scoring = self.scoring_sync()?;
        let entries: Vec<RelativeEntry> = Self::vehicles(&scoring)
            .iter()
            .map(|v| RelativeEntry {
                id: v.id,
                driver_name: cstr_bytes_to_string(&v.driver_name),
                place: v.place as i32,
                laps_completed: v.total_laps,
                time_behind_next_sec: v.time_behind_next_sec,
                laps_behind_next: v.laps_behind_next,
                in_pits: v.in_pits != 0,
            })
            .collect();
        let count = entries.len();
        Ok(Relatives { entries, count })
    }

    async fn get_weather(&self) -> Result<WeatherState, AdapterError> {
        let weather = self.weather_sync()?;
        Ok(WeatherState {
            ambient_temp_c: weather.ambient_temp_c,
            track_temp_c: weather.track_temp_c,
            raining: weather.raining,
            cloudiness: weather.cloudiness,
            wind_speed_ms: weather.wind_speed_ms,
        })
    }

    async fn get_pit_info(&self) -> Result<PitInfoState, AdapterError> {
        let pit_info = self.pit_info_sync()?;
        Ok(PitInfoState {
            in_pits: pit_info.in_pits != 0,
            pit_state: format!("state={}", pit_info.pit_state),
            num_pitstops: pit_info.num_pitstops,
            num_penalties: pit_info.num_penalties,
        })
    }

    async fn pit_menu_command(&self, control: HwControlCommand) -> Result<(), AdapterError> {
        let mut control_name = [0u8; 32];
        let name_bytes = control.control_name.as_bytes();
        let copy_len = name_bytes.len().min(control_name.len() - 1);
        control_name[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

        write_shared_buffer(
            HW_CONTROL_MAP_NAME,
            RawHwControl {
                version_update_begin: 0,
                control_name,
                value: control.value,
                version_update_end: 0,
            },
        )
    }

    async fn set_weather(&self, weather: WeatherControl) -> Result<(), AdapterError> {
        if !(0.0..=1.0).contains(&weather.raining) {
            return Err(AdapterError::InvalidArgument(
                "raining must be in 0.0..=1.0".to_string(),
            ));
        }

        write_shared_buffer(
            WEATHER_CONTROL_MAP_NAME,
            RawWeatherControl {
                version_update_begin: 0,
                raining: weather.raining,
                cloudiness: weather.cloudiness.unwrap_or(0.0),
                ambient_temp_c: weather.ambient_temp_c.unwrap_or(0.0),
                version_update_end: 0,
            },
        )
    }

    async fn camera_focus(&self, _car_idx: i32) -> Result<(), AdapterError> {
        Err(AdapterError::NotSupported("camera_focus"))
    }

    async fn replay_seek_session_time(&self, _session_time_ms: i32) -> Result<(), AdapterError> {
        Err(AdapterError::NotSupported("replay_seek_session_time"))
    }
}
