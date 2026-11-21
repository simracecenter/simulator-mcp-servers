// SPDX-License-Identifier: GPL-3.0-or-later

//! What became of a car after a hard incident signal.
//!
//! `INCIDENT_ALERT` says a car hit something; it cannot say whether the car
//! drove on or was finished. The tracker watches each such car for a fixed
//! window and reports what it saw as one `INCIDENT_OUTCOME`.

use std::collections::VecDeque;

use crate::basic_incident::{
    HardEvent, STATIONARY_SPEED_MPS, TRACK_SURFACE_NOT_IN_WORLD, TRACK_SURFACE_OFF_TRACK,
};
use crate::race_event::RaceEvent;

/// How long a car is watched after its hard event.
pub const OUTCOME_WINDOW_S: f32 = 20.0;
/// A car that keeps having hard events is still reported: later events
/// extend the watch, but never past this long after the first.
const MAX_WATCH_S: f32 = 60.0;
/// Distance a car must travel against the direction of the lap, in total,
/// before it is reported as having gone backwards. Filters a car rocking on
/// the spot.
const BACKWARDS_MIN_M: f32 = 5.0;
/// A drop in lap distance larger than this is the start/finish wrap (or a
/// teleport), not a car reversing.
const WRAP_DELTA_PCT: f32 = 0.5;
/// The lowest speed is looked for this long after the first hard event. The
/// alert fires on the frame the drop crosses its threshold; the bottom of the
/// drop comes within a few seconds.
pub const PEAK_LOSS_WINDOW_S: f32 = 3.0;
/// Below this speed before the hard event there is nothing to lose: no peak
/// loss is reported.
const MIN_PEAK_SPEED_MPS: f32 = 1.0;

/// `irsdk_Flags` bits of `CarIdxSessionFlags` read into the outcome, as named
/// in the SDK's `irsdk_defines.h` and seen on a live race (ADR 0008, "Probe
/// findings").
pub const FLAG_BLACK: u32 = 0x0001_0000;
pub const FLAG_DISQUALIFY: u32 = 0x0002_0000;
pub const FLAG_REPAIR: u32 = 0x0010_0000;

/// What the tracker reads about one car at one moment. Built by the caller so
/// the tracker needs neither the registry nor the frame.
#[derive(Clone, Copy, Debug)]
pub struct CarSample {
    pub track_surface: i32,
    pub on_pit_road: bool,
    /// Derived from lap distance for every car but the rig's own, so it is
    /// speed along the track: a car spinning on the spot reads as stopped.
    pub speed_mps: f32,
    pub lap_dist_pct: f32,
    /// Overall position, `None` when the car has none.
    pub position: Option<u8>,
    /// Position in class, `None` when the SDK has no value.
    pub class_position: Option<u8>,
    /// `CarIdxSessionFlags` of the car, `None` when the SDK does not expose
    /// the variable.
    pub session_flags: Option<u32>,
}

struct Watch {
    car_idx: u8,
    onset_session_time: f32,
    /// Tick of the latest hard event folded into this watch, so the same
    /// event seen twice does not extend it.
    last_hard_event: (f32, i64),
    close_at: f32,
    incident_id: Option<u32>,
    position_from: Option<u8>,
    class_position_from: Option<u8>,
    position_last: Option<u8>,
    class_position_last: Option<u8>,
    last_sample_time: f32,
    last_lap_dist_pct: f32,
    stopped_s: f32,
    off_track_s: f32,
    backwards_pct: f32,
    pitted: bool,
    /// Speed before the first hard event, from the detector.
    peak_speed_mps: f32,
    /// Lowest speed seen in the first `PEAK_LOSS_WINDOW_S`, in the world and
    /// off pit road. Starts at the speed on the onset frame.
    lowest_speed_mps: Option<f32>,
    /// The car left the world inside `PEAK_LOSS_WINDOW_S`.
    gone_early: bool,
    /// Every flag bit seen at any sample, `None` when the SDK never gave
    /// the variable.
    flags_seen: Option<u32>,
    /// Flags on the onset frame.
    flags_at_onset: Option<u32>,
}

pub struct IncidentOutcomeTracker {
    /// Open watches, oldest first. At most one per car.
    watches: VecDeque<Watch>,
    last_session_time: Option<f32>,
}

impl IncidentOutcomeTracker {
    pub fn new() -> Self {
        Self {
            watches: VecDeque::new(),
            last_session_time: None,
        }
    }

    /// Number of cars currently being watched.
    pub fn open_watches(&self) -> usize {
        self.watches.len()
    }

    /// Open (or extend) a watch for every hard event raised on this frame.
    ///
    /// Runs every frame, not on the incident cadence: a hard event can be
    /// raised on any frame and its onset must be the frame it happened on.
    pub fn observe(
        &mut self,
        session_time: f32,
        session_tick: i64,
        hard_events: &VecDeque<HardEvent>,
        sample: impl Fn(u8) -> Option<CarSample>,
    ) {
        self.note_clock(session_time);
        // Newest last: only the tail can belong to this frame.
        for ev in hard_events
            .iter()
            .rev()
            .take_while(|ev| ev.session_time == session_time && ev.session_tick == session_tick)
        {
            let stamp = (ev.session_time, ev.session_tick);
            if let Some(watch) = self.watches.iter_mut().find(|w| w.car_idx == ev.car_idx) {
                // Same car, watch still open: one incident, one outcome.
                if watch.last_hard_event != stamp {
                    watch.last_hard_event = stamp;
                    watch.close_at = (ev.session_time + OUTCOME_WINDOW_S)
                        .min(watch.onset_session_time + MAX_WATCH_S);
                }
                continue;
            }
            // A car in the pit lane or out of the world has no outcome worth
            // reporting; the detector does not alert for one either.
            let Some(car) = sample(ev.car_idx) else {
                continue;
            };
            if car.track_surface <= TRACK_SURFACE_NOT_IN_WORLD || car.on_pit_road {
                continue;
            }
            self.watches.push_back(Watch {
                car_idx: ev.car_idx,
                onset_session_time: ev.session_time,
                last_hard_event: stamp,
                close_at: ev.session_time + OUTCOME_WINDOW_S,
                incident_id: None,
                position_from: car.position,
                class_position_from: car.class_position,
                position_last: car.position,
                class_position_last: car.class_position,
                last_sample_time: ev.session_time,
                last_lap_dist_pct: ev.lap_dist_pct,
                stopped_s: 0.0,
                off_track_s: 0.0,
                backwards_pct: 0.0,
                pitted: false,
                peak_speed_mps: ev.peak_speed_mps,
                lowest_speed_mps: Some(car.speed_mps),
                gone_early: false,
                flags_seen: car.session_flags,
                flags_at_onset: car.session_flags,
            });
        }
    }

    /// Mark the watches of `car_idxs` as part of cluster `incident_id`, so
    /// their outcomes carry the cluster's key. A watch keeps the first
    /// cluster it was linked to.
    pub fn link_cluster(&mut self, incident_id: u32, car_idxs: &[u8]) {
        for watch in &mut self.watches {
            if watch.incident_id.is_none() && car_idxs.contains(&watch.car_idx) {
                watch.incident_id = Some(incident_id);
            }
        }
    }

    /// Sample every watched car and emit an outcome for each watch that
    /// closes. Runs on the incident cadence.
    pub fn update(
        &mut self,
        lap: u32,
        session_time: f32,
        track_length_m: f32,
        sample: impl Fn(u8) -> Option<CarSample>,
    ) -> Vec<RaceEvent> {
        self.note_clock(session_time);
        let mut events = Vec::new();
        self.watches.retain_mut(|watch| {
            // Time past the end of the window is not part of the outcome.
            let now = session_time.min(watch.close_at);
            let dt = (now - watch.last_sample_time).max(0.0);

            // A car the caller no longer knows is gone as surely as one
            // reporting "not in world".
            let car =
                sample(watch.car_idx).filter(|car| car.track_surface > TRACK_SURFACE_NOT_IN_WORLD);
            let in_peak_window = session_time - watch.onset_session_time <= PEAK_LOSS_WINDOW_S;
            let Some(car) = car else {
                watch.gone_early = in_peak_window;
                events.push(watch.outcome(lap, session_time, now, true, track_length_m));
                return false;
            };

            if let Some(flags) = car.session_flags {
                watch.flags_seen = Some(watch.flags_seen.unwrap_or(0) | flags);
            }
            if car.on_pit_road {
                watch.pitted = true;
            } else {
                if in_peak_window {
                    watch.lowest_speed_mps = Some(
                        watch
                            .lowest_speed_mps
                            .map_or(car.speed_mps, |lowest| lowest.min(car.speed_mps)),
                    );
                }
                if car.speed_mps < STATIONARY_SPEED_MPS {
                    watch.stopped_s += dt;
                }
                // Reversing in the pit lane is a pit stop, not an outcome.
                let delta = car.lap_dist_pct - watch.last_lap_dist_pct;
                if delta < 0.0 && delta > -WRAP_DELTA_PCT {
                    watch.backwards_pct -= delta;
                }
            }
            if car.track_surface == TRACK_SURFACE_OFF_TRACK {
                watch.off_track_s += dt;
            }
            watch.position_last = car.position.or(watch.position_last);
            watch.class_position_last = car.class_position.or(watch.class_position_last);
            watch.last_sample_time = now;
            watch.last_lap_dist_pct = car.lap_dist_pct;

            if session_time >= watch.close_at {
                events.push(watch.outcome(lap, session_time, now, false, track_length_m));
                return false;
            }
            true
        });
        events
    }

    /// A clock restart (session transition, replay seek) leaves every open
    /// watch measuring against a time that no longer exists.
    fn note_clock(&mut self, session_time: f32) {
        if self
            .last_session_time
            .is_some_and(|prev| session_time < prev)
        {
            self.watches.clear();
        }
        self.last_session_time = Some(session_time);
    }
}

impl Watch {
    fn outcome(
        &self,
        lap: u32,
        session_time: f32,
        watched_until: f32,
        left_world: bool,
        track_length_m: f32,
    ) -> RaceEvent {
        // Without a position at onset there is nothing to compare against.
        let positions = self
            .position_from
            .map(|from| (from, self.position_last.unwrap_or(from)));
        let (peak_speed_loss_normalized, lowest_speed_mps) = self.peak_loss();
        let flag = |bit: u32| self.flags_seen.map(|flags| flags & bit != 0);
        RaceEvent::IncidentOutcome {
            lap,
            session_time,
            car_idx: self.car_idx,
            onset_session_time: self.onset_session_time,
            incident_id: self.incident_id,
            window_seconds: (watched_until - self.onset_session_time).max(0.0),
            stopped_seconds: self.stopped_s,
            off_track_seconds: self.off_track_s,
            left_world,
            pitted: self.pitted,
            went_backwards: self.backwards_pct * track_length_m >= BACKWARDS_MIN_M,
            position_from: positions.map(|(from, _)| from),
            position_to: positions.map(|(_, to)| to),
            places_lost: positions.map(|(from, to)| to.saturating_sub(from)),
            class_position_from: self.class_position_from,
            class_position_to: self.class_position_last,
            peak_speed_loss_normalized,
            lowest_speed_mps,
            repair_flag: flag(FLAG_REPAIR),
            black_flag: flag(FLAG_BLACK),
            black_flag_at_onset: self.flags_at_onset.map(|flags| flags & FLAG_BLACK != 0),
            disqualified: flag(FLAG_DISQUALIFY),
        }
    }

    /// `(peak_speed_loss_normalized, lowest_speed_mps)`. A car that left the
    /// world inside the window lost everything; a car with no speed before
    /// the hard event has no loss to report.
    fn peak_loss(&self) -> (Option<f32>, Option<f32>) {
        if self.peak_speed_mps < MIN_PEAK_SPEED_MPS {
            return (None, None);
        }
        if self.gone_early {
            return (Some(1.0), Some(0.0));
        }
        let Some(lowest) = self.lowest_speed_mps else {
            return (None, None);
        };
        let lowest = lowest.max(0.0);
        let loss = ((self.peak_speed_mps - lowest) / self.peak_speed_mps).clamp(0.0, 1.0);
        (Some(loss), Some(lowest))
    }
}

impl Default for IncidentOutcomeTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    const TRACK_M: f32 = 5000.0;
    const CAR: u8 = 7;
    /// Speed before every hard event the tests raise.
    const PEAK_MPS: f32 = 60.0;
    const SERVICIBLE: u32 = 0x0004_0000;

    /// A small world the tests move by hand: the cars' current state, the
    /// clock, and the hard events raised so far.
    struct World {
        cars: RefCell<HashMap<u8, CarSample>>,
        hard_events: VecDeque<HardEvent>,
        tracker: IncidentOutcomeTracker,
        tick: i64,
        /// Speed before the next hard event raised.
        peak: f32,
    }

    fn racing(position: u8) -> CarSample {
        CarSample {
            track_surface: 3,
            on_pit_road: false,
            speed_mps: 50.0,
            lap_dist_pct: 0.40,
            position: Some(position),
            class_position: Some(position),
            session_flags: None,
        }
    }

    impl World {
        fn new() -> Self {
            Self {
                cars: RefCell::new(HashMap::from([(CAR, racing(4))])),
                hard_events: VecDeque::new(),
                tracker: IncidentOutcomeTracker::new(),
                tick: 0,
                peak: PEAK_MPS,
            }
        }

        fn set(&self, car_idx: u8, change: impl FnOnce(&mut CarSample)) {
            change(self.cars.borrow_mut().get_mut(&car_idx).unwrap());
        }

        /// Raise a hard event for `car_idx` at `t` and let the tracker see it.
        fn hard_event(&mut self, car_idx: u8, t: f32) {
            self.tick += 1;
            let lap_dist_pct = self
                .cars
                .borrow()
                .get(&car_idx)
                .map_or(0.0, |car| car.lap_dist_pct);
            self.hard_events.push_back(HardEvent {
                car_idx,
                session_time: t,
                session_tick: self.tick,
                lap_dist_pct,
                reason: "speed_drop".to_owned(),
                speed_drop_mps: 30.0,
                severity_normalized: 0.7,
                peak_speed_mps: self.peak,
            });
            let cars = &self.cars;
            self.tracker
                .observe(t, self.tick, &self.hard_events, |idx| {
                    cars.borrow().get(&idx).copied()
                });
        }

        /// One incident cadence at `t`.
        fn cadence(&mut self, t: f32) -> Vec<RaceEvent> {
            self.tick += 1;
            let cars = &self.cars;
            self.tracker
                .update(3, t, TRACK_M, |idx| cars.borrow().get(&idx).copied())
        }

        /// Cadences every 0.25 s over `(from, to]`, collecting what closes.
        fn run(&mut self, from: f32, to: f32) -> Vec<RaceEvent> {
            let steps = ((to - from) / 0.25).round() as u32;
            (1..=steps)
                .flat_map(|i| self.cadence(from + i as f32 * 0.25))
                .collect()
        }
    }

    /// The fields of the single outcome in `events`.
    struct Outcome {
        car_idx: u8,
        session_time: f32,
        onset: f32,
        incident_id: Option<u32>,
        window: f32,
        stopped: f32,
        off_track: f32,
        left_world: bool,
        pitted: bool,
        went_backwards: bool,
        positions: (Option<u8>, Option<u8>, Option<u8>),
        class_positions: (Option<u8>, Option<u8>),
    }

    fn only_outcome(events: &[RaceEvent]) -> Outcome {
        let [RaceEvent::IncidentOutcome {
            session_time,
            car_idx,
            onset_session_time,
            incident_id,
            window_seconds,
            stopped_seconds,
            off_track_seconds,
            left_world,
            pitted,
            went_backwards,
            position_from,
            position_to,
            places_lost,
            class_position_from,
            class_position_to,
            ..
        }] = events
        else {
            panic!("expected exactly one outcome, got {events:?}");
        };
        Outcome {
            car_idx: *car_idx,
            session_time: *session_time,
            onset: *onset_session_time,
            incident_id: *incident_id,
            window: *window_seconds,
            stopped: *stopped_seconds,
            off_track: *off_track_seconds,
            left_world: *left_world,
            pitted: *pitted,
            went_backwards: *went_backwards,
            positions: (*position_from, *position_to, *places_lost),
            class_positions: (*class_position_from, *class_position_to),
        }
    }

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn a_car_that_drives_on_closes_after_the_window_with_nothing_to_report() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        assert_eq!(world.tracker.open_watches(), 1);

        assert!(
            world.run(100.0, 119.75).is_empty(),
            "still inside the window"
        );
        let outcome = only_outcome(&world.cadence(120.0));
        assert_eq!(outcome.car_idx, CAR);
        assert_eq!(outcome.onset, 100.0);
        assert_eq!(outcome.session_time, 120.0);
        assert!(close(outcome.window, OUTCOME_WINDOW_S));
        assert_eq!(outcome.stopped, 0.0);
        assert_eq!(outcome.off_track, 0.0);
        assert!(!outcome.left_world && !outcome.pitted && !outcome.went_backwards);
        assert_eq!(outcome.incident_id, None);
        assert_eq!(world.tracker.open_watches(), 0);
        assert!(world.run(120.0, 125.0).is_empty(), "one outcome, once");
    }

    #[test]
    fn stopped_time_counts_only_while_slow_on_the_racing_surface() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.run(100.0, 102.0);
        // Stopped for 6 s, then away again.
        world.set(CAR, |car| car.speed_mps = 1.0);
        world.run(102.0, 108.0);
        world.set(CAR, |car| car.speed_mps = 40.0);
        let outcome = only_outcome(&world.run(108.0, 120.0));
        assert!(close(outcome.stopped, 6.0), "got {}", outcome.stopped);
        assert_eq!(outcome.off_track, 0.0);
    }

    #[test]
    fn standing_in_the_pit_lane_is_not_stopped_time() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.run(100.0, 105.0);
        world.set(CAR, |car| {
            car.on_pit_road = true;
            car.speed_mps = 0.0;
        });
        let outcome = only_outcome(&world.run(105.0, 120.0));
        assert_eq!(outcome.stopped, 0.0);
        assert!(outcome.pitted);
    }

    #[test]
    fn off_track_time_counts_while_the_surface_is_off_track() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.set(CAR, |car| car.track_surface = TRACK_SURFACE_OFF_TRACK);
        world.run(100.0, 103.5);
        world.set(CAR, |car| car.track_surface = 3);
        let outcome = only_outcome(&world.run(103.5, 120.0));
        assert!(close(outcome.off_track, 3.5), "got {}", outcome.off_track);
        assert_eq!(outcome.stopped, 0.0, "off track but still moving");
    }

    #[test]
    fn a_pit_entry_inside_the_window_is_noted() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.run(100.0, 110.0);
        world.set(CAR, |car| car.on_pit_road = true);
        world.cadence(110.25);
        world.set(CAR, |car| car.on_pit_road = false);
        let outcome = only_outcome(&world.run(110.25, 120.0));
        assert!(outcome.pitted);
        assert!(!outcome.left_world);
    }

    #[test]
    fn lap_distance_running_backwards_is_noted() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        // Rolls back 10 m over a second, then drives on.
        for i in 1..=4 {
            world.set(CAR, |car| {
                car.lap_dist_pct = 0.40 - i as f32 * 2.5 / TRACK_M;
                car.speed_mps = 0.0;
            });
            world.cadence(100.0 + i as f32 * 0.25);
        }
        world.set(CAR, |car| {
            car.lap_dist_pct = 0.45;
            car.speed_mps = 40.0;
        });
        let outcome = only_outcome(&world.run(101.0, 120.0));
        assert!(outcome.went_backwards);
    }

    #[test]
    fn crossing_the_line_or_rocking_on_the_spot_is_not_going_backwards() {
        let mut world = World::new();
        world.set(CAR, |car| car.lap_dist_pct = 0.995);
        world.hard_event(CAR, 100.0);
        // Start/finish wrap: lap distance falls from ~1 to ~0 going forwards.
        world.set(CAR, |car| car.lap_dist_pct = 0.005);
        world.cadence(100.25);
        // Then a metre back and forth, well under the threshold in total.
        world.set(CAR, |car| car.lap_dist_pct = 0.005 - 1.0 / TRACK_M);
        world.cadence(100.5);
        world.set(CAR, |car| car.lap_dist_pct = 0.005);
        let outcome = only_outcome(&world.run(100.5, 120.0));
        assert!(!outcome.went_backwards);
    }

    #[test]
    fn positions_report_where_the_car_started_and_ended_the_window() {
        let mut world = World::new();
        world.set(CAR, |car| car.class_position = Some(2));
        world.hard_event(CAR, 100.0);
        world.run(100.0, 110.0);
        world.set(CAR, |car| {
            car.position = Some(9);
            car.class_position = Some(5);
        });
        let outcome = only_outcome(&world.run(110.0, 120.0));
        assert_eq!(outcome.positions, (Some(4), Some(9), Some(5)));
        assert_eq!(outcome.class_positions, (Some(2), Some(5)));
    }

    #[test]
    fn a_car_that_gains_places_reports_none_lost() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.set(CAR, |car| car.position = Some(2));
        let outcome = only_outcome(&world.run(100.0, 120.0));
        assert_eq!(outcome.positions, (Some(4), Some(2), Some(0)));
    }

    #[test]
    fn positions_are_absent_when_the_car_had_none() {
        let mut world = World::new();
        world.set(CAR, |car| {
            car.position = None;
            car.class_position = None;
        });
        world.hard_event(CAR, 100.0);
        // A position that appears later has no starting point to compare to.
        world.set(CAR, |car| car.position = Some(12));
        let outcome = only_outcome(&world.run(100.0, 120.0));
        assert_eq!(outcome.positions, (None, None, None));
        assert_eq!(outcome.class_positions, (None, None));
    }

    #[test]
    fn a_retirement_closes_the_watch_at_once() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.set(CAR, |car| car.speed_mps = 0.0);
        assert!(world.run(100.0, 104.0).is_empty());
        // Towed / retired: out of the world, position already falling.
        world.set(CAR, |car| {
            car.track_surface = TRACK_SURFACE_NOT_IN_WORLD;
            car.position = Some(20);
        });
        let outcome = only_outcome(&world.cadence(104.25));
        assert!(outcome.left_world);
        assert_eq!(outcome.session_time, 104.25);
        assert!(close(outcome.window, 4.25), "got {}", outcome.window);
        assert!(close(outcome.stopped, 4.0), "got {}", outcome.stopped);
        // The last position seen while the car was still in the world.
        assert_eq!(outcome.positions, (Some(4), Some(4), Some(0)));
        assert!(world.run(104.25, 125.0).is_empty(), "nothing more to say");
    }

    #[test]
    fn a_car_that_vanishes_from_the_field_counts_as_having_left_the_world() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.run(100.0, 102.0);
        world.cars.borrow_mut().remove(&CAR);
        let outcome = only_outcome(&world.cadence(102.25));
        assert!(outcome.left_world);
    }

    #[test]
    fn two_hard_events_for_one_car_make_one_outcome() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        assert!(world.run(100.0, 108.0).is_empty());
        // Hit again 8 s later: same incident, the watch runs on from here.
        world.hard_event(CAR, 108.0);
        assert_eq!(world.tracker.open_watches(), 1);
        assert!(
            world.run(108.0, 127.75).is_empty(),
            "extended past the first 20 s"
        );
        let outcome = only_outcome(&world.cadence(128.0));
        assert_eq!(outcome.onset, 100.0, "onset stays the first hard event");
        assert!(close(outcome.window, 28.0), "got {}", outcome.window);
        assert!(world.run(128.0, 150.0).is_empty());
    }

    #[test]
    fn a_car_that_keeps_crashing_is_still_reported() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        let mut events = Vec::new();
        let mut t = 100.0;
        while t < 170.0 {
            events.extend(world.run(t, t + 15.0));
            t += 15.0;
            if world.tracker.open_watches() == 1 {
                world.hard_event(CAR, t);
            }
        }
        let first = events
            .iter()
            .find_map(|event| match event {
                RaceEvent::IncidentOutcome { window_seconds, .. } => Some(*window_seconds),
                _ => None,
            })
            .expect("the watch closes despite repeated hard events");
        assert!(close(first, MAX_WATCH_S), "got {first}");
    }

    #[test]
    fn the_same_hard_event_seen_twice_does_not_extend_the_watch() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        // The event is still the newest when the next frame repeats the clock.
        let (cars, tick) = (&world.cars, world.tick);
        world
            .tracker
            .observe(100.0, tick, &world.hard_events, |idx| {
                cars.borrow().get(&idx).copied()
            });
        assert_eq!(world.tracker.open_watches(), 1);
        let outcome = only_outcome(&world.run(100.0, 120.0));
        assert!(close(outcome.window, OUTCOME_WINDOW_S));
    }

    #[test]
    fn older_hard_events_are_not_reopened_on_a_later_frame() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        only_outcome(&world.run(100.0, 120.0));
        // The detector still lists the old event; a later frame must not
        // start a second watch from it.
        let (cars, tick) = (&world.cars, world.tick + 1);
        world
            .tracker
            .observe(120.25, tick, &world.hard_events, |idx| {
                cars.borrow().get(&idx).copied()
            });
        assert_eq!(world.tracker.open_watches(), 0);
    }

    #[test]
    fn no_watch_opens_for_a_car_in_the_pit_lane_or_out_of_the_world() {
        let mut world = World::new();
        world.cars.borrow_mut().insert(8, racing(5));
        world.cars.borrow_mut().insert(9, racing(6));
        world.set(8, |car| car.on_pit_road = true);
        world.set(9, |car| car.track_surface = TRACK_SURFACE_NOT_IN_WORLD);
        world.hard_event(8, 100.0);
        world.hard_event(9, 100.0);
        // Car 10 is not known at all.
        world.hard_event(10, 100.0);
        assert_eq!(world.tracker.open_watches(), 0);
        assert!(world.run(100.0, 125.0).is_empty());
    }

    #[test]
    fn a_cluster_gives_its_key_to_the_watches_of_its_cars_only() {
        let mut world = World::new();
        world.cars.borrow_mut().insert(8, racing(5));
        world.cars.borrow_mut().insert(9, racing(6));
        world.hard_event(CAR, 100.0);
        world.hard_event(8, 100.5);
        world.hard_event(9, 101.0);
        world.tracker.link_cluster(42, &[CAR, 8]);
        // A later cluster does not take a watch that already has one.
        world.tracker.link_cluster(43, &[8, 30]);

        let events = world.run(101.0, 122.0);
        let ids: HashMap<u8, Option<u32>> = events
            .iter()
            .map(|event| match event {
                RaceEvent::IncidentOutcome {
                    car_idx,
                    incident_id,
                    ..
                } => (*car_idx, *incident_id),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            HashMap::from([(CAR, Some(42)), (8, Some(42)), (9, None)])
        );
    }

    #[test]
    fn a_clock_restart_drops_every_open_watch() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.run(100.0, 105.0);
        // New session: the clock is back near zero.
        assert!(world.cadence(2.0).is_empty());
        assert_eq!(world.tracker.open_watches(), 0);
        assert!(world.run(2.0, 130.0).is_empty());
    }

    /// The P-3 and P-5 facts of the single outcome in `events`.
    #[derive(Debug, PartialEq)]
    struct Facts {
        peak_loss: Option<f32>,
        lowest: Option<f32>,
        repair: Option<bool>,
        black: Option<bool>,
        black_at_onset: Option<bool>,
        disqualified: Option<bool>,
    }

    fn only_facts(events: &[RaceEvent]) -> Facts {
        let [RaceEvent::IncidentOutcome {
            peak_speed_loss_normalized,
            lowest_speed_mps,
            repair_flag,
            black_flag,
            black_flag_at_onset,
            disqualified,
            ..
        }] = events
        else {
            panic!("expected exactly one outcome, got {events:?}");
        };
        Facts {
            peak_loss: *peak_speed_loss_normalized,
            lowest: *lowest_speed_mps,
            repair: *repair_flag,
            black: *black_flag,
            black_at_onset: *black_flag_at_onset,
            disqualified: *disqualified,
        }
    }

    #[test]
    fn a_lift_reports_the_share_of_speed_really_lost() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        // Bottoms out at 60% of the 60 m/s peak, then drives on.
        world.set(CAR, |car| car.speed_mps = 36.0);
        world.run(100.0, 102.0);
        world.set(CAR, |car| car.speed_mps = 50.0);
        let facts = only_facts(&world.run(102.0, 120.0));
        assert!(close(facts.peak_loss.unwrap(), 0.4), "got {facts:?}");
        assert!(close(facts.lowest.unwrap(), 36.0));
    }

    #[test]
    fn a_crash_to_a_stop_loses_everything() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.set(CAR, |car| car.speed_mps = 0.0);
        let facts = only_facts(&world.run(100.0, 120.0));
        assert_eq!(facts.peak_loss, Some(1.0));
        assert_eq!(facts.lowest, Some(0.0));
    }

    #[test]
    fn speed_after_the_peak_loss_window_does_not_count() {
        let mut world = World::new();
        // 50 m/s on the onset frame and for the first 3 s.
        world.hard_event(CAR, 100.0);
        world.run(100.0, 104.0);
        // Parked later in the window: that is the stopped time, not the hit.
        world.set(CAR, |car| car.speed_mps = 0.0);
        let facts = only_facts(&world.run(104.0, 120.0));
        assert!(
            close(facts.peak_loss.unwrap(), 10.0 / 60.0),
            "got {facts:?}"
        );
        assert!(close(facts.lowest.unwrap(), 50.0));
    }

    #[test]
    fn a_tow_inside_the_peak_loss_window_loses_everything() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.run(100.0, 101.0);
        world.set(CAR, |car| car.track_surface = TRACK_SURFACE_NOT_IN_WORLD);
        let events = world.cadence(101.25);
        assert!(only_outcome(&events).left_world);
        let facts = only_facts(&events);
        assert_eq!(facts.peak_loss, Some(1.0));
        assert_eq!(facts.lowest, Some(0.0));
    }

    #[test]
    fn a_tow_after_the_peak_loss_window_keeps_the_measured_loss() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.run(100.0, 110.0);
        world.set(CAR, |car| car.track_surface = TRACK_SURFACE_NOT_IN_WORLD);
        let facts = only_facts(&world.cadence(110.25));
        assert!(
            close(facts.peak_loss.unwrap(), 10.0 / 60.0),
            "got {facts:?}"
        );
    }

    #[test]
    fn no_peak_loss_without_speed_before_the_hard_event() {
        let mut world = World::new();
        world.peak = 0.5;
        world.hard_event(CAR, 100.0);
        let facts = only_facts(&world.run(100.0, 120.0));
        assert_eq!((facts.peak_loss, facts.lowest), (None, None));
    }

    #[test]
    fn flag_keys_are_absent_when_the_sdk_has_no_per_car_flags() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        let facts = only_facts(&world.run(100.0, 120.0));
        assert_eq!(
            (
                facts.repair,
                facts.black,
                facts.black_at_onset,
                facts.disqualified
            ),
            (None, None, None, None)
        );
    }

    #[test]
    fn a_repair_flag_raised_mid_window_is_reported() {
        let mut world = World::new();
        world.set(CAR, |car| car.session_flags = Some(SERVICIBLE));
        world.hard_event(CAR, 100.0);
        world.run(100.0, 105.0);
        // As seen live: servicible + black + repair, the meatball.
        world.set(CAR, |car| car.session_flags = Some(0x0015_0000));
        world.run(105.0, 110.0);
        world.set(CAR, |car| car.session_flags = Some(SERVICIBLE));
        let facts = only_facts(&world.run(110.0, 120.0));
        assert_eq!(facts.repair, Some(true));
        assert_eq!(facts.black, Some(true));
        assert_eq!(facts.black_at_onset, Some(false), "raised after the hit");
        assert_eq!(facts.disqualified, Some(false));
    }

    #[test]
    fn a_black_flag_already_set_at_the_hard_event_marks_a_penalty() {
        let mut world = World::new();
        world.set(CAR, |car| car.session_flags = Some(SERVICIBLE | FLAG_BLACK));
        world.hard_event(CAR, 100.0);
        world.set(CAR, |car| car.session_flags = Some(SERVICIBLE));
        let facts = only_facts(&world.run(100.0, 120.0));
        assert_eq!(facts.black_at_onset, Some(true));
        assert_eq!(facts.black, Some(true));
        assert_eq!(facts.repair, Some(false));
    }

    #[test]
    fn a_disqualification_in_the_window_is_reported() {
        let mut world = World::new();
        world.set(CAR, |car| car.session_flags = Some(SERVICIBLE));
        world.hard_event(CAR, 100.0);
        world.run(100.0, 104.0);
        world.set(CAR, |car| car.session_flags = Some(0x0006_0000));
        let facts = only_facts(&world.run(104.0, 120.0));
        assert_eq!(facts.disqualified, Some(true));
        assert_eq!(facts.black, Some(false));
    }

    #[test]
    fn a_stalled_cadence_does_not_count_time_past_the_window() {
        let mut world = World::new();
        world.hard_event(CAR, 100.0);
        world.set(CAR, |car| {
            car.speed_mps = 0.0;
            car.track_surface = TRACK_SURFACE_OFF_TRACK;
        });
        // Nothing sampled for 45 s.
        let outcome = only_outcome(&world.cadence(145.0));
        assert!(close(outcome.window, OUTCOME_WINDOW_S));
        assert!(close(outcome.stopped, OUTCOME_WINDOW_S));
        assert!(close(outcome.off_track, OUTCOME_WINDOW_S));
    }
}
