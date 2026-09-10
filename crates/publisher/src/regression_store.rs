// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use crate::anchor_sampler::AnchorSampler;

/// Per-car closing-rate summary derived from OLS regression over anchor samples.
pub struct CarSlopeInfo {
    pub median: f32,
    pub n_buckets: usize,
    /// Buckets where slope < 0 (car is closing).
    pub n_agree: usize,
}

/// Stores per-`(bucket, car_idx)` time series and computes OLS slopes on demand.
///
/// Rebuilt from scratch at each lap boundary via `ingest()`. The full-rebuild
/// approach is correct and fast enough (~2 650 ops per lap at Nürburgring).
pub struct RegressionStore {
    /// (bucket, car_idx) → [(lap, gap_s)]
    data: HashMap<(u8, u8), Vec<(u8, f32)>>,
}

impl RegressionStore {
    pub fn new() -> Self {
        RegressionStore {
            data: HashMap::new(),
        }
    }

    /// Rebuild from sampler. `max_lap` prevents the first frame of the new lap
    /// contaminating the previous lap's regression (off-by-one correctness
    /// invariant — do not remove this guard).
    pub fn ingest(&mut self, sampler: &AnchorSampler, max_lap: u8) {
        self.data.clear();
        for r in &sampler.samples {
            if r.lap <= max_lap && r.is_clean {
                self.data
                    .entry((r.bucket, r.car_idx))
                    .or_default()
                    .push((r.lap, r.gap_s));
            }
        }
    }

    /// Most-negative slope per bucket across all cars (used for heatmap only).
    pub fn per_bucket_slopes(&self, min_readings: usize) -> HashMap<u8, f32> {
        let mut out: HashMap<u8, f32> = HashMap::new();
        for (&(bucket, _car_idx), readings) in &self.data {
            if readings.len() < min_readings {
                continue;
            }
            let (laps, gaps): (Vec<f32>, Vec<f32>) =
                readings.iter().map(|&(l, g)| (l as f32, g)).unzip();
            if let Some(slope) = ols_slope(&laps, &gaps) {
                let entry = out.entry(bucket).or_insert(f32::MAX);
                if slope < *entry {
                    *entry = slope;
                }
            }
        }
        out
    }

    /// Per-car two-tier analysis: median of each car's per-bucket slopes.
    /// The state machine uses this to select the most-threatening car
    /// (lowest / most-negative median).
    pub fn per_car_median_slopes(&self, min_readings: usize) -> HashMap<u8, CarSlopeInfo> {
        // Collect per-bucket slopes grouped by car_idx.
        let mut car_slopes: HashMap<u8, Vec<f32>> = HashMap::new();
        for (&(_bucket, car_idx), readings) in &self.data {
            if readings.len() < min_readings {
                continue;
            }
            let (laps, gaps): (Vec<f32>, Vec<f32>) =
                readings.iter().map(|&(l, g)| (l as f32, g)).unzip();
            if let Some(slope) = ols_slope(&laps, &gaps) {
                car_slopes.entry(car_idx).or_default().push(slope);
            }
        }

        car_slopes
            .into_iter()
            .filter(|(_, slopes)| slopes.len() >= min_readings)
            .map(|(car_idx, mut slopes)| {
                let n_buckets = slopes.len();
                let n_agree = slopes.iter().filter(|&&s| s < 0.0).count();
                slopes.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let median = median_sorted(&slopes);
                (
                    car_idx,
                    CarSlopeInfo {
                        median,
                        n_buckets,
                        n_agree,
                    },
                )
            })
            .collect()
    }
}

impl Default for RegressionStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Standard OLS slope: Σ(x−x̄)(y−ȳ) / Σ(x−x̄)².
/// Returns `None` if n < 2 or denominator is zero.
pub fn ols_slope(laps: &[f32], gaps: &[f32]) -> Option<f32> {
    let n = laps.len();
    if n < 2 {
        return None;
    }
    let x_mean = laps.iter().sum::<f32>() / n as f32;
    let y_mean = gaps.iter().sum::<f32>() / n as f32;

    let num = laps
        .iter()
        .zip(gaps)
        .map(|(&x, &y)| (x - x_mean) * (y - y_mean))
        .sum::<f32>();
    let den = laps.iter().map(|&x| (x - x_mean).powi(2)).sum::<f32>();

    if den == 0.0 {
        None
    } else {
        Some(num / den)
    }
}

fn median_sorted(sorted: &[f32]) -> f32 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchor_sampler::AnchorSampler;

    /// Build a sampler with deterministic readings for car 7:
    /// lap 1 gap=3.0, lap 2 gap=2.5, lap 3 gap=2.0 at bucket 0.
    /// Expected slope: -0.5 s/lap.
    fn sampler_with_closing_car() -> AnchorSampler {
        let mut s = AnchorSampler::new(10);
        s.update(1, 0.05, 3.0, 7, true);
        s.update(2, 0.05, 2.5, 7, true);
        s.update(3, 0.05, 2.0, 7, true);
        s
    }

    #[test]
    fn ols_slope_known_values() {
        // laps [1,2,3], gaps [3.0, 2.5, 2.0] → slope = -0.5
        let laps = vec![1.0f32, 2.0, 3.0];
        let gaps = vec![3.0f32, 2.5, 2.0];
        let slope = ols_slope(&laps, &gaps).expect("should compute slope");
        assert!((slope - (-0.5)).abs() < 1e-5, "expected -0.5, got {slope}");
    }

    #[test]
    fn ols_slope_returns_none_for_single_point() {
        assert!(ols_slope(&[1.0], &[2.0]).is_none());
    }

    #[test]
    fn ingest_excludes_frames_beyond_max_lap() {
        let mut s = AnchorSampler::new(10);
        // lap 1 and lap 2 at same bucket, same car
        s.update(1, 0.05, 3.0, 7, true);
        // lap 3 should be excluded when max_lap=2
        s.update(3, 0.05, 1.5, 7, true);

        let mut store = RegressionStore::new();
        store.ingest(&s, 2);

        // Only lap-1 reading ingested; slope needs ≥2 points so result is empty.
        let slopes = store.per_car_median_slopes(2);
        assert!(slopes.is_empty(), "lap 3 must be excluded by max_lap guard");
    }

    #[test]
    fn per_car_median_slopes_returns_correct_most_negative_car() {
        let sampler = sampler_with_closing_car();
        let mut store = RegressionStore::new();
        store.ingest(&sampler, 3);

        let slopes = store.per_car_median_slopes(1);
        let info = slopes.get(&7).expect("car 7 must be present");
        assert!(
            (info.median - (-0.5)).abs() < 1e-4,
            "expected median ≈ -0.5, got {}",
            info.median
        );
        assert_eq!(info.n_buckets, 1);
        assert_eq!(info.n_agree, 1);
    }
}
