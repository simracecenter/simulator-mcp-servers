// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};

use crate::car_registry::CarRegistry;
use crate::race_event::RaceEvent;

pub struct IncidentClusterDetector {
    pub speed_baseline: HashMap<(u32, u8), f32>,
    pub baseline_samples: HashMap<(u32, u8), VecDeque<f32>>,
    pub active_clusters: HashMap<u8, (u8, Vec<u8>)>,
    pub full_course_caution: bool,
    pub laps_observed: u32,
}

impl IncidentClusterDetector {
    pub fn new() -> Self {
        Self {
            speed_baseline: HashMap::new(),
            baseline_samples: HashMap::new(),
            active_clusters: HashMap::new(),
            full_course_caution: false,
            laps_observed: 0,
        }
    }

    pub fn update(
        &mut self,
        registry: &CarRegistry,
        n_anchors: usize,
        lap: u8,
        session_time: f32,
        is_full_caution: bool,
    ) -> Vec<RaceEvent> {
        self.full_course_caution = is_full_caution;
        self.laps_observed = self.laps_observed.max(lap as u32);
        let mut slowed_by_bucket: HashMap<u8, Vec<u8>> = HashMap::new();
        for car in registry.active_cars() {
            let bucket =
                ((car.lap_dist_pct * n_anchors.max(1) as f32) as usize % n_anchors.max(1)) as u8;
            let key = (car.car_class_id, bucket);
            let baseline = self
                .speed_baseline
                .get(&key)
                .copied()
                .unwrap_or(car.speed_ema_mps.max(1.0));
            let slowed =
                car.speed_ema_mps < baseline * 0.7 || (baseline <= 1.0 && car.speed_ema_mps < 20.0);
            if slowed && !is_full_caution {
                slowed_by_bucket
                    .entry(bucket)
                    .or_default()
                    .push(car.car_idx);
            } else if !is_full_caution {
                let samples = self.baseline_samples.entry(key).or_default();
                samples.push_back(car.speed_ema_mps);
                while samples.len() > 10 {
                    samples.pop_front();
                }
                let mut ordered: Vec<f32> = samples.iter().copied().collect();
                ordered.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let median = ordered[ordered.len() / 2];
                self.speed_baseline.insert(key, median);
            }
        }

        let mut events = Vec::new();
        for (bucket, cars) in slowed_by_bucket {
            if cars.len() >= 3 {
                if let Entry::Vacant(slot) = self.active_clusters.entry(bucket) {
                    let severity = cars.len() as f32;
                    let primary_car_idx = cars.iter().copied().min();
                    slot.insert((lap, cars.clone()));
                    events.push(RaceEvent::IncidentCluster {
                        lap,
                        session_time,
                        bucket,
                        lap_dist_pct_from: bucket as f32 / n_anchors.max(1) as f32,
                        lap_dist_pct_to: (bucket as f32 + 1.0) / n_anchors.max(1) as f32,
                        car_idxs: cars,
                        severity,
                        primary_car_idx,
                        incident_type: Some("Incident".to_owned()),
                    });
                }
            } else if self.active_clusters.remove(&bucket).is_some() {
                events.push(RaceEvent::IncidentClusterResolved {
                    lap,
                    session_time,
                    bucket,
                });
            }
        }
        events
    }
}

impl Default for IncidentClusterDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor_sampler::AnchorSampler;
    use crate::car_registry::{CarRegistry, CarState};

    fn car(car_idx: u8, bucket: u8, speed: f32) -> CarState {
        CarState {
            car_idx,
            car_number: car_idx.to_string(),
            driver_name: car_idx.to_string(),
            car_class_id: 1,
            current_position: car_idx + 1,
            current_lap: 5,
            lap_dist_pct: bucket as f32 / 20.0 + 0.001,
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
    fn cluster_detects_three_slow_cars() {
        let mut registry = CarRegistry::new();
        for idx in 1..=3 {
            registry.insert(car(idx, 18, 60.0), 0);
        }
        let mut detector = IncidentClusterDetector::new();
        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 60.0;
        }
        detector.update(&registry, 20, 1, 60.0, false);
        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 10.0;
        }
        let events = detector.update(&registry, 20, 2, 120.0, false);
        assert!(events.iter().any(
            |e| matches!(e, RaceEvent::IncidentCluster { car_idxs, .. } if car_idxs.len() == 3)
        ));
    }
}
