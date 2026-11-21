// SPDX-License-Identifier: GPL-3.0-or-later

//! Live probe: a diagnostic for questions about iRacing that cannot be
//! answered from code.
//!
//! Off unless `SRC_PUBLISHER_PROBE=1`. When on it only writes log lines; it
//! publishes nothing and changes no event. It answers, from one real session:
//!
//! * which candidate telemetry variables the SDK header has, and their shape;
//! * what `CarIdxSessionFlags`, `CarIdxTrackSurfaceMaterial` and
//!   `CarIdxPosition` read for a car at a hard event and 5 s and 20 s later;
//! * whether the session results list incident counts and retirement reasons
//!   for other cars while the session is running.
//!
//! Log lines carry car indices and counts only, never driver names.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use crate::race_event::RaceEvent;
use crate::telemetry_frame::TelemetryFrame;

/// Environment variable that turns the probe on when set to `1`.
pub const ENV_VAR: &str = "SRC_PUBLISHER_PROBE";

const VAR_SESSION_FLAGS: &str = "CarIdxSessionFlags";
const VAR_SURFACE_MATERIAL: &str = "CarIdxTrackSurfaceMaterial";

/// Variables the probe reports on. None of them is read by the engine.
pub const CANDIDATE_VARS: &[&str] = &[
    VAR_SESSION_FLAGS,
    VAR_SURFACE_MATERIAL,
    "LatAccel",
    "LongAccel",
    "VertAccel",
];

/// When a car is sampled, in seconds after its hard event.
const SAMPLE_OFFSETS_S: [f32; 3] = [0.0, 5.0, 20.0];

// irsdk variable type codes.
const IR_INT: i32 = 2;
const IR_BITFIELD: i32 = 3;

fn is_on(value: Option<&str>) -> bool {
    value == Some("1")
}

/// Whether the probe is switched on. Read once: the answer cannot change
/// while the process runs, and the frame loop must not pay for asking.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| is_on(std::env::var(ENV_VAR).ok().as_deref()))
}

/// One entry of the SDK's variable header, as far as the probe cares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VarHeader {
    pub type_code: i32,
    /// Byte offset inside one telemetry buffer.
    pub offset: usize,
    pub count: usize,
}

fn type_name(type_code: i32) -> &'static str {
    match type_code {
        0 => "char",
        1 => "bool",
        IR_INT => "int",
        IR_BITFIELD => "bitField",
        4 => "float",
        5 => "double",
        _ => "unknown",
    }
}

/// One log line per candidate variable: present or not, type and count.
pub fn var_header_lines(lookup: impl Fn(&str) -> Option<VarHeader>) -> Vec<String> {
    CANDIDATE_VARS
        .iter()
        .map(|name| match lookup(name) {
            Some(var) => format!(
                "[probe] var {name}: present, type {} ({}), count {}",
                type_name(var.type_code),
                var.type_code,
                var.count,
            ),
            None => format!("[probe] var {name}: absent from the SDK header"),
        })
        .collect()
}

/// Read an int or bitfield array, or `None` when the variable is of another
/// type or does not fit the buffer. Never panics: the probe must not be able
/// to take the publisher down.
pub fn read_i32_array(buf: &[u8], var: VarHeader) -> Option<Vec<i32>> {
    if var.type_code != IR_INT && var.type_code != IR_BITFIELD {
        return None;
    }
    let count = var.count.min(64);
    let end = var.offset.checked_add(count.checked_mul(4)?)?;
    let bytes = buf.get(var.offset..end)?;
    let (chunks, _) = bytes.as_chunks::<4>();
    Some(
        chunks
            .iter()
            .map(|chunk| i32::from_le_bytes(*chunk))
            .collect(),
    )
}

/// The per-car candidate arrays at one moment. `None` = the variable is
/// absent (or unreadable).
#[derive(Clone, Debug, Default)]
pub struct CarArrays {
    pub session_flags: Option<Vec<i32>>,
    pub surface_material: Option<Vec<i32>>,
}

impl CarArrays {
    /// Read both arrays from one telemetry buffer.
    pub fn read(buf: &[u8], lookup: impl Fn(&str) -> Option<VarHeader>) -> Self {
        let array = |name: &str| lookup(name).and_then(|var| read_i32_array(buf, var));
        Self {
            session_flags: array(VAR_SESSION_FLAGS),
            surface_material: array(VAR_SURFACE_MATERIAL),
        }
    }
}

struct PendingSample {
    car_idx: u8,
    onset: f32,
    offset_s: f32,
    reason: String,
}

impl PendingSample {
    fn due(&self) -> f32 {
        self.onset + self.offset_s
    }
}

pub struct Probe {
    pending: Vec<PendingSample>,
    last_session_time: Option<f32>,
    /// The results summary last logged, so an unchanged one is not repeated.
    last_results: Vec<String>,
}

impl Probe {
    /// The probe, or `None` when it is switched off — the caller then skips
    /// every probe call with one branch.
    pub fn from_env() -> Option<Self> {
        enabled().then(Self::new)
    }

    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            last_session_time: None,
            last_results: Vec::new(),
        }
    }

    /// Schedule samples for this frame's hard events and return a log line
    /// for every sample now due. `read` is called at most once, and only when
    /// a sample is due.
    pub fn on_frame(
        &mut self,
        frame: &TelemetryFrame,
        events: &[RaceEvent],
        read: impl FnOnce() -> CarArrays,
    ) -> Vec<String> {
        let now = frame.session_time;
        // A clock restart leaves pending samples due at times that will not
        // come, or will come in another session.
        if self.last_session_time.is_some_and(|prev| now < prev) {
            self.pending.clear();
        }
        self.last_session_time = Some(now);

        // Every hard event is an `INCIDENT_ALERT` on the same frame.
        for event in events {
            if let RaceEvent::IncidentAlert {
                car_idx,
                session_time,
                reason,
                ..
            } = event
            {
                self.pending
                    .extend(SAMPLE_OFFSETS_S.iter().map(|offset_s| PendingSample {
                        car_idx: *car_idx,
                        onset: *session_time,
                        offset_s: *offset_s,
                        reason: reason.clone(),
                    }));
            }
        }

        if !self.pending.iter().any(|sample| sample.due() <= now) {
            return Vec::new();
        }
        let arrays = read();
        let (due, waiting): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
            .into_iter()
            .partition(|sample| sample.due() <= now);
        self.pending = waiting;
        due.iter()
            .map(|sample| sample_line(sample, frame, &arrays))
            .collect()
    }

    /// Summarise `ResultsPositions` from the session YAML. Returns log lines
    /// when the summary differs from the one last logged, none otherwise.
    pub fn on_session_info(&mut self, yaml: &str) -> Vec<String> {
        let lines = results_lines(yaml);
        if lines == self.last_results {
            return Vec::new();
        }
        self.last_results = lines.clone();
        lines
    }
}

impl Default for Probe {
    fn default() -> Self {
        Self::new()
    }
}

fn sample_line(sample: &PendingSample, frame: &TelemetryFrame, arrays: &CarArrays) -> String {
    let slot = sample.car_idx as usize;
    let value = |array: &Option<Vec<i32>>, hex: bool| match array {
        None => "absent".to_owned(),
        Some(values) => match values.get(slot) {
            Some(v) if hex => format!("{:#010x}", *v as u32),
            Some(v) => v.to_string(),
            None => "no slot".to_owned(),
        },
    };
    let position = frame
        .car_idx_position
        .get(slot)
        .map_or_else(|| "no slot".to_owned(), u8::to_string);
    format!(
        "[probe] hard event car {} ({}) onset {:.2}s, +{:.0}s at {:.2}s: \
         {VAR_SESSION_FLAGS}={} {VAR_SURFACE_MATERIAL}={} CarIdxPosition={position}",
        sample.car_idx,
        sample.reason,
        sample.onset,
        sample.offset_s,
        frame.session_time,
        value(&arrays.session_flags, true),
        value(&arrays.surface_material, false),
    )
}

#[derive(Default)]
struct SessionResults {
    cars: usize,
    with_incidents: usize,
    incident_total: i64,
    reasons_out: BTreeMap<String, usize>,
}

/// One line per session in the YAML that lists results.
///
/// A line scan rather than a YAML parse: iRacing's blob is not always valid
/// YAML, and the probe needs two keys that appear nowhere else in it.
fn results_lines(yaml: &str) -> Vec<String> {
    let mut sessions: BTreeMap<i64, SessionResults> = BTreeMap::new();
    let mut session_num = 0_i64;
    let mut in_results = false;
    for line in yaml.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("- SessionNum:") {
            session_num = value.trim().parse().unwrap_or(session_num);
            in_results = false;
        } else if line.starts_with("ResultsPositions:") {
            in_results = true;
        } else if line.starts_with("ResultsFastestLap:") {
            in_results = false;
        } else if in_results {
            // Entries start `- Position:`; keys after the first are bare.
            let line = line.trim_start_matches("- ");
            let session = sessions.entry(session_num).or_default();
            if let Some(value) = line.strip_prefix("Incidents:") {
                let incidents: i64 = value.trim().parse().unwrap_or(0);
                session.cars += 1;
                session.incident_total += incidents;
                if incidents != 0 {
                    session.with_incidents += 1;
                }
            } else if let Some(value) = line.strip_prefix("ReasonOutStr:") {
                *session
                    .reasons_out
                    .entry(value.trim().to_owned())
                    .or_default() += 1;
            }
        }
    }
    sessions
        .iter()
        .filter(|(_, session)| session.cars > 0)
        .map(|(num, session)| {
            let reasons: Vec<String> = session
                .reasons_out
                .iter()
                .map(|(reason, count)| format!("\"{reason}\" x{count}"))
                .collect();
            format!(
                "[probe] results session {num}: {} cars listed, {} with non-zero Incidents \
                 (total {}), ReasonOut: {}",
                session.cars,
                session.with_incidents,
                session.incident_total,
                if reasons.is_empty() {
                    "none".to_owned()
                } else {
                    reasons.join(", ")
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn frame(t: f32, positions: Vec<u8>) -> TelemetryFrame {
        serde_json::from_value(serde_json::json!({
            "lap": 1,
            "session_time": t,
            "lap_dist_pct": 0.1,
            "player_car_idx": 0,
            "player_car_position": 1,
            "on_pit_road": false,
            "session_flags": 0,
            "car_idx_lap_dist_pct": [],
            "car_idx_position": positions,
            "car_idx_on_pit_road": [],
        }))
        .unwrap()
    }

    fn alert(car_idx: u8, t: f32) -> RaceEvent {
        RaceEvent::IncidentAlert {
            lap: 1,
            session_time: t,
            car_idx,
            driver_incident_count: None,
            previous_track_surface: 3,
            current_track_surface: 3,
            previous_speed_mps: 50.0,
            current_speed_mps: 10.0,
            speed_drop_mps: 40.0,
            severity: 0.8,
            severity_normalized: 0.8,
            incident_count_delta: None,
            reason: "speed_drop".to_owned(),
            black_flag: None,
        }
    }

    fn arrays(flags: i32, material: i32) -> CarArrays {
        CarArrays {
            session_flags: Some(vec![0, 0, flags]),
            surface_material: Some(vec![0, 0, material]),
        }
    }

    #[test]
    fn only_the_value_one_switches_the_probe_on() {
        assert!(is_on(Some("1")));
        for off in [None, Some(""), Some("0"), Some("true"), Some("yes")] {
            assert!(!is_on(off), "{off:?} must leave the probe off");
        }
    }

    #[test]
    fn var_header_lines_report_presence_type_and_count() {
        let lines = var_header_lines(|name| match name {
            VAR_SESSION_FLAGS => Some(VarHeader {
                type_code: IR_BITFIELD,
                offset: 100,
                count: 64,
            }),
            "LatAccel" => Some(VarHeader {
                type_code: 4,
                offset: 8,
                count: 1,
            }),
            _ => None,
        });
        assert_eq!(lines.len(), CANDIDATE_VARS.len());
        assert_eq!(
            lines[0],
            "[probe] var CarIdxSessionFlags: present, type bitField (3), count 64"
        );
        assert_eq!(
            lines[1],
            "[probe] var CarIdxTrackSurfaceMaterial: absent from the SDK header"
        );
        assert_eq!(
            lines[2],
            "[probe] var LatAccel: present, type float (4), count 1"
        );
    }

    #[test]
    fn arrays_are_read_only_when_the_type_and_bounds_allow() {
        let mut buf = vec![0_u8; 16];
        buf[4..8].copy_from_slice(&7_i32.to_le_bytes());
        buf[8..12].copy_from_slice(&(-1_i32).to_le_bytes());
        let var = |type_code, offset, count| VarHeader {
            type_code,
            offset,
            count,
        };
        assert_eq!(read_i32_array(&buf, var(IR_INT, 4, 2)), Some(vec![7, -1]));
        assert_eq!(read_i32_array(&buf, var(IR_BITFIELD, 4, 1)), Some(vec![7]));
        // A float array is not what the probe expects: say absent, do not
        // reinterpret the bytes.
        assert_eq!(read_i32_array(&buf, var(4, 4, 2)), None);
        // Past the end of the buffer, and an offset that would overflow.
        assert_eq!(read_i32_array(&buf, var(IR_INT, 8, 3)), None);
        assert_eq!(read_i32_array(&buf, var(IR_INT, usize::MAX, 1)), None);
    }

    #[test]
    fn car_arrays_are_absent_when_the_header_has_no_such_variable() {
        let mut buf = vec![0_u8; 12];
        buf[8..12].copy_from_slice(&5_i32.to_le_bytes());
        let read = CarArrays::read(&buf, |name| {
            (name == VAR_SURFACE_MATERIAL).then_some(VarHeader {
                type_code: IR_INT,
                offset: 0,
                count: 3,
            })
        });
        assert_eq!(read.session_flags, None);
        assert_eq!(read.surface_material, Some(vec![0, 0, 5]));
    }

    #[test]
    fn a_hard_event_is_sampled_at_the_event_and_five_and_twenty_seconds_later() {
        let mut probe = Probe::new();
        let reads = Cell::new(0);
        let mut step = |t: f32, events: &[RaceEvent], flags: i32, position: u8| {
            probe.on_frame(&frame(t, vec![1, 2, position]), events, || {
                reads.set(reads.get() + 1);
                arrays(flags, 4)
            })
        };

        assert!(step(99.0, &[], 0, 3).is_empty());
        assert_eq!(reads.get(), 0, "nothing due: the buffer is not read");

        let at_event = step(100.0, &[alert(2, 100.0)], 0x0010_0000, 3);
        assert_eq!(
            at_event,
            vec![
                "[probe] hard event car 2 (speed_drop) onset 100.00s, +0s at 100.00s: \
                 CarIdxSessionFlags=0x00100000 CarIdxTrackSurfaceMaterial=4 CarIdxPosition=3"
            ]
        );
        assert!(step(104.9, &[], 0, 3).is_empty());
        assert_eq!(reads.get(), 1);

        let later = step(105.0, &[], 0x0014_0000, 3);
        assert_eq!(later.len(), 1);
        assert!(later[0].contains("+5s at 105.00s"));
        assert!(later[0].contains("CarIdxSessionFlags=0x00140000"));

        assert!(step(119.9, &[], 0, 9).is_empty());
        let last = step(120.02, &[], 0, 9);
        assert_eq!(last.len(), 1);
        assert!(last[0].contains("+20s at 120.02s"));
        assert!(last[0].contains("CarIdxPosition=9"));
        assert_eq!(reads.get(), 3);

        assert!(step(200.0, &[], 0, 9).is_empty(), "three samples, no more");
        assert_eq!(reads.get(), 3);
    }

    #[test]
    fn absent_variables_and_missing_slots_are_said_so() {
        let mut probe = Probe::new();
        let lines = probe.on_frame(&frame(10.0, vec![1]), &[alert(5, 10.0)], || CarArrays {
            session_flags: None,
            surface_material: Some(vec![0]),
        });
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("CarIdxSessionFlags=absent"));
        assert!(lines[0].contains("CarIdxTrackSurfaceMaterial=no slot"));
        assert!(lines[0].contains("CarIdxPosition=no slot"));
    }

    #[test]
    fn a_clock_restart_drops_pending_samples() {
        let mut probe = Probe::new();
        probe.on_frame(&frame(100.0, vec![1, 2, 3]), &[alert(2, 100.0)], || {
            arrays(0, 0)
        });
        // New session: the +5 s and +20 s samples belong to the old clock.
        for t in [1.0, 105.0, 121.0] {
            let lines = probe.on_frame(&frame(t, vec![1, 2, 3]), &[], || {
                panic!("nothing is due after a restart")
            });
            assert!(lines.is_empty());
        }
    }

    const RESULTS_YAML: &str = "\
SessionInfo:
 Sessions:
 - SessionNum: 0
   SessionType: Practice
   ResultsPositions:
   ResultsFastestLap:
   - CarIdx: 255
 - SessionNum: 1
   SessionType: Race
   ResultsPositions:
   - Position: 1
     CarIdx: 3
     Incidents: 0
     ReasonOutId: 0
     ReasonOutStr: Running
   - Position: 2
     CarIdx: 7
     Incidents: 4
     ReasonOutId: 0
     ReasonOutStr: Running
   - Position: 3
     CarIdx: 9
     Incidents: 12
     ReasonOutId: 1
     ReasonOutStr: Disconnected
   ResultsFastestLap:
   - CarIdx: 3
     FastestLap: 4
DriverInfo:
 Drivers:
 - CarIdx: 3
   UserName: Somebody Private
   CurDriverIncidentCount: 9
";

    #[test]
    fn results_are_summarised_as_counts_without_names() {
        let mut probe = Probe::new();
        let lines = probe.on_session_info(RESULTS_YAML);
        assert_eq!(
            lines,
            vec![
                "[probe] results session 1: 3 cars listed, 2 with non-zero Incidents \
                 (total 16), ReasonOut: \"Disconnected\" x1, \"Running\" x2"
            ]
        );
        assert!(!lines[0].contains("Somebody"));
    }

    #[test]
    fn results_are_logged_again_only_when_the_summary_changes() {
        let mut probe = Probe::new();
        assert_eq!(probe.on_session_info(RESULTS_YAML).len(), 1);
        assert!(probe.on_session_info(RESULTS_YAML).is_empty());
        let changed = RESULTS_YAML.replace("Incidents: 0", "Incidents: 1");
        let lines = probe.on_session_info(&changed);
        assert_eq!(lines.len(), 1);
        assert!(lines[0].contains("3 with non-zero Incidents (total 17)"));
    }

    #[test]
    fn a_session_with_no_results_yet_logs_nothing() {
        let mut probe = Probe::new();
        assert!(probe
            .on_session_info("SessionInfo:\n Sessions:\n")
            .is_empty());
        assert!(probe.on_session_info("not yaml at all: [").is_empty());
    }
}
