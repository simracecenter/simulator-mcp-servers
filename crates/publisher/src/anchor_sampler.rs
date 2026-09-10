// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashSet;

/// A single gap reading captured at a spatial anchor crossing.
#[derive(Debug, Clone)]
pub struct AnchorReading {
    pub lap: u8,
    pub bucket: u8,
    pub gap_s: f32,
    pub car_idx: u8,
    /// false = yellow flag active or pit lap — exclude from regression
    pub is_clean: bool,
}

/// Captures the **first** gap reading per `(lap, bucket, car_idx)` as the
/// player crosses each spatial anchor bucket. Bucket is computed from
/// `lap_dist_pct` (physical track position), not elapsed time.
pub struct AnchorSampler {
    n_buckets: usize,
    seen: HashSet<(u8, u8, u8)>, // (lap, bucket, car_idx)
    pub samples: Vec<AnchorReading>,
}

impl AnchorSampler {
    pub fn new(n_buckets: usize) -> Self {
        AnchorSampler {
            n_buckets,
            seen: HashSet::new(),
            samples: Vec::new(),
        }
    }

    /// Number of spatial buckets this sampler was configured with.
    pub fn n_buckets(&self) -> usize {
        self.n_buckets
    }

    /// Feed one frame. Returns `true` if a new anchor sample was captured.
    ///
    /// `ldp` is `lap_dist_pct` (0.0–1.0). The bucket key is derived from
    /// physical track position so gaps are comparable across laps.
    pub fn update(&mut self, lap: u8, ldp: f32, gap_s: f32, car_idx: u8, is_clean: bool) -> bool {
        let bucket = ((ldp * self.n_buckets as f32) as usize % self.n_buckets) as u8;
        let key = (lap, bucket, car_idx);
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key);
        self.samples.push(AnchorReading {
            lap,
            bucket,
            gap_s,
            car_idx,
            is_clean,
        });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_crossing_captured() {
        let mut s = AnchorSampler::new(10);
        assert!(s.update(1, 0.05, 2.0, 3, true));
        assert_eq!(s.samples.len(), 1);
    }

    #[test]
    fn duplicate_crossing_ignored() {
        let mut s = AnchorSampler::new(10);
        assert!(s.update(1, 0.05, 2.0, 3, true));
        assert!(!s.update(1, 0.05, 1.5, 3, true));
        assert_eq!(
            s.samples.len(),
            1,
            "second call with same key must not add a sample"
        );
    }

    #[test]
    fn different_car_idx_tracked_independently() {
        let mut s = AnchorSampler::new(10);
        assert!(s.update(1, 0.05, 2.0, 3, true));
        assert!(s.update(1, 0.05, 3.0, 7, true));
        assert_eq!(
            s.samples.len(),
            2,
            "each car_idx gets its own sample per bucket"
        );
    }
}
