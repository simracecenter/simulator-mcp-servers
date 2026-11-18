// SPDX-License-Identifier: GPL-3.0-or-later

//! Detects the rig's own car taking the checkered flag: `CAR_FINISHED`.
//!
//! `RACE_CHECKERED` reports the flag falling, which happens when the overall
//! leader reaches the line. Every other car is still racing until it reaches
//! the line itself — in a multi-class race a GT car can be a minute or more
//! behind the prototype that brought the flag out. [`CarFinishDetector`]
//! watches for the rig's own crossing so its result is read at the moment it
//! is final.
//!
//! A car has finished when either:
//!
//! * its completed-lap counter advances while the flag is out, or
//! * it is the overall leader and crossed the line as the flag fell (the
//!   crossing and the flag can land on adjacent frames in either order).
//!
//! State is per iRacing session (`SessionNum`): practice, qualifying and the
//! race each go checkered inside one sub-session, and each is detected once.

use crate::telemetry_frame::TelemetryFrame;

/// `SessionFlags` bit for the checkered flag.
const CHECKERED_FLAG: u32 = 0x0001;
const SESSION_STATE_RACING: i32 = 4;
const SESSION_STATE_CHECKERED: i32 = 5;
const SESSION_STATE_COOLDOWN: i32 = 6;

/// How long after the rig's last line crossing a falling flag is still
/// attributed to that crossing, for the car leading overall. Covers the flag
/// arriving a few frames after the lap counter; far shorter than any lap.
const LEADER_FLAG_WINDOW_S: f32 = 1.0;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Flag {
    /// The flag has not been shown in this session.
    #[default]
    NotShown,
    /// The publisher watched the flag fall.
    Fell,
    /// The flag was already out on the first sample of the session, so the
    /// car may have finished before the publisher was watching.
    Inherited,
}

/// The rig's car has just taken the checkered flag.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CarFinish {
    /// Laps the car completed, the finishing lap included.
    pub laps_completed: i32,
}

/// Tracks one session's checkered flag and the rig's line crossings; see the
/// module docs.
#[derive(Debug, Default)]
pub struct CarFinishDetector {
    session_num: Option<i32>,
    prev_session_state: i32,
    flag: Flag,
    prev_laps_completed: Option<i32>,
    last_crossing_t: Option<f32>,
    emitted: bool,
}

impl CarFinishDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one frame. Returns the finish at most once per `SessionNum`.
    pub fn update(&mut self, frame: &TelemetryFrame) -> Option<CarFinish> {
        let first_sample = self.session_num != Some(frame.session_num);
        if first_sample {
            *self = Self {
                session_num: Some(frame.session_num),
                ..Self::default()
            };
        }

        let t = frame.session_time;
        let laps_completed = laps_completed(frame);
        let crossed = match (self.prev_laps_completed, laps_completed) {
            (Some(prev), Some(now)) => now > prev,
            _ => false,
        };
        // An unknown reading (car out of the world) keeps the last known
        // count, so coming back with the same count is not a crossing.
        if laps_completed.is_some() {
            self.prev_laps_completed = laps_completed;
        }
        if crossed {
            self.last_crossing_t = Some(t);
        }

        // Same three signals as `RACE_CHECKERED`: the state, the flag bit, or
        // a Racing → CoolDown step that skipped the Checkered state.
        let state = frame.session_state;
        let flag_out = state == SESSION_STATE_CHECKERED
            || frame.session_flags & CHECKERED_FLAG != 0
            || (state == SESSION_STATE_COOLDOWN && self.prev_session_state == SESSION_STATE_RACING);
        self.prev_session_state = state;

        let mut fell_now = false;
        if flag_out && self.flag == Flag::NotShown {
            if first_sample {
                self.flag = Flag::Inherited;
            } else {
                self.flag = Flag::Fell;
                fell_now = true;
            }
        }

        if self.emitted || self.flag != Flag::Fell {
            return None;
        }

        let leader_at_flag = fell_now
            && frame.player_car_position == 1
            && self
                .last_crossing_t
                .is_some_and(|crossing| (0.0..=LEADER_FLAG_WINDOW_S).contains(&(t - crossing)));
        if !(crossed || leader_at_flag) {
            return None;
        }

        self.emitted = true;
        Some(CarFinish {
            laps_completed: self.prev_laps_completed.unwrap_or(0),
        })
    }
}

/// Laps the rig's car has completed: its `CarIdxLapCompleted` slot, or
/// `Lap - 1` when the SDK does not expose that array. `None` while the slot
/// reads negative (car not in the world).
fn laps_completed(frame: &TelemetryFrame) -> Option<i32> {
    if frame.car_idx_lap_completed.is_empty() {
        return Some(i32::try_from(frame.lap).unwrap_or(i32::MAX) - 1);
    }
    frame
        .car_idx_lap_completed
        .get(frame.player_car_idx as usize)
        .copied()
        .filter(|laps| *laps >= 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: f32, state: i32, position: u8, completed: i32) -> TelemetryFrame {
        TelemetryFrame {
            lap: (completed + 1).max(0) as u32,
            session_time: t,
            lap_dist_pct: 0.5,
            player_car_idx: 1,
            player_car_position: position,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct: vec![0.6, 0.5],
            car_idx_position: vec![1, position],
            car_idx_on_pit_road: vec![false, false],
            car_idx_track_surface: vec![3, 3],
            lap_last_lap_time: 0.0,
            session_info_update: 0,
            session_tick: 0,
            session_state: state,
            session_num: 2,
            session_time_remain: None,
            session_laps_remain: None,
            player_incident_count: 0,
            player_driver_incident_count: None,
            player_team_incident_count: None,
            player_car_class_position: 0,
            car_idx_class_position: vec![],
            car_idx_lap_completed: vec![completed + 1, completed],
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

    #[test]
    fn a_car_behind_the_leader_finishes_on_its_own_crossing_not_at_the_flag() {
        let mut d = CarFinishDetector::new();
        assert_eq!(d.update(&frame(100.0, 4, 9, 20)), None);
        // Crossing under green is just another lap.
        assert_eq!(d.update(&frame(190.0, 4, 9, 21)), None);
        // The leader takes the flag; the rig is mid-lap.
        assert_eq!(d.update(&frame(200.0, 5, 9, 21)), None);
        assert_eq!(d.update(&frame(250.0, 5, 9, 21)), None);
        // A minute later the rig reaches the line.
        assert_eq!(
            d.update(&frame(262.0, 5, 8, 22)),
            Some(CarFinish { laps_completed: 22 })
        );
        // Once: the cool-down lap is not a second finish.
        assert_eq!(d.update(&frame(360.0, 5, 8, 23)), None);
        assert_eq!(d.update(&frame(361.0, 6, 8, 23)), None);
    }

    #[test]
    fn the_leader_finishes_as_the_flag_falls_when_the_crossing_comes_first() {
        let mut d = CarFinishDetector::new();
        assert_eq!(d.update(&frame(100.0, 4, 1, 20)), None);
        // Lap counter advances one frame before the state flips.
        assert_eq!(d.update(&frame(190.000, 4, 1, 21)), None);
        assert_eq!(
            d.update(&frame(190.017, 5, 1, 21)),
            Some(CarFinish { laps_completed: 21 })
        );
        assert_eq!(d.update(&frame(280.0, 5, 1, 22)), None);
    }

    #[test]
    fn the_leader_finishes_when_flag_and_crossing_share_a_frame() {
        let mut d = CarFinishDetector::new();
        assert_eq!(d.update(&frame(100.0, 4, 1, 20)), None);
        assert_eq!(
            d.update(&frame(190.0, 5, 1, 21)),
            Some(CarFinish { laps_completed: 21 })
        );
    }

    #[test]
    fn a_stale_crossing_does_not_finish_a_car_that_inherits_the_lead() {
        let mut d = CarFinishDetector::new();
        assert_eq!(d.update(&frame(100.0, 4, 2, 20)), None);
        assert_eq!(d.update(&frame(150.0, 4, 2, 21)), None);
        // Flag falls 40 s after the rig's last crossing; it reads P1 because
        // the standings shuffled, but it has not reached the line.
        assert_eq!(d.update(&frame(190.0, 5, 1, 21)), None);
        assert_eq!(
            d.update(&frame(240.0, 5, 1, 22)),
            Some(CarFinish { laps_completed: 22 })
        );
    }

    #[test]
    fn the_flag_bit_alone_counts_as_the_flag_being_out() {
        let mut d = CarFinishDetector::new();
        assert_eq!(d.update(&frame(100.0, 4, 5, 20)), None);
        let mut flagged = frame(200.0, 4, 5, 20);
        flagged.session_flags = CHECKERED_FLAG;
        assert_eq!(d.update(&flagged), None);
        let mut crossing = frame(230.0, 4, 5, 21);
        crossing.session_flags = CHECKERED_FLAG;
        assert_eq!(d.update(&crossing), Some(CarFinish { laps_completed: 21 }));
    }

    #[test]
    fn a_flag_already_out_on_connect_never_reports_a_finish() {
        let mut d = CarFinishDetector::new();
        assert_eq!(d.update(&frame(900.0, 5, 4, 30)), None);
        // Could be the finish, could be the cool-down lap: unknowable.
        assert_eq!(d.update(&frame(950.0, 5, 4, 31)), None);
    }

    #[test]
    fn each_session_num_is_detected_on_its_own() {
        let mut d = CarFinishDetector::new();
        // Qualifying goes checkered and the rig completes its lap.
        let q = |t: f32, state: i32, completed: i32| {
            let mut f = frame(t, state, 3, completed);
            f.session_num = 1;
            f
        };
        assert_eq!(d.update(&q(10.0, 4, 2)), None);
        assert_eq!(d.update(&q(500.0, 5, 2)), None);
        assert_eq!(
            d.update(&q(540.0, 5, 3)),
            Some(CarFinish { laps_completed: 3 })
        );

        // The race starts in the same sub-session with the lap counter reset.
        assert_eq!(d.update(&frame(0.0, 1, 3, 0)), None);
        assert_eq!(d.update(&frame(60.0, 4, 3, 0)), None);
        assert_eq!(d.update(&frame(150.0, 4, 3, 1)), None);
        assert_eq!(d.update(&frame(1960.0, 4, 3, 20)), None);
        assert_eq!(d.update(&frame(2000.0, 5, 3, 20)), None);
        assert_eq!(
            d.update(&frame(2050.0, 5, 3, 21)),
            Some(CarFinish { laps_completed: 21 })
        );
    }

    #[test]
    fn leaving_and_rejoining_the_world_is_not_a_crossing() {
        let mut d = CarFinishDetector::new();
        assert_eq!(d.update(&frame(100.0, 4, 6, 20)), None);
        assert_eq!(d.update(&frame(200.0, 5, 6, 20)), None);
        // Driver gets out: the slot reads -1, then the same count returns.
        assert_eq!(d.update(&frame(210.0, 5, 6, -1)), None);
        assert_eq!(d.update(&frame(220.0, 5, 6, 20)), None);
    }

    #[test]
    fn the_lap_variable_is_used_when_laps_completed_is_not_exposed() {
        let mut d = CarFinishDetector::new();
        let without_array = |t: f32, state: i32, lap: u32| {
            let mut f = frame(t, state, 7, 0);
            f.lap = lap;
            f.car_idx_lap_completed = vec![];
            f
        };
        assert_eq!(d.update(&without_array(100.0, 4, 300)), None);
        assert_eq!(d.update(&without_array(200.0, 5, 300)), None);
        // Lap 301 starts: 300 laps are complete — past the old u8 range.
        assert_eq!(
            d.update(&without_array(260.0, 5, 301)),
            Some(CarFinish {
                laps_completed: 300
            })
        );
    }
}
