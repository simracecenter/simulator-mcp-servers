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
//!   (camera focus, read + verified write).
//! - **Not confirmed / no known endpoint**: session/track identity (`GET
//!   /rest/sessions` only exposes rule settings, not track/session name),
//!   weather, pit info, weather/pit commands, replay seeking. These return
//!   [`AdapterError::NotYetImplemented`] rather than being guessed at.
//! - The REST API's port (6397) is hardcoded below — **not confirmed
//!   stable/configurable across LMU installs or versions**; see the ADR's
//!   Amendment open follow-ups.

use async_trait::async_trait;
use serde::Deserialize;

use super::{
    AdapterError, HwControlCommand, LmuAdapter, PitInfoState, RelativeEntry, Relatives, Roster,
    RosterEntry, SessionData, SessionOverview, Standings, StandingsEntry, WeatherControl,
    WeatherState, WeekendInfo,
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
    in_garage_stall: bool,
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

    fn not_yet_implemented(name: &'static str) -> AdapterError {
        AdapterError::NotYetImplemented(name)
    }
}

#[async_trait]
impl LmuAdapter for SdkAdapter {
    async fn get_session_overview(&self) -> SessionOverview {
        match self.fetch_standings().await {
            Ok(entries) => {
                let is_in_car = entries
                    .iter()
                    .find(|entry| entry.player)
                    .map(|entry| !entry.in_garage_stall)
                    .unwrap_or(false);
                SessionOverview {
                    connected: true,
                    // Unconfirmed via REST — no endpoint found exposing
                    // replay-mode state yet; see ADR 0002 Amendment.
                    is_replay: false,
                    is_in_car,
                    // Unconfirmed via REST — `/rest/sessions` only exposes
                    // rule settings (`SESSSET_*`), not track/session
                    // identity. Not guessed at; see ADR 0002 Amendment.
                    session_name: "unknown (not yet mapped over LMU's REST API)".to_string(),
                    track_name: "unknown (not yet mapped over LMU's REST API)".to_string(),
                }
            }
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
        Err(Self::not_yet_implemented("get_session_data"))
    }

    async fn get_weekend_info(&self) -> Result<WeekendInfo, AdapterError> {
        Err(Self::not_yet_implemented("get_weekend_info"))
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
        Err(Self::not_yet_implemented("get_weather"))
    }

    async fn get_pit_info(&self) -> Result<PitInfoState, AdapterError> {
        Err(Self::not_yet_implemented("get_pit_info"))
    }

    async fn get_camera_focus(&self) -> Result<i32, AdapterError> {
        self.get_json("/rest/watch/focus").await
    }

    async fn pit_menu_command(&self, _control: HwControlCommand) -> Result<(), AdapterError> {
        Err(Self::not_yet_implemented("pit_menu_command"))
    }

    async fn set_weather(&self, _weather: WeatherControl) -> Result<(), AdapterError> {
        Err(Self::not_yet_implemented("set_weather"))
    }

    /// Confirmed live 2026-07-13: `PUT /rest/watch/focus/{slotId}` switches
    /// focus, verified by polling [`Self::get_camera_focus`] (see
    /// `crates/lmu-mcp/src/handler.rs`'s use of `mcp_core::verify::verify_loop`).
    async fn camera_focus(&self, car_idx: i32) -> Result<(), AdapterError> {
        self.put(&format!("/rest/watch/focus/{car_idx}")).await
    }

    async fn replay_seek_session_time(&self, _session_time_ms: i32) -> Result<(), AdapterError> {
        Err(AdapterError::NotSupported("replay_seek_session_time"))
    }
}
