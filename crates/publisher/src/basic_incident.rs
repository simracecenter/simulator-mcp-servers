// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::collections::HashSet;

use crate::car_registry::CarRegistry;
use crate::race_event::RaceEvent;

const SPEED_DROP_THRESHOLD_MPS: f32 = 8.0;
const SPEED_RATIO_THRESHOLD: f32 = 0.80;
/// Minimum severity (speed drop as a fraction of prior speed) for a
/// surface-transition alert. Filters routine off-track excursions where the
/// car barely slows.
const MIN_SURFACE_SEVERITY: f32 = 0.1;
/// A car must have been moving at least this fast for a surface transition to
/// count as an incident. Filters garage/pit-stall jitter.
const MIN_PREV_SPEED_MPS: f32 = 5.0;
/// iRacing `irsdk_TrkLoc`: cars not in world (garage, tow) report -1.
const TRACK_SURFACE_NOT_IN_WORLD: i32 = -1;
/// iRacing `irsdk_TrkLoc`: 0 means off track.
const TRACK_SURFACE_OFF_TRACK: i32 = 0;
/// A car slower than this counts as stationary: incident points gained while
/// crawling (pit stall, grid, tow) describe no on-track moment.
const STATIONARY_SPEED_MPS: f32 = 5.0;
/// Incident points at which an `incident_count_increase` alert is treated as
/// maximally severe. iRacing awards 0x/1x/2x/4x per infraction.
const MAX_INCIDENT_POINTS: f32 = 4.0;

#[derive(Clone, Copy)]
struct CarSnapshot {
    track_surface: i32,
    on_pit_road: bool,
    speed_ema_mps: f32,
}

pub struct BasicIncidentDetector {
    last_snapshot: HashMap<u8, CarSnapshot>,
    active_alerts: HashSet<u8>,
    last_player_incident_count: Option<i32>,
}

impl BasicIncidentDetector {
    pub fn new() -> Self {
        Self {
            last_snapshot: HashMap::new(),
            active_alerts: HashSet::new(),
            last_player_incident_count: None,
        }
    }

    pub fn update(
        &mut self,
        registry: &CarRegistry,
        lap: u8,
        session_time: f32,
        _session_tick: i64,
        player_car_idx: u8,
        player_incident_count: i32,
    ) -> Vec<RaceEvent> {
        let mut events = Vec::new();
        let mut player_alert_emitted = false;
        let player_prev_snapshot = self.last_snapshot.get(&player_car_idx).copied();

        for car in registry.active_cars() {
            let snapshot = CarSnapshot {
                track_surface: car.track_surface,
                on_pit_road: car.on_pit_road,
                speed_ema_mps: car.speed_ema_mps,
            };

            if let Some(prev) = self.last_snapshot.get(&car.car_idx).copied() {
                // Cars not in world (garage, tow) on either side of the
                // transition carry no incident signal.
                let in_world = snapshot.track_surface > TRACK_SURFACE_NOT_IN_WORLD
                    && prev.track_surface > TRACK_SURFACE_NOT_IN_WORLD;
                let speed_drop_mps = (prev.speed_ema_mps - snapshot.speed_ema_mps).max(0.0);
                let speed_ratio = if prev.speed_ema_mps > 1.0 {
                    snapshot.speed_ema_mps / prev.speed_ema_mps
                } else {
                    1.0
                };
                let severity = speed_drop_mps / prev.speed_ema_mps.max(1.0);
                let was_moving = prev.speed_ema_mps >= MIN_PREV_SPEED_MPS;
                let severe_drop = in_world
                    && was_moving
                    && speed_drop_mps >= SPEED_DROP_THRESHOLD_MPS
                    && speed_ratio <= SPEED_RATIO_THRESHOLD;
                // Only a transition onto the off-track surface counts; surface
                // recovery (off-track back onto the track) is not an incident.
                let went_off_track = in_world
                    && was_moving
                    && snapshot.track_surface == TRACK_SURFACE_OFF_TRACK
                    && prev.track_surface > TRACK_SURFACE_OFF_TRACK
                    && severity >= MIN_SURFACE_SEVERITY;
                let not_pit_transition = !snapshot.on_pit_road && !prev.on_pit_road;

                // Emit credible incident signatures only:
                // - off-track excursions with a real speed loss
                // - severe speed collapses
                // Keep edge-triggering so a single sustained condition does not spam.
                let incident_condition = not_pit_transition && (went_off_track || severe_drop);
                if !incident_condition {
                    self.active_alerts.remove(&car.car_idx);
                }

                if incident_condition && !self.active_alerts.contains(&car.car_idx) {
                    let reason = if went_off_track && severe_drop {
                        "surface_change_and_speed_drop"
                    } else if severe_drop {
                        "speed_drop"
                    } else {
                        "surface_drop"
                    }
                    .to_owned();

                    events.push(RaceEvent::IncidentAlert {
                        lap,
                        session_time,
                        car_idx: car.car_idx,
                        driver_incident_count: (car.car_idx == player_car_idx)
                            .then_some(player_incident_count),
                        previous_track_surface: prev.track_surface,
                        current_track_surface: snapshot.track_surface,
                        previous_speed_mps: prev.speed_ema_mps,
                        current_speed_mps: snapshot.speed_ema_mps,
                        speed_drop_mps,
                        severity,
                        severity_normalized: severity.clamp(0.0, 1.0),
                        incident_count_delta: None,
                        reason,
                    });
                    if car.car_idx == player_car_idx {
                        player_alert_emitted = true;
                    }
                    self.active_alerts.insert(car.car_idx);
                }
            }

            self.last_snapshot.insert(car.car_idx, snapshot);
        }

        let previous_count = self
            .last_player_incident_count
            .unwrap_or(player_incident_count);
        if player_incident_count > previous_count && !player_alert_emitted {
            if let Some(car) = registry.get(player_car_idx) {
                let prev = player_prev_snapshot.unwrap_or(CarSnapshot {
                    track_surface: car.track_surface,
                    on_pit_road: car.on_pit_road,
                    speed_ema_mps: car.speed_ema_mps,
                });
                let speed_drop_mps = (prev.speed_ema_mps - car.speed_ema_mps).max(0.0);
                let points = (player_incident_count - previous_count) as f32;

                // Points accrued in the pit lane, out of the world, or while
                // crawling are bookkeeping, not a broadcastable moment.
                let in_world = car.track_surface > TRACK_SURFACE_NOT_IN_WORLD
                    && prev.track_surface > TRACK_SURFACE_NOT_IN_WORLD;
                let in_pits = car.on_pit_road || prev.on_pit_road;
                let was_moving = prev.speed_ema_mps >= STATIONARY_SPEED_MPS
                    || car.speed_ema_mps >= STATIONARY_SPEED_MPS;

                if in_world && !in_pits && was_moving {
                    events.push(RaceEvent::IncidentAlert {
                        lap,
                        session_time,
                        car_idx: player_car_idx,
                        driver_incident_count: Some(player_incident_count),
                        previous_track_surface: prev.track_surface,
                        current_track_surface: car.track_surface,
                        previous_speed_mps: prev.speed_ema_mps,
                        current_speed_mps: car.speed_ema_mps,
                        speed_drop_mps,
                        severity: points,
                        severity_normalized: (points / MAX_INCIDENT_POINTS).clamp(0.0, 1.0),
                        incident_count_delta: Some(player_incident_count - previous_count),
                        reason: "incident_count_increase".to_owned(),
                    });
                }
            }
        }
        self.last_player_incident_count = Some(player_incident_count);

        events
    }
}

impl Default for BasicIncidentDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor_sampler::AnchorSampler;
    use crate::car_registry::CarState;

    fn car(car_idx: u8, track_surface: i32, on_pit_road: bool, speed_ema_mps: f32) -> CarState {
        CarState {
            car_idx,
            car_number: car_idx.to_string(),
            driver_name: car_idx.to_string(),
            car_class_id: 1,
            current_position: car_idx + 1,
            current_lap: 4,
            lap_dist_pct: 0.25,
            on_pit_road,
            track_surface,
            last_lap_time_s: 0.0,
            best_lap_time_s: 0.0,
            speed_ema_mps,
            sampler: AnchorSampler::new(10),
            opponent_history: Vec::new(),
        }
    }

    #[test]
    fn emits_basic_incident_on_surface_change_with_speed_drop() {
        let mut registry = CarRegistry::new();
        registry.insert(car(7, 3, false, 72.0), 0);

        let mut detector = BasicIncidentDetector::new();
        let first = detector.update(&registry, 4, 120.0, 60, 7, 0);
        assert!(first.is_empty());

        registry.get_mut(7).unwrap().track_surface = 1;
        registry.get_mut(7).unwrap().speed_ema_mps = 52.0;
        let events = detector.update(&registry, 4, 121.0, 1_920, 7, 6);

        assert!(events.iter().any(|event| {
            matches!(
                event,
                RaceEvent::IncidentAlert {
                    car_idx: 7,
                    driver_incident_count: Some(6),
                    ..
                }
            )
        }));

        let repeated = detector.update(&registry, 4, 121.2, 1_950, 7, 6);
        assert!(
            repeated.is_empty(),
            "duplicate alert should be suppressed while condition remains active"
        );
    }

    #[test]
    fn ignores_pit_transitions() {
        let mut registry = CarRegistry::new();
        registry.insert(car(7, 3, false, 72.0), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 7, 0);

        registry.get_mut(7).unwrap().on_pit_road = true;
        registry.get_mut(7).unwrap().track_surface = 2;
        registry.get_mut(7).unwrap().speed_ema_mps = 20.0;
        let events = detector.update(&registry, 4, 121.0, 120, 7, 0);

        assert!(events.is_empty());
    }

    #[test]
    fn ignores_surface_change_without_speed_loss() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 60.0), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 0, 0);

        // Brief off-track excursion with no meaningful slowdown.
        registry.get_mut(9).unwrap().track_surface = 0;
        registry.get_mut(9).unwrap().speed_ema_mps = 59.0;
        let events = detector.update(&registry, 4, 121.0, 120, 0, 0);
        assert!(events.is_empty(), "low-severity off-track must not alert");
    }

    #[test]
    fn ignores_surface_recovery() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 0, false, 30.0), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 0, 0);

        // Rejoining the track (0 -> 3) is not an incident.
        registry.get_mut(9).unwrap().track_surface = 3;
        let events = detector.update(&registry, 4, 121.0, 120, 0, 0);
        assert!(events.is_empty(), "surface recovery must not alert");
    }

    #[test]
    fn ignores_not_in_world_transitions() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, -1, false, 0.0), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 0, 0);

        // Car enters the world (garage -> on track).
        registry.get_mut(9).unwrap().track_surface = 3;
        registry.get_mut(9).unwrap().speed_ema_mps = 0.0001;
        let events = detector.update(&registry, 4, 121.0, 120, 0, 0);
        assert!(events.is_empty(), "world enter/exit must not alert");
    }

    #[test]
    fn emits_off_track_with_speed_loss() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 60.0), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 0, 0);

        registry.get_mut(9).unwrap().track_surface = 0;
        registry.get_mut(9).unwrap().speed_ema_mps = 50.0;
        let events = detector.update(&registry, 4, 121.0, 120, 0, 0);
        assert!(events
            .iter()
            .any(|event| matches!(event, RaceEvent::IncidentAlert { car_idx: 9, .. })));
    }

    #[test]
    fn ignores_incident_points_gained_in_the_pit_stall() {
        let mut registry = CarRegistry::new();
        // Crawling in the pit stall, as in the pit-lane noise from the capture.
        registry.insert(car(7, 2, true, 0.9), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 7, 0);

        registry.get_mut(7).unwrap().speed_ema_mps = 0.5;
        let events = detector.update(&registry, 4, 121.0, 120, 7, 2);

        assert!(
            events.is_empty(),
            "pit-stall incident points must not alert"
        );
    }

    #[test]
    fn ignores_incident_points_gained_while_stationary_on_track() {
        let mut registry = CarRegistry::new();
        registry.insert(car(7, 3, false, 0.4), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 7, 0);

        registry.get_mut(7).unwrap().speed_ema_mps = 0.2;
        let events = detector.update(&registry, 4, 121.0, 120, 7, 1);

        assert!(
            events.is_empty(),
            "stationary incident points must not alert"
        );
    }

    #[test]
    fn incident_points_on_track_normalize_severity() {
        let mut registry = CarRegistry::new();
        registry.insert(car(7, 3, false, 60.0), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 7, 0);

        // Two points gained while racing, without a surface or speed signature.
        let events = detector.update(&registry, 4, 121.0, 120, 7, 2);

        let alert = events
            .iter()
            .find(|event| matches!(event, RaceEvent::IncidentAlert { .. }))
            .expect("on-track incident points should alert");
        let RaceEvent::IncidentAlert {
            severity,
            severity_normalized,
            incident_count_delta,
            reason,
            ..
        } = alert
        else {
            unreachable!("filtered to IncidentAlert above");
        };

        assert_eq!(reason, "incident_count_increase");
        // Raw severity keeps the iRacing point delta; the normalized score is
        // always inside 0.0–1.0.
        assert_eq!(*severity, 2.0);
        assert_eq!(*incident_count_delta, Some(2));
        assert_eq!(*severity_normalized, 0.5);
    }

    #[test]
    fn surface_alerts_normalize_severity_into_zero_to_one() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 60.0), 0);

        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 120.0, 60, 0, 0);

        registry.get_mut(9).unwrap().track_surface = 0;
        registry.get_mut(9).unwrap().speed_ema_mps = 20.0;
        let events = detector.update(&registry, 4, 121.0, 120, 0, 0);

        let alert = events
            .iter()
            .find(|event| matches!(event, RaceEvent::IncidentAlert { .. }))
            .expect("off-track with speed loss should alert");
        let RaceEvent::IncidentAlert {
            severity_normalized,
            incident_count_delta,
            ..
        } = alert
        else {
            unreachable!("filtered to IncidentAlert above");
        };

        assert!(
            (0.0..=1.0).contains(severity_normalized),
            "expected 0-1, got {severity_normalized}"
        );
        assert!(incident_count_delta.is_none());
    }
}
