// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use crate::car_registry::{CarRegistry, CarState};
use crate::race_event::RaceEvent;

/// Physical bounds for a credible closing speed between two cars. Values
/// outside this range indicate corrupted speed estimates (teleports, garage
/// jitter) or a closure too slow to matter.
const MIN_RELATIVE_SPEED_MPS: f32 = 1.0;
const MAX_RELATIVE_SPEED_MPS: f32 = 100.0;

pub struct TrafficInterceptDetector {
    pub last_bucket: HashMap<(u8, u8), u8>,
}

pub struct InterceptPrediction {
    pub distance_m: f32,
    pub relative_speed_mps: f32,
    pub time_to_intercept_s: f32,
    pub intercept_bucket: u8,
    pub intercept_lap_dist_pct: f32,
}

impl TrafficInterceptDetector {
    pub fn new() -> Self {
        Self {
            last_bucket: HashMap::new(),
        }
    }

    pub fn detect(
        &mut self,
        registry: &CarRegistry,
        n_anchors: usize,
        track_length_m: f32,
        lap: u8,
        session_time: f32,
    ) -> Vec<RaceEvent> {
        let cars: Vec<_> = registry
            .active_cars()
            .filter(|car| !car.on_pit_road && car.track_surface >= 0)
            .collect();
        let mut events = Vec::new();
        for leader in &cars {
            for traffic in &cars {
                if leader.car_idx == traffic.car_idx {
                    continue;
                }
                let cross_class = leader.car_class_id != traffic.car_class_id;
                if !cross_class && leader.current_lap - traffic.current_lap < 1 {
                    continue;
                }
                let Some(prediction) =
                    predict_intercept(leader, traffic, n_anchors, track_length_m)
                else {
                    continue;
                };
                if prediction.time_to_intercept_s >= 30.0 {
                    continue;
                }
                let key = (leader.car_idx, traffic.car_idx);
                let previous_bucket = self.last_bucket.get(&key).copied();
                if previous_bucket.is_some_and(|prev| {
                    (prediction.intercept_bucket as i16 - prev as i16).abs() < 2
                }) {
                    continue;
                }
                self.last_bucket.insert(key, prediction.intercept_bucket);
                events.push(RaceEvent::TrafficIntercept {
                    lap,
                    session_time,
                    leader_car_idx: leader.car_idx,
                    traffic_car_idx: traffic.car_idx,
                    cross_class,
                    distance_m: prediction.distance_m,
                    relative_speed_mps: prediction.relative_speed_mps,
                    time_to_intercept_s: prediction.time_to_intercept_s,
                    intercept_bucket: prediction.intercept_bucket,
                    intercept_lap_dist_pct: prediction.intercept_lap_dist_pct,
                    predicted_intercept_session_time: session_time + prediction.time_to_intercept_s,
                });
            }
        }
        events
    }
}

pub fn predict_intercept(
    leader: &CarState,
    traffic: &CarState,
    n_anchors: usize,
    track_length_m: f32,
) -> Option<InterceptPrediction> {
    let relative_speed_mps = leader.speed_ema_mps - traffic.speed_ema_mps;
    if !(MIN_RELATIVE_SPEED_MPS..=MAX_RELATIVE_SPEED_MPS).contains(&relative_speed_mps) {
        return None;
    }
    let mut delta_pct = traffic.lap_dist_pct - leader.lap_dist_pct;
    if delta_pct <= 0.0 {
        delta_pct += 1.0;
    }
    let distance_m = delta_pct * track_length_m;
    let time_to_intercept_s = distance_m / relative_speed_mps;
    let intercept_lap_dist_pct =
        (leader.lap_dist_pct + leader.speed_ema_mps * time_to_intercept_s / track_length_m).fract();
    let intercept_bucket =
        ((intercept_lap_dist_pct * n_anchors.max(1) as f32) as usize % n_anchors.max(1)) as u8;
    Some(InterceptPrediction {
        distance_m,
        relative_speed_mps,
        time_to_intercept_s,
        intercept_bucket,
        intercept_lap_dist_pct,
    })
}

impl Default for TrafficInterceptDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor_sampler::AnchorSampler;
    use crate::car_registry::{CarRegistry, CarState};

    fn car(car_idx: u8, lap: i32, ldp: f32, speed: f32, class_id: u32) -> CarState {
        CarState {
            car_idx,
            car_number: car_idx.to_string(),
            driver_name: format!("Car {car_idx}"),
            car_class_id: class_id,
            current_position: car_idx + 1,
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
    fn intercept_within_anchor() {
        let mut registry = CarRegistry::new();
        registry.insert(car(1, 10, 0.10, 70.0, 1), 0);
        registry.insert(car(2, 9, 0.20, 40.0, 1), 0);
        let mut detector = TrafficInterceptDetector::new();
        let events = detector.detect(&registry, 20, 5000.0, 10, 600.0);
        assert!(events.iter().any(|e| matches!(
            e,
            RaceEvent::TrafficIntercept {
                leader_car_idx: 1,
                traffic_car_idx: 2,
                ..
            }
        )));
    }

    #[test]
    fn rejects_implausible_relative_speed() {
        let mut registry = CarRegistry::new();
        // Poisoned speed estimate (teleport artefact) must not predict an intercept.
        registry.insert(car(1, 10, 0.10, 90_000.0, 1), 0);
        registry.insert(car(2, 9, 0.20, 40.0, 1), 0);
        let mut detector = TrafficInterceptDetector::new();
        let events = detector.detect(&registry, 20, 5000.0, 10, 600.0);
        assert!(events.is_empty());
    }

    #[test]
    fn skips_cars_not_in_world_or_in_pit() {
        let mut registry = CarRegistry::new();
        let mut leader = car(1, 10, 0.10, 70.0, 1);
        leader.track_surface = -1;
        registry.insert(leader, 0);
        let mut traffic = car(2, 9, 0.20, 40.0, 1);
        traffic.on_pit_road = true;
        registry.insert(traffic, 0);
        let mut detector = TrafficInterceptDetector::new();
        let events = detector.detect(&registry, 20, 5000.0, 10, 600.0);
        assert!(events.is_empty());
    }
}
