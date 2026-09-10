// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::hash_map::Entry;
use std::collections::HashMap;

const LAP_TIME_FALLBACK_S: f32 = 540.0;

/// Tracks lap start times and completed-lap durations.
pub struct LapTimer {
    starts: HashMap<u8, f32>, // lap → first session_time seen
    times: HashMap<u8, f32>,  // lap → duration in seconds
}

impl LapTimer {
    pub fn new() -> Self {
        LapTimer {
            starts: HashMap::new(),
            times: HashMap::new(),
        }
    }

    /// Call once per frame. On the first time lap `n` is seen, the duration
    /// of lap `n-1` is finalised (if we have a start time for it).
    pub fn update(&mut self, lap: u8, t: f32) {
        if let Entry::Vacant(slot) = self.starts.entry(lap) {
            slot.insert(t);
            if lap > 0 {
                let prev = lap - 1;
                if let Some(&start) = self.starts.get(&prev) {
                    self.times.entry(prev).or_insert(t - start);
                }
            }
        }
    }

    /// Most recent completed lap time, or the Nürburgring fallback (540 s).
    pub fn best_estimate(&self) -> f32 {
        self.times
            .values()
            .copied()
            .reduce(f32::max)
            .unwrap_or(LAP_TIME_FALLBACK_S)
    }

    /// `true` once at least one full lap has been timed, i.e. `best_estimate`
    /// is a measurement rather than the fallback.
    pub fn has_completed_lap(&self) -> bool {
        !self.times.is_empty()
    }

    /// Completed lap time for a specific lap number, if available.
    pub fn completed(&self, lap: u8) -> Option<f32> {
        self.times.get(&lap).copied()
    }
}

impl Default for LapTimer {
    fn default() -> Self {
        Self::new()
    }
}
