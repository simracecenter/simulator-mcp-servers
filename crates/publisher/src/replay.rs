// SPDX-License-Identifier: GPL-3.0-or-later

use crate::engine::NarrativeEngine;
use crate::race_event::RaceEvent;
use crate::telemetry_frame::TelemetryFrame;

const TARGET_CADENCE_S: f32 = 5.0;
const NURBURGRING_LAP_EST_S: f32 = 540.0;

/// Estimate the number of spatial anchor buckets from the frame stream.
///
/// Finds the first frame where `lap == 1` and the first frame where `lap == 2`,
/// then divides the elapsed time by `TARGET_CADENCE_S`. Falls back to the
/// Nürburgring lap estimate (540 s) if lap 2 is never seen.
pub fn compute_anchor_count(frames: &[TelemetryFrame]) -> usize {
    let lap1_start = frames.iter().find(|f| f.lap == 1).map(|f| f.session_time);
    let lap2_start = frames.iter().find(|f| f.lap == 2).map(|f| f.session_time);
    let duration = match (lap1_start, lap2_start) {
        (Some(s), Some(e)) => e - s,
        _ => NURBURGRING_LAP_EST_S,
    };
    ((duration / TARGET_CADENCE_S).floor() as usize).max(10)
}

/// Run the narrative engine over a pre-loaded slice of frames and return all
/// emitted events in chronological order.
pub fn replay_frames(frames: &[TelemetryFrame]) -> Vec<RaceEvent> {
    let anchor_count = compute_anchor_count(frames);
    let mut engine = NarrativeEngine::new(anchor_count);
    frames
        .iter()
        .flat_map(|f| engine.process_frame(f))
        .collect()
}
