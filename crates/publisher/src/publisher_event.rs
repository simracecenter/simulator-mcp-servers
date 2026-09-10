// SPDX-License-Identifier: GPL-3.0-or-later

//! `PublisherEvent` envelope — wire format for `/api/publisher/v2/ingest`.
//!
//! Every [`RaceEvent`] emitted by the engine is wrapped in a [`PublisherEvent`]
//! before being batched and POSTed to Race Control. The envelope carries
//! identity, timing, and session-context metadata that the engine itself does
//! not need.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::race_event::{EventScope, FlagScope, RaceEvent};
use crate::session_info::{CarRef, SessionMetadata, SessionRoster};
use crate::telemetry_frame::TelemetryFrame;

/// Revision of the payload contract this publisher writes. Emitted on every
/// envelope as `contractVersion` so a consumer can gate on the identity,
/// subject, and severity fields introduced by a revision instead of sniffing
/// for individual keys.
///
/// * `1` — implicit; envelope identity only (`rigId`, `car`).
/// * `2` — publisher and subject identity on every event, canonical camelCase
///   payload keys, unique `eventKey`/`sequence`, normalised incident severity.
pub const PAYLOAD_CONTRACT_VERSION: u32 = 2;

/// Monotonic counter over every event this process builds. Ticks repeat and
/// several events can share one, so the counter — not the tick — is what makes
/// [`PublisherEvent::event_key`] unique.
static EVENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Identity of this publisher process, minted once at first use. Together
/// with [`PublisherEvent::sequence`] it lets a consumer tell a restart (new
/// run id, sequence back to zero) from a gap or reordering inside one run.
static PUBLISHER_RUN_ID: OnceLock<String> = OnceLock::new();

/// Stable id of the running publisher process.
pub fn publisher_run_id() -> &'static str {
    PUBLISHER_RUN_ID.get_or_init(|| Uuid::new_v4().to_string())
}

/// Format wall-clock milliseconds since the Unix epoch as RFC 3339 UTC with
/// millisecond precision, e.g. `2026-09-03T01:37:00.123Z`.
pub fn format_rfc3339_ms(epoch_ms: i64) -> String {
    let secs = epoch_ms.div_euclid(1000);
    let millis = epoch_ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (hour, minute, second) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{millis:03}Z")
}

// ── Envelope types ────────────────────────────────────────────────────────────

/// Wire envelope — serialises to the Race Control API schema exactly.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherEvent {
    /// UUID v4 — idempotency key; unique per event emission.
    pub id: String,
    pub race_session_id: String,
    pub rig_id: String,
    /// `PublisherEventType` string value (e.g. `"BATTLE_CLOSING"`).
    #[serde(rename = "type")]
    pub event_type: String,
    /// Wall-clock milliseconds since Unix epoch at the moment of construction.
    pub timestamp: i64,
    /// The same instant as `timestamp`, as RFC 3339 UTC (`...Z`, millisecond
    /// precision). Consumers that compare against an ISO `receivedAt` can
    /// measure delivery lag from this without parsing an integer epoch.
    pub emitted_at: String,
    /// Identity of the publisher process that built this event. Changes on
    /// every restart; `sequence` is monotonic within one run id.
    pub publisher_run_id: String,
    /// iRacing `SessionTime` in seconds.
    pub session_time: f64,
    /// iRacing `SessionTick` counter.
    pub session_tick: i64,
    /// Revision of the payload contract — see [`PAYLOAD_CONTRACT_VERSION`].
    pub contract_version: u32,
    /// Monotonic per-process counter, ordering events published on one tick.
    pub sequence: u64,
    /// Unique idempotency key, stable across transport retries of the same
    /// event: `v2-<subSessionId>-<sessionTick>-<TYPE>-<sequence>`. Unlike a
    /// key derived from the tick alone, a burst published on one tick does
    /// not collide.
    pub event_key: String,
    /// High-level ownership of the event.
    pub scope: EventScope,
    /// Identity of the *publishing rig* — who is sending this event, never
    /// who the event is about. Re-resolved per event so a mid-session car
    /// index reassignment cannot leave a stale identity on the wire.
    pub publisher: PublisherIdentity,
    /// Identity of the car the event is *about*, with its canonical role.
    /// `None` only for session-wide events, which have no subject car.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<SubjectRef>,
    /// Car identity resolved from the session roster for car-scoped events.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub car: Option<CarRef>,
    /// Event-specific fields (all fields of the `RaceEvent` variant except
    /// the `event_type` discriminator tag, which is hoisted to the envelope).
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<PublisherEventContext>,
}

/// Identity of the rig that published an event.
///
/// Deliberately separate from [`SubjectRef`]: the publishing rig and the car
/// an event is about are frequently different cars, and conflating them
/// credits one driver's coverage to another.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherIdentity {
    /// Same value as the envelope's `rigId` — the friendly rig label.
    pub rig_id: String,
    /// Alias of `rig_id`, named for how the label reads in a UI.
    pub rig_label: String,
    /// Car index the rig occupies *now*, not when the session started.
    pub car_idx: u8,
    pub car_number: String,
    /// Durable driver identity — see [`CarRef::driver_id`].
    pub driver_id: String,
    pub driver_name: String,
}

/// Canonical role vocabulary for the car an event is about.
///
/// One vocabulary across every family, so a consumer never has to know that
/// one family says `leader_car_idx` and another `battle_attacker_idx`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SubjectRole {
    /// The car closing, catching, or passing.
    Attacker,
    /// The car being caught, held up, or passed.
    Defender,
    /// The publishing rig's own car; the event describes its driving.
    Driver,
    /// A car involved in an incident.
    Incident,
    /// The car that triggered a flag condition.
    Trigger,
    /// The publishing rig itself — publisher lifecycle and control events.
    Rig,
    /// The session as a whole; there is no subject car.
    Session,
}

/// Identity of the car an event is about.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SubjectRef {
    pub role: SubjectRole,
    pub car_idx: u8,
    pub car_number: String,
    pub driver_id: String,
    pub driver_name: String,
    /// Full roster entry for the subject car.
    pub car: CarRef,
}

/// Supplementary session context attached to every envelope.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublisherEventContext {
    /// Highest value in `CarIdxLapCompleted` — the leader's completed laps.
    pub leader_lap: Option<i32>,
    pub session_state: Option<i32>,
    pub session_flags: Option<u32>,
    pub session_num: Option<i32>,
    pub sub_session_id: Option<i64>,
    pub session_type: Option<String>,
    pub track_name: Option<String>,
}

// ── Builder ───────────────────────────────────────────────────────────────────

/// Wrap a [`RaceEvent`] in a [`PublisherEvent`] envelope ready for serialisation.
///
/// * `roster` — optional current session roster. Car-scoped events resolve a
///   `car` envelope field from the roster, falling back to a stub containing
///   only `carIdx` and the stringified index as `carNumber` when the slot is
///   absent. Session- and rig-scoped events omit `car` entirely.
pub fn build_event(
    race_event: &RaceEvent,
    frame: &TelemetryFrame,
    roster: Option<&SessionRoster>,
    race_session_id: &str,
    rig_id: &str,
    session_meta: Option<&SessionMetadata>,
    sub_session_id: Option<i64>,
) -> PublisherEvent {
    let id = Uuid::new_v4().to_string();
    let sequence = EVENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    let emitted_at = format_rfc3339_ms(timestamp);

    // Serialise the event, hoist the discriminator tag, use remainder as payload.
    let mut event_value =
        serde_json::to_value(race_event).expect("RaceEvent is always serialisable");
    if let Some(map) = event_value.as_object_mut() {
        map.remove("event_type");
    }
    // Taken from the catalogued kind rather than the serialised tag so the wire
    // `type` and `contracts/publisher-event-catalog.json` cannot drift apart.
    let event_type = race_event.kind().event_type();
    let scope = race_event.event_scope();
    let event_key = format!(
        "v2-{}-{}-{}-{}",
        sub_session_id.unwrap_or(0),
        frame.session_tick,
        event_type,
        sequence,
    );

    let publisher = publisher_identity(rig_id, frame.player_car_idx, roster);
    let roles = event_roles(race_event, frame);
    let subject = roles
        .subject
        .map(|idx| subject_ref(roles.role, idx, roster));

    let mut payload = event_value;
    enrich_payload(&mut payload, race_event, frame, roster);
    // camelCase is canonical, so every snake_case key from `RaceEvent` gains a
    // camelCase twin before identity is written in camelCase only.
    add_camel_case_twins(&mut payload);
    insert_identity(&mut payload, &publisher, &roles, subject.as_ref(), roster);

    let car = event_car(race_event, frame.player_car_idx, roster);

    let leader_lap = frame.car_idx_lap_completed.iter().copied().max();
    let context = Some(PublisherEventContext {
        leader_lap,
        session_state: Some(frame.session_state),
        session_flags: Some(frame.session_flags),
        session_num: Some(frame.session_num),
        sub_session_id,
        session_type: session_meta.and_then(|m| m.session_type.clone()),
        track_name: session_meta.and_then(|m| m.track_name.clone()),
    });

    PublisherEvent {
        id,
        race_session_id: race_session_id.to_owned(),
        rig_id: rig_id.to_owned(),
        event_type,
        timestamp,
        emitted_at,
        publisher_run_id: publisher_run_id().to_owned(),
        session_time: frame.session_time as f64,
        session_tick: frame.session_tick,
        contract_version: PAYLOAD_CONTRACT_VERSION,
        sequence,
        event_key,
        scope,
        publisher,
        subject,
        car,
        payload,
        context,
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Return the primary `car_idx` for a given event.
///
/// Battle events are keyed on the *opponent* car; all other events
/// (session, flag, lap, position) are keyed on the player's own car.
fn primary_car_idx(event: &RaceEvent, player_car_idx: u8) -> u8 {
    match event {
        RaceEvent::BattleEngaged {
            opponent_car_idx, ..
        }
        | RaceEvent::BattleBroken {
            opponent_car_idx, ..
        }
        | RaceEvent::BattleClosing {
            opponent_car_idx, ..
        } => *opponent_car_idx,
        RaceEvent::IncidentAlert { car_idx, .. } => *car_idx,
        RaceEvent::FocusMeRequested { player_car_idx, .. } => *player_car_idx,
        _ => player_car_idx,
    }
}

fn event_car(
    event: &RaceEvent,
    player_car_idx: u8,
    roster: Option<&SessionRoster>,
) -> Option<CarRef> {
    match event.event_scope() {
        EventScope::SessionScoped | EventScope::RigScoped => None,
        EventScope::CarScoped => Some(resolve_car(primary_car_idx(event, player_car_idx), roster)),
    }
}

fn event_scope_label(scope: EventScope) -> &'static str {
    match scope {
        EventScope::CarScoped => "CAR_SCOPED",
        EventScope::RigScoped => "RIG_SCOPED",
        EventScope::SessionScoped => "SESSION_SCOPED",
    }
}

/// Resolve a [`CarRef`] from the roster.
///
/// Falls back to a minimal stub when the roster is unavailable or the slot
/// is not yet populated.
fn resolve_car(car_idx: u8, roster: Option<&SessionRoster>) -> CarRef {
    roster
        .and_then(|r| r.lookup(car_idx))
        .cloned()
        .unwrap_or_else(|| CarRef {
            car_idx,
            car_number: car_idx.to_string(),
            driver_name: String::new(),
            team_name: None,
            car_class_short_name: None,
            car_class_id: None,
            user_id: None,
            irating: None,
            lic_string: None,
            flair_name: None,
        })
}

/// Resolve the publishing rig's identity for the car it currently occupies.
fn publisher_identity(
    rig_id: &str,
    player_car_idx: u8,
    roster: Option<&SessionRoster>,
) -> PublisherIdentity {
    let car = resolve_car(player_car_idx, roster);
    PublisherIdentity {
        rig_id: rig_id.to_owned(),
        rig_label: rig_id.to_owned(),
        car_idx: player_car_idx,
        car_number: car.car_number.clone(),
        driver_id: car.driver_id(),
        driver_name: car.driver_name.clone(),
    }
}

fn subject_ref(role: SubjectRole, car_idx: u8, roster: Option<&SessionRoster>) -> SubjectRef {
    let car = resolve_car(car_idx, roster);
    SubjectRef {
        role,
        car_idx,
        car_number: car.car_number.clone(),
        driver_id: car.driver_id(),
        driver_name: car.driver_name.clone(),
        car,
    }
}

/// Canonical roles for one event: the car it is about, plus the two sides of a
/// duel where the family has them.
struct EventRoles {
    subject: Option<u8>,
    role: SubjectRole,
    attacker: Option<u8>,
    defender: Option<u8>,
}

impl EventRoles {
    /// A duel: the attacker is the subject, since it is the car creating the
    /// moment.
    fn duel(attacker: u8, defender: Option<u8>) -> Self {
        Self {
            subject: Some(attacker),
            role: SubjectRole::Attacker,
            attacker: Some(attacker),
            defender,
        }
    }

    /// A duel told from the defender's side (vulnerability).
    fn defence(defender: u8, attacker: u8) -> Self {
        Self {
            subject: Some(defender),
            role: SubjectRole::Defender,
            attacker: Some(attacker),
            defender: Some(defender),
        }
    }

    fn solo(car_idx: u8, role: SubjectRole) -> Self {
        Self {
            subject: Some(car_idx),
            role,
            attacker: None,
            defender: None,
        }
    }

    fn session(role: SubjectRole) -> Self {
        Self {
            subject: None,
            role,
            attacker: None,
            defender: None,
        }
    }
}

/// Map an event onto the canonical role vocabulary.
///
/// This is the single place that knows a family's own field names
/// (`leader_car_idx`, `battle_attacker_idx`, `attacker_idx`, …); everything
/// downstream reads `subject`/`attacker`/`defender`.
fn event_roles(event: &RaceEvent, frame: &TelemetryFrame) -> EventRoles {
    let me = frame.player_car_idx;
    match event {
        // The car behind is attacking, whichever side the rig is on.
        RaceEvent::BattleEngaged {
            player_car_idx,
            opponent_car_idx,
            ..
        }
        | RaceEvent::BattleBroken {
            player_car_idx,
            opponent_car_idx,
            ..
        }
        | RaceEvent::BattleClosing {
            player_car_idx,
            opponent_car_idx,
            ..
        } => {
            let (leader_idx, follower_idx) =
                leader_follower_indices(frame, *player_car_idx, *opponent_car_idx);
            EventRoles::duel(follower_idx, Some(leader_idx))
        }
        RaceEvent::HorizonClosing {
            attacker_car_idx,
            defender_car_idx,
            ..
        }
        | RaceEvent::HorizonClosingResolved {
            attacker_car_idx,
            defender_car_idx,
            ..
        } => EventRoles::duel(*attacker_car_idx, Some(*defender_car_idx)),
        RaceEvent::VulnerabilityAlert {
            attacker_idx,
            defender_idx,
            ..
        }
        | RaceEvent::VulnerabilityResolved {
            attacker_idx,
            defender_idx,
            ..
        } => EventRoles::defence(*defender_idx, *attacker_idx),
        // The lapping/faster car is closing on the traffic car ahead.
        RaceEvent::TrafficIntercept {
            leader_car_idx,
            traffic_car_idx,
            ..
        } => EventRoles::duel(*leader_car_idx, Some(*traffic_car_idx)),
        RaceEvent::TrafficCompressionZone {
            battle_attacker_idx,
            battle_defender_idx,
            ..
        } => EventRoles::duel(*battle_attacker_idx, Some(*battle_defender_idx)),
        RaceEvent::Overtake {
            car_idx,
            overtaken_car_idx,
            ..
        }
        | RaceEvent::OvertakeForLead {
            car_idx,
            overtaken_car_idx,
            ..
        } => EventRoles::duel(*car_idx, *overtaken_car_idx),
        RaceEvent::IncidentAlert { car_idx, .. } => {
            EventRoles::solo(*car_idx, SubjectRole::Incident)
        }
        RaceEvent::IncidentCluster {
            primary_car_idx,
            car_idxs,
            ..
        } => {
            match primary_car_idx.or_else(|| car_idxs.first().copied()) {
                Some(idx) => EventRoles::solo(idx, SubjectRole::Incident),
                // A cluster with no cars left in it has no subject to name.
                None => EventRoles::session(SubjectRole::Incident),
            }
        }
        // Resolution of a cluster is about the cluster, not a car.
        RaceEvent::IncidentClusterResolved { .. } => EventRoles::session(SubjectRole::Incident),
        RaceEvent::FlagYellowLocal {
            trigger_car_idx, ..
        } => match trigger_car_idx {
            Some(idx) => EventRoles::solo(*idx, SubjectRole::Trigger),
            None => EventRoles::session(SubjectRole::Trigger),
        },
        RaceEvent::RaceGreen { .. }
        | RaceEvent::RaceCheckered { .. }
        | RaceEvent::FlagYellowFullCourse { .. }
        | RaceEvent::SessionReset { .. }
        | RaceEvent::SessionState { .. } => EventRoles::session(SubjectRole::Session),
        // System events are about the rig, but still name its car so the
        // consumer can bind the rig to a car without a separate lookup.
        RaceEvent::PublisherHello { .. }
        | RaceEvent::PublisherGoodbye { .. }
        | RaceEvent::PublisherHeartbeat { .. }
        | RaceEvent::IracingConnected { .. }
        | RaceEvent::IracingDisconnected { .. }
        | RaceEvent::BroadcastControlRequested { .. } => EventRoles::solo(me, SubjectRole::Rig),
        RaceEvent::FocusMeRequested { player_car_idx, .. }
        | RaceEvent::DriverMaterial { player_car_idx, .. } => {
            EventRoles::solo(*player_car_idx, SubjectRole::Driver)
        }
        // Everything else describes the rig's own driving: laps, pit, tires,
        // fuel, braking, micro-sectors, driver swaps.
        _ => EventRoles::solo(primary_car_idx(event, me), SubjectRole::Driver),
    }
}

/// Write publisher and subject identity into a payload.
///
/// Both go on *every* event, race and system alike, and stay separate: a
/// consumer reads `publisherCarIdx` to learn which car the rig is in and
/// `subjectCarIdx` to learn which car the event is about.
fn insert_identity(
    payload: &mut Value,
    publisher: &PublisherIdentity,
    roles: &EventRoles,
    subject: Option<&SubjectRef>,
    roster: Option<&SessionRoster>,
) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };

    obj.insert(
        "publisher".to_owned(),
        serde_json::to_value(publisher).unwrap_or(Value::Null),
    );
    obj.insert("rigId".to_owned(), Value::String(publisher.rig_id.clone()));
    obj.insert(
        "rigLabel".to_owned(),
        Value::String(publisher.rig_label.clone()),
    );
    obj.insert("publisherCarIdx".to_owned(), json!(publisher.car_idx));
    obj.insert(
        "publisherCarNumber".to_owned(),
        Value::String(publisher.car_number.clone()),
    );
    obj.insert(
        "publisherDriverId".to_owned(),
        Value::String(publisher.driver_id.clone()),
    );
    obj.insert(
        "publisherDriverName".to_owned(),
        Value::String(publisher.driver_name.clone()),
    );

    obj.insert(
        "subjectRole".to_owned(),
        serde_json::to_value(roles.role).unwrap_or(Value::Null),
    );
    obj.insert(
        "subject".to_owned(),
        serde_json::to_value(subject).unwrap_or(Value::Null),
    );
    obj.insert(
        "subjectCarIdx".to_owned(),
        option_u8_json(subject.map(|s| s.car_idx)),
    );
    obj.insert(
        "subjectCarNumber".to_owned(),
        subject.map_or(Value::Null, |s| Value::String(s.car_number.clone())),
    );
    obj.insert(
        "subjectDriverId".to_owned(),
        subject.map_or(Value::Null, |s| Value::String(s.driver_id.clone())),
    );

    for (prefix, car_idx) in [("attacker", roles.attacker), ("defender", roles.defender)] {
        let car = car_idx.map(|idx| resolve_car(idx, roster));
        obj.insert(format!("{prefix}CarIdx"), option_u8_json(car_idx));
        obj.insert(
            format!("{prefix}CarNumber"),
            car.as_ref()
                .map_or(Value::Null, |c| Value::String(c.car_number.clone())),
        );
        obj.insert(
            format!("{prefix}Car"),
            car.as_ref().map_or(Value::Null, |c| {
                serde_json::to_value(c).unwrap_or(Value::Null)
            }),
        );
        obj.insert(
            format!("{prefix}DriverId"),
            car.as_ref()
                .map_or(Value::Null, |c| Value::String(c.driver_id())),
        );
    }
}

/// Give every `snake_case` payload key a `camelCase` twin, recursively.
///
/// camelCase is the canonical scheme for payload keys, but `RaceEvent`
/// serialises its fields in snake_case. Rather than rename variant fields —
/// which would break the production consumer — the canonical key is added
/// alongside and the snake_case original is left in place for the transition
/// window. Existing keys are never overwritten, so hand-written aliases such
/// as `lapTime` win over a mechanical twin.
fn add_camel_case_twins(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (_, nested) in map.iter_mut() {
                add_camel_case_twins(nested);
            }
            let twins: Vec<(String, Value)> = map
                .iter()
                .filter_map(|(key, nested)| {
                    let camel = to_camel_case(key);
                    if camel == *key || map.contains_key(&camel) {
                        None
                    } else {
                        Some((camel, nested.clone()))
                    }
                })
                .collect();
            for (key, nested) in twins {
                map.insert(key, nested);
            }
        }
        Value::Array(items) => {
            for item in items {
                add_camel_case_twins(item);
            }
        }
        _ => {}
    }
}

fn to_camel_case(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper_next = false;
    for ch in key.chars() {
        if ch == '_' {
            upper_next = !out.is_empty();
        } else if upper_next {
            out.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            out.push(ch);
        }
    }
    out
}

fn enrich_payload(
    payload: &mut Value,
    race_event: &RaceEvent,
    frame: &TelemetryFrame,
    roster: Option<&SessionRoster>,
) {
    let Some(obj) = payload.as_object_mut() else {
        return;
    };

    obj.insert(
        "eventScope".to_owned(),
        Value::String(event_scope_label(race_event.event_scope()).to_owned()),
    );

    match race_event {
        RaceEvent::LapCompleted {
            lap_time_s,
            best_lap_time_s,
            ..
        } => {
            obj.insert("lapTime".to_owned(), option_f32_json(*lap_time_s));
            obj.insert("bestLapTime".to_owned(), option_f32_json(*best_lap_time_s));
        }
        RaceEvent::BattleEngaged {
            player_car_idx,
            opponent_car_idx,
            gap_s,
            engagement_started_at_session_time_s,
            ..
        } => {
            let (leader_idx, follower_idx) =
                leader_follower_indices(frame, *player_car_idx, *opponent_car_idx);
            let leader = resolve_car(leader_idx, roster);
            let follower = resolve_car(follower_idx, roster);
            let leader_pos = car_race_position(frame, leader_idx);
            let follower_pos = car_race_position(frame, follower_idx);

            // Legacy fields for transition window
            obj.insert(
                "leaderCarNumber".to_owned(),
                Value::String(leader.car_number.clone()),
            );
            obj.insert(
                "followerCarNumber".to_owned(),
                Value::String(follower.car_number.clone()),
            );

            // New structured car references (primary source of truth)
            obj.insert(
                "leaderCar".to_owned(),
                serde_json::to_value(&leader).unwrap_or(Value::Null),
            );
            obj.insert(
                "followerCar".to_owned(),
                serde_json::to_value(&follower).unwrap_or(Value::Null),
            );

            // Explicit race positions for both sides of the battle.
            obj.insert("leaderRacePosition".to_owned(), option_u8_json(leader_pos));
            obj.insert(
                "followerRacePosition".to_owned(),
                option_u8_json(follower_pos),
            );
            obj.insert(
                "playerRacePosition".to_owned(),
                option_u8_json(car_race_position(frame, *player_car_idx)),
            );
            obj.insert(
                "opponentRacePosition".to_owned(),
                option_u8_json(car_race_position(frame, *opponent_car_idx)),
            );

            // Gap at engagement (sanitized) and engagement start time (camelCase aliases)
            obj.insert(
                "engagementGapSec".to_owned(),
                sanitize_sentinel_json(*gap_s),
            );
            obj.insert(
                "engagementStartedAtSessionTime".to_owned(),
                json!(engagement_started_at_session_time_s),
            );
        }
        RaceEvent::BattleBroken {
            player_car_idx,
            opponent_car_idx,
            final_gap_sec,
            engagement_started_at_session_time_s,
            session_time,
            ..
        } => {
            let (leader_idx, follower_idx) =
                leader_follower_indices(frame, *player_car_idx, *opponent_car_idx);
            let leader = resolve_car(leader_idx, roster);
            let follower = resolve_car(follower_idx, roster);
            let leader_pos = car_race_position(frame, leader_idx);
            let follower_pos = car_race_position(frame, follower_idx);

            // Legacy fields for transition window
            obj.insert(
                "leaderCarNumber".to_owned(),
                Value::String(leader.car_number.clone()),
            );
            obj.insert(
                "followerCarNumber".to_owned(),
                Value::String(follower.car_number.clone()),
            );

            // New structured car references (primary source of truth)
            obj.insert(
                "leaderCar".to_owned(),
                serde_json::to_value(&leader).unwrap_or(Value::Null),
            );
            obj.insert(
                "followerCar".to_owned(),
                serde_json::to_value(&follower).unwrap_or(Value::Null),
            );

            // Explicit race positions for both sides of the battle.
            obj.insert("leaderRacePosition".to_owned(), option_u8_json(leader_pos));
            obj.insert(
                "followerRacePosition".to_owned(),
                option_u8_json(follower_pos),
            );
            obj.insert(
                "playerRacePosition".to_owned(),
                option_u8_json(car_race_position(frame, *player_car_idx)),
            );
            obj.insert(
                "opponentRacePosition".to_owned(),
                option_u8_json(car_race_position(frame, *opponent_car_idx)),
            );

            // Final gap (None when the gap was a sentinel / car no longer visible)
            obj.insert("finalGapSec".to_owned(), option_f32_json(*final_gap_sec));
            let duration = (session_time - engagement_started_at_session_time_s).max(0.0);
            obj.insert("engagementDurationSec".to_owned(), json!(duration));
        }
        RaceEvent::BattleClosing {
            player_car_idx,
            opponent_car_idx,
            ..
        } => {
            let (leader_idx, follower_idx) =
                leader_follower_indices(frame, *player_car_idx, *opponent_car_idx);
            let leader = resolve_car(leader_idx, roster);
            let follower = resolve_car(follower_idx, roster);
            let leader_pos = car_race_position(frame, leader_idx);
            let follower_pos = car_race_position(frame, follower_idx);

            // Legacy fields for transition window
            obj.insert(
                "leaderCarNumber".to_owned(),
                Value::String(leader.car_number.clone()),
            );
            obj.insert(
                "followerCarNumber".to_owned(),
                Value::String(follower.car_number.clone()),
            );

            // New structured car references (primary source of truth)
            obj.insert(
                "leaderCar".to_owned(),
                serde_json::to_value(&leader).unwrap_or(Value::Null),
            );
            obj.insert(
                "followerCar".to_owned(),
                serde_json::to_value(&follower).unwrap_or(Value::Null),
            );

            // Explicit race positions for both sides of the battle.
            obj.insert("leaderRacePosition".to_owned(), option_u8_json(leader_pos));
            obj.insert(
                "followerRacePosition".to_owned(),
                option_u8_json(follower_pos),
            );
            obj.insert(
                "playerRacePosition".to_owned(),
                option_u8_json(car_race_position(frame, *player_car_idx)),
            );
            obj.insert(
                "opponentRacePosition".to_owned(),
                option_u8_json(car_race_position(frame, *opponent_car_idx)),
            );
        }
        RaceEvent::DriverMaterial {
            last_lap_time_s,
            best_lap_time_s,
            gap_ahead_s,
            car_ahead_idx,
            gap_behind_s,
            car_behind_idx,
            delta_to_best_s,
            sector_delta_to_best_s,
            track_surface,
            ..
        } => {
            obj.insert("lastLapTime".to_owned(), option_f32_json(*last_lap_time_s));
            obj.insert("bestLapTime".to_owned(), option_f32_json(*best_lap_time_s));
            obj.insert("deltaToBest".to_owned(), option_f32_json(*delta_to_best_s));
            obj.insert(
                "sectorDeltaToBest".to_owned(),
                option_f32_json(*sector_delta_to_best_s),
            );
            obj.insert("gapAhead".to_owned(), option_f32_json(*gap_ahead_s));
            obj.insert("gapBehind".to_owned(), option_f32_json(*gap_behind_s));
            obj.insert("trackSurface".to_owned(), json!(track_surface));
            // Name the neighbours so the consumer can build a battle without a
            // second lookup against its own roster copy.
            let neighbour = |idx: &Option<u8>| {
                idx.map(|i| serde_json::to_value(resolve_car(i, roster)).unwrap_or(Value::Null))
                    .unwrap_or(Value::Null)
            };
            obj.insert("carAhead".to_owned(), neighbour(car_ahead_idx));
            obj.insert("carBehind".to_owned(), neighbour(car_behind_idx));
        }
        RaceEvent::VulnerabilityAlert {
            attacker_idx,
            defender_idx,
            ..
        } => {
            obj.insert(
                "attackerPosition".to_owned(),
                option_u8_json(car_race_position(frame, *attacker_idx)),
            );
            obj.insert(
                "defenderPosition".to_owned(),
                option_u8_json(car_race_position(frame, *defender_idx)),
            );
        }
        RaceEvent::Overtake {
            car_idx,
            overtaken_car_idx,
            ..
        } => {
            let overtaking_car = resolve_car(*car_idx, roster);

            // New structured car references (primary source of truth)
            obj.insert(
                "overtakingCar".to_owned(),
                serde_json::to_value(&overtaking_car).unwrap_or(Value::Null),
            );

            if let Some(overtaken_idx) = overtaken_car_idx {
                let overtaken_car = resolve_car(*overtaken_idx, roster);
                obj.insert(
                    "overtakenCar".to_owned(),
                    serde_json::to_value(&overtaken_car).unwrap_or(Value::Null),
                );
            }
        }
        RaceEvent::OvertakeForLead {
            car_idx,
            overtaken_car_idx,
            ..
        } => {
            let overtaking_car = resolve_car(*car_idx, roster);

            // New structured car references (primary source of truth)
            obj.insert(
                "overtakingCar".to_owned(),
                serde_json::to_value(&overtaking_car).unwrap_or(Value::Null),
            );

            if let Some(overtaken_idx) = overtaken_car_idx {
                let overtaken_car = resolve_car(*overtaken_idx, roster);
                obj.insert(
                    "overtakenCar".to_owned(),
                    serde_json::to_value(&overtaken_car).unwrap_or(Value::Null),
                );
            }
        }
        RaceEvent::TrafficIntercept {
            traffic_car_idx, ..
        } => {
            let traffic_car = resolve_car(*traffic_car_idx, roster);
            obj.insert(
                "trafficCar".to_owned(),
                serde_json::to_value(&traffic_car).unwrap_or(Value::Null),
            );
        }
        RaceEvent::HorizonClosing {
            attacker_car_idx,
            defender_car_idx,
            ..
        } => {
            let attacker_car = resolve_car(*attacker_car_idx, roster);
            let defender_car = resolve_car(*defender_car_idx, roster);
            obj.insert(
                "attackerCar".to_owned(),
                serde_json::to_value(&attacker_car).unwrap_or(Value::Null),
            );
            obj.insert(
                "defenderCar".to_owned(),
                serde_json::to_value(&defender_car).unwrap_or(Value::Null),
            );
        }
        RaceEvent::IncidentCluster {
            car_idxs,
            primary_car_idx,
            incident_type,
            lap_dist_pct_from,
            lap_dist_pct_to,
            ..
        } => {
            // Resolve all involved cars to a CarRef array
            let involved_cars: Vec<Value> = car_idxs
                .iter()
                .map(|&idx| serde_json::to_value(resolve_car(idx, roster)).unwrap_or(Value::Null))
                .collect();
            obj.insert("involvedCars".to_owned(), Value::Array(involved_cars));

            // Resolve primary car (most-culpable)
            let primary = primary_car_idx
                .map(|idx| serde_json::to_value(resolve_car(idx, roster)).unwrap_or(Value::Null))
                .unwrap_or(Value::Null);
            obj.insert("primaryCar".to_owned(), primary);

            // Incident classification
            let itype = incident_type
                .as_deref()
                .map(Value::from)
                .unwrap_or(Value::Null);
            obj.insert("incidentType".to_owned(), itype);

            // Cluster centroid lap distance percentage
            obj.insert(
                "lapDistPct".to_owned(),
                json!((lap_dist_pct_from + lap_dist_pct_to) / 2.0),
            );
        }
        RaceEvent::IncidentAlert {
            driver_incident_count,
            previous_track_surface,
            current_track_surface,
            previous_speed_mps,
            current_speed_mps,
            speed_drop_mps,
            severity,
            severity_normalized,
            incident_count_delta,
            reason,
            ..
        } => {
            obj.insert(
                "driverIncidentCount".to_owned(),
                driver_incident_count
                    .map(Value::from)
                    .unwrap_or(Value::Null),
            );
            obj.insert(
                "previousTrackSurface".to_owned(),
                json!(previous_track_surface),
            );
            obj.insert(
                "currentTrackSurface".to_owned(),
                json!(current_track_surface),
            );
            // Documented name for the surface a car is on *now*; the
            // `current`-prefixed keys stay for the transition window.
            obj.insert("trackSurface".to_owned(), json!(current_track_surface));
            obj.insert("previousSpeedMps".to_owned(), json!(previous_speed_mps));
            obj.insert("currentSpeedMps".to_owned(), json!(current_speed_mps));
            obj.insert("speedDropMps".to_owned(), json!(speed_drop_mps));
            // `severityScore`/`severity` are the raw magnitude, whose units
            // depend on `reason`; `severityNormalized` is always 0.0–1.0 and
            // is what a quality floor should compare against.
            obj.insert("severityScore".to_owned(), json!(severity));
            obj.insert("severityNormalized".to_owned(), json!(severity_normalized));
            obj.insert(
                "incidentCountDelta".to_owned(),
                incident_count_delta.map(Value::from).unwrap_or(Value::Null),
            );
            obj.insert("reason".to_owned(), Value::String(reason.clone()));
        }
        RaceEvent::FlagYellowLocal {
            trigger_car_idx,
            lap_dist_pct,
            scope,
            ..
        } => {
            // Resolve trigger car to a structured CarRef when known.
            if let Some(idx) = trigger_car_idx {
                let trigger_car = resolve_car(*idx, roster);
                obj.insert(
                    "triggerCar".to_owned(),
                    serde_json::to_value(&trigger_car).unwrap_or(Value::Null),
                );
            }
            // Format track location as a human-readable percentage string, reused in the reason below.
            let location_pct_str = lap_dist_pct.map(|p| format!("{:.1}%", p * 100.0));
            if let Some(ref s) = location_pct_str {
                obj.insert("trackLocationPct".to_owned(), Value::String(s.clone()));
            }
            // Flag scope as a string matching the FlagScope enum variant name.
            let scope_str = match scope {
                FlagScope::SelfCaused => "SelfCaused",
                FlagScope::Nearby => "Nearby",
                FlagScope::SessionWide => "SessionWide",
                FlagScope::Unknown => "Unknown",
            };
            obj.insert("flagScope".to_owned(), Value::String(scope_str.to_owned()));
            // Human-readable summary for narration.
            let reason = match scope {
                FlagScope::SelfCaused => "Incident caused by player".to_owned(),
                FlagScope::Nearby => {
                    let loc = location_pct_str.as_deref().unwrap_or("unknown location");
                    format!("Incident nearby at {loc}")
                }
                FlagScope::SessionWide => "Session-wide caution".to_owned(),
                FlagScope::Unknown => "Yellow flag condition".to_owned(),
            };
            obj.insert("reason".to_owned(), Value::String(reason));
        }
        _ => {}
    }
}

/// Threshold above which a raw f32 telemetry value is treated as a
/// missing-data sentinel (e.g. iRacing's `f32::MAX` / `3.4e+38`).
const F32_SENTINEL_THRESHOLD: f32 = 1e30;

fn option_f32_json(v: Option<f32>) -> Value {
    match v {
        Some(n) => json!(n),
        None => Value::Null,
    }
}

fn option_u8_json(v: Option<u8>) -> Value {
    match v {
        Some(n) => json!(n),
        None => Value::Null,
    }
}

/// Convert a raw f32 telemetry value to JSON, mapping sentinel values
/// (NaN, Infinite, or values >= `F32_SENTINEL_THRESHOLD` such as `f32::MAX`)
/// to `null` to prevent invalid data from reaching Cosmos.
fn sanitize_sentinel_json(v: f32) -> Value {
    if v.is_nan() || v.is_infinite() || v >= F32_SENTINEL_THRESHOLD {
        if cfg!(debug_assertions) {
            eprintln!("[publisher] sentinel value detected in telemetry: {v}");
        }
        Value::Null
    } else {
        json!(v)
    }
}

fn leader_follower_indices(frame: &TelemetryFrame, player_idx: u8, opponent_idx: u8) -> (u8, u8) {
    let player_pos = frame
        .car_idx_position
        .get(player_idx as usize)
        .copied()
        .filter(|p| *p > 0);
    let opponent_pos = frame
        .car_idx_position
        .get(opponent_idx as usize)
        .copied()
        .filter(|p| *p > 0);

    match (player_pos, opponent_pos) {
        (Some(pp), Some(op)) if op < pp => (opponent_idx, player_idx),
        (Some(pp), Some(op)) if pp < op => (player_idx, opponent_idx),
        _ => (player_idx, opponent_idx),
    }
}

fn car_race_position(frame: &TelemetryFrame, car_idx: u8) -> Option<u8> {
    frame
        .car_idx_position
        .get(car_idx as usize)
        .copied()
        .filter(|p| *p > 0)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::battle_state::SlopeInfo;
    use crate::race_event::{LifecycleOrigin, RaceEvent};
    use crate::session_info::{CarRef, SessionMetadata, SessionRoster};
    use crate::telemetry_frame::TelemetryFrame;

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

    fn enriched_roster() -> SessionRoster {
        SessionRoster::from_cars(vec![
            CarRef {
                car_idx: 0,
                car_number: "42".to_owned(),
                driver_name: "Player".to_owned(),
                team_name: Some("Team A".to_owned()),
                car_class_short_name: Some("BMW M2".to_owned()),
                car_class_id: Some(4073),
                user_id: Some(123456),
                irating: Some(2680),
                lic_string: Some("D 2.79".to_owned()),
                flair_name: Some("United States".to_owned()),
            },
            CarRef {
                car_idx: 1,
                car_number: "7".to_owned(),
                driver_name: "Opponent".to_owned(),
                team_name: Some("Team B".to_owned()),
                car_class_short_name: Some("BMW M2".to_owned()),
                car_class_id: Some(4073),
                user_id: Some(999111),
                irating: Some(1400),
                lic_string: Some("R 2.50".to_owned()),
                flair_name: Some("Spain".to_owned()),
            },
        ])
    }

    #[test]
    fn battle_closing_json_shape() {
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
        let json: Value = serde_json::to_value(&env).unwrap();

        // Envelope-level fields
        assert!(
            json["id"].as_str().map(|s| s.len() == 36).unwrap_or(false),
            "id should be a UUID string"
        );
        assert_eq!(json["type"], "BATTLE_CLOSING");
        assert_eq!(json["raceSessionId"], "session-abc");
        assert_eq!(json["rigId"], "rig-001");
        assert_eq!(json["sessionTime"], 1234.5_f64);
        assert_eq!(json["sessionTick"], 9876_i64);

        // Car fallback (no roster)
        assert_eq!(json["car"]["carIdx"], 1); // opponent car_idx, not player

        // Payload contains event-specific fields (field names are snake_case)
        let rate = json["payload"]["closing_rate_sec_per_lap"]
            .as_f64()
            .unwrap_or(0.0);
        assert!((rate - 0.43).abs() < 1e-4, "expected ~0.43, got {rate}");
        assert_eq!(json["payload"]["opponent_car_idx"], 1);
        assert_eq!(json["payload"]["player_car_idx"], 0);
        assert_eq!(json["payload"]["lap"], 4);
        assert!(
            json["payload"].get("event_type").is_none(),
            "event_type should be hoisted out of payload"
        );

        // Context block
        assert_eq!(json["context"]["leaderLap"], 3);
        assert_eq!(json["context"]["sessionState"], 4);
        assert_eq!(json["context"]["sessionFlags"], 0);
        assert_eq!(json["context"]["sessionNum"], 0);
    }

    #[test]
    fn uuid_is_unique_across_calls() {
        let event = RaceEvent::RaceGreen {
            lap: 1,
            session_time: 0.0,
            synthetic: false,
            origin: LifecycleOrigin::SessionStateTransition,
        };
        let frame = minimal_frame();
        let e1 = build_event(&event, &frame, None, "s", "r", None, None);
        let e2 = build_event(&event, &frame, None, "s", "r", None, None);
        assert_ne!(e1.id, e2.id);
    }

    #[test]
    fn session_events_omit_car_and_use_session_scope() {
        let event = RaceEvent::RaceGreen {
            lap: 1,
            session_time: 0.0,
            synthetic: false,
            origin: LifecycleOrigin::SessionStateTransition,
        };
        let frame = minimal_frame(); // player_car_idx = 0
        let env = build_event(&event, &frame, None, "s", "r", None, None);
        let json: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(env.scope, EventScope::SessionScoped);
        assert!(env.car.is_none());
        assert!(json.get("car").is_none());
        assert_eq!(json["scope"], "SESSION_SCOPED");
        assert_eq!(json["payload"]["eventScope"], "SESSION_SCOPED");
    }

    #[test]
    fn publisher_lifecycle_events_use_rig_scope() {
        let event = RaceEvent::PublisherHello {
            lap: 1,
            session_time: 0.0,
            version: "0.1.0".to_owned(),
            scope: "driver".to_owned(),
        };
        let env = build_event(&event, &minimal_frame(), None, "s", "r", None, None);
        let json: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(env.scope, EventScope::RigScoped);
        assert!(env.car.is_none());
        assert_eq!(json["scope"], "RIG_SCOPED");
        assert_eq!(json["payload"]["eventScope"], "RIG_SCOPED");
    }

    #[test]
    fn battle_payload_includes_leader_and_follower_car_numbers() {
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

        let env = build_event(&event, &minimal_frame(), None, "s", "r", None, None);
        let json: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(json["payload"]["leaderCarNumber"], "1");
        assert_eq!(json["payload"]["followerCarNumber"], "0");
        assert_eq!(json["payload"]["leaderRacePosition"], 4);
        assert_eq!(json["payload"]["followerRacePosition"], 5);
        assert_eq!(json["payload"]["playerRacePosition"], 5);
        assert_eq!(json["payload"]["opponentRacePosition"], 4);
    }

    #[test]
    fn battle_closing_and_broken_payloads_include_player_and_opponent_positions() {
        let closing = RaceEvent::BattleClosing {
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
        let broken = RaceEvent::BattleBroken {
            lap: 5,
            session_time: 1300.0,
            player_car_idx: 0,
            opponent_car_idx: 1,
            final_gap_sec: Some(2.1),
            car_race_position: 3,
            engagement_started_at_session_time_s: 1234.5,
            battle: None,
        };

        for event in [closing, broken] {
            let env = build_event(&event, &minimal_frame(), None, "s", "r", None, None);
            let json: Value = serde_json::to_value(&env).unwrap();
            assert_eq!(json["payload"]["playerRacePosition"], 5);
            assert_eq!(json["payload"]["opponentRacePosition"], 4);
            assert_eq!(json["payload"]["leaderRacePosition"], 4);
            assert_eq!(json["payload"]["followerRacePosition"], 5);
        }
    }

    #[test]
    fn battle_positions_null_when_unresolvable() {
        let event = RaceEvent::BattleEngaged {
            lap: 2,
            session_time: 12.0,
            player_car_idx: 0,
            opponent_car_idx: 9, // out of range of car_idx_position
            gap_s: 0.4,
            car_race_position: 4,
            prior_skirmishes: 0,
            prior_attack_time_s: 0.0,
            engagement_started_at_session_time_s: 12.0,
            battle: None,
        };

        let env = build_event(&event, &minimal_frame(), None, "s", "r", None, None);
        let json: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(json["payload"]["playerRacePosition"], 5);
        assert!(json["payload"]["opponentRacePosition"].is_null());
    }

    #[test]
    fn vulnerability_alert_payload_includes_attacker_and_defender_positions() {
        let event = RaceEvent::VulnerabilityAlert {
            lap: 4,
            session_time: 1234.5,
            vulnerability: 0.8,
            defender_idx: 1,
            attacker_idx: 0,
            tire_contribution: 0.3,
            closing_contribution: 0.2,
            proximity_contribution: 0.2,
            fuel_contribution: 0.1,
        };

        let env = build_event(&event, &minimal_frame(), None, "s", "r", None, None);
        let json: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(json["payload"]["attackerPosition"], 5);
        assert_eq!(json["payload"]["defenderPosition"], 4);
    }

    #[test]
    fn heartbeat_event_uses_rig_scope_and_carries_counters() {
        let event = RaceEvent::PublisherHeartbeat {
            lap: 4,
            session_time: 1234.5,
            version: "0.1.0".to_owned(),
            events_enqueued_total: 17,
        };

        let env = build_event(&event, &minimal_frame(), None, "s", "r", None, None);
        let json: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(env.scope, EventScope::RigScoped);
        assert!(env.car.is_none());
        assert_eq!(json["type"], "PUBLISHER_HEARTBEAT");
        assert_eq!(json["scope"], "RIG_SCOPED");
        assert_eq!(json["payload"]["version"], "0.1.0");
        assert_eq!(json["payload"]["events_enqueued_total"], 17);
    }

    #[test]
    fn lap_completed_payload_includes_camel_case_aliases() {
        let event = RaceEvent::LapCompleted {
            lap: 2,
            session_time: 99.0,
            player_car_idx: 0,
            lap_time_s: Some(88.2),
            best_lap_time_s: Some(87.9),
            position: 5,
            pit_frames: 0,
        };

        let env = build_event(&event, &minimal_frame(), None, "s", "r", None, None);
        let json: Value = serde_json::to_value(&env).unwrap();
        let lap_time = json["payload"]["lapTime"].as_f64().unwrap_or_default();
        let best_lap = json["payload"]["bestLapTime"].as_f64().unwrap_or_default();
        let lap_time_snake = json["payload"]["lap_time_s"].as_f64().unwrap_or_default();
        let best_lap_snake = json["payload"]["best_lap_time_s"]
            .as_f64()
            .unwrap_or_default();
        assert!((lap_time - 88.2).abs() < 1e-3);
        assert!((best_lap - 87.9).abs() < 1e-3);
        assert!((lap_time_snake - 88.2).abs() < 1e-3);
        assert!((best_lap_snake - 87.9).abs() < 1e-3);
    }

    #[test]
    fn car_refs_include_enrichment_fields_when_roster_present() {
        let event = RaceEvent::Overtake {
            lap: 3,
            session_time: 42.0,
            car_idx: 0,
            overtaken_car_idx: Some(1),
            position_from: 6,
            position_to: 5,
            positions_gained: 1,
        };
        let frame = minimal_frame();
        let roster = enriched_roster();

        let env = build_event(&event, &frame, Some(&roster), "s", "r", None, None);
        let json: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(json["payload"]["overtakingCar"]["irating"], 2680);
        assert_eq!(json["payload"]["overtakingCar"]["licString"], "D 2.79");
        assert_eq!(
            json["payload"]["overtakingCar"]["flairName"],
            "United States"
        );
    }

    #[test]
    fn context_includes_session_metadata_when_provided() {
        let event = RaceEvent::RaceGreen {
            lap: 1,
            session_time: 1.0,
            synthetic: false,
            origin: LifecycleOrigin::SessionStateTransition,
        };
        let frame = minimal_frame();
        let meta = SessionMetadata {
            track_name: Some("Winton National".to_owned()),
            session_type: Some("Practice".to_owned()),
            session_laps: Some("unlimited".to_owned()),
            track_length_m: Some(3000.0),
        };

        let env = build_event(&event, &frame, None, "s", "r", Some(&meta), Some(86268796));
        let json: Value = serde_json::to_value(&env).unwrap();
        assert_eq!(json["context"]["subSessionId"], 86268796);
        assert_eq!(json["context"]["sessionType"], "Practice");
        assert_eq!(json["context"]["trackName"], "Winton National");
    }
}
