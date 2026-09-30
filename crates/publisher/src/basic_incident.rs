// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use crate::car_registry::CarRegistry;
use crate::race_event::RaceEvent;

/// Sliding window over which a car's EMA speed is tracked. A hard event needs
/// the drop measured against the peak inside this window, not the previous
/// frame — EMA smoothing means a real crash unfolds over many frames and a
/// frame-over-frame threshold almost never trips.
const HARD_DROP_WINDOW_S: f32 = 1.0;
/// Minimum peak-to-current speed loss for a `speed_drop` alert.
const HARD_DROP_MIN_MPS: f32 = 10.0;
/// Current speed must also fall to at most this fraction of the window peak.
const HARD_DROP_RATIO: f32 = 0.5;
/// Once a car goes off track at speed, a qualifying speed loss may alert for
/// this long after the transition frame.
const OFF_TRACK_ARM_S: f32 = 1.0;
/// Hard events stay visible to the incident-cluster corroboration logic for
/// this long.
const HARD_EVENT_RETENTION_S: f32 = 3.0;
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

/// A single hard incident signal — one emitted alert. The incident-cluster
/// detector corroborates traffic slowdowns against the recent history of
/// these so a pack of cars merely catching slower traffic does not read as
/// an incident.
#[derive(Clone, Debug)]
pub struct HardEvent {
    pub car_idx: u8,
    pub session_time: f32,
    pub session_tick: i64,
    pub lap_dist_pct: f32,
    pub reason: String,
}

pub struct BasicIncidentDetector {
    last_snapshot: HashMap<u8, CarSnapshot>,
    active_alerts: HashSet<u8>,
    last_player_incident_count: Option<i32>,
    /// Per-car EMA speed samples over the last `HARD_DROP_WINDOW_S` seconds,
    /// `(session_time, speed_ema_mps)` oldest first.
    speed_history: HashMap<u8, VecDeque<(f32, f32)>>,
    /// Cars armed by an on→off-track surface transition; value is the session
    /// time at which the arm expires.
    off_track_armed_until: HashMap<u8, f32>,
    recent_hard_events: VecDeque<HardEvent>,
    last_session_time: Option<f32>,
}

impl BasicIncidentDetector {
    pub fn new() -> Self {
        Self {
            last_snapshot: HashMap::new(),
            active_alerts: HashSet::new(),
            last_player_incident_count: None,
            speed_history: HashMap::new(),
            off_track_armed_until: HashMap::new(),
            recent_hard_events: VecDeque::new(),
            last_session_time: None,
        }
    }

    /// Recent hard incident signals, newest last. Pruned to
    /// `HARD_EVENT_RETENTION_S` on every update.
    pub fn recent_hard_events(&self) -> &VecDeque<HardEvent> {
        &self.recent_hard_events
    }

    #[cfg(test)]
    pub(crate) fn push_hard_event_for_test(
        &mut self,
        car_idx: u8,
        session_time: f32,
        session_tick: i64,
        lap_dist_pct: f32,
        reason: &str,
    ) {
        self.push_hard_event(car_idx, session_time, session_tick, lap_dist_pct, reason);
    }

    fn push_hard_event(
        &mut self,
        car_idx: u8,
        session_time: f32,
        session_tick: i64,
        lap_dist_pct: f32,
        reason: &str,
    ) {
        self.recent_hard_events.push_back(HardEvent {
            car_idx,
            session_time,
            session_tick,
            lap_dist_pct,
            reason: reason.to_owned(),
        });
    }

    pub fn update(
        &mut self,
        registry: &CarRegistry,
        lap: u8,
        session_time: f32,
        session_tick: i64,
        player_car_idx: u8,
        player_incident_count: i32,
    ) -> Vec<RaceEvent> {
        // A clock restart (session transition, replay seek) invalidates every
        // windowed signal at once.
        if self
            .last_session_time
            .is_some_and(|prev| session_time < prev)
        {
            self.speed_history.clear();
            self.off_track_armed_until.clear();
            self.recent_hard_events.clear();
        }
        self.last_session_time = Some(session_time);
        while self
            .recent_hard_events
            .front()
            .is_some_and(|ev| session_time - ev.session_time > HARD_EVENT_RETENTION_S)
        {
            self.recent_hard_events.pop_front();
        }
        self.off_track_armed_until
            .retain(|_, until| session_time <= *until);

        let mut events = Vec::new();
        let mut player_alert_emitted = false;
        let player_prev_snapshot = self.last_snapshot.get(&player_car_idx).copied();

        for car in registry.active_cars() {
            let snapshot = CarSnapshot {
                track_surface: car.track_surface,
                on_pit_road: car.on_pit_road,
                speed_ema_mps: car.speed_ema_mps,
            };

            // Speed history: cars out of world or on pit road produce no usable
            // signal, so their window is reset rather than polluted.
            let history = self.speed_history.entry(car.car_idx).or_default();
            if snapshot.track_surface <= TRACK_SURFACE_NOT_IN_WORLD || snapshot.on_pit_road {
                history.clear();
            } else {
                if history.back().is_some_and(|(t, _)| session_time < *t) {
                    history.clear();
                }
                history.push_back((session_time, snapshot.speed_ema_mps));
                while history
                    .front()
                    .is_some_and(|(t, _)| session_time - *t > HARD_DROP_WINDOW_S)
                {
                    history.pop_front();
                }
            }
            let peak = history
                .iter()
                .map(|(_, speed)| *speed)
                .fold(snapshot.speed_ema_mps, f32::max);

            if let Some(prev) = self.last_snapshot.get(&car.car_idx).copied() {
                // Cars not in world (garage, tow) on either side of the
                // transition carry no incident signal.
                let in_world = snapshot.track_surface > TRACK_SURFACE_NOT_IN_WORLD
                    && prev.track_surface > TRACK_SURFACE_NOT_IN_WORLD;
                let cur = snapshot.speed_ema_mps;
                let speed_drop_mps = (peak - cur).max(0.0);
                let severity = speed_drop_mps / peak.max(1.0);
                let not_pit_transition = !snapshot.on_pit_road && !prev.on_pit_road;
                let was_moving = peak >= MIN_PREV_SPEED_MPS;
                // A hard collapse measured against the recent peak, so an EMA
                // that unwinds over many frames still qualifies.
                let severe_drop = in_world
                    && not_pit_transition
                    && was_moving
                    && speed_drop_mps >= HARD_DROP_MIN_MPS
                    && cur <= HARD_DROP_RATIO * peak;

                // A transition onto the off-track surface while moving arms the
                // car: the speed loss may take up to OFF_TRACK_ARM_S to show.
                if in_world
                    && was_moving
                    && snapshot.track_surface == TRACK_SURFACE_OFF_TRACK
                    && prev.track_surface > TRACK_SURFACE_OFF_TRACK
                {
                    self.off_track_armed_until
                        .insert(car.car_idx, session_time + OFF_TRACK_ARM_S);
                }
                let went_off_track = in_world
                    && not_pit_transition
                    && self
                        .off_track_armed_until
                        .get(&car.car_idx)
                        .is_some_and(|until| session_time <= *until)
                    && severity >= MIN_SURFACE_SEVERITY;

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
                        previous_speed_mps: peak,
                        current_speed_mps: cur,
                        speed_drop_mps,
                        severity,
                        severity_normalized: severity.clamp(0.0, 1.0),
                        incident_count_delta: None,
                        reason: reason.clone(),
                    });
                    self.push_hard_event(
                        car.car_idx,
                        session_time,
                        session_tick,
                        car.lap_dist_pct,
                        &reason,
                    );
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
                    self.push_hard_event(
                        player_car_idx,
                        session_time,
                        session_tick,
                        car.lap_dist_pct,
                        "incident_count_increase",
                    );
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

    /// Drive the detector at 60 Hz: `speeds` steps the car's EMA one tick
    /// per entry starting at `start_time`.
    fn drive(
        detector: &mut BasicIncidentDetector,
        registry: &mut CarRegistry,
        car_idx: u8,
        speeds: &[f32],
        start_time: f32,
        start_tick: i64,
    ) -> Vec<Vec<RaceEvent>> {
        speeds
            .iter()
            .enumerate()
            .map(|(i, &speed)| {
                registry.get_mut(car_idx).unwrap().speed_ema_mps = speed;
                let tick = start_tick + i as i64;
                detector.update(registry, 4, start_time + i as f32 / 60.0, tick, 0, 0)
            })
            .collect()
    }

    #[test]
    fn gradual_braking_does_not_alert() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 50.0), 0);
        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 100.0, 0, 0, 0);

        // 50 -> 28 m/s over 1.5 s (90 frames): a braking zone, not a crash.
        let speeds: Vec<f32> = (1..=90)
            .map(|i| 50.0 - (50.0 - 28.0) * i as f32 / 90.0)
            .collect();
        let emitted = drive(
            &mut detector,
            &mut registry,
            9,
            &speeds,
            100.0 + 1.0 / 60.0,
            1,
        );
        assert!(
            emitted.iter().flatten().count() == 0,
            "gradual braking must not alert"
        );
    }

    #[test]
    fn crash_emits_one_speed_drop_alert_and_one_hard_event() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 45.0), 0);
        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 100.0, 0, 0, 0);

        // 45 -> 10 m/s over ~0.5 s (30 frames), then hold at 10.
        let mut speeds: Vec<f32> = (1..=30)
            .map(|i| 45.0 - (45.0 - 10.0) * i as f32 / 30.0)
            .collect();
        speeds.extend([10.0; 30]);
        let emitted = drive(
            &mut detector,
            &mut registry,
            9,
            &speeds,
            100.0 + 1.0 / 60.0,
            1,
        );
        let alerts: Vec<&RaceEvent> = emitted.iter().flatten().collect();
        assert_eq!(alerts.len(), 1, "edge-triggered: exactly one alert");
        assert!(matches!(
            alerts[0],
            RaceEvent::IncidentAlert {
                car_idx: 9,
                reason,
                ..
            } if reason == "speed_drop"
        ));
        let hard: Vec<&HardEvent> = detector.recent_hard_events().iter().collect();
        assert_eq!(hard.len(), 1);
        assert_eq!(hard[0].car_idx, 9);
        assert!(hard[0].session_time > 100.0);
        assert!(
            hard[0].session_time < 101.0,
            "crash detected inside the window"
        );
    }

    #[test]
    fn off_track_with_delayed_speed_loss_alerts_within_arm_window() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 60.0), 0);
        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 100.0, 0, 0, 0);

        // Go off track still near full speed, then bleed off 15% within 1 s.
        registry.get_mut(9).unwrap().track_surface = 0;
        let mut speeds = vec![59.0_f32];
        speeds.extend((1..=30).map(|i| 59.0 - (59.0 - 51.0) * i as f32 / 30.0));
        let emitted = drive(
            &mut detector,
            &mut registry,
            9,
            &speeds,
            100.0 + 1.0 / 60.0,
            1,
        );
        assert!(
            emitted.iter().flatten().any(|event| matches!(
                event,
                RaceEvent::IncidentAlert {
                    car_idx: 9,
                    reason,
                    ..
                } if reason == "surface_drop"
            )),
            "off-track excursion losing 15% within the arm window must alert"
        );
    }

    #[test]
    fn off_track_below_severity_threshold_never_alerts() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 60.0), 0);
        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 100.0, 0, 0, 0);

        // Off track losing <10%, held there for 2 s — past the arm window.
        registry.get_mut(9).unwrap().track_surface = 0;
        let emitted = drive(
            &mut detector,
            &mut registry,
            9,
            &[55.0; 120],
            100.0 + 1.0 / 60.0,
            1,
        );
        assert!(emitted.iter().flatten().count() == 0);
    }

    #[test]
    fn pit_road_transitions_do_not_alert() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 60.0), 0);
        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 100.0, 0, 0, 0);

        // Pit entry with a hard slowdown: speed collapse on pit road is a pit
        // stop, not an incident.
        registry.get_mut(9).unwrap().on_pit_road = true;
        let emitted = drive(
            &mut detector,
            &mut registry,
            9,
            &[15.0; 30],
            100.0 + 1.0 / 60.0,
            1,
        );
        assert!(emitted.iter().flatten().count() == 0);
    }

    #[test]
    fn hard_events_are_pruned_after_retention_window() {
        let mut registry = CarRegistry::new();
        registry.insert(car(9, 3, false, 45.0), 0);
        let mut detector = BasicIncidentDetector::new();
        let _ = detector.update(&registry, 4, 100.0, 0, 0, 0);

        let mut speeds: Vec<f32> = (1..=30)
            .map(|i| 45.0 - (45.0 - 10.0) * i as f32 / 30.0)
            .collect();
        speeds.extend([10.0; 10]);
        let emitted = drive(
            &mut detector,
            &mut registry,
            9,
            &speeds,
            100.0 + 1.0 / 60.0,
            1,
        );
        assert!(emitted.iter().flatten().count() == 1);
        assert_eq!(detector.recent_hard_events().len(), 1);

        // 3.1 s later the event is gone.
        let _ = detector.update(&registry, 4, 104.0, 240, 0, 0);
        assert!(detector.recent_hard_events().is_empty());
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
