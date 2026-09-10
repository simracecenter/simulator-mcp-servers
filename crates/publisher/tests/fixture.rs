// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::HashMap;

use publisher::anchor_sampler::AnchorSampler;
use publisher::battle_state::CLOSE_APPROACH_THRESH_S;
use publisher::car_registry::{CarRegistry, CarState};
use publisher::horizon::HorizonDetector;
use publisher::race_event::RaceEvent;
use publisher::regression_store::CarSlopeInfo;
use publisher::replay::replay_frames;
use publisher::telemetry_frame::TelemetryFrame;

fn load_fixture() -> Vec<TelemetryFrame> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/data/test_fixture.jsonl");
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .filter(|l| !l.is_empty())
        .enumerate()
        .map(|(i, line)| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {}: {e}", i + 1))
        })
        .collect()
}

#[test]
fn lap3_push_car7() {
    let frames = load_fixture();
    if frames.is_empty() {
        return;
    }
    let events = replay_frames(&frames);
    let battle_closing = events.iter().find(|e| {
        matches!(
            e,
            RaceEvent::BattleClosing {
                opponent_car_idx: 7,
                ..
            }
        )
    });
    assert!(
        battle_closing.is_some(),
        "expected a BATTLE_CLOSING event for opponent_car_idx=7"
    );
    if let Some(RaceEvent::BattleClosing {
        lap, slope_info, ..
    }) = battle_closing
    {
        assert_eq!(
            *lap, 3,
            "BATTLE_CLOSING should first be emitted at lap 3, got lap {lap}"
        );
        assert!((slope_info.median_slope - (-0.4998)).abs() < 0.01);
    }
}

#[test]
fn lap4_attack_setup_car7() {
    let frames = load_fixture();
    if frames.is_empty() {
        return;
    }
    let events = replay_frames(&frames);
    let attack = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                RaceEvent::BattleClosing {
                    opponent_car_idx: 7,
                    ..
                }
            )
        })
        .nth(1);
    assert!(
        attack.is_some(),
        "expected a second BATTLE_CLOSING event for opponent_car_idx=7"
    );
    if let Some(RaceEvent::BattleClosing {
        lap, slope_info, ..
    }) = attack
    {
        assert_eq!(*lap, 4);
        assert!((slope_info.median_slope - (-0.5999)).abs() < 0.01);
    }
}

#[test]
fn close_approach_car7() {
    let frames = load_fixture();
    if frames.is_empty() {
        return;
    }
    let events = replay_frames(&frames);
    let close = events.iter().find(|e| {
        matches!(
            e,
            RaceEvent::BattleEngaged {
                opponent_car_idx: 7,
                ..
            }
        )
    });
    assert!(close.is_some());
    if let Some(RaceEvent::BattleEngaged { lap, gap_s, .. }) = close {
        assert!(*lap >= 5 && *lap <= 7);
        assert!(*gap_s < CLOSE_APPROACH_THRESH_S);
    }
}

fn seeded_car(car_idx: u8, position: u8, lap: i32, ldp: f32, speed: f32) -> CarState {
    CarState {
        car_idx,
        car_number: car_idx.to_string(),
        driver_name: car_idx.to_string(),
        car_class_id: 1,
        current_position: position,
        current_lap: lap,
        lap_dist_pct: ldp,
        on_pit_road: false,
        track_surface: 0,
        last_lap_time_s: 0.0,
        best_lap_time_s: 0.0,
        speed_ema_mps: speed,
        sampler: AnchorSampler::new(20),
        opponent_history: Vec::new(),
    }
}

#[test]
fn global_registry_horizon_closing() {
    let mut registry = CarRegistry::new();
    registry.insert(seeded_car(1, 4, 10, 0.20, 60.0), 0);
    registry.insert(seeded_car(2, 1, 10, 0.40, 55.0), 0);
    registry.insert(seeded_car(3, 2, 10, 0.30, 54.0), 0);
    let mut slopes = HashMap::new();
    slopes.insert(
        1,
        CarSlopeInfo {
            median: -0.5,
            n_buckets: 3,
            n_agree: 3,
        },
    );
    let events = HorizonDetector::new().detect(&registry, &slopes, 10, 600.0, 100.0);
    assert!(events.iter().any(|e| matches!(
        e,
        RaceEvent::HorizonClosing {
            attacker_car_idx: 1,
            defender_car_idx: 2,
            ..
        }
    )));
}
