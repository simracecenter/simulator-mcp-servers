// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use serde::Serialize;

use crate::regression_store::CarSlopeInfo;

// ── Constants ────────────────────────────────────────────────────────────────

pub const PUSH_SLOPE_THRESHOLD: f32 = -0.05;
pub const ATTACK_SLOPE_THRESHOLD: f32 = -0.10;
pub const MIN_PUSH_READINGS: usize = 2;
pub const MIN_ATTACK_READINGS: usize = 3;
pub const MAX_BATTLE_GAP_S: f32 = 5.0;
pub const SCAN_FIELD_POSITIONS: usize = 5;
pub const CLOSE_APPROACH_THRESH_S: f32 = 1.5;
pub const CLOSE_APPROACH_MIN_FRAMES: u32 = 5;

// ── BattleState ──────────────────────────────────────────────────────────────

/// Race-threat state for a single direction (forward or defensive).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BattleState {
    Idle,
    Tracking,
    Push,
    AttackSetup,
}

// ── Supporting types ─────────────────────────────────────────────────────────

/// Enriched slope summary emitted alongside a `RaceEvent`.
#[derive(Debug, Clone, Serialize)]
pub struct SlopeInfo {
    pub median_slope: f32,
    pub anchors_qualifying: usize,
    pub anchors_agreeing: usize,
    pub hotspot_lap_dist_pct: f32,
}

/// Output of `classify()`.
pub struct ClassifyResult {
    pub state: BattleState,
    /// `car_idx` of the most-threatening opponent, if any.
    pub threat_car: Option<u8>,
    pub slope_info: Option<SlopeInfo>,
}

// ── classify() ───────────────────────────────────────────────────────────────

/// Determine the new `BattleState` given the current regression output.
///
/// Transition rules (evaluated in priority order):
/// 1. pit lap or no data           → `Idle`
/// 2. slope ≤ -0.10, buckets ≥ 3,
///    and slope accelerating       → `AttackSetup`
/// 3. slope ≤ -0.05, buckets ≥ 2  → `Push`
/// 4. any car present              → `Tracking`
/// 5. fallthrough                  → `Idle`
pub fn classify(
    car_medians: &HashMap<u8, CarSlopeInfo>,
    per_bucket: &HashMap<u8, f32>,
    anchor_count: usize,
    prev_slope: Option<f32>,
    is_pit_lap: bool,
) -> ClassifyResult {
    if is_pit_lap || car_medians.is_empty() {
        return ClassifyResult {
            state: BattleState::Idle,
            threat_car: None,
            slope_info: None,
        };
    }

    // Most-threatening car = lowest (most-negative) median slope.
    let (&threat_idx, threat_info) = car_medians
        .iter()
        .min_by(|a, b| a.1.median.partial_cmp(&b.1.median).unwrap())
        .unwrap(); // safe: car_medians is non-empty

    let threat_slope = threat_info.median;
    let n_buckets = threat_info.n_buckets;

    // Hotspot = bucket with the most-negative per-bucket slope.
    let hotspot_lap_dist_pct = per_bucket
        .iter()
        .min_by(|a, b| a.1.partial_cmp(b.1).unwrap())
        .map(|(&bucket, _)| (bucket as f32 + 0.5) / anchor_count as f32)
        .unwrap_or(0.0);

    let slope_info = SlopeInfo {
        median_slope: threat_slope,
        anchors_qualifying: n_buckets,
        anchors_agreeing: threat_info.n_agree,
        hotspot_lap_dist_pct,
    };

    // ATTACK_SETUP: slope is accelerating (more negative than last lap).
    let state = if threat_slope <= ATTACK_SLOPE_THRESHOLD
        && n_buckets >= MIN_ATTACK_READINGS
        && prev_slope.is_some_and(|p| threat_slope < p)
    {
        BattleState::AttackSetup
    } else if threat_slope <= PUSH_SLOPE_THRESHOLD && n_buckets >= MIN_PUSH_READINGS {
        BattleState::Push
    } else {
        BattleState::Tracking
    };

    ClassifyResult {
        state,
        threat_car: Some(threat_idx),
        slope_info: Some(slope_info),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::regression_store::CarSlopeInfo;

    fn single_car(median: f32, n_buckets: usize) -> HashMap<u8, CarSlopeInfo> {
        let mut m = HashMap::new();
        m.insert(
            7,
            CarSlopeInfo {
                median,
                n_buckets,
                n_agree: n_buckets,
            },
        );
        m
    }

    #[test]
    fn classify_push_when_slope_at_threshold() {
        let medians = single_car(-0.06, 2);
        let r = classify(&medians, &HashMap::new(), 10, None, false);
        assert_eq!(r.state, BattleState::Push);
    }

    #[test]
    fn classify_attack_setup_when_slope_accelerating() {
        // slope -0.11 < prev_slope -0.10 (more negative = accelerating close)
        let medians = single_car(-0.11, 3);
        let r = classify(&medians, &HashMap::new(), 10, Some(-0.10), false);
        assert_eq!(r.state, BattleState::AttackSetup);
    }

    #[test]
    fn classify_tracking_not_push_when_slope_shallow() {
        let medians = single_car(-0.03, 2);
        let r = classify(&medians, &HashMap::new(), 10, None, false);
        assert_eq!(r.state, BattleState::Tracking);
    }

    #[test]
    fn classify_idle_on_pit_lap() {
        let medians = single_car(-0.20, 5);
        let r = classify(&medians, &HashMap::new(), 10, None, true);
        assert_eq!(r.state, BattleState::Idle);
    }

    #[test]
    fn classify_push_not_attack_when_slope_not_accelerating() {
        // slope -0.11, prev_slope -0.12 → slope is LESS negative (decelerating) → stays Push
        let medians = single_car(-0.11, 3);
        let r = classify(&medians, &HashMap::new(), 10, Some(-0.12), false);
        assert_eq!(r.state, BattleState::Push);
    }
}
