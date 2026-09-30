// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet, VecDeque};

use crate::basic_incident::HardEvent;
use crate::car_registry::{CarRegistry, CarState};
use crate::race_event::{IncidentHardEvent, IncidentParticipant, RaceEvent};

const MIN_CLUSTER_CARS: usize = 3;
/// Distinct cars with recent hard events that corroborate a cluster on their
/// own, even when fewer than `MIN_CLUSTER_CARS` cars read as slowed.
const MIN_HARD_EVENT_CARS: usize = 2;
/// A hard event corroborates a cluster emitted within this many seconds of the
/// event.
const CORROBORATION_WINDOW_S: f32 = 3.0;
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
    /// Number of slowdown episodes (≥3 slowed cars in a bucket) that produced
    /// no cluster because no recent hard event corroborated them.
    pub uncorroborated_slowdowns: u64,
    next_incident_id: u32,
    /// Buckets currently inside an uncorroborated ≥3-slowed episode — counted
    /// and logged once, on episode start, until the slowdown clears.
    uncorroborated_buckets: HashSet<u8>,
}

impl IncidentClusterDetector {
    pub fn new() -> Self {
        Self {
            speed_baseline: HashMap::new(),
            baseline_samples: HashMap::new(),
            active_clusters: HashMap::new(),
            full_course_caution: false,
            laps_observed: 0,
            uncorroborated_slowdowns: 0,
            next_incident_id: 1,
            uncorroborated_buckets: HashSet::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        registry: &CarRegistry,
        n_anchors: usize,
        lap: u8,
        session_time: f32,
        session_tick: i64,
        is_full_caution: bool,
        hard_events: &VecDeque<HardEvent>,
    ) -> Vec<RaceEvent> {
        self.full_course_caution = is_full_caution;
        self.laps_observed = self.laps_observed.max(lap as u32);
        let anchor_count = n_anchors.max(1);
        let bucket_of = |lap_dist_pct: f32| {
            ((lap_dist_pct * anchor_count as f32) as usize % anchor_count) as u8
        };
        let mut slowed_by_bucket: HashMap<u8, Vec<&CarState>> = HashMap::new();

        for car in registry
            .recently_observed_cars(session_tick, MAX_CAR_AGE_TICKS)
            .filter(|car| !car.on_pit_road && car.track_surface >= 0)
        {
            let bucket = bucket_of(car.lap_dist_pct);
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

        // Hard events corroborate the bucket they happened in and the one
        // before it (an incident's debris field spreads downstream).
        let mut related_by_bucket: HashMap<u8, Vec<&HardEvent>> = HashMap::new();
        for ev in hard_events {
            let age = session_time - ev.session_time;
            if !(0.0..=CORROBORATION_WINDOW_S).contains(&age) {
                continue;
            }
            let ev_bucket = bucket_of(ev.lap_dist_pct);
            for bucket in [
                ev_bucket,
                (ev_bucket + anchor_count as u8 - 1) % anchor_count as u8,
            ] {
                related_by_bucket.entry(bucket).or_default().push(ev);
            }
        }

        let mut observed_clusters = HashSet::new();
        let mut events = Vec::new();
        let mut buckets: HashSet<u8> = slowed_by_bucket.keys().copied().collect();
        buckets.extend(related_by_bucket.keys().copied());
        let mut buckets: Vec<u8> = buckets.into_iter().collect();
        buckets.sort_unstable();

        for bucket in buckets {
            let slowed = slowed_by_bucket
                .get(&bucket)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let related = related_by_bucket
                .get(&bucket)
                .map(Vec::as_slice)
                .unwrap_or(&[]);
            let mut hard_cars: Vec<u8> = related
                .iter()
                .map(|ev| ev.car_idx)
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            hard_cars.sort_unstable();
            // A cluster of hard events alone is raised on the bucket they
            // happened in — otherwise the same pair of events would fire a
            // second cluster one bucket upstream (they are also related
            // there).
            let own_hard_cars: HashSet<u8> = related
                .iter()
                .filter(|ev| bucket_of(ev.lap_dist_pct) == bucket)
                .map(|ev| ev.car_idx)
                .collect();
            let qualifies = (slowed.len() >= MIN_CLUSTER_CARS && !related.is_empty())
                || own_hard_cars.len() >= MIN_HARD_EVENT_CARS;

            if let Some(active) = self.active_clusters.get_mut(&bucket) {
                if qualifies || slowed.len() >= MIN_CLUSTER_CARS {
                    // Still alive — including while its hard event has aged
                    // out but the slowdown persists — so it resolves once
                    // instead of re-firing.
                    let mut car_idxs: Vec<u8> = slowed
                        .iter()
                        .map(|car| car.car_idx)
                        .chain(hard_cars.iter().copied())
                        .collect::<HashSet<_>>()
                        .into_iter()
                        .collect();
                    car_idxs.sort_unstable();
                    active.lap = lap;
                    active.car_idxs = car_idxs;
                    active.missing_cadences = 0;
                    observed_clusters.insert(bucket);
                }
                continue;
            }

            if !qualifies {
                if !is_full_caution
                    && slowed.len() >= MIN_CLUSTER_CARS
                    && self.uncorroborated_buckets.insert(bucket)
                {
                    self.uncorroborated_slowdowns = self.uncorroborated_slowdowns.saturating_add(1);
                    crate::log_info!(
                        "[incident_cluster] bucket {bucket}: {} slowed cars without a corroborating hard event — no cluster (total {})",
                        slowed.len(),
                        self.uncorroborated_slowdowns,
                    );
                }
                continue;
            }
            observed_clusters.insert(bucket);
            if is_full_caution {
                continue;
            }

            // Earliest related hard event is the incident's onset and names
            // the trigger car.
            let earliest = related
                .iter()
                .min_by(|a, b| a.session_time.total_cmp(&b.session_time))
                .expect("a qualifying bucket always has a related hard event");

            let mut car_idxs: Vec<u8> = slowed
                .iter()
                .map(|car| car.car_idx)
                .chain(hard_cars.iter().copied())
                .collect::<HashSet<_>>()
                .into_iter()
                .collect();
            car_idxs.sort_unstable();
            let participants: Vec<IncidentParticipant> = car_idxs
                .iter()
                .filter_map(|idx| registry.get(*idx).map(participant))
                .collect();

            let incident_id = self.next_incident_id;
            self.next_incident_id = self.next_incident_id.saturating_add(1);
            let severity = car_idxs.len() as f32;
            let mut severity_normalized =
                ((car_idxs.len().saturating_sub(MIN_CLUSTER_CARS - 1)) as f32 / 6.0).min(1.0);
            if own_hard_cars.len() >= MIN_HARD_EVENT_CARS {
                severity_normalized = severity_normalized.max(0.5);
            }
            let corroboration = if slowed.len() >= MIN_CLUSTER_CARS {
                Some("slowed_and_hard_event".to_owned())
            } else {
                Some("multiple_hard_events".to_owned())
            };
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
                session_time: earliest.session_time,
                session_tick: earliest.session_tick,
                incident_id,
                bucket,
                lap_dist_pct_from: bucket as f32 / anchor_count as f32,
                lap_dist_pct_to: (bucket as f32 + 1.0) / anchor_count as f32,
                car_idxs,
                participants,
                severity,
                severity_normalized,
                primary_car_idx: Some(earliest.car_idx),
                trigger_car_idx: Some(earliest.car_idx),
                incident_type: Some("Incident".to_owned()),
                detected_session_time: session_time,
                hard_events: related
                    .iter()
                    .map(|ev| IncidentHardEvent {
                        car_idx: ev.car_idx,
                        session_time: ev.session_time,
                        lap_dist_pct: ev.lap_dist_pct,
                        reason: ev.reason.clone(),
                    })
                    .collect(),
                corroboration,
            });
        }

        // An episode ends when the bucket no longer has ≥3 slowed cars.
        let slowed_buckets: HashSet<u8> = slowed_by_bucket
            .iter()
            .filter(|(_, cars)| cars.len() >= MIN_CLUSTER_CARS)
            .map(|(bucket, _)| *bucket)
            .collect();
        self.uncorroborated_buckets
            .retain(|bucket| slowed_buckets.contains(bucket));

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

    const N: usize = 20;
    const CLUSTER_BUCKET: u8 = 18;

    fn car(car_idx: u8, bucket: u8, speed: f32) -> CarState {
        CarState {
            car_idx,
            car_number: car_idx.to_string(),
            driver_name: car_idx.to_string(),
            car_class_id: 1,
            current_position: car_idx + 1,
            current_lap: 5,
            lap_dist_pct: bucket as f32 / N as f32 + 0.001,
            on_pit_road: false,
            track_surface: 3,
            last_lap_time_s: 0.0,
            best_lap_time_s: 0.0,
            speed_ema_mps: speed,
            sampler: AnchorSampler::new(N),
            opponent_history: Vec::new(),
        }
    }

    fn registry_with_cluster(speed: f32, tick: i64) -> CarRegistry {
        let mut registry = CarRegistry::new();
        for idx in 1..=3 {
            registry.insert(car(idx, CLUSTER_BUCKET, speed), tick);
        }
        registry
    }

    /// A hard event on `car_idx` at `session_time`, located in `bucket`.
    fn hard(car_idx: u8, session_time: f32, bucket: u8) -> HardEvent {
        HardEvent {
            car_idx,
            session_time,
            session_tick: 0,
            lap_dist_pct: bucket as f32 / N as f32 + 0.001,
            reason: "speed_drop".to_owned(),
        }
    }

    fn none() -> VecDeque<HardEvent> {
        VecDeque::new()
    }

    fn deque(events: Vec<HardEvent>) -> VecDeque<HardEvent> {
        events.into_iter().collect()
    }

    /// Build the baseline (3 cadences at `speed`), then slow all cluster cars
    /// to `slow_speed`.
    fn slowed_registry() -> (CarRegistry, IncidentClusterDetector) {
        let mut registry = registry_with_cluster(60.0, 1);
        let mut detector = IncidentClusterDetector::new();
        for tick in 1..=3 {
            detector.update(&registry, N, 1, tick as f32, tick, false, &none());
        }
        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 10.0;
        }
        (registry, detector)
    }

    #[test]
    fn cluster_detects_three_slow_cars_after_baseline() {
        let (registry, mut detector) = slowed_registry();
        let events = detector.update(
            &registry,
            N,
            1,
            4.0,
            4,
            false,
            &deque(vec![hard(1, 3.9, CLUSTER_BUCKET)]),
        );
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
        let (mut registry, mut detector) = slowed_registry();
        let hard_events = deque(vec![hard(1, 3.9, CLUSTER_BUCKET)]);
        let opened = detector.update(&registry, N, 1, 4.0, 4, false, &hard_events);
        // The hard event has aged out, but the slowdown keeps the cluster
        // alive without re-emitting.
        let duplicate = detector.update(&registry, N, 1, 8.0, 5, false, &none());
        assert_eq!(opened.len(), 1);
        assert!(duplicate.is_empty());

        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 60.0;
        }
        assert!(detector
            .update(&registry, N, 1, 9.0, 6, false, &none())
            .is_empty());
        let resolved = detector.update(&registry, N, 1, 10.0, 7, false, &none());
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
        let (mut registry, mut detector) = slowed_registry();
        // Two of the three slowed cars are excluded (pit / not in world), so
        // even a corroborating hard event cannot lift the bucket to a cluster
        // unless two hard-event cars exist.
        registry.get_mut(1).unwrap().on_pit_road = true;
        registry.get_mut(2).unwrap().track_surface = -1;
        let events = detector.update(
            &registry,
            N,
            1,
            4.0,
            4,
            false,
            &deque(vec![hard(3, 3.9, CLUSTER_BUCKET)]),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn three_slowed_cars_without_hard_event_emit_nothing_and_count_once() {
        let (registry, mut detector) = slowed_registry();
        for i in 0..3 {
            let events = detector.update(&registry, N, 1, 4.0 + i as f32, 4 + i, false, &none());
            assert!(events.is_empty(), "uncorroborated slowdown emits nothing");
        }
        assert_eq!(detector.uncorroborated_slowdowns, 1);
    }

    #[test]
    fn uncorroborated_episode_counts_again_after_slowdown_clears() {
        let (mut registry, mut detector) = slowed_registry();
        detector.update(&registry, N, 1, 4.0, 4, false, &none());
        assert_eq!(detector.uncorroborated_slowdowns, 1);

        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 60.0;
        }
        detector.update(&registry, N, 1, 5.0, 5, false, &none());
        for idx in 1..=3 {
            registry.get_mut(idx).unwrap().speed_ema_mps = 10.0;
        }
        detector.update(&registry, N, 1, 6.0, 6, false, &none());
        assert_eq!(detector.uncorroborated_slowdowns, 2);
    }

    #[test]
    fn corroborated_cluster_uses_hard_event_onset_and_car() {
        let (registry, mut detector) = slowed_registry();
        let events = detector.update(
            &registry,
            N,
            1,
            4.0,
            40,
            false,
            &deque(vec![hard(7, 3.5, CLUSTER_BUCKET)]),
        );
        let [RaceEvent::IncidentCluster {
            session_time,
            detected_session_time,
            primary_car_idx,
            trigger_car_idx,
            corroboration,
            hard_events,
            ..
        }] = events.as_slice()
        else {
            panic!("expected exactly one cluster, got {events:?}");
        };
        assert_eq!(*session_time, 3.5);
        assert_eq!(*detected_session_time, 4.0);
        assert_eq!(*primary_car_idx, Some(7));
        assert_eq!(*trigger_car_idx, Some(7));
        assert_eq!(corroboration.as_deref(), Some("slowed_and_hard_event"));
        assert_eq!(hard_events.len(), 1);
        assert_eq!(hard_events[0].car_idx, 7);
    }

    #[test]
    fn hard_event_in_next_bucket_corroborates_but_not_two_away() {
        // Event one bucket downstream (19) corroborates bucket 18.
        let (registry, mut detector) = slowed_registry();
        let events = detector.update(
            &registry,
            N,
            1,
            4.0,
            4,
            false,
            &deque(vec![hard(9, 3.9, CLUSTER_BUCKET + 1)]),
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, RaceEvent::IncidentCluster { bucket: 18, .. })));

        // Events upstream (17) or two downstream (0 via wrap) do not.
        let (registry, mut detector) = slowed_registry();
        let events = detector.update(
            &registry,
            N,
            1,
            4.0,
            4,
            false,
            &deque(vec![
                hard(9, 3.9, CLUSTER_BUCKET - 1),
                hard(10, 3.9, CLUSTER_BUCKET + 2),
            ]),
        );
        assert!(events.is_empty());
        assert_eq!(detector.uncorroborated_slowdowns, 1);
    }

    #[test]
    fn stale_hard_event_does_not_corroborate() {
        let (registry, mut detector) = slowed_registry();
        let events = detector.update(
            &registry,
            N,
            1,
            4.0,
            4,
            false,
            &deque(vec![hard(1, 0.5, CLUSTER_BUCKET)]),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn two_hard_event_cars_emit_cluster_without_slowdown() {
        let mut registry = CarRegistry::new();
        registry.insert(car(4, CLUSTER_BUCKET, 60.0), 1);
        registry.insert(car(5, CLUSTER_BUCKET, 60.0), 1);
        let mut detector = IncidentClusterDetector::new();
        let events = detector.update(
            &registry,
            N,
            1,
            4.0,
            4,
            false,
            &deque(vec![
                hard(4, 3.8, CLUSTER_BUCKET),
                hard(5, 3.9, CLUSTER_BUCKET),
            ]),
        );
        let [RaceEvent::IncidentCluster {
            corroboration,
            severity_normalized,
            car_idxs,
            primary_car_idx,
            ..
        }] = events.as_slice()
        else {
            panic!("expected exactly one cluster, got {events:?}");
        };
        assert_eq!(corroboration.as_deref(), Some("multiple_hard_events"));
        assert!(*severity_normalized >= 0.5);
        assert_eq!(car_idxs.as_slice(), &[4, 5]);
        assert_eq!(*primary_car_idx, Some(4));
    }

    #[test]
    fn several_hard_events_produce_one_cluster_listing_all() {
        let (registry, mut detector) = slowed_registry();
        let events = detector.update(
            &registry,
            N,
            1,
            4.0,
            4,
            false,
            &deque(vec![
                hard(1, 3.5, CLUSTER_BUCKET),
                hard(8, 3.8, CLUSTER_BUCKET),
                hard(9, 3.9, CLUSTER_BUCKET + 1),
            ]),
        );
        let [RaceEvent::IncidentCluster {
            hard_events,
            car_idxs,
            session_time,
            ..
        }] = events.as_slice()
        else {
            panic!("expected a single cluster, got {events:?}");
        };
        assert_eq!(hard_events.len(), 3);
        assert_eq!(*session_time, 3.5, "onset is the earliest hard event");
        for idx in [1, 8, 9] {
            assert!(car_idxs.contains(&idx));
        }
    }
}
