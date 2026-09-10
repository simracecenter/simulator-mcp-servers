// SPDX-License-Identifier: GPL-3.0-or-later

//! Wire-contract guard for the publisher event catalog.
//!
//! `contracts/publisher-event-catalog.json` is the checked-in list of every
//! `type` value (and its envelope scope) this publisher can put on the wire.
//! Race Control keeps a byte-identical copy at
//! `api/contracts/publisher-event-catalog.json` and asserts its ingest
//! allowlist accepts every entry, so a type added here without being added
//! there fails Race Control's test suite instead of being silently rejected at
//! runtime with `unknown type "..."`.
//!
//! Regenerate after adding or renaming a `RaceEvent` variant:
//!
//! ```text
//! UPDATE_PUBLISHER_EVENT_CATALOG=1 cargo test --test event_catalog_contract
//! ```
//!
//! then copy the file into margic/racecontrol.

use std::fs;
use std::path::PathBuf;

use publisher::race_event::{EventScope, LifecycleOrigin, RaceEvent, RaceEventKind};
use serde_json::{json, Value};

const CATALOG_PATH: &str = "contracts/publisher-event-catalog.json";
const UPDATE_ENV: &str = "UPDATE_PUBLISHER_EVENT_CATALOG";

fn catalog_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(CATALOG_PATH)
}

fn scope_label(scope: EventScope) -> &'static str {
    match scope {
        EventScope::CarScoped => "CAR_SCOPED",
        EventScope::RigScoped => "RIG_SCOPED",
        EventScope::SessionScoped => "SESSION_SCOPED",
    }
}

/// The catalog as the code currently defines it.
fn current_catalog() -> Value {
    let events: Vec<Value> = RaceEventKind::all()
        .map(|kind| {
            json!({
                "type": kind.event_type(),
                "scope": scope_label(kind.scope()),
            })
        })
        .collect();

    json!({
        "schemaVersion": 1,
        "source": "margic/director-narrative-core src/race_event.rs",
        "description": "Every event type the publisher emits, with the envelope scope it emits. Mirrored into margic/racecontrol at api/contracts/publisher-event-catalog.json.",
        "events": events,
    })
}

fn serialize(catalog: &Value) -> String {
    format!("{}\n", serde_json::to_string_pretty(catalog).unwrap())
}

#[test]
fn catalog_file_matches_the_emitted_event_types() {
    let expected = serialize(&current_catalog());
    let path = catalog_path();

    if std::env::var_os(UPDATE_ENV).is_some() {
        fs::write(&path, &expected).expect("catalog file is writable");
        return;
    }

    let committed = fs::read_to_string(&path).expect("catalog file exists");
    assert_eq!(
        committed.replace("\r\n", "\n"),
        expected,
        "{CATALOG_PATH} is stale.\n\
         Regenerate it with `{UPDATE_ENV}=1 cargo test --test event_catalog_contract`, \
         then copy it to margic/racecontrol api/contracts/publisher-event-catalog.json \
         and add any new type to PUBLISHER_EVENT_TYPES there — otherwise Race Control \
         rejects the event with `unknown type`."
    );
}

#[test]
fn catalog_entries_are_unique_and_screaming_snake_case() {
    let mut types: Vec<String> = RaceEventKind::all().map(|k| k.event_type()).collect();
    let count = types.len();
    types.sort();
    types.dedup();
    assert_eq!(types.len(), count, "duplicate event type in the catalog");

    for event_type in &types {
        assert!(
            event_type
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'),
            "event type {event_type} is not SCREAMING_SNAKE_CASE"
        );
    }
}

#[test]
fn kind_matches_the_serialised_discriminator_tag() {
    // `build_event` hoists the type from `RaceEvent::kind`, so this pins that
    // mapping to what serde would have written for the same variant.
    let samples = [
        RaceEvent::RaceGreen {
            lap: 1,
            session_time: 10.0,
            synthetic: false,
            origin: LifecycleOrigin::SessionStateTransition,
        },
        RaceEvent::IracingConnected {
            lap: 0,
            session_time: 0.0,
        },
        RaceEvent::DriverEnteredCar {
            lap: 1,
            session_time: 1.0,
            player_car_idx: 3,
        },
        RaceEvent::FocusMeRequested {
            lap: 4,
            session_time: 300.5,
            player_car_idx: 3,
            request_id: "req-1".into(),
            press_seq: 1,
            driver_id: "user:1".into(),
            rig_id: "rig-sim01".into(),
            source: "simulated".into(),
            button: 8,
            requested_at_ms: 1,
            dwell_ms: 10_000,
        },
        RaceEvent::BroadcastControlRequested {
            lap: 4,
            session_time: 300.5,
            action: "toggle".into(),
            request_id: "req-2".into(),
            press_seq: 2,
            driver_id: "user:1".into(),
            rig_id: "rig-sim01".into(),
            source: "simulated".into(),
            button: 7,
            requested_at_ms: 1,
        },
    ];

    for event in &samples {
        let serialised = serde_json::to_value(event).unwrap();
        assert_eq!(
            serialised["event_type"].as_str().unwrap(),
            event.kind().event_type(),
            "kind() disagrees with the serde discriminator tag"
        );
    }
}
