// SPDX-License-Identifier: GPL-3.0-or-later

use crate::{race_event::RaceEvent, telemetry_frame::TelemetryFrame};

enum CoastState {
    Idle,
    Coasting {
        start_time: f32,
        start_lap_dist_pct: f32,
        start_speed_mps: f32,
    },
}

pub struct LiftCoastDetector {
    state: CoastState,
    min_duration_s: f32,
    min_speed_mps: f32,
}

impl LiftCoastDetector {
    pub fn new() -> Self {
        Self {
            state: CoastState::Idle,
            min_duration_s: 1.0,
            min_speed_mps: 25.0,
        }
    }

    pub fn update(&mut self, frame: &TelemetryFrame) -> Option<RaceEvent> {
        match self.state {
            CoastState::Idle => {
                if frame.throttle < 0.05 && frame.brake < 0.05 && frame.speed > self.min_speed_mps {
                    self.state = CoastState::Coasting {
                        start_time: frame.session_time,
                        start_lap_dist_pct: frame.lap_dist_pct,
                        start_speed_mps: frame.speed,
                    };
                }
                None
            }
            CoastState::Coasting {
                start_time,
                start_lap_dist_pct,
                start_speed_mps,
            } => {
                let duration = frame.session_time - start_time;
                if frame.brake > 0.05 || frame.throttle > 0.2 {
                    self.state = CoastState::Idle;
                    if duration >= self.min_duration_s {
                        return Some(RaceEvent::FuelSavingTechnique {
                            lap: frame.lap,
                            session_time: frame.session_time,
                            coast_duration_s: duration,
                            coast_start_lap_dist_pct: start_lap_dist_pct,
                            coast_start_speed_mps: start_speed_mps,
                        });
                    }
                }
                None
            }
        }
    }
}

impl Default for LiftCoastDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(t: f32, throttle: f32, brake: f32, speed: f32) -> TelemetryFrame {
        TelemetryFrame {
            lap: 2,
            session_time: t,
            lap_dist_pct: 0.5,
            player_car_idx: 0,
            player_car_position: 1,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct: vec![],
            car_idx_position: vec![],
            car_idx_on_pit_road: vec![],
            car_idx_track_surface: vec![],
            lap_last_lap_time: 0.0,
            session_info_update: 0,
            session_tick: 0,
            session_state: 4,
            session_num: 0,
            session_time_remain: None,
            session_laps_remain: None,
            player_incident_count: 0,
            car_idx_lap_completed: vec![],
            lf_temp_m: 0.0,
            rf_temp_m: 0.0,
            lr_temp_m: 0.0,
            rr_temp_m: 0.0,
            fuel_level: 0.0,
            throttle,
            brake,
            speed,
        }
    }

    #[test]
    fn emits_after_valid_coast() {
        let mut detector = LiftCoastDetector::new();
        detector.update(&frame(0.0, 0.0, 0.0, 50.0));
        let event = detector.update(&frame(1.5, 0.0, 0.3, 40.0));
        assert!(matches!(event, Some(RaceEvent::FuelSavingTechnique { .. })));
    }

    #[test]
    fn ignores_short_blip() {
        let mut detector = LiftCoastDetector::new();
        detector.update(&frame(0.0, 0.0, 0.0, 50.0));
        let event = detector.update(&frame(0.5, 0.0, 0.4, 40.0));
        assert!(event.is_none());
    }
}
