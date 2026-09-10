// SPDX-License-Identifier: GPL-3.0-or-later

use crate::car_registry::CarRegistry;
use crate::race_event::RaceEvent;
use crate::traffic_intercept::predict_intercept;

pub struct CompressionZoneDetector;

impl CompressionZoneDetector {
    pub fn new() -> Self {
        Self
    }

    pub fn detect(
        &self,
        registry: &CarRegistry,
        n_anchors: usize,
        track_length_m: f32,
        lap: u8,
        session_time: f32,
    ) -> Vec<RaceEvent> {
        if registry.class_count() <= 1 {
            return Vec::new();
        }
        let cars: Vec<_> = registry.active_cars().collect();
        let mut events = Vec::new();
        for attacker in &cars {
            for defender in &cars {
                if attacker.car_idx == defender.car_idx
                    || attacker.car_class_id != defender.car_class_id
                    || attacker.current_position <= defender.current_position
                    || attacker.current_position - defender.current_position > 1
                {
                    continue;
                }
                let mut traffic = Vec::new();
                let mut first: Option<(u8, f32, u8)> = None;
                for car in &cars {
                    if car.car_class_id == attacker.car_class_id {
                        continue;
                    }
                    let p1 = predict_intercept(attacker, car, n_anchors, track_length_m);
                    let p2 = predict_intercept(defender, car, n_anchors, track_length_m);
                    let best = [p1, p2].into_iter().flatten().min_by(|a, b| {
                        a.time_to_intercept_s
                            .partial_cmp(&b.time_to_intercept_s)
                            .unwrap()
                    });
                    if let Some(prediction) = best.filter(|p| p.time_to_intercept_s < 30.0) {
                        traffic.push(car.car_idx);
                        if first.is_none_or(|(_, t, _)| prediction.time_to_intercept_s < t) {
                            first = Some((
                                car.car_idx,
                                prediction.time_to_intercept_s,
                                prediction.intercept_bucket,
                            ));
                        }
                    }
                }
                if traffic.len() >= 3 {
                    let first = first.expect("traffic present");
                    events.push(RaceEvent::TrafficCompressionZone {
                        lap,
                        session_time,
                        battle_attacker_idx: attacker.car_idx,
                        battle_defender_idx: defender.car_idx,
                        window_start_pct: attacker.lap_dist_pct.min(defender.lap_dist_pct),
                        window_end_pct: attacker.lap_dist_pct.max(defender.lap_dist_pct),
                        traffic_car_idxs: traffic.clone(),
                        compression_score: traffic.len() as u8,
                        first_intercept_car_idx: first.0,
                        first_intercept_time_s: first.1,
                        first_intercept_bucket: first.2,
                    });
                }
            }
        }
        events
    }
}

impl Default for CompressionZoneDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor_sampler::AnchorSampler;
    use crate::car_registry::{CarRegistry, CarState};

    fn car(car_idx: u8, class_id: u32, pos: u8, lap: i32, ldp: f32, speed: f32) -> CarState {
        CarState {
            car_idx,
            car_number: car_idx.to_string(),
            driver_name: car_idx.to_string(),
            car_class_id: class_id,
            current_position: pos,
            current_lap: lap,
            lap_dist_pct: ldp,
            on_pit_road: false,
            track_surface: 0,
            last_lap_time_s: 0.0,
            best_lap_time_s: 0.0,
            speed_ema_mps: speed,
            sampler: AnchorSampler::new(20),
            opponent_history: Vec::new(),
        }
    }

    #[test]
    fn detects_multiclass_compression_zone() {
        let mut registry = CarRegistry::new();
        registry.insert(car(1, 10, 2, 10, 0.10, 72.0), 0);
        registry.insert(car(2, 10, 1, 10, 0.11, 70.0), 0);
        for offset in 0..5u8 {
            registry.insert(
                car(
                    10 + offset,
                    20,
                    20 + offset,
                    9,
                    0.18 + offset as f32 * 0.015,
                    40.0,
                ),
                0,
            );
        }
        let events = CompressionZoneDetector::new().detect(&registry, 20, 5000.0, 10, 600.0);
        assert!(events.iter().any(|e| matches!(
            e,
            RaceEvent::TrafficCompressionZone {
                compression_score: 5,
                ..
            }
        )));
    }
}
