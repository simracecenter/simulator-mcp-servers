// SPDX-License-Identifier: GPL-3.0-or-later

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod anchor_sampler;
pub mod basic_incident;
pub mod battle_pairs;
pub mod battle_state;
pub mod braking_profile;
pub mod car_registry;
pub mod compression_zone;
pub mod config;
pub mod controls;
pub mod controls_input;
pub mod delivery;
pub mod engine;
pub mod fuel_projection;
pub mod gap_finder;
pub mod headless;
pub mod horizon;
pub mod incident_cluster;
#[cfg(target_os = "windows")]
mod irsdk;
#[cfg(target_os = "windows")]
pub mod sim_bridge {
    pub use crate::irsdk::*;
}
pub mod lap_timer;
pub mod lifecycle;
pub mod lift_coast;
pub mod micro_sector;
pub mod outbox;
pub mod pipeline;
pub mod publisher_event;
pub mod publisher_status;
pub mod race_event;
pub mod regression_store;
pub mod replay;
pub mod session_info;
pub mod session_lifecycle;
pub mod telemetry_frame;
pub mod tire_degradation;
pub mod tls_pin;
pub mod traffic_intercept;
pub mod transport;
pub mod vulnerability;
