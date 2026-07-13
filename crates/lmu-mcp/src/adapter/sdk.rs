// SPDX-License-Identifier: GPL-3.0-or-later
//! `SdkAdapter` — real LMU adapter, backed by LMU's local REST API
//! (`127.0.0.1:6397`), per
//! [ADR 0002's Amendment](../../../../docs/adr/0002-lmu-adapter-design.md#amendment-2026-07-13-live-verification-reveals-a-rest-api-pivot-away-from-shared-memory-only).
//!
//! Originally designed around `rF2SharedMemoryMapPlugin` shared memory (ADR
//! 0002 D1-D4); pivoted after live-testing against a running LMU instance
//! found (a) the plugin wasn't installed and LMU's real install layout has
//! no `Bin64/Plugins` folder to put it in, and (b) LMU exposes a live local
//! REST API that already covers standings/roster/relatives with richer,
//! pre-resolved data, and a confirmed, verified `camera_focus` command —
//! with zero plugin dependency. Unlike the old shared-memory path, this is a
//! plain HTTP client, so it compiles and runs identically on every target
//! (Linux CI naturally gets `NotConnected` since nothing listens on
//! `127.0.0.1:6397` there — no `#[cfg(windows)]` split needed).
//!
//! ## What's confirmed vs. not (see the ADR Amendment for full detail)
//!
//! - **Confirmed live** (2026-07-13): `GET /rest/watch/standings` (roster/
//!   standings/relatives source), `GET`/`PUT /rest/watch/focus[/{slotId}]`
//!   (camera focus, read + verified write), `GET /rest/watch/sessionInfo`
//!   (track/session identity, temps, elapsed/end time), `GET
//!   /rest/sessions/GetGameState` (game phase, pit state, replay/in-car
//!   flags, nearest weather node), `GET
//!   /rest/replay/CameraController/getCameraInfo` + `PUT
//!   /rest/watch/focus/{cameraType}/{trackSideGroup}/false` (camera
//!   type/track-side-group switching, discovered via the full OpenAPI spec
//!   at `/swagger-schema.json`).
//! - **Not confirmed / no known endpoint, or tested-but-inconclusive**:
//!   weather/pit *commands*, replay seeking (`PUT
//!   /rest/watch/replaytime/{time}` exists per the spec but live-testing
//!   showed no observable effect from the current live-monitor context —
//!   see the ADR Amendment). These return [`AdapterError::NotYetImplemented`]
//!   or [`AdapterError::NotSupported`] rather than being guessed at.
//! - The REST API's port (6397) is hardcoded below — **not confirmed
//!   stable/configurable across LMU installs or versions**; see the ADR's
//!   Amendment open follow-ups.

use async_trait::async_trait;
use serde::Deserialize;

use super::{
    AdapterError, CameraFocusState, HwControlCommand, LmuAdapter, PitInfoState, RelativeEntry,
    Relatives, Roster, RosterEntry, SessionData, SessionOverview, Standings, StandingsEntry,
    WeatherControl, WeatherState, WeekendInfo,
};

/// LMU's local REST API base URL. Loopback-only, confirmed live
/// 2026-07-13 — see this module's doc comment.
const BASE_URL: &str = "http://127.0.0.1:6397";

/// One entry of `GET /rest/watch/standings`'s response array. Field names
/// mirror the real API responses observed live; `slotID` (capital `ID`) is
/// the one field that doesn't follow the API's otherwise-consistent
/// camelCase convention, hence the explicit rename.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestCarEntry {
    #[serde(rename = "slotID")]
    slot_id: i32,
    driver_name: String,
    vehicle_name: String,
    car_class: String,
    position: i32,
    laps_completed: i32,
    /// e.g. `"SECTOR1"`/`"SECTOR2"`/`"SECTOR3"` — see [`sector_to_index`].
    sector: String,
    best_lap_time: f64,
    last_lap_time: f64,
    time_behind_leader: f64,
    laps_behind_leader: i32,
    time_behind_next: f64,
    laps_behind_next: i32,
    pitting: bool,
    /// e.g. `"FSTAT_NONE"` — see [`humanize_finish_status`].
    finish_status: String,
    player: bool,
    pitstops: i32,
    penalties: i32,
}

/// `GET /rest/watch/sessionInfo`'s response — confirmed live 2026-07-13
/// (track: Fuji Speedway, session: PRACTICE1). Only the fields this adapter
/// actually uses are modeled; the real response has more.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestSessionInfo {
    track_name: String,
    session: String,
    ambient_temp: f64,
    track_temp: f64,
    current_event_time: f64,
    end_event_time: f64,
    /// LMU reports `u32::MAX` when there's no lap limit (e.g. a timed
    /// practice session) — see [`SdkAdapter::normalize_max_laps`].
    maximum_laps: u32,
    number_of_vehicles: i32,
    /// `0.0` (clear) to `1.0` (overcast) — matches the old rF2 `mDarkCloud`
    /// concept exactly ("cloud darkness? 0.0-1.0").
    dark_cloud: f64,
}

/// `GET /rest/sessions/GetGameState`'s response — confirmed live 2026-07-13.
/// Note `PitState` and `closeestWeatherNode` (sic, real API typo) don't
/// follow the API's usual camelCase convention, hence explicit renames.
#[derive(Debug, Clone, Deserialize)]
struct RestGameState {
    #[serde(rename = "gamePhase")]
    game_phase: String,
    #[serde(rename = "PitState")]
    pit_state: String,
    #[serde(rename = "isReplayActive")]
    is_replay_active: bool,
    #[serde(rename = "inControlOfVehicle")]
    in_control_of_vehicle: bool,
    #[serde(rename = "closeestWeatherNode")]
    closeest_weather_node: RestWeatherNode,
}

#[derive(Debug, Clone, Deserialize)]
struct RestWeatherNode {
    #[serde(rename = "RainChance")]
    rain_chance: f64,
    /// Unit unconfirmed — passed through as-is by `get_weather` rather than
    /// assuming m/s (see this crate's module doc comment / ADR 0002
    /// Amendment's open follow-ups).
    #[serde(rename = "WindSpeed")]
    wind_speed: f64,
}

/// `GET /rest/replay/CameraController/getCameraInfo`'s response — confirmed
/// live 2026-07-13: `{"cameraName":"COCKPIT","currentCameraGroup":"Driving"}`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RestCameraInfo {
    camera_name: String,
    current_camera_group: String,
}

fn sector_to_index(sector: &str) -> i32 {
    match sector {
        "SECTOR1" => 1,
        "SECTOR2" => 2,
        "SECTOR3" => 3,
        _ => 0,
    }
}

fn humanize_finish_status(raw: &str) -> String {
    raw.trim_start_matches("FSTAT_").to_lowercase()
}

#[derive(Debug)]
pub struct SdkAdapter {
    client: reqwest::Client,
}

impl Default for SdkAdapter {
    fn default() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }
}

impl SdkAdapter {
    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, AdapterError> {
        let url = format!("{BASE_URL}{path}");
        let response = self.client.get(&url).send().await.map_err(|error| {
            AdapterError::NotConnected(format!(
                "GET {path} failed — is LMU running with its REST API reachable at {BASE_URL}? ({error})"
            ))
        })?;

        if !response.status().is_success() {
            return Err(AdapterError::RestApi(format!(
                "GET {path} returned {}",
                response.status()
            )));
        }

        response.json::<T>().await.map_err(|error| {
            AdapterError::RestApi(format!(
                "GET {path} returned unexpected JSON shape: {error}"
            ))
        })
    }

    async fn put(&self, path: &str) -> Result<(), AdapterError> {
        let url = format!("{BASE_URL}{path}");
        let response = self.client.put(&url).send().await.map_err(|error| {
            AdapterError::NotConnected(format!(
                "PUT {path} failed — is LMU running with its REST API reachable at {BASE_URL}? ({error})"
            ))
        })?;

        if !response.status().is_success() {
            return Err(AdapterError::RestApi(format!(
                "PUT {path} returned {}",
                response.status()
            )));
        }

        Ok(())
    }

    async fn fetch_standings(&self) -> Result<Vec<RestCarEntry>, AdapterError> {
        self.get_json("/rest/watch/standings").await
    }

    async fn fetch_session_info(&self) -> Result<RestSessionInfo, AdapterError> {
        self.get_json("/rest/watch/sessionInfo").await
    }

    async fn fetch_game_state(&self) -> Result<RestGameState, AdapterError> {
        self.get_json("/rest/sessions/GetGameState").await
    }

    /// LMU reports `u32::MAX` for "no lap limit" (confirmed live in a timed
    /// practice session) — map that to `0`, matching `StubAdapter`'s existing
    /// "no limit" convention rather than overflowing an `i32`.
    fn normalize_max_laps(maximum_laps: u32) -> i32 {
        if maximum_laps >= i32::MAX as u32 {
            0
        } else {
            maximum_laps as i32
        }
    }

    fn not_yet_implemented(name: &'static str) -> AdapterError {
        AdapterError::NotYetImplemented(name)
    }
}

#[async_trait]
impl LmuAdapter for SdkAdapter {
    async fn get_session_overview(&self) -> SessionOverview {
        match (
            self.fetch_session_info().await,
            self.fetch_game_state().await,
        ) {
            (Ok(info), Ok(game_state)) => SessionOverview {
                connected: true,
                is_replay: game_state.is_replay_active,
                is_in_car: game_state.in_control_of_vehicle,
                session_name: info.session,
                track_name: info.track_name,
            },
            _ => SessionOverview {
                connected: false,
                is_replay: false,
                is_in_car: false,
                session_name: "Disconnected".to_string(),
                track_name: "Disconnected".to_string(),
            },
        }
    }

    async fn get_session_data(&self) -> Result<SessionData, AdapterError> {
        let info = self.fetch_session_info().await?;
        let game_state = self.fetch_game_state().await?;
        Ok(SessionData {
            track_name: info.track_name,
            session_type: info.session,
            game_phase: game_state.game_phase,
            current_et_sec: info.current_event_time,
            end_et_sec: info.end_event_time,
            max_laps: Self::normalize_max_laps(info.maximum_laps),
            driver_count: info.number_of_vehicles.max(0) as usize,
        })
    }

    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError> {
        let info = self.fetch_session_info().await?;
        let game_state = self.fetch_game_state().await?;
        Ok(WeekendInfo {
            track_name: info.track_name,
            session_type: info.session,
            max_laps: Self::normalize_max_laps(info.maximum_laps),
            end_et_sec: info.end_event_time,
            ambient_temp_c: info.ambient_temp,
            track_temp_c: info.track_temp,
            raining: (game_state.closeest_weather_node.rain_chance / 100.0).clamp(0.0, 1.0),
        })
    }

    async fn get_roster(&self, _include_spectators: bool) -> Result<Roster, AdapterError> {
        let entries = self.fetch_standings().await?;
        let entries: Vec<RosterEntry> = entries
            .iter()
            .map(|entry| RosterEntry {
                id: entry.slot_id,
                driver_name: entry.driver_name.clone(),
                vehicle_name: entry.vehicle_name.clone(),
                vehicle_class: entry.car_class.clone(),
                is_player: entry.player,
            })
            .collect();
        let count = entries.len();
        Ok(Roster { entries, count })
    }

    async fn get_standings(&self, _session_num: Option<i32>) -> Result<Standings, AdapterError> {
        let entries = self.fetch_standings().await?;
        let positions: Vec<StandingsEntry> = entries
            .iter()
            .map(|entry| StandingsEntry {
                place: entry.position,
                id: entry.slot_id,
                driver_name: entry.driver_name.clone(),
                vehicle_name: entry.vehicle_name.clone(),
                laps_completed: entry.laps_completed,
                sector: sector_to_index(&entry.sector),
                best_lap_time_sec: entry.best_lap_time,
                last_lap_time_sec: entry.last_lap_time,
                time_behind_leader_sec: entry.time_behind_leader,
                laps_behind_leader: entry.laps_behind_leader,
                in_pits: entry.pitting,
                finish_status: humanize_finish_status(&entry.finish_status),
            })
            .collect();
        Ok(Standings {
            // Unconfirmed via REST — no session-type field found yet.
            session_type: "live".to_string(),
            positions,
        })
    }

    async fn get_relatives(&self) -> Result<Relatives, AdapterError> {
        let entries = self.fetch_standings().await?;
        let entries: Vec<RelativeEntry> = entries
            .iter()
            .map(|entry| RelativeEntry {
                id: entry.slot_id,
                driver_name: entry.driver_name.clone(),
                place: entry.position,
                laps_completed: entry.laps_completed,
                time_behind_next_sec: entry.time_behind_next,
                laps_behind_next: entry.laps_behind_next,
                in_pits: entry.pitting,
            })
            .collect();
        let count = entries.len();
        Ok(Relatives { entries, count })
    }

    async fn get_weather(&self) -> Result<WeatherState, AdapterError> {
        let info = self.fetch_session_info().await?;
        let game_state = self.fetch_game_state().await?;
        let node = game_state.closeest_weather_node;
        Ok(WeatherState {
            ambient_temp_c: info.ambient_temp,
            track_temp_c: info.track_temp,
            raining: (node.rain_chance / 100.0).clamp(0.0, 1.0),
            cloudiness: info.dark_cloud.clamp(0.0, 1.0),
            wind_speed_ms: node.wind_speed,
        })
    }

    async fn get_pit_info(&self) -> Result<PitInfoState, AdapterError> {
        let entries = self.fetch_standings().await?;
        let game_state = self.fetch_game_state().await?;
        let player = entries.iter().find(|entry| entry.player);
        Ok(PitInfoState {
            in_pits: player.map(|entry| entry.pitting).unwrap_or(false),
            pit_state: game_state.pit_state.to_lowercase(),
            num_pitstops: player.map(|entry| entry.pitstops).unwrap_or(0),
            num_penalties: player.map(|entry| entry.penalties).unwrap_or(0),
        })
    }

    async fn get_camera_state(&self) -> Result<CameraFocusState, AdapterError> {
        let focus_slot_id: i32 = self.get_json("/rest/watch/focus").await?;
        let camera_info: RestCameraInfo = self
            .get_json("/rest/replay/CameraController/getCameraInfo")
            .await?;
        Ok(CameraFocusState {
            focus_slot_id,
            camera_name: camera_info.camera_name,
            camera_group: camera_info.current_camera_group,
        })
    }

    async fn pit_menu_command(&self, _control: HwControlCommand) -> Result<(), AdapterError> {
        Err(Self::not_yet_implemented("pit_menu_command"))
    }

    async fn set_weather(&self, _weather: WeatherControl) -> Result<(), AdapterError> {
        Err(Self::not_yet_implemented("set_weather"))
    }

    /// Confirmed live 2026-07-13: `PUT /rest/watch/focus/{slotId}` switches
    /// car focus; if `camera_type` is given, also `PUT
    /// /rest/watch/focus/{cameraType}/{trackSideGroup}/false` switches the
    /// active camera (`track_side_group` defaults to `0`). Verified by
    /// polling [`Self::get_camera_state`] (see
    /// `crates/lmu-mcp/src/handler.rs`'s use of `mcp_core::verify::verify_loop`).
    async fn camera_focus(
        &self,
        car_idx: i32,
        camera_type: Option<i32>,
        track_side_group: Option<i32>,
    ) -> Result<(), AdapterError> {
        self.put(&format!("/rest/watch/focus/{car_idx}")).await?;
        if let Some(camera_type) = camera_type {
            let group = track_side_group.unwrap_or(0);
            self.put(&format!("/rest/watch/focus/{camera_type}/{group}/false"))
                .await?;
        }
        Ok(())
    }

    async fn replay_seek_session_time(&self, _session_time_ms: i32) -> Result<(), AdapterError> {
        Err(AdapterError::NotSupported("replay_seek_session_time"))
    }
}
