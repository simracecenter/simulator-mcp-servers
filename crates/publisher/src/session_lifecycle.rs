// SPDX-License-Identifier: GPL-3.0-or-later

//! Session lifecycle beyond the flags: `SESSION_STATE`.
//!
//! [`SessionLifecycleTracker`] watches iRacing `SessionNum`, `SessionState`,
//! the `SessionType` string from `SessionInfo`, and the session clock, and
//! emits one [`RaceEvent::SessionState`] per meaningful change so a consumer
//! knows "this is qualifying, cover hotlaps" or "the race is about to go
//! green" without inferring it from flags.
//!
//! Every value here is read from telemetry the SDK provides. iRacing exposes
//! no explicit "next session starts in N seconds" variable; the closest
//! signal is `SessionTimeRemain`, which this tracker turns into a single
//! `SESSION_ENDING` emission when the clock enters its final stretch.

use crate::race_event::{
    RaceEvent, SessionKind, SessionPhase, SessionStateName, SessionStateReason,
};
use crate::telemetry_frame::TelemetryFrame;

/// Seconds of `SessionTimeRemain` left below which the session is reported
/// as ending (`sessionChangeImminent = true`, one `SESSION_ENDING` event).
pub const SESSION_ENDING_WINDOW_S: f64 = 60.0;

#[derive(Clone, Debug, PartialEq)]
struct Observed {
    session_num: i32,
    session_state: i32,
    session_type: Option<String>,
    phase: SessionPhase,
}

/// Tracks session identity and phase across frames; see module docs.
#[derive(Debug, Default)]
pub struct SessionLifecycleTracker {
    last: Option<Observed>,
    /// `SessionNum` for which `SESSION_ENDING` has already been emitted.
    ending_announced_for: Option<i32>,
}

impl SessionLifecycleTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one frame plus the current `SessionType` (from `SessionInfo`,
    /// `None` while unresolved). Returns at most one event.
    pub fn update(
        &mut self,
        frame: &TelemetryFrame,
        session_type: Option<&str>,
    ) -> Option<RaceEvent> {
        let kind = SessionKind::from_session_type(session_type);
        let state_name = SessionStateName::from_raw(frame.session_state);
        let phase = SessionPhase::derive(kind, state_name);
        let now = Observed {
            session_num: frame.session_num,
            session_state: frame.session_state,
            session_type: session_type.map(str::to_owned),
            phase,
        };

        let reason = match &self.last {
            None => Some(SessionStateReason::ConnectSnapshot),
            Some(prev) if prev.session_num != now.session_num => {
                Some(SessionStateReason::SessionNumChanged)
            }
            Some(prev) if prev.session_state != now.session_state => {
                Some(SessionStateReason::SessionStateChanged)
            }
            Some(prev) if prev.session_type != now.session_type => {
                Some(SessionStateReason::SessionTypeResolved)
            }
            Some(_) => None,
        };

        if matches!(reason, Some(SessionStateReason::SessionNumChanged)) {
            self.ending_announced_for = None;
        }

        let clock_ending = frame
            .session_time_remain
            .is_some_and(|remain| remain <= SESSION_ENDING_WINDOW_S);
        let state_ending = matches!(
            state_name,
            SessionStateName::Checkered | SessionStateName::CoolDown
        );
        let imminent = clock_ending || state_ending;

        let reason = reason.or_else(|| {
            (clock_ending && self.ending_announced_for != Some(now.session_num))
                .then_some(SessionStateReason::SessionEnding)
        });

        let prev = self.last.replace(now.clone());
        let reason = reason?;
        if imminent {
            self.ending_announced_for = Some(now.session_num);
        }

        Some(RaceEvent::SessionState {
            lap: frame.lap,
            session_time: frame.session_time,
            session_num: now.session_num,
            previous_session_num: prev.as_ref().map(|p| p.session_num),
            session_type: now.session_type,
            session_kind: kind,
            session_state: now.session_state,
            session_state_name: state_name,
            previous_session_state: prev.as_ref().map(|p| p.session_state),
            phase,
            previous_phase: prev.as_ref().map(|p| p.phase),
            reason,
            synthetic: reason == SessionStateReason::ConnectSnapshot,
            session_time_remain_s: frame.session_time_remain,
            session_laps_remain: frame.session_laps_remain,
            session_change_imminent: imminent,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(session_num: i32, session_state: i32, remain: Option<f64>) -> TelemetryFrame {
        TelemetryFrame {
            lap: 1,
            session_time: 10.0,
            lap_dist_pct: 0.1,
            player_car_idx: 3,
            player_car_position: 2,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct: vec![],
            car_idx_position: vec![],
            car_idx_on_pit_road: vec![],
            car_idx_track_surface: vec![],
            lap_last_lap_time: 0.0,
            session_info_update: 0,
            session_tick: 0,
            session_state,
            session_num,
            session_time_remain: remain,
            session_laps_remain: None,
            player_incident_count: 0,
            car_idx_lap_completed: vec![],
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

    fn fields(ev: &RaceEvent) -> (SessionKind, SessionPhase, SessionStateReason, bool, bool) {
        match ev {
            RaceEvent::SessionState {
                session_kind,
                phase,
                reason,
                synthetic,
                session_change_imminent,
                ..
            } => (
                *session_kind,
                *phase,
                *reason,
                *synthetic,
                *session_change_imminent,
            ),
            other => panic!("expected SESSION_STATE, got {other:?}"),
        }
    }

    #[test]
    fn first_sample_is_a_synthetic_connect_snapshot() {
        let mut t = SessionLifecycleTracker::new();
        let ev = t
            .update(&frame(0, 4, None), Some("Practice"))
            .expect("snapshot");
        let (kind, phase, reason, synthetic, imminent) = fields(&ev);
        assert_eq!(kind, SessionKind::Practice);
        assert_eq!(phase, SessionPhase::PracticeOpen);
        assert_eq!(reason, SessionStateReason::ConnectSnapshot);
        assert!(synthetic);
        assert!(!imminent);
        assert!(t.update(&frame(0, 4, None), Some("Practice")).is_none());
    }

    #[test]
    fn session_num_change_reports_qualifying_hotlaps() {
        let mut t = SessionLifecycleTracker::new();
        t.update(&frame(0, 4, None), Some("Practice"));
        let ev = t
            .update(&frame(1, 4, None), Some("Lone Qualify"))
            .expect("num change");
        let (kind, phase, reason, synthetic, _) = fields(&ev);
        assert_eq!(kind, SessionKind::Qualifying);
        assert_eq!(phase, SessionPhase::QualifyingHotlaps);
        assert_eq!(reason, SessionStateReason::SessionNumChanged);
        assert!(!synthetic);
        match ev {
            RaceEvent::SessionState {
                previous_session_num,
                previous_phase,
                ..
            } => {
                assert_eq!(previous_session_num, Some(0));
                assert_eq!(previous_phase, Some(SessionPhase::PracticeOpen));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn race_walks_gridding_formation_green_checkered() {
        let mut t = SessionLifecycleTracker::new();
        t.update(&frame(2, 1, None), Some("Race"));
        let phases: Vec<SessionPhase> = [2, 3, 4, 5, 6]
            .into_iter()
            .map(|state| {
                fields(
                    &t.update(&frame(2, state, None), Some("Race"))
                        .expect("change"),
                )
                .1
            })
            .collect();
        assert_eq!(
            phases,
            vec![
                SessionPhase::RaceGridding,
                SessionPhase::RaceFormation,
                SessionPhase::RaceGreen,
                SessionPhase::RaceCheckered,
                SessionPhase::CoolDown,
            ]
        );
    }

    #[test]
    fn unchanged_state_is_silent_and_type_resolution_is_reported_once() {
        let mut t = SessionLifecycleTracker::new();
        t.update(&frame(2, 4, None), None);
        assert!(t.update(&frame(2, 4, None), None).is_none());
        let ev = t
            .update(&frame(2, 4, None), Some("Race"))
            .expect("type resolved");
        assert_eq!(fields(&ev).2, SessionStateReason::SessionTypeResolved);
        assert!(t.update(&frame(2, 4, None), Some("Race")).is_none());
    }

    #[test]
    fn session_ending_fires_once_per_session_from_the_clock() {
        let mut t = SessionLifecycleTracker::new();
        t.update(&frame(1, 4, Some(600.0)), Some("Lone Qualify"));
        assert!(t
            .update(&frame(1, 4, Some(61.0)), Some("Lone Qualify"))
            .is_none());
        let ev = t
            .update(&frame(1, 4, Some(59.0)), Some("Lone Qualify"))
            .expect("ending");
        let (_, _, reason, _, imminent) = fields(&ev);
        assert_eq!(reason, SessionStateReason::SessionEnding);
        assert!(imminent);
        assert!(t
            .update(&frame(1, 4, Some(30.0)), Some("Lone Qualify"))
            .is_none());
        // Next session resets the latch.
        t.update(&frame(2, 1, Some(900.0)), Some("Race"));
        let ev = t
            .update(&frame(2, 1, Some(10.0)), Some("Race"))
            .expect("ending again");
        assert_eq!(fields(&ev).2, SessionStateReason::SessionEnding);
    }

    #[test]
    fn checkered_marks_change_imminent_without_a_clock() {
        let mut t = SessionLifecycleTracker::new();
        t.update(&frame(1, 4, None), Some("Open Qualify"));
        let ev = t
            .update(&frame(1, 5, None), Some("Open Qualify"))
            .expect("checkered");
        let (_, phase, _, _, imminent) = fields(&ev);
        assert_eq!(phase, SessionPhase::QualifyingClosed);
        assert!(imminent);
    }
}
