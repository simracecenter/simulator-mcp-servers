// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};

use crate::car_registry::CarRegistry;
use crate::race_event::RaceEvent;
use crate::regression_store::CarSlopeInfo;

pub struct HorizonDetector {
    active_pairs: HashSet<(u8, u8)>,
    slope_history: HashMap<(u8, u8), Vec<f32>>,
}

impl HorizonDetector {
    pub fn new() -> Self {
        Self {
            active_pairs: HashSet::new(),
            slope_history: HashMap::new(),
        }
    }

    pub fn detect(
        &mut self,
        registry: &CarRegistry,
        regression_per_car: &HashMap<u8, CarSlopeInfo>,
        lap: u8,
        session_time: f32,
        lap_time_s: f32,
    ) -> Vec<RaceEvent> {
        let mut events = Vec::new();
        let active_idxs: HashSet<u8> = registry.active_cars().map(|c| c.car_idx).collect();
        self.clear_stale_pairs(&active_idxs);

        let cars: Vec<_> = registry.active_cars().collect();
        for attacker in &cars {
            for defender in &cars {
                if attacker.car_idx == defender.car_idx {
                    continue;
                }
                if attacker.current_position <= defender.current_position {
                    continue;
                }
                if attacker.current_position - defender.current_position > 3 {
                    continue;
                }
                let mut gap_pct = defender.lap_dist_pct - attacker.lap_dist_pct;
                if gap_pct <= 0.0 {
                    gap_pct += 1.0;
                }
                let current_gap_s = gap_pct * lap_time_s;
                if current_gap_s > 60.0 {
                    continue;
                }
                let slope = regression_per_car
                    .get(&attacker.car_idx)
                    .map(|s| s.median)
                    .unwrap_or(0.0);
                self.slope_history
                    .entry((attacker.car_idx, defender.car_idx))
                    .or_default()
                    .push(slope);
                let pair = (attacker.car_idx, defender.car_idx);
                if slope < -0.05 && current_gap_s > 5.0 {
                    if self.active_pairs.insert(pair) {
                        let estimated_laps_to_contact =
                            (current_gap_s / slope.abs()).ceil().max(1.0) as u16;
                        events.push(RaceEvent::HorizonClosing {
                            lap,
                            session_time,
                            attacker_car_idx: attacker.car_idx,
                            defender_car_idx: defender.car_idx,
                            attacker_position: attacker.current_position,
                            defender_position: defender.current_position,
                            current_gap_s,
                            closing_rate_sec_per_lap: slope.abs(),
                            estimated_laps_to_contact,
                        });
                    }
                } else if slope > -0.02 && self.active_pairs.remove(&pair) {
                    events.push(RaceEvent::HorizonClosingResolved {
                        lap,
                        session_time,
                        attacker_car_idx: attacker.car_idx,
                        defender_car_idx: defender.car_idx,
                    });
                }
            }
        }
        events
    }

    pub fn clear_stale_pairs(&mut self, active_car_idxs: &HashSet<u8>) {
        self.active_pairs
            .retain(|(a, b)| active_car_idxs.contains(a) && active_car_idxs.contains(b));
        self.slope_history
            .retain(|(a, b), _| active_car_idxs.contains(a) && active_car_idxs.contains(b));
    }
}

impl Default for HorizonDetector {
    fn default() -> Self {
        Self::new()
    }
}
