// SPDX-License-Identifier: GPL-3.0-or-later

use crate::battle_state::MAX_BATTLE_GAP_S;
use crate::telemetry_frame::TelemetryFrame;

/// Return up to `n` cars that are **ahead** of the player in race order,
/// sorted by ascending gap (nearest first).
///
/// Gap is computed from `lap_dist_pct` difference scaled by `lap_time_s`.
/// Start/finish wrap: a car that has just crossed S/F has a smaller `ldp`
/// than the player; adding 1.0 restores the correct positive gap.
pub fn find_cars_ahead(frame: &TelemetryFrame, lap_time_s: f32, n: usize) -> Vec<(u8, f32)> {
    let player_ldp = frame.lap_dist_pct;
    let player_pos = frame.player_car_position;
    let player_idx = frame.player_car_idx;

    let mut gaps: Vec<(u8, f32)> = frame
        .car_idx_lap_dist_pct
        .iter()
        .enumerate()
        .filter_map(|(i, &car_ldp)| {
            let car_idx = i as u8;
            let position = frame.car_idx_position.get(i).copied().unwrap_or(0);
            let on_pit = frame.car_idx_on_pit_road.get(i).copied().unwrap_or(false);

            if car_idx == player_idx {
                return None;
            } // skip self
            if position == 0 {
                return None;
            } // inactive slot
            if on_pit {
                return None;
            } // in pit lane
            if car_ldp < -0.5 {
                return None;
            } // iRacing sentinel
            if position >= player_pos {
                return None;
            } // not ahead in race order

            let mut diff = car_ldp - player_ldp;
            if diff < -0.5 {
                diff += 1.0;
            } // S/F line wrap

            let gap_s = diff * lap_time_s;
            if gap_s < 0.0 {
                return None;
            } // race order / track position disagree
            if gap_s > MAX_BATTLE_GAP_S {
                return None;
            } // too far away

            Some((car_idx, gap_s))
        })
        .collect();

    gaps.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    gaps.truncate(n);
    gaps
}

/// Return up to `n` cars that are **behind** the player in race order,
/// sorted by ascending gap (nearest first).
pub fn find_cars_behind(frame: &TelemetryFrame, lap_time_s: f32, n: usize) -> Vec<(u8, f32)> {
    let player_ldp = frame.lap_dist_pct;
    let player_pos = frame.player_car_position;
    let player_idx = frame.player_car_idx;

    let mut gaps: Vec<(u8, f32)> = frame
        .car_idx_lap_dist_pct
        .iter()
        .enumerate()
        .filter_map(|(i, &car_ldp)| {
            let car_idx = i as u8;
            let position = frame.car_idx_position.get(i).copied().unwrap_or(0);
            let on_pit = frame.car_idx_on_pit_road.get(i).copied().unwrap_or(false);

            if car_idx == player_idx {
                return None;
            }
            if position == 0 {
                return None;
            }
            if on_pit {
                return None;
            }
            if car_ldp < -0.5 {
                return None;
            }
            if position <= player_pos {
                return None;
            } // not behind in race order

            let mut diff = player_ldp - car_ldp;
            if diff < 0.0 {
                diff += 1.0;
            } // S/F line wrap

            let gap_s = diff * lap_time_s;
            if gap_s < 0.0 {
                return None;
            }
            if gap_s > MAX_BATTLE_GAP_S {
                return None;
            }

            Some((car_idx, gap_s))
        })
        .collect();

    gaps.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    gaps.truncate(n);
    gaps
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal TelemetryFrame for testing. Player is car_idx=0, position=5.
    fn frame(player_ldp: f32, cars: &[(u8, f32, u8)]) -> TelemetryFrame {
        // cars: (car_idx, ldp, position)
        let n = cars
            .iter()
            .map(|c| c.0 as usize + 1)
            .max()
            .unwrap_or(1)
            .max(1);
        let mut car_idx_lap_dist_pct = vec![-1.0f32; n];
        let mut car_idx_position = vec![0u8; n];
        let car_idx_on_pit_road = vec![false; n];

        // Place player at index 0
        car_idx_lap_dist_pct[0] = player_ldp;
        car_idx_position[0] = 5;

        for &(idx, ldp, pos) in cars {
            car_idx_lap_dist_pct[idx as usize] = ldp;
            car_idx_position[idx as usize] = pos;
        }

        TelemetryFrame {
            lap: 1,
            session_time: 0.0,
            lap_dist_pct: player_ldp,
            player_car_idx: 0,
            player_car_position: 5,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct,
            car_idx_position,
            car_idx_on_pit_road,
            car_idx_track_surface: vec![0; n],
            lap_last_lap_time: 0.0,
            session_info_update: 0,
            session_tick: 0,
            session_state: 0,
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
            throttle: 0.0,
            brake: 0.0,
            speed: 0.0,
        }
    }

    #[test]
    fn ahead_normal_gap() {
        // Player at 0.5, car 1 at 0.7 (position 3 = ahead of player at 5)
        // diff=0.2, lap_time=10s → gap=2.0s (within MAX_BATTLE_GAP_S=5s)
        let f = frame(0.5, &[(1, 0.7, 3)]);
        let gaps = find_cars_ahead(&f, 10.0, 5);
        assert_eq!(gaps.len(), 1);
        assert_eq!(gaps[0].0, 1);
        assert!(
            (gaps[0].1 - 2.0).abs() < 0.01,
            "expected gap 2.0s, got {}",
            gaps[0].1
        );
    }

    #[test]
    fn ahead_start_finish_wrap() {
        // Player at ldp 0.9, car 1 at ldp 0.1 (crossed S/F, position 3 = ahead)
        // diff = 0.1 - 0.9 = -0.8 → wrap → 0.2 → gap = 0.2 * 10 = 2.0s
        let f = frame(0.9, &[(1, 0.1, 3)]);
        let gaps = find_cars_ahead(&f, 10.0, 5);
        assert_eq!(gaps.len(), 1);
        assert!(
            (gaps[0].1 - 2.0).abs() < 0.01,
            "expected 2.0s after S/F wrap, got {}",
            gaps[0].1
        );
    }

    #[test]
    fn behind_normal_gap() {
        // Player at 0.6, car 1 at 0.4 (position 7 = behind player at 5)
        // diff = 0.6 - 0.4 = 0.2 → gap = 0.2 * 10 = 2.0s
        let f = frame(0.6, &[(1, 0.4, 7)]);
        let gaps = find_cars_behind(&f, 10.0, 5);
        assert_eq!(gaps.len(), 1);
        assert!(
            (gaps[0].1 - 2.0).abs() < 0.01,
            "expected gap 2.0s, got {}",
            gaps[0].1
        );
    }

    #[test]
    fn car_on_pit_excluded() {
        let mut f = frame(0.5, &[(1, 0.7, 3)]);
        f.car_idx_on_pit_road[1] = true;
        let gaps = find_cars_ahead(&f, 10.0, 5);
        assert!(gaps.is_empty(), "pit-road car must be excluded");
    }

    #[test]
    fn negative_gap_excluded() {
        // Car 1 is ahead in race order (position 3) but physically slightly
        // behind on lap_dist_pct (e.g. lapped-traffic edge case): diff = -0.1
        // does not trip the -0.5 wrap threshold and would yield a negative gap.
        let f = frame(0.5, &[(1, 0.4, 3)]);
        let gaps = find_cars_ahead(&f, 10.0, 5);
        assert!(gaps.is_empty(), "negative gaps must be excluded");
    }

    #[test]
    fn beyond_max_gap_excluded() {
        // diff = 0.8, lap_time=10 → gap = 8.0s > MAX_BATTLE_GAP_S=5.0
        let f = frame(0.1, &[(1, 0.9, 3)]);
        let gaps = find_cars_ahead(&f, 10.0, 5);
        assert!(
            gaps.is_empty(),
            "car beyond MAX_BATTLE_GAP_S must be excluded"
        );
    }
}
