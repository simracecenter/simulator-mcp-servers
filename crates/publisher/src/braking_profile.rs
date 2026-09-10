// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;

use crate::{race_event::RaceEvent, telemetry_frame::TelemetryFrame};

#[derive(Clone)]
struct BrakeSample {
    time: f32,
    ldp: f32,
    throttle: f32,
    brake: f32,
    speed: f32,
}

pub struct BrakeWindow {
    anchor_bucket: u8,
    lap: u8,
    samples: Vec<BrakeSample>,
}

pub struct BrakingProfileDetector {
    pub in_window: Option<BrakeWindow>,
    emitted_this_lap: HashSet<u8>,
    heavy_braking_anchors: Vec<u8>,
    current_lap: Option<u8>,
}

impl BrakingProfileDetector {
    pub fn new() -> Self {
        Self {
            in_window: None,
            emitted_this_lap: HashSet::new(),
            heavy_braking_anchors: Vec::new(),
            current_lap: None,
        }
    }

    pub fn with_anchors(anchors: Vec<u8>) -> Self {
        let mut detector = Self::new();
        detector.heavy_braking_anchors = anchors;
        detector
    }

    pub fn update(&mut self, frame: &TelemetryFrame, anchor_count: usize) -> Option<RaceEvent> {
        if self.current_lap != Some(frame.lap) {
            self.current_lap = Some(frame.lap);
            self.emitted_this_lap.clear();
        }
        if anchor_count == 0 {
            return None;
        }
        let bucket = ((frame.lap_dist_pct * anchor_count as f32) as usize % anchor_count) as u8;
        let heavy = self.heavy_braking_anchors.contains(&bucket);
        match &mut self.in_window {
            None if heavy && frame.brake > 0.0 && !self.emitted_this_lap.contains(&bucket) => {
                self.in_window = Some(BrakeWindow {
                    anchor_bucket: bucket,
                    lap: frame.lap,
                    samples: vec![BrakeSample {
                        time: frame.session_time,
                        ldp: frame.lap_dist_pct,
                        throttle: frame.throttle,
                        brake: frame.brake,
                        speed: frame.speed,
                    }],
                });
                None
            }
            Some(window) => {
                let same_bucket = bucket == window.anchor_bucket;
                if same_bucket || frame.brake > 0.0 {
                    window.samples.push(BrakeSample {
                        time: frame.session_time,
                        ldp: frame.lap_dist_pct,
                        throttle: frame.throttle,
                        brake: frame.brake,
                        speed: frame.speed,
                    });
                    None
                } else {
                    let event = build_event(window, frame.session_time);
                    self.emitted_this_lap.insert(window.anchor_bucket);
                    self.in_window = None;
                    event
                }
            }
            _ => None,
        }
    }
}

fn build_event(window: &BrakeWindow, session_time: f32) -> Option<RaceEvent> {
    let first = window.samples.first()?;
    let last = window.samples.last()?;
    let peak_brake_pct = window.samples.iter().map(|s| s.brake).fold(0.0, f32::max);
    let min_speed_mps = window
        .samples
        .iter()
        .map(|s| s.speed)
        .fold(f32::MAX, f32::min);
    let entry_speed_mps = first.speed;
    let throttle_release_pct = window
        .samples
        .iter()
        .find(|s| s.throttle < 0.05)
        .map(|s| s.ldp)
        .unwrap_or(first.ldp);
    let braking_energy = window
        .samples
        .windows(2)
        .map(|pair| {
            let dt = (pair[1].time - pair[0].time).max(0.0);
            pair[0].brake * pair[0].speed * dt
        })
        .sum::<f32>();
    Some(RaceEvent::BrakingProfile {
        lap: window.lap,
        session_time,
        anchor_bucket: window.anchor_bucket,
        brake_point_pct: first.ldp,
        brake_release_pct: last.ldp,
        peak_brake_pct,
        braking_energy,
        entry_speed_mps,
        min_speed_mps,
        throttle_release_pct,
    })
}

impl Default for BrakingProfileDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(ldp: f32, t: f32, throttle: f32, brake: f32, speed: f32) -> TelemetryFrame {
        TelemetryFrame {
            lap: 1,
            session_time: t,
            lap_dist_pct: ldp,
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
    fn braking_energy_accumulates() {
        let mut detector = BrakingProfileDetector::with_anchors(vec![2]);
        assert!(detector
            .update(&frame(0.20, 0.0, 0.0, 0.5, 60.0), 10)
            .is_none());
        assert!(detector
            .update(&frame(0.21, 0.5, 0.0, 0.8, 50.0), 10)
            .is_none());
        let event = detector
            .update(&frame(0.31, 1.0, 0.3, 0.0, 45.0), 10)
            .expect("braking event");
        match event {
            RaceEvent::BrakingProfile { braking_energy, .. } => assert!(braking_energy > 0.0),
            _ => unreachable!(),
        }
    }
}
