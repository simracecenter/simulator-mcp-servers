// SPDX-License-Identifier: GPL-3.0-or-later

//! Field-wide pit entry and exit detection (ADR 0008, "Field-wide pit events").
//!
//! Every car's pit stop is tracked, not only the rig's. The tracker reads the
//! per-car surface, pit-road flag and positions straight from the
//! [`TelemetryFrame`] arrays rather than from the `CarRegistry`, because the
//! registry skips cars reporting position 0 — which is what a car sitting in
//! its pit stall reports.
//!
//! - `PIT_ENTRY` fires on the first of the pit-approach edge (surface
//!   `OnTrack` → `AproachingPits` while moving) or `on_pit_road` rising, so it
//!   lands at the pit-entry braking rather than at the pit-lane line.
//! - `PIT_EXIT` fires once `on_pit_road` has fallen and stayed false for
//!   [`EXIT_DEBOUNCE_S`].

use std::collections::HashMap;

use crate::basic_incident::{
    TRACK_SURFACE_APPROACHING_PITS, TRACK_SURFACE_IN_PIT_STALL, TRACK_SURFACE_NOT_IN_WORLD,
    TRACK_SURFACE_ON_TRACK,
};
use crate::race_event::RaceEvent;
use crate::telemetry_frame::TelemetryFrame;

/// `on_pit_road` must stay false this long before a pit exit is reported, so a
/// one-frame flicker at the pit-lane line does not split one stop in two. The
/// same hold cancels an approach the car drives back out of.
pub const EXIT_DEBOUNCE_S: f32 = 0.5;
/// A car must be at least this fast on the approach edge for it to count as a
/// pit entry rather than surface jitter.
const APPROACH_MIN_SPEED_MPS: f32 = 5.0;
/// Below this speed in its stall a car counts as stopped.
const STALL_STOP_SPEED_MPS: f32 = 1.0;
/// Smoothing for the tracker's own lap-distance speed estimate.
const SPEED_EMA_ALPHA: f32 = 0.3;
/// iRacing reports at most 64 car slots.
const MAX_CARS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Phase {
    OnTrack,
    /// Entry fired at the approach edge; pit road not reached yet.
    Approach,
    /// On pit road (or in the stall).
    PitRoad,
}

#[derive(Clone, Debug)]
struct CarPit {
    phase: Phase,
    /// When `PIT_ENTRY` fired (approach or pit road).
    entry_time: Option<f32>,
    /// When `on_pit_road` rose; `None` when it was already set on first sight.
    pit_road_since: Option<f32>,
    /// Time spent stopped in the stall this stop.
    stop_s: f32,
    /// Whether this stop's entry was seen. `false` for a car first seen on
    /// pit road (out of the garage, or already there at connect).
    saw_entry: bool,
    /// Start of the current debounce hold: `on_pit_road` false while on pit
    /// road, or back on track while on the approach.
    pending_since: Option<f32>,
    last_surface: Option<i32>,
    last_time: f32,
    last_lap_dist_pct: Option<f32>,
    speed_mps: Option<f32>,
}

impl CarPit {
    fn new(time: f32) -> Self {
        Self {
            phase: Phase::OnTrack,
            entry_time: None,
            pit_road_since: None,
            stop_s: 0.0,
            saw_entry: false,
            pending_since: None,
            last_surface: None,
            last_time: time,
            last_lap_dist_pct: None,
            speed_mps: None,
        }
    }
}

/// Per-car pit state machine for the whole field.
#[derive(Default)]
pub struct PitTracker {
    cars: HashMap<u8, CarPit>,
    last_session_time: Option<f32>,
}

/// What the tracker reads for one car on one frame.
struct CarReading {
    on_pit_road: bool,
    surface: Option<i32>,
    lap_dist_pct: Option<f32>,
    position: u8,
    class_position: Option<u8>,
}

impl PitTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance every car by one frame and return the pit events it produced.
    ///
    /// `track_length_m` scales the tracker's own speed estimate; when it is
    /// unknown (0) the approach edge is trusted without a speed check and all
    /// stall time counts as stopped.
    pub fn update(&mut self, frame: &TelemetryFrame, track_length_m: f32) -> Vec<RaceEvent> {
        let t = frame.session_time;
        if self.last_session_time.is_some_and(|prev| t < prev) {
            self.cars.clear();
        }
        self.last_session_time = Some(t);

        let slots = frame
            .car_idx_on_pit_road
            .len()
            .max(frame.car_idx_track_surface.len())
            .max(frame.player_car_idx as usize + 1)
            .min(MAX_CARS);
        let mut events = Vec::new();
        for i in 0..slots {
            let car_idx = i as u8;
            let reading = Self::read(frame, car_idx);
            if let Some(event) = self.step(frame, car_idx, reading, track_length_m) {
                events.push(event);
            }
        }
        events
    }

    fn read(frame: &TelemetryFrame, car_idx: u8) -> CarReading {
        let i = car_idx as usize;
        let is_rig = car_idx == frame.player_car_idx;
        CarReading {
            // The rig's own flags are the authoritative source for its car.
            on_pit_road: if is_rig {
                frame.on_pit_road
            } else {
                frame.car_idx_on_pit_road.get(i).copied().unwrap_or(false)
            },
            surface: frame.car_idx_track_surface.get(i).copied(),
            lap_dist_pct: frame
                .car_idx_lap_dist_pct
                .get(i)
                .copied()
                .filter(|ldp| *ldp >= 0.0),
            position: if is_rig {
                frame.player_car_position
            } else {
                frame.car_idx_position.get(i).copied().unwrap_or(0)
            },
            class_position: if is_rig {
                frame.player_class_position()
            } else {
                frame.class_position_of(car_idx)
            },
        }
    }

    fn step(
        &mut self,
        frame: &TelemetryFrame,
        car_idx: u8,
        r: CarReading,
        track_length_m: f32,
    ) -> Option<RaceEvent> {
        let t = frame.session_time;
        // Out of the world (garage, tow): whatever stop was in progress is
        // over without an event; the next appearance starts fresh.
        if r.surface == Some(TRACK_SURFACE_NOT_IN_WORLD) {
            self.cars.remove(&car_idx);
            return None;
        }

        let first_sight = !self.cars.contains_key(&car_idx);
        let car = self.cars.entry(car_idx).or_insert_with(|| CarPit::new(t));
        let dt = (t - car.last_time).max(0.0);

        // Own speed estimate from lap distance; the registry has none for a
        // position-0 car.
        if let (Some(ldp), Some(prev_ldp)) = (r.lap_dist_pct, car.last_lap_dist_pct) {
            if dt > 0.0 && track_length_m > 0.0 {
                let mut delta = ldp - prev_ldp;
                if delta < -0.5 {
                    delta += 1.0;
                }
                let raw = (delta.max(0.0) * track_length_m) / dt;
                car.speed_mps = Some(match car.speed_mps {
                    Some(prev) => SPEED_EMA_ALPHA * raw + (1.0 - SPEED_EMA_ALPHA) * prev,
                    None => raw,
                });
            }
        }
        let prev_surface = car.last_surface;
        car.last_surface = r.surface;
        car.last_time = t;
        car.last_lap_dist_pct = r.lap_dist_pct;

        if first_sight {
            // Already on pit road when first seen: out of the garage or a
            // stop under way at connect. No entry was seen.
            if r.on_pit_road {
                car.phase = Phase::PitRoad;
                car.saw_entry = false;
            }
            return None;
        }

        let entry_event = |detected_by: &str, car_idx: u8| RaceEvent::PitEntry {
            lap: frame.lap,
            session_time: t,
            car_idx,
            player_car_idx: (car_idx == frame.player_car_idx).then_some(car_idx),
            position: r.position,
            class_position: r.class_position,
            detected_by: Some(detected_by.to_owned()),
        };

        match car.phase {
            Phase::OnTrack => {
                if r.on_pit_road {
                    car.phase = Phase::PitRoad;
                    car.entry_time = Some(t);
                    car.pit_road_since = Some(t);
                    car.saw_entry = true;
                    car.stop_s = 0.0;
                    car.pending_since = None;
                    return Some(entry_event("pit_road", car_idx));
                }
                let approach_edge = prev_surface == Some(TRACK_SURFACE_ON_TRACK)
                    && r.surface == Some(TRACK_SURFACE_APPROACHING_PITS);
                let moving = car
                    .speed_mps
                    .is_none_or(|speed| speed >= APPROACH_MIN_SPEED_MPS);
                if approach_edge && moving {
                    car.phase = Phase::Approach;
                    car.entry_time = Some(t);
                    car.pit_road_since = None;
                    car.saw_entry = true;
                    car.stop_s = 0.0;
                    car.pending_since = None;
                    return Some(entry_event("approach", car_idx));
                }
                None
            }
            Phase::Approach => {
                if r.on_pit_road {
                    car.phase = Phase::PitRoad;
                    car.pit_road_since = Some(t);
                    car.pending_since = None;
                    return None;
                }
                let still_approaching = matches!(
                    r.surface,
                    Some(TRACK_SURFACE_APPROACHING_PITS) | Some(TRACK_SURFACE_IN_PIT_STALL)
                );
                if still_approaching {
                    car.pending_since = None;
                } else {
                    let since = *car.pending_since.get_or_insert(t);
                    if t - since >= EXIT_DEBOUNCE_S {
                        // Aborted approach: back on track without reaching
                        // pit road. The entry already went out; nothing to
                        // retract.
                        *car = CarPit {
                            last_surface: car.last_surface,
                            last_time: car.last_time,
                            last_lap_dist_pct: car.last_lap_dist_pct,
                            speed_mps: car.speed_mps,
                            ..CarPit::new(t)
                        };
                    }
                }
                None
            }
            Phase::PitRoad => {
                let stopped = car
                    .speed_mps
                    .is_none_or(|speed| speed < STALL_STOP_SPEED_MPS);
                if r.on_pit_road {
                    car.pending_since = None;
                    if r.surface == Some(TRACK_SURFACE_IN_PIT_STALL) && stopped {
                        car.stop_s += dt;
                    }
                    return None;
                }
                let left_at = *car.pending_since.get_or_insert(t);
                if t - left_at < EXIT_DEBOUNCE_S {
                    return None;
                }
                let pit_lane_s = car.pit_road_since.map(|since| (left_at - since).max(0.0));
                let event = RaceEvent::PitExit {
                    lap: frame.lap,
                    session_time: t,
                    car_idx,
                    player_car_idx: (car_idx == frame.player_car_idx).then_some(car_idx),
                    position: r.position,
                    class_position: r.class_position,
                    stop_s: Some(car.stop_s),
                    pit_lane_s,
                    drive_through: Some(car.saw_entry && car.stop_s == 0.0),
                    from_garage: Some(!car.saw_entry),
                };
                *car = CarPit {
                    last_surface: car.last_surface,
                    last_time: car.last_time,
                    last_lap_dist_pct: car.last_lap_dist_pct,
                    speed_mps: car.speed_mps,
                    ..CarPit::new(t)
                };
                Some(event)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RIG: u8 = 0;
    const CAR: u8 = 5;
    const TRACK_M: f32 = 4000.0;
    const DT: f32 = 1.0 / 60.0;

    fn frame(t: f32) -> TelemetryFrame {
        TelemetryFrame {
            lap: 3,
            session_time: t,
            lap_dist_pct: 0.5,
            player_car_idx: RIG,
            player_car_position: 4,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct: vec![0.5; 8],
            car_idx_position: vec![4, 0, 0, 0, 0, 7, 0, 0],
            car_idx_on_pit_road: vec![false; 8],
            car_idx_track_surface: vec![3; 8],
            car_idx_session_flags: Vec::new(),
            lap_last_lap_time: 0.0,
            session_info_update: 0,
            session_tick: (t * 60.0) as i64,
            session_state: 4,
            session_num: 0,
            session_time_remain: None,
            session_laps_remain: None,
            player_incident_count: 0,
            player_driver_incident_count: None,
            player_team_incident_count: None,
            player_car_class_position: 0,
            car_idx_class_position: vec![2, 0, 0, 0, 0, 3, 0, 0],
            car_idx_lap_completed: Vec::new(),
            lf_temp_m: 0.0,
            rf_temp_m: 0.0,
            lr_temp_m: 0.0,
            rr_temp_m: 0.0,
            fuel_level: 0.0,
            throttle: 0.0,
            brake: 0.0,
            speed: 0.0,
        }
    }

    /// A scripted car: each step sets surface, pit flag and speed, and the
    /// lap distance advances by that speed.
    struct Sim {
        tracker: PitTracker,
        t: f32,
        ldp: f32,
        events: Vec<RaceEvent>,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                tracker: PitTracker::new(),
                t: 100.0,
                ldp: 0.80,
                events: Vec::new(),
            }
        }

        /// Run `seconds` of frames with car `CAR` in the given state.
        fn run(&mut self, seconds: f32, surface: i32, on_pit_road: bool, speed_mps: f32) {
            let frames = (seconds / DT).round() as usize;
            for _ in 0..frames {
                self.t += DT;
                self.ldp = (self.ldp + speed_mps * DT / TRACK_M) % 1.0;
                let mut f = frame(self.t);
                f.car_idx_track_surface[CAR as usize] = surface;
                f.car_idx_on_pit_road[CAR as usize] = on_pit_road;
                f.car_idx_lap_dist_pct[CAR as usize] = self.ldp;
                if surface == TRACK_SURFACE_IN_PIT_STALL {
                    f.car_idx_position[CAR as usize] = 0;
                    f.car_idx_class_position[CAR as usize] = 0;
                }
                self.events.extend(self.tracker.update(&f, TRACK_M));
            }
        }

        fn entries(&self) -> Vec<&RaceEvent> {
            self.events
                .iter()
                .filter(|e| matches!(e, RaceEvent::PitEntry { .. }))
                .collect()
        }

        fn exits(&self) -> Vec<&RaceEvent> {
            self.events
                .iter()
                .filter(|e| matches!(e, RaceEvent::PitExit { .. }))
                .collect()
        }

        /// Racing, approach, pit lane, a `stop` in the stall, rejoin.
        fn full_stop(&mut self, stop: f32) {
            self.run(1.0, 3, false, 60.0);
            self.run(2.0, 2, false, 30.0);
            self.run(3.0, 2, true, 20.0);
            if stop > 0.0 {
                self.run(stop, 1, true, 0.0);
            }
            self.run(3.0, 2, true, 20.0);
            self.run(2.0, 3, false, 40.0);
        }
    }

    fn entry_detected_by(event: &RaceEvent) -> Option<&str> {
        match event {
            RaceEvent::PitEntry { detected_by, .. } => detected_by.as_deref(),
            _ => None,
        }
    }

    #[test]
    fn approach_edge_fires_entry_before_pit_road() {
        let mut sim = Sim::new();
        sim.run(1.0, 3, false, 60.0);
        sim.run(0.5, 2, false, 40.0);
        let entries = sim.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entry_detected_by(entries[0]), Some("approach"));
        let RaceEvent::PitEntry {
            car_idx,
            player_car_idx,
            position,
            class_position,
            ..
        } = entries[0]
        else {
            unreachable!()
        };
        assert_eq!(*car_idx, CAR);
        assert_eq!(*player_car_idx, None, "not the rig's car");
        assert_eq!(*position, 7);
        assert_eq!(*class_position, Some(3));

        // Reaching pit road does not fire a second entry.
        sim.run(1.0, 2, true, 20.0);
        assert_eq!(sim.entries().len(), 1);
    }

    #[test]
    fn pit_road_rising_without_approach_fires_entry() {
        let mut sim = Sim::new();
        sim.run(1.0, 3, false, 60.0);
        sim.run(0.5, 3, true, 20.0);
        let entries = sim.entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entry_detected_by(entries[0]), Some("pit_road"));
    }

    #[test]
    fn stall_stop_is_timed_and_exit_carries_the_stop() {
        let mut sim = Sim::new();
        sim.full_stop(10.0);
        let exits = sim.exits();
        assert_eq!(exits.len(), 1);
        let RaceEvent::PitExit {
            car_idx,
            stop_s,
            pit_lane_s,
            drive_through,
            from_garage,
            position,
            ..
        } = exits[0]
        else {
            unreachable!()
        };
        assert_eq!(*car_idx, CAR);
        let stop = stop_s.unwrap();
        assert!((9.0..=10.1).contains(&stop), "stop_s {stop}");
        let lane = pit_lane_s.unwrap();
        assert!((15.5..=16.1).contains(&lane), "pit_lane_s {lane}");
        assert_eq!(*drive_through, Some(false));
        assert_eq!(*from_garage, Some(false));
        assert_eq!(*position, 7);
    }

    #[test]
    fn exit_waits_for_the_debounce() {
        let mut sim = Sim::new();
        sim.run(1.0, 3, false, 60.0);
        sim.run(2.0, 2, true, 20.0);
        sim.run(0.3, 3, false, 30.0);
        assert!(sim.exits().is_empty(), "inside the debounce hold");
        sim.run(0.3, 3, false, 30.0);
        assert_eq!(sim.exits().len(), 1);
    }

    #[test]
    fn drive_through_has_no_stop() {
        let mut sim = Sim::new();
        sim.full_stop(0.0);
        let exits = sim.exits();
        assert_eq!(exits.len(), 1);
        assert!(matches!(
            exits[0],
            RaceEvent::PitExit {
                drive_through: Some(true),
                stop_s: Some(s),
                ..
            } if *s == 0.0
        ));
    }

    #[test]
    fn exit_from_the_garage_is_flagged() {
        let mut sim = Sim::new();
        // Not in world, then dropped into the stall on pit road.
        sim.run(0.5, -1, false, 0.0);
        sim.run(5.0, 1, true, 0.0);
        sim.run(3.0, 2, true, 20.0);
        sim.run(1.0, 3, false, 40.0);
        assert!(sim.entries().is_empty(), "no entry was seen");
        let exits = sim.exits();
        assert_eq!(exits.len(), 1);
        assert!(matches!(
            exits[0],
            RaceEvent::PitExit {
                from_garage: Some(true),
                drive_through: Some(false),
                pit_lane_s: None,
                ..
            }
        ));
    }

    #[test]
    fn a_pit_road_flicker_is_one_stop() {
        let mut sim = Sim::new();
        sim.run(1.0, 3, false, 60.0);
        sim.run(2.0, 2, true, 20.0);
        sim.run(0.1, 2, false, 20.0);
        sim.run(2.0, 2, true, 20.0);
        sim.run(1.0, 3, false, 40.0);
        assert_eq!(sim.entries().len(), 1);
        assert_eq!(sim.exits().len(), 1);
    }

    #[test]
    fn aborted_approach_emits_nothing_more() {
        let mut sim = Sim::new();
        sim.run(1.0, 3, false, 60.0);
        sim.run(0.5, 2, false, 50.0);
        sim.run(2.0, 3, false, 60.0);
        assert_eq!(sim.entries().len(), 1);
        assert!(sim.exits().is_empty());

        // The next real stop is detected afresh.
        sim.full_stop(5.0);
        assert_eq!(sim.entries().len(), 2);
        assert_eq!(sim.exits().len(), 1);
    }

    #[test]
    fn slow_approach_edge_is_ignored() {
        let mut sim = Sim::new();
        sim.run(1.0, 3, false, 2.0);
        sim.run(0.5, 2, false, 2.0);
        assert!(sim.entries().is_empty());
    }

    #[test]
    fn clock_restart_clears_state() {
        let mut sim = Sim::new();
        sim.run(1.0, 3, false, 60.0);
        sim.run(1.0, 2, true, 20.0);
        assert_eq!(sim.entries().len(), 1);

        // Session restart: the clock goes back and the car is on track.
        sim.t = 10.0;
        sim.run(2.0, 3, false, 40.0);
        assert!(sim.exits().is_empty(), "no exit for a stop the reset cut");
    }

    #[test]
    fn not_in_world_clears_state_without_events() {
        let mut sim = Sim::new();
        sim.run(1.0, 3, false, 60.0);
        sim.run(1.0, 2, true, 20.0);
        sim.run(1.0, -1, false, 0.0);
        sim.run(1.0, 3, false, 40.0);
        assert_eq!(sim.entries().len(), 1);
        assert!(sim.exits().is_empty());
    }

    #[test]
    fn rig_car_carries_player_car_idx() {
        let mut tracker = PitTracker::new();
        let mut events = Vec::new();
        for (i, on_pit) in [false, true].into_iter().enumerate() {
            let mut f = frame(100.0 + i as f32 * DT);
            f.on_pit_road = on_pit;
            f.player_car_class_position = 2;
            events.extend(tracker.update(&f, TRACK_M));
        }
        assert!(matches!(
            events.as_slice(),
            [RaceEvent::PitEntry {
                car_idx: RIG,
                player_car_idx: Some(RIG),
                position: 4,
                class_position: Some(2),
                ..
            }]
        ));
    }
}
