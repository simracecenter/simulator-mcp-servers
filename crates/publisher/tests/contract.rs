// SPDX-License-Identifier: GPL-3.0-or-later

use publisher::battle_state::SlopeInfo;
use publisher::publisher_event::{
    build_event, format_rfc3339_ms, publisher_run_id, PublisherEvent,
};
use publisher::race_event::{
    BattleBreakReason, BattleIdentity, BattlePhase, DriverEffort, FlagScope, GapTrend,
    LifecycleOrigin, RaceEvent, SessionKind, SessionPhase, SessionStateName, SessionStateReason,
};
use publisher::telemetry_frame::TelemetryFrame;
use serde_json::Value;

fn minimal_frame() -> TelemetryFrame {
    TelemetryFrame {
        lap: 4,
        session_time: 1234.5,
        lap_dist_pct: 0.5,
        player_car_idx: 0,
        player_car_position: 5,
        on_pit_road: false,
        session_flags: 0,
        car_idx_lap_dist_pct: vec![0.5, 0.51],
        car_idx_position: vec![5, 4],
        car_idx_on_pit_road: vec![false, false],
        car_idx_track_surface: vec![0, 0],
        lap_last_lap_time: 540.0,
        session_info_update: 1,
        session_tick: 9876,
        session_state: 4,
        session_num: 0,
        session_time_remain: None,
        session_laps_remain: None,
        player_incident_count: 0,
        car_idx_lap_completed: vec![3, 3],
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

fn normalized_event_json(event: &PublisherEvent) -> Value {
    let mut json = serde_json::to_value(event).expect("publisher event should serialise");
    let Some(obj) = json.as_object_mut() else {
        panic!("publisher event should serialise to a JSON object");
    };
    obj.insert("id".to_owned(), Value::String("<redacted>".to_owned()));
    obj.insert("timestamp".to_owned(), Value::Number(0.into()));
    json
}

fn assert_envelope_contract(json: &Value, expected_type: &str) {
    assert_eq!(json["type"].as_str(), Some(expected_type));
    assert!(json["id"].as_str().is_some());
    assert!(json["raceSessionId"].as_str().is_some());
    assert!(json["rigId"].as_str().is_some());
    assert!(json["timestamp"].is_number());
    assert!(json["sessionTime"].is_number());
    assert!(json["sessionTick"].is_i64());
    assert!(json["scope"].is_string());
    assert!(json["payload"].is_object());
    assert!(json["payload"].get("event_type").is_none());
    assert!(json["context"].is_object());
    assert_identity_contract(json);
}

/// Publisher and subject identity are on *every* event, race and system alike.
fn assert_identity_contract(json: &Value) {
    assert_eq!(json["contractVersion"], 2);
    assert!(json["sequence"].is_u64());
    assert!(json["eventKey"]
        .as_str()
        .is_some_and(|k| k.starts_with("v2-")));

    let rig_id = json["rigId"].as_str().expect("rigId");
    let publisher = &json["publisher"];
    assert_eq!(publisher["rigId"], rig_id);
    assert_eq!(publisher["rigLabel"], rig_id);
    assert!(publisher["carIdx"].is_u64());
    assert!(publisher["carNumber"].is_string());
    assert!(publisher["driverId"]
        .as_str()
        .is_some_and(|d| !d.is_empty()));

    let payload = &json["payload"];
    assert_eq!(payload["rigId"], rig_id);
    assert_eq!(payload["rigLabel"], rig_id);
    assert_eq!(payload["publisherCarIdx"], publisher["carIdx"]);
    assert_eq!(payload["publisherCarNumber"], publisher["carNumber"]);
    assert_eq!(payload["publisherDriverId"], publisher["driverId"]);
    assert_eq!(payload["publisher"], *publisher);

    assert!(payload["subjectRole"].is_string());
    match json.get("subject") {
        Some(subject) => {
            assert_eq!(payload["subjectCarIdx"], subject["carIdx"]);
            assert_eq!(payload["subjectCarNumber"], subject["carNumber"]);
            assert_eq!(payload["subjectDriverId"], subject["driverId"]);
            assert_eq!(subject["role"], payload["subjectRole"]);
            assert!(subject["car"].is_object());
        }
        // Session-wide events have no subject car; the role still says so.
        None => {
            assert!(payload["subjectCarIdx"].is_null());
            assert!(payload["subject"].is_null());
        }
    }
}

#[test]
fn lap_completed_contract_includes_aliases_and_snake_case_fields() {
    let event = RaceEvent::LapCompleted {
        lap: 2,
        session_time: 99.0,
        player_car_idx: 0,
        lap_time_s: Some(88.2),
        best_lap_time_s: Some(87.9),
        position: 5,
        pit_frames: 0,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "LAP_COMPLETED");
    let lap_time = json["payload"]["lapTime"].as_f64().unwrap();
    let best_lap_time = json["payload"]["bestLapTime"].as_f64().unwrap();
    let lap_time_snake = json["payload"]["lap_time_s"].as_f64().unwrap();
    let best_lap_time_snake = json["payload"]["best_lap_time_s"].as_f64().unwrap();

    assert!((lap_time - 88.2).abs() < 1e-4);
    assert!((best_lap_time - 87.9).abs() < 1e-4);
    assert!((lap_time_snake - 88.2).abs() < 1e-4);
    assert!((best_lap_time_snake - 87.9).abs() < 1e-4);
}

#[test]
fn battle_contract_includes_leader_and_follower_numbers() {
    let event = RaceEvent::BattleEngaged {
        lap: 2,
        session_time: 12.0,
        player_car_idx: 0,
        opponent_car_idx: 1,
        gap_s: 0.4,
        car_race_position: 4,
        prior_skirmishes: 0,
        prior_attack_time_s: 0.0,
        engagement_started_at_session_time_s: 12.0,
        battle: None,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "BATTLE_ENGAGED");
    assert_eq!(json["scope"], "CAR_SCOPED");
    assert!(json["car"].is_object());
    assert_eq!(json["car"]["carIdx"], 1);

    // Legacy fields still present for transition
    assert_eq!(json["payload"]["leaderCarNumber"], "1");
    assert_eq!(json["payload"]["followerCarNumber"], "0");

    // New structured car references (primary source of truth)
    assert!(json["payload"]["leaderCar"].is_object());
    assert!(json["payload"]["followerCar"].is_object());
    assert_eq!(json["payload"]["leaderCar"]["carIdx"], 1);
    assert_eq!(json["payload"]["followerCar"]["carIdx"], 0);
}

#[test]
fn battle_closing_contract_uses_opponent_car_as_primary_identity() {
    let event = RaceEvent::BattleClosing {
        lap: 4,
        session_time: 1234.5,
        player_car_idx: 0,
        opponent_car_idx: 1,
        car_race_position: 3,
        closing_rate_sec_per_lap: 0.43,
        slope_info: SlopeInfo {
            median_slope: -0.43,
            anchors_qualifying: 5,
            anchors_agreeing: 4,
            hotspot_lap_dist_pct: 0.62,
        },
        prior_skirmishes: 0,
        prior_attack_time_s: 0.0,
        battle: None,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "BATTLE_CLOSING");
    assert_eq!(json["scope"], "CAR_SCOPED");
    assert!(json["car"].is_object());
    assert_eq!(json["car"]["carIdx"], 1);
    assert_eq!(json["payload"]["opponent_car_idx"], 1);
    assert_eq!(json["payload"]["player_car_idx"], 0);
    assert_eq!(json["context"]["leaderLap"], 3);
}

#[test]
fn overtake_includes_overtaking_and_overtaken_cars() {
    let event = RaceEvent::Overtake {
        lap: 3,
        session_time: 125.0,
        car_idx: 0,
        overtaken_car_idx: Some(2),
        position_from: 4,
        position_to: 3,
        positions_gained: 1,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "OVERTAKE");
    assert_eq!(json["scope"], "CAR_SCOPED");
    assert!(json["car"].is_object());

    // Envelope should show player as primary car
    assert_eq!(json["car"]["carIdx"], 0);

    // Payload should include structured car references (primary source of truth)
    assert!(json["payload"]["overtakingCar"].is_object());
    assert!(json["payload"]["overtakenCar"].is_object());
    assert_eq!(json["payload"]["overtakingCar"]["carIdx"], 0);
    assert_eq!(json["payload"]["overtakenCar"]["carIdx"], 2);

    // Legacy position fields still present
    assert_eq!(json["payload"]["position_from"], 4);
    assert_eq!(json["payload"]["position_to"], 3);
}

#[test]
fn overtake_can_emit_without_overtaken_car_when_uncertain() {
    let event = RaceEvent::Overtake {
        lap: 3,
        session_time: 125.0,
        car_idx: 0,
        overtaken_car_idx: None,
        position_from: 4,
        position_to: 3,
        positions_gained: 1,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "OVERTAKE");
    assert_eq!(json["scope"], "CAR_SCOPED");
    assert!(json["car"].is_object());

    // Legacy position fields are still present for narration
    assert_eq!(json["payload"]["position_from"], 4);
    assert_eq!(json["payload"]["position_to"], 3);

    // Overtaking car should still be identified
    assert!(json["payload"]["overtakingCar"].is_object());
    assert_eq!(json["payload"]["overtakingCar"]["carIdx"], 0);

    // Overtaken car should be null when not determined
    assert_eq!(json["payload"]["overtakenCar"], Value::Null);
}

#[test]
fn overtake_for_lead_includes_both_cars() {
    let event = RaceEvent::OvertakeForLead {
        lap: 5,
        session_time: 234.0,
        car_idx: 0,
        overtaken_car_idx: Some(1),
        position_from: 2,
        positions_gained: 1,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "OVERTAKE_FOR_LEAD");
    assert_eq!(json["scope"], "CAR_SCOPED");

    // Should identify both cars
    assert!(json["payload"]["overtakingCar"].is_object());
    assert!(json["payload"]["overtakenCar"].is_object());
    assert_eq!(json["payload"]["overtakingCar"]["carIdx"], 0);
    assert_eq!(json["payload"]["overtakenCar"]["carIdx"], 1);
}

#[test]
fn battle_events_identify_both_sides_directly() {
    let event = RaceEvent::BattleEngaged {
        lap: 2,
        session_time: 50.0,
        player_car_idx: 0,
        opponent_car_idx: 3,
        gap_s: 0.35,
        car_race_position: 5,
        prior_skirmishes: 1,
        prior_attack_time_s: 12.5,
        engagement_started_at_session_time_s: 50.0,
        battle: None,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "BATTLE_ENGAGED");
    assert_eq!(json["scope"], "CAR_SCOPED");

    // Payload should include structured car references for both battle participants
    assert!(json["payload"]["leaderCar"].is_object());
    assert!(json["payload"]["followerCar"].is_object());

    // Can determine who was where without heuristics
    let leader_idx = json["payload"]["leaderCar"]["carIdx"].as_i64().unwrap_or(0) as u8;
    let follower_idx = json["payload"]["followerCar"]["carIdx"]
        .as_i64()
        .unwrap_or(0) as u8;

    // Leader and follower should be different cars
    assert_ne!(leader_idx, follower_idx);
    assert!((leader_idx == 0 && follower_idx == 3) || (leader_idx == 3 && follower_idx == 0));
}

#[test]
fn battle_engaged_includes_engagement_start_time() {
    let event = RaceEvent::BattleEngaged {
        lap: 3,
        session_time: 200.0,
        player_car_idx: 0,
        opponent_car_idx: 1,
        gap_s: 0.45,
        car_race_position: 4,
        prior_skirmishes: 0,
        prior_attack_time_s: 0.0,
        engagement_started_at_session_time_s: 200.0,
        battle: None,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "BATTLE_ENGAGED");

    // Snake-case field present in payload
    let start_snake = json["payload"]["engagement_started_at_session_time_s"].as_f64();
    assert!(
        start_snake.is_some(),
        "engagement_started_at_session_time_s should be in payload"
    );
    assert!((start_snake.unwrap() - 200.0).abs() < 1e-4);

    // camelCase alias added by enrichment
    let start_camel = json["payload"]["engagementStartedAtSessionTime"].as_f64();
    assert!(
        start_camel.is_some(),
        "engagementStartedAtSessionTime should be in payload"
    );
    assert!((start_camel.unwrap() - 200.0).abs() < 1e-4);

    // engagementGapSec camelCase alias
    let gap = json["payload"]["engagementGapSec"].as_f64();
    assert!(gap.is_some(), "engagementGapSec should be in payload");
    assert!((gap.unwrap() - 0.45).abs() < 1e-4);
}

#[test]
fn battle_broken_contract_uses_final_gap_sec_and_duration() {
    let event = RaceEvent::BattleBroken {
        lap: 5,
        session_time: 350.0,
        player_car_idx: 0,
        opponent_car_idx: 1,
        final_gap_sec: Some(1.8),
        car_race_position: 4,
        engagement_started_at_session_time_s: 320.0,
        battle: None,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "BATTLE_BROKEN");

    // Snake-case field final_gap_sec present in payload
    let final_gap_snake = json["payload"]["final_gap_sec"].as_f64();
    assert!(
        final_gap_snake.is_some(),
        "final_gap_sec should be in payload"
    );
    assert!((final_gap_snake.unwrap() - 1.8).abs() < 1e-4);

    // camelCase alias added by enrichment
    let final_gap_camel = json["payload"]["finalGapSec"].as_f64();
    assert!(
        final_gap_camel.is_some(),
        "finalGapSec should be in payload"
    );
    assert!((final_gap_camel.unwrap() - 1.8).abs() < 1e-4);

    // finalGapSec should never be f32::MAX
    assert!(
        final_gap_camel.unwrap() < f32::MAX as f64,
        "finalGapSec should not be f32::MAX sentinel"
    );

    // engagementDurationSec computed from start time
    let duration = json["payload"]["engagementDurationSec"].as_f64();
    assert!(
        duration.is_some(),
        "engagementDurationSec should be in payload"
    );
    assert!(
        (duration.unwrap() - 30.0).abs() < 1e-4,
        "duration should be 350.0 - 320.0 = 30.0s"
    );

    // leaderCar and followerCar still present
    assert!(json["payload"]["leaderCar"].is_object());
    assert!(json["payload"]["followerCar"].is_object());
}

#[test]
fn traffic_intercept_includes_structured_car_ref() {
    let event = RaceEvent::TrafficIntercept {
        lap: 3,
        session_time: 180.0,
        leader_car_idx: 0,
        traffic_car_idx: 5,
        cross_class: false,
        distance_m: 200.0,
        relative_speed_mps: 10.0,
        time_to_intercept_s: 20.0,
        intercept_bucket: 12,
        intercept_lap_dist_pct: 0.6,
        predicted_intercept_session_time: 200.0,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "TRAFFIC_INTERCEPT");

    // Numeric field kept for backward compatibility
    assert_eq!(json["payload"]["traffic_car_idx"], 5);

    // Structured trafficCar object
    assert!(
        json["payload"]["trafficCar"].is_object(),
        "trafficCar should be a structured object"
    );
    assert_eq!(json["payload"]["trafficCar"]["carIdx"], 5);
    assert!(json["payload"]["trafficCar"]["carNumber"].is_string());

    // Canonical role vocabulary: the catching car is the subject/attacker and
    // the car being caught is the defender, whatever the family calls them.
    assert_eq!(json["subject"]["role"], "ATTACKER");
    assert_eq!(json["subject"]["carIdx"], 0);
    assert_eq!(json["payload"]["attackerCarIdx"], 0);
    assert_eq!(json["payload"]["defenderCarIdx"], 5);
    assert_eq!(json["payload"]["defenderCar"]["carIdx"], 5);
    assert_eq!(
        json["payload"]["leaderCarIdx"], 0,
        "camelCase twin of leader_car_idx"
    );
    assert_eq!(
        json["payload"]["trafficCarIdx"], 5,
        "camelCase twin of traffic_car_idx"
    );
}

#[test]
fn micro_sector_gain_names_the_publishing_driver_as_subject() {
    let event = RaceEvent::MicroSectorGain {
        lap: 5,
        session_time: 300.0,
        bucket_from: 3,
        bucket_to: 4,
        lap_dist_pct_from: 0.3,
        lap_dist_pct_to: 0.4,
        cumulative_delta_s: -0.21,
        technique_hint: "earlier throttle".to_owned(),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-rig1",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "MICRO_SECTOR_GAIN");
    assert_eq!(json["subject"]["role"], "DRIVER");
    assert_eq!(json["subject"]["carIdx"], 0);
    assert_eq!(json["payload"]["subjectCarIdx"], 0);
    let camel = json["payload"]["cumulativeDeltaS"]
        .as_f64()
        .expect("camelCase twin");
    let snake = json["payload"]["cumulative_delta_s"]
        .as_f64()
        .expect("snake_case original kept");
    assert!((camel - -0.21).abs() < 1e-4);
    assert_eq!(camel, snake);
}

#[test]
fn publisher_identity_is_distinct_from_the_subject_car() {
    // The rig is car 0; the event is about car 5's incident.
    let event = RaceEvent::IncidentAlert {
        lap: 6,
        session_time: 370.0,
        car_idx: 5,
        driver_incident_count: Some(4),
        previous_track_surface: 3,
        current_track_surface: 1,
        previous_speed_mps: 68.0,
        current_speed_mps: 48.0,
        speed_drop_mps: 20.0,
        severity: 0.29411766,
        severity_normalized: 0.29411766,
        incident_count_delta: None,
        reason: "surface_change".to_owned(),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-rig1",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "INCIDENT_ALERT");
    assert_eq!(json["publisher"]["carIdx"], 0);
    assert_eq!(json["subject"]["carIdx"], 5);
    assert_eq!(json["subject"]["role"], "INCIDENT");
    assert_ne!(
        json["payload"]["publisherCarIdx"],
        json["payload"]["subjectCarIdx"]
    );
}

#[test]
fn system_events_carry_the_publishing_rig_identity() {
    let event = RaceEvent::PublisherHeartbeat {
        lap: 4,
        session_time: 1234.5,
        version: "0.1.5".to_owned(),
        events_enqueued_total: 17,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-rig1",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "PUBLISHER_HEARTBEAT");
    assert_eq!(json["scope"], "RIG_SCOPED");
    assert_eq!(json["payload"]["rigLabel"], "rig-rig1");
    assert_eq!(json["subject"]["role"], "RIG");
    assert_eq!(json["subject"]["carIdx"], 0);
}

#[test]
fn events_published_on_one_tick_have_distinct_event_keys() {
    let event = RaceEvent::TrafficIntercept {
        lap: 3,
        session_time: 180.0,
        leader_car_idx: 0,
        traffic_car_idx: 5,
        cross_class: false,
        distance_m: 200.0,
        relative_speed_mps: 10.0,
        time_to_intercept_s: 20.0,
        intercept_bucket: 12,
        intercept_lap_dist_pct: 0.6,
        predicted_intercept_session_time: 200.0,
    };
    let frame = minimal_frame();

    let first = build_event(&event, &frame, None, "s", "rig-rig1", None, Some(88087370));
    let second = build_event(&event, &frame, None, "s", "rig-rig1", None, Some(88087370));

    assert_eq!(first.session_tick, second.session_tick);
    assert_ne!(first.event_key, second.event_key);
    assert_ne!(first.id, second.id);
    assert!(first
        .event_key
        .starts_with("v2-88087370-9876-TRAFFIC_INTERCEPT-"));
}

#[test]
fn horizon_closing_includes_both_car_refs() {
    let event = RaceEvent::HorizonClosing {
        lap: 8,
        session_time: 500.0,
        attacker_car_idx: 3,
        defender_car_idx: 7,
        attacker_position: 4,
        defender_position: 3,
        current_gap_s: 2.5,
        closing_rate_sec_per_lap: 0.3,
        estimated_laps_to_contact: 8,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "HORIZON_CLOSING");

    // Numeric fields kept for backward compatibility
    assert_eq!(json["payload"]["attacker_car_idx"], 3);
    assert_eq!(json["payload"]["defender_car_idx"], 7);

    // Structured attackerCar and defenderCar objects
    assert!(
        json["payload"]["attackerCar"].is_object(),
        "attackerCar should be a structured object"
    );
    assert!(
        json["payload"]["defenderCar"].is_object(),
        "defenderCar should be a structured object"
    );
    assert_eq!(json["payload"]["attackerCar"]["carIdx"], 3);
    assert_eq!(json["payload"]["defenderCar"]["carIdx"], 7);
    assert!(json["payload"]["attackerCar"]["carNumber"].is_string());
    assert!(json["payload"]["defenderCar"]["carNumber"].is_string());
}

#[test]
fn incident_cluster_includes_all_car_refs() {
    let event = RaceEvent::IncidentCluster {
        lap: 6,
        session_time: 370.0,
        bucket: 15,
        lap_dist_pct_from: 0.75,
        lap_dist_pct_to: 0.80,
        car_idxs: vec![1, 2, 4],
        severity: 3.0,
        primary_car_idx: Some(1),
        incident_type: Some("Incident".to_owned()),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "INCIDENT_CLUSTER");

    // Numeric car_idxs kept for backward compatibility
    assert!(json["payload"]["car_idxs"].is_array());
    assert_eq!(json["payload"]["car_idxs"].as_array().unwrap().len(), 3);

    // Structured involvedCars array with all participants
    assert!(
        json["payload"]["involvedCars"].is_array(),
        "involvedCars should be an array"
    );
    let involved = json["payload"]["involvedCars"].as_array().unwrap();
    assert_eq!(
        involved.len(),
        3,
        "involvedCars should contain all 3 participants"
    );
    assert!(involved[0].is_object());
    assert!(involved[0]["carIdx"].is_number());

    // primaryCar object for most-culpable car
    assert!(
        json["payload"]["primaryCar"].is_object(),
        "primaryCar should be a structured object"
    );
    assert_eq!(json["payload"]["primaryCar"]["carIdx"], 1);

    // Incident type
    assert_eq!(json["payload"]["incidentType"], "Incident");

    // Location: centroid of the cluster bucket
    let lap_dist_pct = json["payload"]["lapDistPct"].as_f64().unwrap();
    assert!(
        (lap_dist_pct - 0.775).abs() < 1e-3,
        "lapDistPct should be centroid ~0.775, got {lap_dist_pct}"
    );
}

#[test]
fn incident_alert_includes_surface_and_speed_fields() {
    let event = RaceEvent::IncidentAlert {
        lap: 6,
        session_time: 370.0,
        car_idx: 1,
        driver_incident_count: Some(4),
        previous_track_surface: 3,
        current_track_surface: 1,
        previous_speed_mps: 68.0,
        current_speed_mps: 48.0,
        speed_drop_mps: 20.0,
        severity: 0.29411766,
        severity_normalized: 0.29411766,
        incident_count_delta: None,
        reason: "surface_change".to_owned(),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "INCIDENT_ALERT");
    assert_eq!(json["scope"], "CAR_SCOPED");
    assert!(json["car"].is_object());
    assert_eq!(json["car"]["carIdx"], 1);
    assert_eq!(json["payload"]["previousTrackSurface"], 3);
    assert_eq!(json["payload"]["currentTrackSurface"], 1);
    assert_eq!(json["payload"]["driverIncidentCount"], 4);
    assert_eq!(json["payload"]["previousSpeedMps"], 68.0);
    assert_eq!(json["payload"]["currentSpeedMps"], 48.0);
    assert_eq!(json["payload"]["speedDropMps"], 20.0);
    assert_eq!(json["payload"]["reason"], "surface_change");
    // Documented surface key, alongside the current-prefixed one.
    assert_eq!(json["payload"]["trackSurface"], 1);
    // Raw magnitude and the 0–1 score a quality floor can use.
    let score = json["payload"]["severityScore"].as_f64().unwrap();
    let normalized = json["payload"]["severityNormalized"].as_f64().unwrap();
    assert!((score - 0.294).abs() < 1e-3, "expected ~0.294, got {score}");
    assert!(
        (normalized - 0.294).abs() < 1e-3,
        "expected ~0.294, got {normalized}"
    );
    assert!(json["payload"]["incidentCountDelta"].is_null());
}

#[test]
fn incident_count_alert_normalizes_raw_iracing_points() {
    let event = RaceEvent::IncidentAlert {
        lap: 6,
        session_time: 370.0,
        car_idx: 0,
        driver_incident_count: Some(6),
        previous_track_surface: 3,
        current_track_surface: 3,
        previous_speed_mps: 40.0,
        current_speed_mps: 39.6,
        speed_drop_mps: 0.4,
        severity: 2.0,
        severity_normalized: 0.5,
        incident_count_delta: Some(2),
        reason: "incident_count_increase".to_owned(),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "INCIDENT_ALERT");
    // `severity` stays the raw iRacing point delta for existing consumers.
    assert_eq!(json["payload"]["severity"], 2.0);
    assert_eq!(json["payload"]["severityScore"], 2.0);
    assert_eq!(json["payload"]["incidentCountDelta"], 2);
    // …and the normalized score is inside the documented 0–1 range.
    let normalized = json["payload"]["severityNormalized"].as_f64().unwrap();
    assert!(
        (0.0..=1.0).contains(&normalized),
        "expected 0-1, got {normalized}"
    );
    assert_eq!(normalized, 0.5);
}

#[test]
fn battle_broken_with_no_gap_emits_null_final_gap() {
    let event = RaceEvent::BattleBroken {
        lap: 7,
        session_time: 420.0,
        player_car_idx: 0,
        opponent_car_idx: 2,
        final_gap_sec: None, // gap unknown — opponent left scan range
        car_race_position: 3,
        engagement_started_at_session_time_s: 390.0,
        battle: None,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "BATTLE_BROKEN");

    // Snake-case field present in payload and is null
    assert_eq!(
        json["payload"]["final_gap_sec"],
        Value::Null,
        "final_gap_sec should be null when gap is unknown"
    );

    // camelCase alias also null (no sentinel written)
    assert_eq!(
        json["payload"]["finalGapSec"],
        Value::Null,
        "finalGapSec should be null when gap is unknown — no f32::MAX sentinel"
    );

    // Duration still computable
    let duration = json["payload"]["engagementDurationSec"].as_f64();
    assert!(
        duration.is_some(),
        "engagementDurationSec should still be computed"
    );
    assert!((duration.unwrap() - 30.0).abs() < 1e-4);
}
#[test]
fn flag_yellow_local_includes_location_and_trigger() {
    // Yellow with a known nearby trigger car and location.
    let event = RaceEvent::FlagYellowLocal {
        lap: 3,
        session_time: 180.0,
        trigger_car_idx: Some(5),
        lap_dist_pct: Some(0.425),
        sector: None,
        scope: FlagScope::Nearby,
        linked_incident_id: Some(8),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "FLAG_YELLOW_LOCAL");

    // Structured trigger car reference
    assert!(
        json["payload"]["triggerCar"].is_object(),
        "triggerCar should be a structured CarRef object"
    );
    assert_eq!(json["payload"]["triggerCar"]["carIdx"], 5);
    assert!(json["payload"]["triggerCar"]["carNumber"].is_string());

    // Track location formatted as percentage string
    let track_pct = json["payload"]["trackLocationPct"].as_str();
    assert!(track_pct.is_some(), "trackLocationPct should be present");
    assert_eq!(track_pct.unwrap(), "42.5%");

    // Flag scope is one of the four valid enum variants
    let flag_scope = json["payload"]["flagScope"].as_str();
    assert!(flag_scope.is_some(), "flagScope should be present");
    assert!(
        ["SelfCaused", "Nearby", "SessionWide", "Unknown"].contains(&flag_scope.unwrap()),
        "flagScope must be one of the four valid values, got: {}",
        flag_scope.unwrap()
    );
    assert_eq!(flag_scope.unwrap(), "Nearby");

    // Human-readable reason
    assert!(
        json["payload"]["reason"].is_string(),
        "reason should be a string"
    );

    // Raw fields still present for backward compatibility
    let raw_pct = json["payload"]["lap_dist_pct"]
        .as_f64()
        .expect("lap_dist_pct should be a number");
    assert!(
        (raw_pct - 0.425).abs() < 1e-4,
        "lap_dist_pct should be ~0.425, got {raw_pct}"
    );
    assert_eq!(json["payload"]["linked_incident_id"], 8);
}

#[test]
fn flag_yellow_local_unknown_scope_omits_trigger_car() {
    // Yellow with no determinable cause.
    let event = RaceEvent::FlagYellowLocal {
        lap: 5,
        session_time: 310.0,
        trigger_car_idx: None,
        lap_dist_pct: Some(0.72),
        sector: None,
        scope: FlagScope::Unknown,
        linked_incident_id: None,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "FLAG_YELLOW_LOCAL");

    // No trigger car when cause is unknown
    assert!(
        json["payload"].get("triggerCar").is_none() || json["payload"]["triggerCar"].is_null(),
        "triggerCar should be absent or null when scope is Unknown"
    );

    // flagScope still present and valid
    assert_eq!(json["payload"]["flagScope"].as_str(), Some("Unknown"));

    // reason still present
    assert!(json["payload"]["reason"].is_string());
}

#[test]
fn session_wide_events_not_car_scoped() {
    let event = RaceEvent::RaceGreen {
        lap: 1,
        session_time: 10.0,
        synthetic: false,
        origin: LifecycleOrigin::SessionStateTransition,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "RACE_GREEN");
    assert_eq!(json["scope"], "SESSION_SCOPED");
    assert_eq!(json["payload"]["eventScope"], "SESSION_SCOPED");
    assert_eq!(json["payload"]["synthetic"], false);
    assert_eq!(json["payload"]["origin"], "SESSION_STATE_TRANSITION");
    assert!(json.get("car").is_none());
}

#[test]
fn session_event_envelope_does_not_include_player_car_ref() {
    let event = RaceEvent::FlagYellowFullCourse {
        lap: 4,
        session_time: 200.0,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "FLAG_YELLOW_FULL_COURSE");
    assert_eq!(json["scope"], "SESSION_SCOPED");
    assert_eq!(json["payload"]["eventScope"], "SESSION_SCOPED");
    assert!(json.get("car").is_none());
}

#[test]
fn micro_sector_loss_contract_is_car_scoped() {
    let event = RaceEvent::MicroSectorLoss {
        lap: 7,
        session_time: 420.0,
        bucket_from: 12,
        bucket_to: 15,
        lap_dist_pct_from: 0.60,
        lap_dist_pct_to: 0.80,
        cumulative_delta_s: 0.24,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "MICRO_SECTOR_LOSS");
    assert_eq!(json["scope"], "CAR_SCOPED");
    assert_eq!(json["payload"]["eventScope"], "CAR_SCOPED");
    assert!(json["car"].is_object());
    assert_eq!(json["payload"]["bucket_from"], 12);
    assert_eq!(json["payload"]["bucket_to"], 15);
    let delta = json["payload"]["cumulative_delta_s"]
        .as_f64()
        .expect("delta should be numeric");
    assert!((delta - 0.24).abs() < 1e-4);
}

#[test]
fn focus_me_requested_contract_carries_requester_identity_and_dwell() {
    let event = RaceEvent::FocusMeRequested {
        lap: 4,
        session_time: 1234.5,
        player_car_idx: 1,
        request_id: "req-1".to_owned(),
        press_seq: 3,
        driver_id: "user:123456".to_owned(),
        rig_id: "rig-001".to_owned(),
        source: "wheel_button".to_owned(),
        button: 7,
        requested_at_ms: 1_700_000_000_000,
        dwell_ms: 10_000,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "FOCUS_ME_REQUESTED");
    assert_eq!(json["scope"], "CAR_SCOPED");
    // The subject is the requesting driver's car.
    assert_eq!(json["car"]["carIdx"], 1);
    assert_eq!(json["payload"]["request_id"], "req-1");
    assert_eq!(json["payload"]["press_seq"], 3);
    assert_eq!(json["payload"]["driver_id"], "user:123456");
    assert_eq!(json["payload"]["rig_id"], "rig-001");
    assert_eq!(json["payload"]["source"], "wheel_button");
    assert_eq!(json["payload"]["button"], 7);
    assert_eq!(json["payload"]["dwell_ms"], 10_000);
    assert_eq!(json["payload"]["requested_at_ms"], 1_700_000_000_000i64);
}

#[test]
fn broadcast_control_requested_is_rig_scoped_and_carries_no_car() {
    let event = RaceEvent::BroadcastControlRequested {
        lap: 4,
        session_time: 1234.5,
        action: "toggle".to_owned(),
        request_id: "req-2".to_owned(),
        press_seq: 4,
        driver_id: "user:123456".to_owned(),
        rig_id: "rig-001".to_owned(),
        source: "simulated".to_owned(),
        button: 3,
        requested_at_ms: 1_700_000_000_001,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "BROADCAST_CONTROL_REQUESTED");
    assert_eq!(json["scope"], "RIG_SCOPED");
    assert!(json.get("car").is_none());
    assert_eq!(json["payload"]["action"], "toggle");
    assert_eq!(json["payload"]["request_id"], "req-2");
    assert_eq!(json["payload"]["source"], "simulated");
}

#[test]
fn two_requests_in_the_same_tick_stay_distinguishable() {
    let request =
        |request_id: &str, press_seq: u64, driver: &str, car_idx: u8| RaceEvent::FocusMeRequested {
            lap: 4,
            session_time: 1234.5,
            player_car_idx: car_idx,
            request_id: request_id.to_owned(),
            press_seq,
            driver_id: driver.to_owned(),
            rig_id: format!("rig-{driver}"),
            source: "wheel_button".to_owned(),
            button: 7,
            requested_at_ms: 1_700_000_000_000,
            dwell_ms: 10_000,
        };

    let frame = minimal_frame();
    let a = build_event(
        &request("req-a", 1, "user:1", 0),
        &frame,
        None,
        "s",
        "rig-a",
        None,
        None,
    );
    let b = build_event(
        &request("req-b", 2, "user:2", 1),
        &frame,
        None,
        "s",
        "rig-b",
        None,
        None,
    );

    // Same subSessionId, tick, and type: only the request identity separates
    // them, which is what the sandbox deduplicates on.
    assert_eq!(a.session_tick, b.session_tick);
    assert_ne!(a.id, b.id);
    assert_ne!(a.payload["request_id"], b.payload["request_id"]);
    assert_ne!(a.payload["press_seq"], b.payload["press_seq"]);
    assert_ne!(a.payload["driver_id"], b.payload["driver_id"]);
}

#[test]
fn driver_material_publishes_the_full_state_of_the_rigs_own_driver() {
    let event = RaceEvent::DriverMaterial {
        lap: 4,
        session_time: 1234.5,
        player_car_idx: 0,
        position: 5,
        laps_completed: 3,
        lap_dist_pct: 0.5,
        last_lap_time_s: Some(88.2),
        best_lap_time_s: Some(87.9),
        gap_ahead_s: Some(1.4),
        car_ahead_idx: Some(1),
        gap_behind_s: None,
        car_behind_idx: None,
        delta_to_best_s: Some(0.3),
        sector_bucket: Some(4),
        sector_delta_to_best_s: Some(-0.12),
        gap_ahead_trend: GapTrend::Closing,
        gap_behind_trend: GapTrend::Unknown,
        effort: DriverEffort::Pushing,
        on_pit_road: false,
        track_surface: 3,
        speed_mps: 61.5,
        fuel_level_l: 42.25,
        incident_count: 4,
        session_state: 4,
        interval_s: 25.0,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "DRIVER_MATERIAL");
    let payload = &json["payload"];
    assert_eq!(payload["subjectRole"], "DRIVER");
    assert_eq!(payload["subjectCarIdx"], 0);
    assert_eq!(payload["position"], 5);
    assert_eq!(payload["lapsCompleted"], 3);
    assert_eq!(payload["onPitRoad"], false);
    assert_eq!(payload["trackSurface"], 3);
    assert_eq!(payload["incidentCount"], 4);
    assert_eq!(payload["sessionState"], 4);
    assert!((payload["intervalS"].as_f64().unwrap() - 25.0).abs() < 1e-6);
    assert!((payload["lastLapTime"].as_f64().unwrap() - 88.2).abs() < 1e-4);
    assert!((payload["bestLapTime"].as_f64().unwrap() - 87.9).abs() < 1e-4);
    assert!((payload["gapAhead"].as_f64().unwrap() - 1.4).abs() < 1e-4);
    assert!(payload["gapBehind"].is_null());
    assert_eq!(payload["carAheadIdx"], 1);
    assert!(payload["carAhead"].is_object());
    assert!(payload["carBehind"].is_null());
    // Pace and effort material (item 1): the consumer can rank a quiet stint
    // without waiting for a relational event.
    assert!((payload["deltaToBest"].as_f64().unwrap() - 0.3).abs() < 1e-4);
    assert!((payload["sectorDeltaToBest"].as_f64().unwrap() + 0.12).abs() < 1e-4);
    assert_eq!(payload["sectorBucket"], 4);
    assert_eq!(payload["gapAheadTrend"], "CLOSING");
    assert_eq!(payload["gapBehindTrend"], "UNKNOWN");
    assert_eq!(payload["effort"], "PUSHING");
    // Snake-case originals stay for the transition window.
    assert_eq!(payload["laps_completed"], 3);
    assert_eq!(payload["track_surface"], 3);
}

#[test]
fn session_reset_reports_a_session_clock_restart_inside_one_sub_session() {
    let event = RaceEvent::SessionReset {
        lap: 0,
        session_time: 1.05,
        previous_sub_session_id: Some(88_087_370),
        sub_session_id: 88_087_370,
        previous_session_num: Some(0),
        session_num: 0,
        previous_session_time: Some(1983.23),
        reason: "session_clock_restarted".to_owned(),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "88087370",
        "rig-001",
        None,
        Some(88_087_370),
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "SESSION_RESET");
    let payload = &json["payload"];
    assert_eq!(json["scope"], "SESSION_SCOPED");
    assert_eq!(payload["reason"], "session_clock_restarted");
    assert_eq!(payload["subSessionId"], 88_087_370i64);
    assert_eq!(payload["previousSubSessionId"], 88_087_370i64);
    assert_eq!(payload["sessionNum"], 0);
    assert!((payload["previousSessionTime"].as_f64().unwrap() - 1983.23).abs() < 1e-2);
}

#[test]
fn a_synthetic_green_is_marked_as_a_connect_snapshot() {
    let event = RaceEvent::RaceGreen {
        lap: 0,
        session_time: 5.0,
        synthetic: true,
        origin: LifecycleOrigin::ConnectSnapshot,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "RACE_GREEN");
    assert_eq!(json["payload"]["synthetic"], true);
    assert_eq!(json["payload"]["origin"], "CONNECT_SNAPSHOT");
}

#[test]
fn session_reset_names_the_old_and_new_sessions() {
    let event = RaceEvent::SessionReset {
        lap: 0,
        session_time: 12.0,
        previous_sub_session_id: Some(88_087_370),
        sub_session_id: 88_087_411,
        previous_session_num: Some(2),
        session_num: 0,
        previous_session_time: None,
        reason: "sub_session_changed".to_owned(),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "88087411",
        "rig-001",
        None,
        Some(88_087_411),
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "SESSION_RESET");
    let payload = &json["payload"];
    // Session-scoped: no subject car to invalidate against.
    assert_eq!(payload["subjectRole"], "SESSION");
    assert_eq!(json["scope"], "SESSION_SCOPED");
    assert_eq!(payload["previousSubSessionId"], 88_087_370i64);
    assert_eq!(payload["subSessionId"], 88_087_411i64);
    assert_eq!(payload["previousSessionNum"], 2);
    assert_eq!(payload["sessionNum"], 0);
    assert_eq!(payload["reason"], "sub_session_changed");
    assert_eq!(json["context"]["subSessionId"], 88_087_411i64);
}

#[test]
fn pit_exit_publishes_an_unclassified_car_without_dropping_identity() {
    let event = RaceEvent::PitExit {
        lap: 0,
        session_time: 40.0,
        player_car_idx: 0,
        position: 0,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "PIT_EXIT");
    assert_eq!(json["payload"]["position"], 0);
    assert_eq!(json["payload"]["subjectRole"], "DRIVER");
    assert_eq!(json["payload"]["subjectCarIdx"], 0);
}

#[test]
fn race_checkered_is_session_scoped() {
    let event = RaceEvent::RaceCheckered {
        lap: 42,
        session_time: 3600.0,
        synthetic: false,
        origin: LifecycleOrigin::SessionStateTransition,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "RACE_CHECKERED");
    assert_eq!(json["scope"], "SESSION_SCOPED");
    assert_eq!(json["payload"]["synthetic"], false);
    assert_eq!(json["payload"]["origin"], "SESSION_STATE_TRANSITION");
    assert_eq!(json["payload"]["subjectRole"], "SESSION");
}

// ── Battle identity (third-party pair tracker) ───────────────────────────────

fn third_party_frame() -> TelemetryFrame {
    // Rig is car 0 at P1; cars 3 (P2) and 5 (P3) are fighting behind it.
    let mut frame = minimal_frame();
    frame.car_idx_lap_dist_pct = vec![0.5, -1.0, -1.0, 0.20, -1.0, 0.195];
    frame.car_idx_position = vec![1, 0, 0, 2, 0, 3];
    frame.car_idx_on_pit_road = vec![false; 6];
    frame.car_idx_track_surface = vec![0; 6];
    frame.car_idx_lap_completed = vec![3; 6];
    frame.player_car_position = 1;
    frame
}

fn sample_identity(phase: BattlePhase) -> BattleIdentity {
    BattleIdentity {
        battle_id: "btl-03-05-1200000-1".into(),
        battle_phase: phase,
        ahead_car_idx: 3,
        behind_car_idx: 5,
        engaged_at: 1200.0,
        battle_age_s: 34.5,
        current_gap_s: Some(0.5),
        closing_rate_s_per_lap: Some(1.25),
        battle_confidence: 0.9,
        battle_involves_publisher: false,
        battle_break_reason: None,
    }
}

/// Every additive identity field, in both snake_case and camelCase.
fn assert_battle_identity_contract(payload: &Value, phase: &str) {
    assert_eq!(payload["battleId"], "btl-03-05-1200000-1");
    assert_eq!(payload["battle_id"], "btl-03-05-1200000-1");
    assert_eq!(payload["battlePhase"], phase);
    assert_eq!(payload["aheadCarIdx"], 3);
    assert_eq!(payload["behindCarIdx"], 5);
    assert_eq!(payload["ahead_car_idx"], 3);
    assert_eq!(payload["behind_car_idx"], 5);
    assert!((payload["engagedAt"].as_f64().unwrap() - 1200.0).abs() < 1e-3);
    assert!((payload["battleAgeS"].as_f64().unwrap() - 34.5).abs() < 1e-3);
    assert!((payload["currentGapS"].as_f64().unwrap() - 0.5).abs() < 1e-4);
    assert!((payload["closingRateSPerLap"].as_f64().unwrap() - 1.25).abs() < 1e-4);
    assert!((payload["battleConfidence"].as_f64().unwrap() - 0.9).abs() < 1e-4);
    assert_eq!(payload["battleInvolvesPublisher"], false);
}

#[test]
fn battle_engaged_for_a_third_party_pair_carries_identity_and_roles() {
    let event = RaceEvent::BattleEngaged {
        lap: 4,
        session_time: 1234.5,
        player_car_idx: 5,
        opponent_car_idx: 3,
        gap_s: 0.5,
        car_race_position: 2,
        prior_skirmishes: 0,
        prior_attack_time_s: 0.0,
        engagement_started_at_session_time_s: 1200.0,
        battle: Some(sample_identity(BattlePhase::Engaged)),
    };

    let env = build_event(
        &event,
        &third_party_frame(),
        None,
        "session-abc",
        "rig-001",
        None,
        None,
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "BATTLE_ENGAGED");
    let payload = &json["payload"];
    assert_battle_identity_contract(payload, "ENGAGED");
    assert!(payload["battleBreakReason"].is_null());

    // Legacy leader/follower derivation still holds for a pair that does
    // not involve the rig, and the rig's identity stays separate.
    assert_eq!(payload["leaderCar"]["carIdx"], 3);
    assert_eq!(payload["followerCar"]["carIdx"], 5);
    assert_eq!(payload["attackerCarIdx"], 5);
    assert_eq!(payload["defenderCarIdx"], 3);
    assert_eq!(payload["subjectCarIdx"], 5);
    assert_eq!(payload["publisherCarIdx"], 0);
    assert_eq!(json["car"]["carIdx"], 3);
}

#[test]
fn battle_closing_and_broken_share_the_battle_id() {
    let closing = RaceEvent::BattleClosing {
        lap: 4,
        session_time: 1234.5,
        player_car_idx: 5,
        opponent_car_idx: 3,
        car_race_position: 2,
        closing_rate_sec_per_lap: 1.25,
        slope_info: SlopeInfo {
            median_slope: -1.25,
            anchors_qualifying: 60,
            anchors_agreeing: 60,
            hotspot_lap_dist_pct: 0.195,
        },
        prior_skirmishes: 0,
        prior_attack_time_s: 0.0,
        battle: Some(sample_identity(BattlePhase::Closing)),
    };
    let broken = RaceEvent::BattleBroken {
        lap: 4,
        session_time: 1234.5,
        player_car_idx: 5,
        opponent_car_idx: 3,
        final_gap_sec: Some(2.6),
        car_race_position: 2,
        engagement_started_at_session_time_s: 1200.0,
        battle: Some(BattleIdentity {
            battle_break_reason: Some(BattleBreakReason::GapOpened),
            current_gap_s: Some(0.5),
            ..sample_identity(BattlePhase::Broken)
        }),
    };

    let frame = third_party_frame();
    let closing_json = normalized_event_json(&build_event(
        &closing, &frame, None, "s", "rig-001", None, None,
    ));
    let broken_json = normalized_event_json(&build_event(
        &broken, &frame, None, "s", "rig-001", None, None,
    ));

    assert_envelope_contract(&closing_json, "BATTLE_CLOSING");
    assert_envelope_contract(&broken_json, "BATTLE_BROKEN");
    assert_battle_identity_contract(&closing_json["payload"], "CLOSING");
    assert_battle_identity_contract(&broken_json["payload"], "BROKEN");
    assert_eq!(
        closing_json["payload"]["battleId"],
        broken_json["payload"]["battleId"]
    );
    assert_eq!(broken_json["payload"]["battleBreakReason"], "GAP_OPENED");
    assert_eq!(broken_json["payload"]["battle_break_reason"], "GAP_OPENED");
    // Legacy closing-rate field is still populated alongside the new one.
    assert!(
        (closing_json["payload"]["closing_rate_sec_per_lap"]
            .as_f64()
            .unwrap()
            - 1.25)
            .abs()
            < 1e-4
    );
}

#[test]
fn battle_events_without_identity_omit_the_new_fields() {
    // An event from the legacy player-threat path with no tracked pair must
    // serialise exactly as before: none of the identity keys appear.
    let event = RaceEvent::BattleEngaged {
        lap: 2,
        session_time: 12.0,
        player_car_idx: 0,
        opponent_car_idx: 1,
        gap_s: 0.4,
        car_race_position: 4,
        prior_skirmishes: 0,
        prior_attack_time_s: 0.0,
        engagement_started_at_session_time_s: 12.0,
        battle: None,
    };
    let json = normalized_event_json(&build_event(
        &event,
        &minimal_frame(),
        None,
        "s",
        "rig-001",
        None,
        None,
    ));
    let payload = json["payload"].as_object().unwrap();
    for key in [
        "battleId",
        "battle_id",
        "battlePhase",
        "aheadCarIdx",
        "behindCarIdx",
        "engagedAt",
        "battleAgeS",
        "currentGapS",
        "closingRateSPerLap",
        "battleConfidence",
        "battleInvolvesPublisher",
        "battleBreakReason",
        "battle",
    ] {
        assert!(!payload.contains_key(key), "unexpected key {key}");
    }
    assert_eq!(payload["leaderCarNumber"], "1");
    assert_eq!(payload["followerCarNumber"], "0");
}

#[test]
fn session_state_describes_the_session_kind_phase_and_imminent_change() {
    let event = RaceEvent::SessionState {
        lap: 0,
        session_time: 12.0,
        session_num: 2,
        previous_session_num: Some(1),
        session_type: Some("Race".to_owned()),
        session_kind: SessionKind::Race,
        session_state: 3,
        session_state_name: SessionStateName::ParadeLaps,
        previous_session_state: Some(2),
        phase: SessionPhase::RaceFormation,
        previous_phase: Some(SessionPhase::RaceGridding),
        reason: SessionStateReason::SessionStateChanged,
        synthetic: false,
        session_time_remain_s: Some(2700.5),
        session_laps_remain: Some(20),
        session_change_imminent: false,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "88087411",
        "rig-001",
        None,
        Some(88_087_411),
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "SESSION_STATE");
    assert_eq!(json["scope"], "SESSION_SCOPED");
    let payload = &json["payload"];
    assert_eq!(payload["subjectRole"], "SESSION");
    assert_eq!(payload["sessionNum"], 2);
    assert_eq!(payload["previousSessionNum"], 1);
    assert_eq!(payload["sessionType"], "Race");
    assert_eq!(payload["sessionKind"], "RACE");
    assert_eq!(payload["sessionState"], 3);
    assert_eq!(payload["sessionStateName"], "PARADE_LAPS");
    assert_eq!(payload["previousSessionState"], 2);
    assert_eq!(payload["phase"], "RACE_FORMATION");
    assert_eq!(payload["previousPhase"], "RACE_GRIDDING");
    assert_eq!(payload["reason"], "SESSION_STATE_CHANGED");
    assert_eq!(payload["synthetic"], false);
    assert!((payload["sessionTimeRemainS"].as_f64().unwrap() - 2700.5).abs() < 1e-6);
    assert_eq!(payload["sessionLapsRemain"], 20);
    assert_eq!(payload["sessionChangeImminent"], false);
    // Snake-case originals stay for the transition window.
    assert_eq!(payload["session_kind"], "RACE");
    assert_eq!(payload["session_change_imminent"], false);
}

#[test]
fn session_state_ending_signal_carries_the_clock() {
    let event = RaceEvent::SessionState {
        lap: 5,
        session_time: 1140.0,
        session_num: 1,
        previous_session_num: Some(1),
        session_type: Some("Lone Qualify".to_owned()),
        session_kind: SessionKind::Qualifying,
        session_state: 4,
        session_state_name: SessionStateName::Racing,
        previous_session_state: Some(4),
        phase: SessionPhase::QualifyingHotlaps,
        previous_phase: Some(SessionPhase::QualifyingHotlaps),
        reason: SessionStateReason::SessionEnding,
        synthetic: false,
        session_time_remain_s: Some(59.0),
        session_laps_remain: None,
        session_change_imminent: true,
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "88087411",
        "rig-001",
        None,
        Some(88_087_411),
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "SESSION_STATE");
    let payload = &json["payload"];
    assert_eq!(payload["reason"], "SESSION_ENDING");
    assert_eq!(payload["sessionChangeImminent"], true);
    assert_eq!(payload["phase"], "QUALIFYING_HOTLAPS");
    assert!(payload["sessionLapsRemain"].is_null());
}

#[test]
fn transition_goodbye_is_stamped_with_the_live_sub_session_and_names_the_old_one() {
    let event = RaceEvent::PublisherGoodbye {
        lap: 0,
        session_time: 103.0,
        reason: "session_transition".to_owned(),
        previous_sub_session_id: Some(88_429_240),
    };

    let env = build_event(
        &event,
        &minimal_frame(),
        None,
        "88430365",
        "rig-001",
        None,
        Some(88_430_365),
    );
    let json = normalized_event_json(&env);

    assert_envelope_contract(&json, "PUBLISHER_GOODBYE");
    assert_eq!(json["raceSessionId"], "88430365");
    assert_eq!(json["context"]["subSessionId"], 88_430_365i64);
    assert_eq!(json["payload"]["reason"], "session_transition");
    assert_eq!(json["payload"]["previousSubSessionId"], 88_429_240i64);
    // Legacy fields are untouched.
    assert_eq!(json["payload"]["lap"], 0);
    assert!((json["payload"]["sessionTime"].as_f64().unwrap() - 103.0).abs() < 1e-6);
}

#[test]
fn every_envelope_carries_emitted_at_and_a_run_scoped_monotonic_sequence() {
    let event = RaceEvent::RaceGreen {
        lap: 1,
        session_time: 5.0,
        synthetic: false,
        origin: LifecycleOrigin::SessionStateTransition,
    };
    let first = build_event(
        &event,
        &minimal_frame(),
        None,
        "s",
        "rig-001",
        None,
        Some(1),
    );
    let second = build_event(
        &event,
        &minimal_frame(),
        None,
        "s",
        "rig-001",
        None,
        Some(1),
    );

    let a = serde_json::to_value(&first).unwrap();
    let b = serde_json::to_value(&second).unwrap();

    // emittedAt is the RFC 3339 rendering of the same instant as `timestamp`.
    let emitted = a["emittedAt"].as_str().expect("emittedAt string");
    assert!(emitted.ends_with('Z') && emitted.len() == 24, "{emitted}");
    assert_eq!(emitted, format_rfc3339_ms(a["timestamp"].as_i64().unwrap()));
    // One run id per process; sequence strictly increases inside it.
    assert_eq!(a["publisherRunId"], b["publisherRunId"]);
    assert_eq!(a["publisherRunId"], publisher_run_id());
    assert!(b["sequence"].as_u64().unwrap() > a["sequence"].as_u64().unwrap());
    assert!(b["timestamp"].as_i64().unwrap() >= a["timestamp"].as_i64().unwrap());
    // Existing envelope fields are unchanged in type.
    assert!(a["timestamp"].is_i64());
    assert!(a["sequence"].is_u64());
}

#[test]
fn rfc3339_formatting_matches_known_instants() {
    assert_eq!(format_rfc3339_ms(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(
        format_rfc3339_ms(951_782_400_000),
        "2000-02-29T00:00:00.000Z"
    );
    assert_eq!(
        format_rfc3339_ms(1_788_391_719_745),
        "2026-09-02T23:28:39.745Z"
    );
}
