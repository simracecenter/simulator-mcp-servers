// SPDX-License-Identifier: GPL-3.0-or-later

use crate::race_event::RaceEvent;
use crate::telemetry_frame::TelemetryFrame;

pub struct TireDegradation {
    ema_lf: f32,
    ema_rf: f32,
    ema_lr: f32,
    ema_rr: f32,
    prev_ema: [Option<(f32, f32, f32, f32)>; 4],
    lap_emas: Vec<(u8, f32, f32, f32, f32, f32)>,
    alpha: f32,
    lap_count: u32,
    was_pit_lap: bool,
    last_max_slope: f32,
}

impl TireDegradation {
    pub fn new(alpha: f32) -> Self {
        Self {
            ema_lf: 0.0,
            ema_rf: 0.0,
            ema_lr: 0.0,
            ema_rr: 0.0,
            prev_ema: [None, None, None, None],
            lap_emas: Vec::new(),
            alpha,
            lap_count: 0,
            was_pit_lap: false,
            last_max_slope: 0.0,
        }
    }

    pub fn update_ema(&mut self, frame: &TelemetryFrame) {
        self.ema_lf = ema(self.ema_lf, frame.lf_temp_m, self.alpha);
        self.ema_rf = ema(self.ema_rf, frame.rf_temp_m, self.alpha);
        self.ema_lr = ema(self.ema_lr, frame.lr_temp_m, self.alpha);
        self.ema_rr = ema(self.ema_rr, frame.rr_temp_m, self.alpha);
    }

    pub fn on_lap_crossing(
        &mut self,
        lap: u8,
        session_time: f32,
        is_pit_lap: bool,
    ) -> Option<RaceEvent> {
        if is_pit_lap {
            self.was_pit_lap = true;
            self.lap_emas.clear();
            self.prev_ema = [None, None, None, None];
            return None;
        }
        if self.was_pit_lap {
            self.was_pit_lap = false;
            self.lap_emas.clear();
        }

        self.lap_emas.push((
            lap,
            session_time,
            self.ema_lf,
            self.ema_rf,
            self.ema_lr,
            self.ema_rr,
        ));
        self.lap_count += 1;
        if self.lap_emas.len() < 3 {
            return None;
        }
        while self.lap_emas.len() > 4 {
            self.lap_emas.remove(0);
        }

        let lf = slope_per_min(&self.lap_emas, |x| x.2)?;
        let rf = slope_per_min(&self.lap_emas, |x| x.3)?;
        let lr = slope_per_min(&self.lap_emas, |x| x.4)?;
        let rr = slope_per_min(&self.lap_emas, |x| x.5)?;
        self.last_max_slope = lf.max(rf).max(lr).max(rr);
        let hottest_corner = hottest([
            (self.ema_lf, "LF"),
            (self.ema_rf, "RF"),
            (self.ema_lr, "LR"),
            (self.ema_rr, "RR"),
        ]);
        Some(RaceEvent::TireDegradation {
            lap,
            session_time,
            lf_temp_c: self.ema_lf,
            rf_temp_c: self.ema_rf,
            lr_temp_c: self.ema_lr,
            rr_temp_c: self.ema_rr,
            lf_slope_c_per_min: lf,
            rf_slope_c_per_min: rf,
            lr_slope_c_per_min: lr,
            rr_slope_c_per_min: rr,
            hottest_corner,
        })
    }

    pub fn latest_max_slope(&self) -> f32 {
        self.last_max_slope
    }

    /// Returns true if real temperature telemetry has been received (EMA non-zero).
    /// When telemetry is unavailable, all EMA values stay at 0.0 and events
    /// should be suppressed to avoid emitting all-zero slope data.
    pub fn has_valid_data(&self) -> bool {
        self.ema_lf > 0.0 || self.ema_rf > 0.0 || self.ema_lr > 0.0 || self.ema_rr > 0.0
    }
}

fn ema(prev: f32, sample: f32, alpha: f32) -> f32 {
    if prev == 0.0 {
        sample
    } else {
        alpha * sample + (1.0 - alpha) * prev
    }
}

fn slope_per_min<F>(points: &[(u8, f32, f32, f32, f32, f32)], f: F) -> Option<f32>
where
    F: Fn(&(u8, f32, f32, f32, f32, f32)) -> f32,
{
    if points.len() < 2 {
        return None;
    }
    let xs: Vec<f32> = points.iter().map(|p| p.1 / 60.0).collect();
    let ys: Vec<f32> = points.iter().map(f).collect();
    crate::regression_store::ols_slope(&xs, &ys)
}

fn hottest(corners: [(f32, &str); 4]) -> String {
    corners
        .iter()
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap())
        .map(|(_, name)| (*name).to_string())
        .unwrap_or_else(|| "LF".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry_frame::TelemetryFrame;

    fn frame(t: f32, temp: f32) -> TelemetryFrame {
        TelemetryFrame {
            lap: 1,
            session_time: t,
            lap_dist_pct: 0.0,
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
            lf_temp_m: temp,
            rf_temp_m: temp,
            lr_temp_m: temp,
            rr_temp_m: temp,
            fuel_level: 0.0,
            throttle: 0.0,
            brake: 0.0,
            speed: 0.0,
        }
    }

    #[test]
    fn synthetic_temp_ramp_produces_expected_slope() {
        let mut tracker = TireDegradation::new(1.0);
        for lap in 1..=4 {
            tracker.update_ema(&frame((lap as f32 - 1.0) * 30.0, 80.0 + lap as f32));
            let event = tracker.on_lap_crossing(lap, lap as f32 * 30.0, false);
            if lap >= 3 {
                match event.expect("event") {
                    RaceEvent::TireDegradation {
                        lf_slope_c_per_min, ..
                    } => {
                        assert!((lf_slope_c_per_min - 2.0).abs() < 0.25);
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    #[test]
    fn newly_created_detector_returns_no_valid_data() {
        let tracker = TireDegradation::new(1.0);
        assert!(!tracker.has_valid_data());
    }

    #[test]
    fn detector_with_samples_returns_valid_data() {
        let mut tracker = TireDegradation::new(1.0);
        tracker.update_ema(&frame(0.0, 80.0));
        assert!(tracker.has_valid_data());
    }

    #[test]
    fn pit_cycle_reset() {
        let mut tracker = TireDegradation::new(1.0);
        tracker.update_ema(&frame(0.0, 80.0));
        tracker.on_lap_crossing(1, 60.0, false);
        tracker.update_ema(&frame(60.0, 81.0));
        assert!(tracker.on_lap_crossing(2, 120.0, true).is_none());
        tracker.update_ema(&frame(120.0, 82.0));
        assert!(tracker.on_lap_crossing(3, 180.0, false).is_none());
    }
}
