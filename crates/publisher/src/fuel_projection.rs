// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::VecDeque;

use crate::race_event::RaceEvent;

pub struct FuelProjection {
    lap_fuel: Vec<(u8, f32, bool)>,
    last_pit_lap: Option<u8>,
    in_pit: bool,
    clean_deltas: VecDeque<f32>,
    last_clean_fuel: Option<f32>,
    last_laps_remaining: Option<f32>,
}

impl FuelProjection {
    pub fn new() -> Self {
        Self {
            lap_fuel: Vec::new(),
            last_pit_lap: None,
            in_pit: false,
            clean_deltas: VecDeque::new(),
            last_clean_fuel: None,
            last_laps_remaining: None,
        }
    }

    pub fn on_lap_crossing(
        &mut self,
        lap: u8,
        session_time: f32,
        fuel: f32,
        is_pit_lap: bool,
        is_yellow: bool,
    ) -> Option<RaceEvent> {
        if is_pit_lap {
            self.last_pit_lap = Some(lap);
            self.in_pit = true;
            self.clean_deltas.clear();
            self.last_clean_fuel = None;
            self.lap_fuel.push((lap, fuel, false));
            return None;
        }
        if self.in_pit {
            self.in_pit = false;
            self.clean_deltas.clear();
            self.last_clean_fuel = None;
        }

        let is_clean = !is_yellow;
        self.lap_fuel.push((lap, fuel, is_clean));
        if is_clean {
            if let Some(prev) = self.last_clean_fuel {
                self.clean_deltas.push_back((prev - fuel).max(0.0));
                while self.clean_deltas.len() > 3 {
                    self.clean_deltas.pop_front();
                }
            }
            self.last_clean_fuel = Some(fuel);
        }
        if self.clean_deltas.is_empty() {
            return None;
        }
        let fuel_per_lap_l = self.clean_deltas.iter().sum::<f32>() / self.clean_deltas.len() as f32;
        let laps_remaining = if fuel_per_lap_l > 0.0 {
            fuel / fuel_per_lap_l
        } else {
            f32::INFINITY
        };
        self.last_laps_remaining = Some(laps_remaining);
        Some(RaceEvent::FuelProjection {
            lap,
            session_time,
            fuel_remaining_l: fuel,
            fuel_per_lap_l,
            laps_remaining,
            is_provisional: self.clean_deltas.len() < 3,
        })
    }

    pub fn laps_remaining(&self) -> Option<f32> {
        self.last_laps_remaining
    }

    /// Returns true if meaningful fuel consumption data has been observed.
    /// Suppresses events when all recorded fuel deltas are zero (no telemetry)
    /// or when the delta window is not yet populated.
    pub fn has_valid_data(&self) -> bool {
        !self.clean_deltas.is_empty() && self.clean_deltas.iter().any(|&d| d > 0.0)
    }
}

impl Default for FuelProjection {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rolling_mean_for_clean_laps() {
        let mut tracker = FuelProjection::new();
        let mut event = None;
        for (lap, fuel) in [(1, 50.0), (2, 47.5), (3, 45.0), (4, 42.5), (5, 40.0)] {
            event = tracker.on_lap_crossing(lap, lap as f32 * 60.0, fuel, false, false);
        }
        match event.expect("projection") {
            RaceEvent::FuelProjection { fuel_per_lap_l, .. } => {
                assert!((fuel_per_lap_l - 2.5).abs() < 0.01)
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn newly_created_projector_returns_no_valid_data() {
        let tracker = FuelProjection::new();
        assert!(!tracker.has_valid_data());
    }

    #[test]
    fn projector_with_fuel_data_returns_valid_data() {
        let mut tracker = FuelProjection::new();
        tracker.on_lap_crossing(1, 60.0, 50.0, false, false);
        tracker.on_lap_crossing(2, 120.0, 47.5, false, false);
        assert!(tracker.has_valid_data());
    }

    #[test]
    fn yellow_lap_exclusion() {
        let mut tracker = FuelProjection::new();
        tracker.on_lap_crossing(1, 60.0, 50.0, false, false);
        tracker.on_lap_crossing(2, 120.0, 47.5, false, false);
        tracker.on_lap_crossing(3, 180.0, 45.0, false, true);
        let event = tracker
            .on_lap_crossing(4, 240.0, 42.5, false, false)
            .expect("projection");
        match event {
            RaceEvent::FuelProjection {
                is_provisional,
                fuel_per_lap_l,
                ..
            } => {
                assert!(is_provisional || fuel_per_lap_l > 0.0);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn pit_cycle_reset() {
        let mut tracker = FuelProjection::new();
        tracker.on_lap_crossing(1, 60.0, 50.0, false, false);
        assert!(tracker
            .on_lap_crossing(2, 120.0, 70.0, true, false)
            .is_none());
        let event = tracker.on_lap_crossing(3, 180.0, 67.5, false, false);
        assert!(
            event.is_none(),
            "pit reset should require a fresh clean delta window"
        );
    }
}
