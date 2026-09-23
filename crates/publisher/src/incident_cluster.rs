// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet, VecDeque};

use crate::car_registry::{CarRegistry, CarState};
use crate::race_event::{IncidentParticipant, RaceEvent};

const MIN_CLUSTER_CARS: usize = 3;
const MIN_BASELINE_SAMPLES: usize = 3;
const MAX_BASELINE_SAMPLES: usize = 10;
const MIN_BASELINE_SPEED_MPS: f32 = 10.0;
const SLOW_SPEED_RATIO: f32 = 0.7;
const MAX_CAR_AGE_TICKS: i64 = 60;
const RESOLUTION_GRACE_CADENCES: u8 = 2;

#[derive(Clone, Debug)]
pub struct ActiveIncidentCluster {
    pub incident_id: u32,
    pub lap: u8,
    pub bucket: u8,
    pub car_idxs: Vec<u8>,
    pub started_session_time: f32,
    pub started_session_tick: i64,
    missing_cadences: u8,
}

pub struct IncidentClusterDetector {
    pub speed_baseline: HashMap<(u32, u8), f32>,
    pub baseline_samples: HashMap<(u32, u8), VecDeque<f32>>,
    pub active_clusters: HashMap<u8, ActiveIncidentCluster>,
    pub full_course_caution: bool,
    pub laps_observed: u32,
    next_incident_id: u32,
}

impl IncidentClusterDetector {
    pub fn new() -> Self {
        Self {
            speed_baseline: HashMap::new(),
            baseline_samples: HashMap::new(),
            active_clusters: HashMap::new(),
            full_course_caution: false,
            laps_observed: 0,
            next_incident_id: 1,
        }
    }

    pub fn update(
        &mut self,
        registry: &CarRegistry,
        n_anchors: usize,
        lap: u8,
        session_time: f32,
        session_tick: i64,
        is_full_caution: bool,
    ) -> Vec<RaceEvent> {
        self.full_course_caution = is_full_caution;
        self.laps_observed = self.laps_observed.max(lap as u32);
        let anchor_count = n_anchors.max(1);
        let mut slowed_by_bucket: HashMap<u8, Vec<&CarState>> = HashMap::new();

        for car in registry
            .recently_observed_cars(session_tick, MAX_CAR_AGE_TICKS)
            .filter(|car| !car.on_pit_road && car.track_surface >= 0)
        {
            let bucket = ((car.lap_dist_pct * anchor_count as f32) as usize % anchor_count) as u8;
            let key = (car.car_class_id, bucket);
            let sample_count = self
                .baseline_samples
                .get(&key)
                .map(VecDeque::len)
                .unwrap_or_default();
            let baseline = self.speed_baseline.get(&key).copied();
            let slowed = baseline.is_some_and(|speed| {
                sample_count >= MIN_BASELINE_SAMPLES
                    && speed >= MIN_BASELINE_SPEED_MPS
                    && car.speed_ema_mps < speed * SLOW_SPEED_RATIO
            });

            if slowed {
                slowed_by_bucket.entry(bucket).or_default().push(car);
            } else if !is_full_caution && car.speed_ema_mps >= MIN_BASELINE_SPEED_MPS {
                let samples = self.baseline_samples.entry(key).or_default();
                samples.push_back(car.speed_ema_mps);
                while samples.len() > MAX_BASELINE_SAMPLES {
                    samples.pop_front();
                }
                let mut ordered: Vec<f32> = samples.iter().copied().collect();
                ordered.sort_by(f32::total_cmp);
                self.speed_baseline.insert(key, ordered[ordered.len() / 2]);
            }
        }

        let mut observed_clusters = HashSet::new();
        let mut events = Vec::new();
        let mut buckets: Vec<u8> = slowed_by_bucket.keys().copied().collect();
        buckets.sort_unstable();

        for bucket in buckets {
            let cars = slowed_by_bucket
                .remove(&bucket)
                .expect("bucket came from slowed_by_bucket");
            if cars.len() < MIN_CLUSTER_CARS {
                continue;
            }
            observed_clusters.insert(bucket);
            let car_idxs: Vec<u8> = cars.iter().map(|car| car.car_idx).collect();
            if let Some(active) = self.active_clusters.get_mut(&bucket) {
                active.lap = lap;
                active.car_idxs = car_idxs;
                active.missing_cadences = 0;
                continue;
            }
            if is_full_caution {
                continue;
            }

            let incident_id = self.next_incident_id;
            self.next_incident_id = self.next_incident_id.saturating_add(1);
            let severity = cars.len() as f32;
            let severity_normalized =
                ((cars.len().saturating_sub(MIN_CLUSTER_CARS - 1)) as f32 / 6.0).min(1.0);
            let primary_car_idx = cars.iter().map(|car| car.car_idx).min();
            let participants = cars.iter().map(|car| participant(car)).collect();
            self.active_clusters.insert(
                bucket,
                ActiveIncidentCluster {
                    incident_id,
                    lap,
                    bucket,
                    car_idxs: car_idxs.clone(),
                    started_session_time: session_time,
                    started_session_tick: session_tick,
                    missing_cadences: 0,
                },
            );
            events.push(RaceEvent::IncidentCluster {
                lap,
                session_time,
                session_tick,
                incident_id,
                bucket,
                lap_dist_pct_from: bucket as f32 / anchor_count as f32,
                lap_dist_pct_to: (bucket as f32 + 1.0) / anchor_count as f32,
                car_idxs,
                participants,
                severity,
                severity_normalized,
                primary_car_idx,
                incident_type: Some("Incident".to_owned()),
            });
        }

        let missing_buckets: Vec<u8> = self
            .active_clusters
            .keys()
            .copied()
            .filter(|bucket| !observed_clusters.contains(bucket))
            .collect();
        for bucket in missing_buckets {
            let should_resolve = self.active_clusters.get_mut(&bucket).is_some_and(|active| {
                active.missing_cadences = active.missing_cadences.saturating_add(1);
                active.missing_cadences >= RESOLUTION_GRACE_CADENCES
            });
            if should_resolve {
                let active = self
                    .active_clusters
                    .remove(&bucket)
                    .expect("active cluster existed");
                events.push(RaceEvent::IncidentClusterResolved {
                    lap,
                    session_time,
                    session_tick,
                    incident_id: active.incident_id,
                    bucket,
                    started_session_time: active.started_session_time,
                    started_session_tick: active.started_session_tick,
                    car_idxs: active.car_idxs,
                });
            }
        }

        events
    }
}

fn participant(car: &CarState) -> IncidentParticipant {
    IncidentParticipant {
        car_idx: car.car_idx,
        lap: car.current_lap,
        lap_dist_pct: car.lap_dist_pct,
        speed_mps: car.speed_ema_mps,
        on_pit_road: car.on_pit_road,
        track_surface: car.track_surface,
        in_world: car.track_surface >= 0,
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
            track_surface: 3,
            last_lap_time_s: 0.0,
            best_lap_time_s: 0.0,
            speed_ema_mps: speed,
            sampler: AnchorSampler::new(20),
            opponent_history: Vec::new(),
        }
    }

    fn registry_with_cluster(speed: f32, tick: i64) -> CarRegistry {
        let mut registry = CarRegistry::new();
        for idx in 1..=3 {
            registry.insert(car(idx, 18, speed), tick);
        }
        registry
    }

    #[test]
    fn cluster_detects_three_slow_cars_after_baseline() {
        let mut registry = registry_with_cluster(60.0, 1);
        let mut detector = IncidentClusterDetector::new();
        for tick in 1..=3 {
            for idx in 1..=3 {
                registry.get_mut(idx).unwrap().speed_ema_mps = 60.0;
            }
            detector.update(&registry, 20, 1, tick as f32, tick, false);
        }
        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 10.0;
        }
        let events = detector.update(&registry, 20, 1, 4.0, 4, false);
        assert!(events.iter().any(|event| matches!(
            event,
            RaceEvent::IncidentCluster {
                incident_id: 1,
                car_idxs,
                participants,
                ..
            } if car_idxs.len() == 3 && participants.len() == 3
        )));
    }

    #[test]
    fn active_cluster_is_deduplicated_and_resolves_with_same_identity() {
        let mut registry = registry_with_cluster(60.0, 1);
        let mut detector = IncidentClusterDetector::new();
        for tick in 1..=3 {
            detector.update(&registry, 20, 1, tick as f32, tick, false);
        }
        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 10.0;
        }
        let opened = detector.update(&registry, 20, 1, 4.0, 4, false);
        let duplicate = detector.update(&registry, 20, 1, 5.0, 5, false);
        assert_eq!(opened.len(), 1);
        assert!(duplicate.is_empty());

        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 60.0;
        }
        assert!(detector.update(&registry, 20, 1, 6.0, 6, false).is_empty());
        let resolved = detector.update(&registry, 20, 1, 7.0, 7, false);
        assert!(matches!(
            resolved.as_slice(),
            [RaceEvent::IncidentClusterResolved {
                incident_id: 1,
                started_session_tick: 4,
                ..
            }]
        ));
    }

    #[test]
    fn ignores_pit_and_not_in_world_cars() {
        let mut registry = registry_with_cluster(60.0, 1);
        let mut detector = IncidentClusterDetector::new();
        for tick in 1..=3 {
            detector.update(&registry, 20, 1, tick as f32, tick, false);
        }
        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 10.0;
        }
        registry.get_mut(1).unwrap().on_pit_road = true;
        registry.get_mut(2).unwrap().track_surface = -1;
        assert!(detector.update(&registry, 20, 1, 4.0, 4, false).is_empty());
    }
}
