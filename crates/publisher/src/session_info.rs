// SPDX-License-Identifier: GPL-3.0-or-later

//! iRacing SessionInfo YAML parser and driver roster.
//!
//! iRacing exposes a `SessionInfo` string variable containing a YAML blob with
//! the full entry list: car number, driver name, team name, car class, etc.
//! A monotonically-increasing `SessionInfoUpdate` header counter signals when
//! the blob has changed (e.g. a driver disconnects or reconnects).
//!
//! # Usage
//!
//! ```no_run
//! use publisher::session_info::{SessionInfoParser, RosterCache};
//!
//! let mut cache = RosterCache::new();
//!
//! // Called each frame with the current update tick and a closure that
//! // provides the raw YAML string only when the cache is stale.
//! let session_info_update: u32 = 3; // from TelemetryFrame (header counter)
//! if cache.needs_update(session_info_update) {
//!     let yaml = "<yaml from mmap>";
//!     cache.update(session_info_update, yaml).ok();
//! }
//!
//! if let Some(car) = cache.roster().and_then(|r| r.lookup(42)) {
//!     println!("{} — {}", car.car_number, car.driver_name);
//! }
//! ```

use std::collections::HashMap;

use serde::Serialize;

// ── Public types ─────────────────────────────────────────────────────────────

/// Resolved identity for a single car slot in the iRacing entry list.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CarRef {
    pub car_idx: u8,
    /// iRacing car number string (e.g. `"42"`, `"042"`, `"P1"`).
    pub car_number: String,
    /// Driver display name as returned by iRacing (`UserName`).
    pub driver_name: String,
    pub team_name: Option<String>,
    pub car_class_short_name: Option<String>,
    pub car_class_id: Option<u32>,
    /// iRacing CustID — used by Race Control for identity resolution.
    pub user_id: Option<u32>,
    /// iRacing iRating from SessionInfo.DriverInfo.Drivers[].
    pub irating: Option<u32>,
    /// Driver license string (e.g. "D 2.79").
    pub lic_string: Option<String>,
    /// Country/region flair label from iRacing (e.g. "United States").
    pub flair_name: Option<String>,
}

impl CarRef {
    /// Stable driver identity used by the sandbox to match a driver across
    /// sessions, where `carIdx` is not stable.
    ///
    /// Prefers the iRacing CustID and falls back to the case-folded display
    /// name, mirroring the sandbox's `driver_id_from_roster_entry`.
    pub fn driver_id(&self) -> String {
        match self.user_id {
            Some(id) => format!("user:{id}"),
            None => format!("name:{}", self.driver_name.trim().to_lowercase()),
        }
    }
}

/// Immutable roster built from one parse of the `SessionInfo` YAML.
pub struct SessionRoster {
    cars: HashMap<u8, CarRef>,
}

impl SessionRoster {
    pub fn empty() -> Self {
        Self {
            cars: HashMap::new(),
        }
    }

    pub fn from_cars<I>(cars: I) -> Self
    where
        I: IntoIterator<Item = CarRef>,
    {
        Self {
            cars: cars.into_iter().map(|car| (car.car_idx, car)).collect(),
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &CarRef> {
        self.cars.values()
    }

    /// Look up a car by its `carIdx` slot number.
    pub fn lookup(&self, car_idx: u8) -> Option<&CarRef> {
        self.cars.get(&car_idx)
    }

    /// Number of car slots in the roster (includes inactive pacecar slots).
    pub fn len(&self) -> usize {
        self.cars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cars.is_empty()
    }
}

/// Stateless YAML parser. Call [`SessionInfoParser::build`] whenever the
/// `SessionInfoUpdate` counter changes.
pub struct SessionInfoParser;

impl Default for SessionRoster {
    fn default() -> Self {
        Self::empty()
    }
}

impl SessionInfoParser {
    /// Parse a raw `SessionInfo` YAML string into a [`SessionRoster`].
    ///
    /// Returns an error if the YAML is malformed or the required
    /// `DriverInfo.Drivers` key is missing.
    pub fn build(yaml_str: &str) -> Result<SessionRoster, serde_yaml::Error> {
        let root: YamlRoot = serde_yaml::from_str(yaml_str)?;
        let cars = root
            .driver_info
            .drivers
            .into_iter()
            .map(|d| {
                let car_ref = CarRef {
                    car_idx: d.car_idx,
                    car_number: d.car_number,
                    driver_name: d.user_name,
                    team_name: non_empty(d.team_name),
                    car_class_short_name: non_empty(d.car_class_short_name),
                    car_class_id: d.car_class_id,
                    user_id: d.user_id,
                    irating: d.irating,
                    lic_string: non_empty(d.lic_string),
                    flair_name: non_empty(d.flair_name),
                };
                (d.car_idx, car_ref)
            })
            .collect();
        Ok(SessionRoster { cars })
    }
}

// ── RosterCache ───────────────────────────────────────────────────────────────

/// Wraps a [`SessionRoster`] and tracks the last-parsed `SessionInfoUpdate`
/// tick so callers can detect when a re-parse is needed.
///
/// The cache starts empty (no roster). Call [`needs_update`] each frame;
/// when it returns `true`, read the YAML string from the mmap and call
/// [`update`].
///
/// [`needs_update`]: RosterCache::needs_update
/// [`update`]: RosterCache::update
pub struct RosterCache {
    last_tick: Option<u32>,
    roster: Option<SessionRoster>,
}

impl RosterCache {
    pub fn new() -> Self {
        Self {
            last_tick: None,
            roster: None,
        }
    }

    /// Returns `true` if `current_tick` differs from the last tick this cache
    /// was updated at, or if the cache has never been populated.
    pub fn needs_update(&self, current_tick: u32) -> bool {
        self.last_tick != Some(current_tick)
    }

    /// Parse `yaml_str` and store the resulting roster tagged with `tick`.
    ///
    /// Replaces the previous roster on success. On parse failure the previous
    /// roster (if any) is retained and the tick is **not** advanced, so the
    /// next frame will retry.
    pub fn update(&mut self, tick: u32, yaml_str: &str) -> Result<(), serde_yaml::Error> {
        let roster = SessionInfoParser::build(yaml_str)?;
        self.roster = Some(roster);
        self.last_tick = Some(tick);
        Ok(())
    }

    /// Returns the current roster, or `None` if the cache has never been
    /// successfully populated.
    pub fn roster(&self) -> Option<&SessionRoster> {
        self.roster.as_ref()
    }
}

impl Default for RosterCache {
    fn default() -> Self {
        Self::new()
    }
}

// ── Public utilities ─────────────────────────────────────────────────────────

/// Track and session metadata parsed from the `SessionInfo` YAML.
#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    /// e.g. "Tsukuba Circuit 2k Full"
    pub track_name: Option<String>,
    /// e.g. "Practice", "Race", "Qualify"
    pub session_type: Option<String>,
    /// "unlimited" or a lap count string
    pub session_laps: Option<String>,
    /// Track length in metres, from `WeekendInfo.TrackLength` (e.g. "3.70 km").
    pub track_length_m: Option<f32>,
}

impl SessionMetadata {
    /// Parse track name and the active session's type/laps from the raw YAML.
    /// `session_num` is `TelemetryFrame::session_num` (practice=0, qualify=1, race=2).
    pub fn parse(yaml: &str, session_num: i32) -> Self {
        let root: YamlRootFull = match serde_yaml::from_str(yaml) {
            Ok(r) => r,
            Err(_) => return parse_metadata_fallback(yaml),
        };

        let track_name = root
            .weekend_info
            .track_display_name
            .or(root.weekend_info.track_name)
            .filter(|s| !s.is_empty());

        let track_length_m = root
            .weekend_info
            .track_length
            .as_deref()
            .and_then(parse_track_length_meters);

        let sessions = &root.session_info.sessions;
        let session = if sessions.is_empty() {
            None
        } else {
            let idx = usize::try_from(session_num)
                .ok()
                .filter(|&i| i < sessions.len())
                .unwrap_or(0);
            sessions.get(idx)
        };

        let session_type = session
            .and_then(|s| s.session_type.clone())
            .filter(|s| !s.is_empty());
        let session_laps = session
            .and_then(|s| s.session_laps())
            .filter(|s| !s.is_empty());

        let mut meta = Self {
            track_name,
            session_type,
            session_laps,
            track_length_m,
        };

        if meta.track_name.is_none()
            || meta.session_type.is_none()
            || meta.session_laps.is_none()
            || meta.track_length_m.is_none()
        {
            let fallback = parse_metadata_fallback(yaml);
            if meta.track_name.is_none() {
                meta.track_name = fallback.track_name;
            }
            if meta.session_type.is_none() {
                meta.session_type = fallback.session_type;
            }
            if meta.session_laps.is_none() {
                meta.session_laps = fallback.session_laps;
            }
            if meta.track_length_m.is_none() {
                meta.track_length_m = fallback.track_length_m;
            }
        }

        meta
    }
}

/// Parse an iRacing `TrackLength` string (e.g. "3.70 km", "0.90 mi") into metres.
pub fn parse_track_length_meters(raw: &str) -> Option<f32> {
    let raw = raw.trim().trim_matches('"');
    let numeric: String = raw
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
        .collect();
    let value: f32 = numeric.parse().ok()?;
    let unit = raw[numeric.len()..].trim().to_ascii_lowercase();
    let meters = if unit.starts_with("mi") {
        value * 1609.344
    } else if unit.starts_with('m') && !unit.starts_with("mi") {
        value
    } else {
        // iRacing reports km; treat missing/unknown units as km.
        value * 1000.0
    };
    (meters > 0.0).then_some(meters)
}

fn parse_metadata_fallback(yaml: &str) -> SessionMetadata {
    let mut track_name: Option<String> = None;
    let mut session_type: Option<String> = None;
    let mut session_laps: Option<String> = None;
    let mut track_length_m: Option<f32> = None;

    for line in yaml.lines() {
        let line = line.trim();
        let line_no_dash = line.trim_start_matches('-').trim_start();

        if track_name.is_none() {
            if let Some(rest) = line.strip_prefix("TrackDisplayName:") {
                let v = rest.trim().trim_matches('"');
                if !v.is_empty() {
                    track_name = Some(v.to_string());
                }
            } else if let Some(rest) = line.strip_prefix("TrackName:") {
                let v = rest.trim().trim_matches('"');
                if !v.is_empty() {
                    track_name = Some(v.to_string());
                }
            }
        }

        if session_type.is_none() {
            if let Some(rest) = line_no_dash.strip_prefix("SessionType:") {
                let v = rest.trim().trim_matches('"');
                if !v.is_empty() {
                    session_type = Some(v.to_string());
                }
            }
        }

        if session_laps.is_none() {
            if let Some(rest) = line.strip_prefix("SessionLaps:") {
                let v = rest.trim().trim_matches('"');
                if !v.is_empty() {
                    session_laps = Some(v.to_string());
                }
            }
        }

        if track_length_m.is_none() {
            if let Some(rest) = line.strip_prefix("TrackLength:") {
                track_length_m = parse_track_length_meters(rest);
            }
        }

        if track_name.is_some()
            && session_type.is_some()
            && session_laps.is_some()
            && track_length_m.is_some()
        {
            break;
        }
    }

    SessionMetadata {
        track_name,
        session_type,
        session_laps,
        track_length_m,
    }
}

/// Extract `WeekendInfo.SubSessionID` from the raw `SessionInfo` YAML string.
///
/// Uses a fast line scan rather than a full YAML parse so it can be called
/// on every `SessionInfoUpdate` without the serde_yaml overhead.
/// Returns `None` if the key is absent or not parseable as `i64`.
pub fn parse_sub_session_id(yaml: &str) -> Option<i64> {
    for line in yaml.lines() {
        let line = line.trim().trim_start_matches('-').trim_start();
        if let Some((key, rest)) = line.split_once(':') {
            // Accept common casing variants seen across SDK versions.
            let is_sub_session_key = key.eq_ignore_ascii_case("SubSessionID")
                || key.eq_ignore_ascii_case("SubSessionId");
            if !is_sub_session_key {
                continue;
            }

            // Ignore placeholder zeros and keep scanning. Some SessionInfo blobs
            // can contain multiple SubSessionID keys, with an early value of 0.
            let raw = rest.trim().trim_matches('"').replace(',', "");
            let parsed: Option<i64> = raw.parse().ok();
            if let Some(v) = parsed.filter(|&v| v > 0) {
                return Some(v);
            }
        }
    }
    None
}

/// Returns `true` when the YAML indicates this is an iRacing AI session.
///
/// AI sessions never receive a real `SubSessionID` from the iRacing server.
/// The presence of `AIRosterName:` is the most reliable indicator — it is
/// only set when the session was started with an AI driver roster.
pub fn is_ai_session(yaml: &str) -> bool {
    yaml.lines().any(|ln| {
        let t = ln.trim();
        if let Some(rest) = t.strip_prefix("AIRosterName:") {
            let val = rest.trim().trim_matches('"');
            !val.is_empty()
        } else {
            false
        }
    })
}

/// Generate a stable synthetic `SubSessionID` for an AI session.
///
/// AI sessions never get a real SubSessionID from iRacing servers. We derive
/// a reproducible negative ID from the track name and current UTC day so it
/// changes each calendar day (preventing cross-day event collisions) but
/// stays constant for the lifetime of a single session.
///
/// The ID is always negative to make it distinguishable from real iRacing
/// SubSessionIDs (which are large positive integers).
pub fn synthetic_sub_session_id(yaml: &str) -> i64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    // Use track name as the session-stable component.
    let track = yaml
        .lines()
        .find_map(|ln| {
            ln.trim()
                .strip_prefix("TrackName:")
                .map(|s| s.trim().trim_matches('"').to_string())
        })
        .unwrap_or_default();

    // Day number since epoch — changes at UTC midnight.
    let day = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / 86400;

    let mut h = DefaultHasher::new();
    track.hash(&mut h);
    day.hash(&mut h);
    let hash = h.finish();

    // Map to a positive i64, staying well away from i64::MAX.
    (hash & 0x7FFF_FFFF_FFFF) as i64 + 1
}

// ── Private serde types ───────────────────────────────────────────────────────

/// Normalise iRacing's empty-string sentinels to `None`.
fn non_empty(s: Option<String>) -> Option<String> {
    s.filter(|v| !v.trim().is_empty())
}

// ── Roster-only YAML types (existing parser) ──────────────────────────────────

#[derive(serde::Deserialize)]
struct YamlRoot {
    #[serde(rename = "DriverInfo")]
    driver_info: YamlDriverInfo,
}

// ── Full YAML types (metadata parser) ────────────────────────────────────────

#[derive(serde::Deserialize, Default)]
struct YamlRootFull {
    #[serde(rename = "WeekendInfo", default)]
    weekend_info: YamlWeekendInfo,
    #[serde(rename = "SessionInfo", default)]
    session_info: YamlSessionInfo,
}

#[derive(serde::Deserialize, Default)]
struct YamlWeekendInfo {
    #[serde(rename = "TrackDisplayName", default)]
    track_display_name: Option<String>,
    #[serde(rename = "TrackName", default)]
    track_name: Option<String>,
    #[serde(rename = "TrackLength", default)]
    track_length: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct YamlSessionInfo {
    #[serde(rename = "Sessions", default)]
    sessions: Vec<YamlSession>,
}

#[derive(serde::Deserialize)]
struct YamlSession {
    #[serde(rename = "SessionType", default)]
    session_type: Option<String>,
    #[serde(rename = "SessionLaps", default)]
    session_laps_raw: Option<serde_yaml::Value>,
}

impl YamlSession {
    fn session_laps(&self) -> Option<String> {
        self.session_laps_raw.as_ref().map(|v| match v {
            serde_yaml::Value::Number(n) => n.to_string(),
            serde_yaml::Value::String(s) => s.clone(),
            other => format!("{other:?}"),
        })
    }
}

#[derive(serde::Deserialize)]
struct YamlDriverInfo {
    #[serde(rename = "Drivers")]
    drivers: Vec<YamlDriver>,
}

#[derive(serde::Deserialize)]
struct YamlDriver {
    #[serde(rename = "CarIdx")]
    car_idx: u8,
    #[serde(rename = "UserName")]
    user_name: String,
    /// Deserialised as `i64` because iRacing uses `-1` as a sentinel for
    /// the pace car and inactive slots. Negative values are mapped to `None`.
    #[serde(rename = "UserID", default, deserialize_with = "deserialize_nonneg_id")]
    user_id: Option<u32>,
    #[serde(
        rename = "IRating",
        default,
        deserialize_with = "deserialize_nonneg_id"
    )]
    irating: Option<u32>,
    #[serde(rename = "LicString", default)]
    lic_string: Option<String>,
    #[serde(rename = "FlairName", default)]
    flair_name: Option<String>,
    #[serde(rename = "TeamName", default)]
    team_name: Option<String>,
    #[serde(rename = "CarNumber")]
    car_number: String,
    /// Same sentinel treatment as `UserID`.
    #[serde(
        rename = "CarClassID",
        default,
        deserialize_with = "deserialize_nonneg_id"
    )]
    car_class_id: Option<u32>,
    #[serde(rename = "CarClassShortName", default)]
    car_class_short_name: Option<String>,
}

/// Deserialise an integer field that iRacing may set to `-1` (or any negative
/// value) as a sentinel meaning "not present". Negative values become `None`;
/// non-negative values become `Some(v as u32)`.
fn deserialize_nonneg_id<'de, D>(deserializer: D) -> Result<Option<u32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Accept either a JSON/YAML integer or the absence of the field (handled
    // by `#[serde(default)]` which calls `Option::default() = None` before
    // this function is invoked for missing fields).
    let opt: Option<i64> = serde::Deserialize::deserialize(deserializer)?;
    Ok(opt.and_then(|v| if v < 0 { None } else { Some(v as u32) }))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal SessionInfo YAML covering 4 drivers with varying optional fields.
    const FIXTURE_YAML: &str = concat!(
        "DriverInfo:\n",
        " DriverCarIdx: 0\n",
        " Drivers:\n",
        " - CarIdx: 0\n",
        "   UserName: Paul Crofts\n",
        "   UserID: 123456\n",
        "   TeamName: Team Margic\n",
        "   CarNumber: \"42\"\n",
        "   CarClassID: 4011\n",
        "   CarClassShortName: GTP\n",
        " - CarIdx: 1\n",
        "   UserName: Alice Racer\n",
        "   UserID: 234567\n",
        "   TeamName: \"\"\n",
        "   CarNumber: \"7\"\n",
        "   CarClassID: 4011\n",
        "   CarClassShortName: GTP\n",
        " - CarIdx: 2\n",
        "   UserName: Bob Speedman\n",
        "   CarNumber: \"18\"\n",
        "   CarClassID: 2523\n",
        "   CarClassShortName: LMP2\n",
        " - CarIdx: 63\n",
        "   UserName: Pace Car\n",
        "   UserID: 0\n",
        "   CarNumber: \"0\"\n",
        "   CarClassID: 11\n",
        "   CarClassShortName: Pace\n",
    );

    #[test]
    fn parse_returns_all_drivers() {
        let roster = SessionInfoParser::build(FIXTURE_YAML).expect("parse should succeed");
        assert_eq!(roster.len(), 4);
    }

    #[test]
    fn driver_id_prefers_the_iracing_cust_id() {
        let roster = SessionInfoParser::build(FIXTURE_YAML).expect("parse");
        let car = roster.lookup(0).expect("carIdx 0 should exist");
        assert_eq!(car.driver_id(), "user:123456");
    }

    #[test]
    fn driver_id_falls_back_to_the_normalized_name() {
        let car = CarRef {
            car_idx: 3,
            car_number: "7".to_owned(),
            driver_name: "  Ada Lovelace ".to_owned(),
            team_name: None,
            car_class_short_name: None,
            car_class_id: None,
            user_id: None,
            irating: None,
            lic_string: None,
            flair_name: None,
        };
        assert_eq!(car.driver_id(), "name:ada lovelace");
    }

    #[test]
    fn lookup_known_car_returns_correct_ref() {
        let roster = SessionInfoParser::build(FIXTURE_YAML).expect("parse");
        let car = roster.lookup(0).expect("carIdx 0 should exist");
        assert_eq!(car.car_number, "42");
        assert_eq!(car.driver_name, "Paul Crofts");
        assert_eq!(car.team_name, Some("Team Margic".to_string()));
        assert_eq!(car.car_class_short_name, Some("GTP".to_string()));
        assert_eq!(car.car_class_id, Some(4011));
        assert_eq!(car.user_id, Some(123456));
        assert_eq!(car.irating, None);
        assert_eq!(car.lic_string, None);
        assert_eq!(car.flair_name, None);
    }

    #[test]
    fn lookup_unknown_car_returns_none() {
        let roster = SessionInfoParser::build(FIXTURE_YAML).expect("parse");
        assert!(roster.lookup(99).is_none());
    }

    #[test]
    fn empty_team_name_normalised_to_none() {
        let roster = SessionInfoParser::build(FIXTURE_YAML).expect("parse");
        let car = roster.lookup(1).expect("carIdx 1 should exist");
        // TeamName was "" — should be None after normalisation
        assert_eq!(car.team_name, None);
    }

    #[test]
    fn absent_optional_fields_are_none() {
        let roster = SessionInfoParser::build(FIXTURE_YAML).expect("parse");
        let car = roster.lookup(2).expect("carIdx 2 should exist");
        // carIdx 2 has no TeamName or UserID keys at all
        assert_eq!(car.team_name, None);
        assert_eq!(car.user_id, None);
        assert_eq!(car.irating, None);
        assert_eq!(car.lic_string, None);
        assert_eq!(car.flair_name, None);
    }

    #[test]
    fn high_car_idx_slot_parsed_correctly() {
        let roster = SessionInfoParser::build(FIXTURE_YAML).expect("parse");
        let car = roster
            .lookup(63)
            .expect("carIdx 63 (pace car) should exist");
        assert_eq!(car.car_number, "0");
        assert_eq!(car.driver_name, "Pace Car");
    }

    #[test]
    fn roster_cache_starts_needing_update() {
        let cache = RosterCache::new();
        assert!(cache.needs_update(1));
        assert!(cache.roster().is_none());
    }

    #[test]
    fn roster_cache_satisfied_after_update() {
        let mut cache = RosterCache::new();
        cache
            .update(5, FIXTURE_YAML)
            .expect("update should succeed");
        assert!(!cache.needs_update(5));
        assert!(cache.needs_update(6));
    }

    #[test]
    fn roster_cache_retains_roster_on_bad_yaml() {
        let mut cache = RosterCache::new();
        cache.update(1, FIXTURE_YAML).expect("first update");
        // Simulate a corrupted re-read — the parse fails
        let result = cache.update(2, "not: valid: yaml: at: all: {{");
        assert!(result.is_err());
        // Previous roster is still accessible and tick has NOT advanced
        assert!(cache.needs_update(2));
        assert!(cache.roster().is_some());
    }

    #[test]
    fn roster_cache_lookup_via_cache() {
        let mut cache = RosterCache::new();
        cache.update(3, FIXTURE_YAML).expect("update");
        let car = cache
            .roster()
            .and_then(|r| r.lookup(0))
            .expect("should find carIdx 0");
        assert_eq!(car.driver_name, "Paul Crofts");
    }

    #[test]
    fn parse_sub_session_id_skips_zero_and_finds_later_positive() {
        let yaml = r#"
WeekendInfo:
    TrackName: okayama full
    SubSessionID: 0
SessionInfo:
    Sessions:
        - SessionType: Practice
WeekendInfo2:
    SubSessionID: 123456789
"#;
        assert_eq!(parse_sub_session_id(yaml), Some(123456789));
    }

    #[test]
    fn parse_sub_session_id_accepts_quoted_values() {
        let yaml = r#"
WeekendInfo:
    SubSessionId: "987654321"
"#;
        assert_eq!(parse_sub_session_id(yaml), Some(987654321));
    }

    #[test]
    fn enrichment_fields_parse_when_present() {
        let yaml = r#"
DriverInfo:
 DriverCarIdx: 0
 Drivers:
 - { CarIdx: 0, UserName: Paul Crofts, UserID: 123456, IRating: 2680, LicString: "D 2.79", FlairName: "United States", CarNumber: "42", CarClassID: 4011, CarClassShortName: GTP }
"#;
        let roster = SessionInfoParser::build(yaml).expect("parse");
        let car = roster.lookup(0).expect("carIdx 0 should exist");
        assert_eq!(car.irating, Some(2680));
        assert_eq!(car.lic_string, Some("D 2.79".to_string()));
        assert_eq!(car.flair_name, Some("United States".to_string()));
    }

    #[test]
    fn session_metadata_uses_trackname_and_default_session_when_index_invalid() {
        let yaml = r#"
WeekendInfo:
    TrackName: test_track
SessionInfo:
    Sessions:
        - SessionType: Practice
            SessionLaps: "unlimited"
        - SessionType: Race
            SessionLaps: 30
"#;
        let meta = SessionMetadata::parse(yaml, -1);
        assert_eq!(meta.track_name.as_deref(), Some("test_track"));
        assert_eq!(meta.session_type.as_deref(), Some("Practice"));
        assert_eq!(meta.session_laps.as_deref(), Some("unlimited"));
    }

    #[test]
    fn session_metadata_fallback_line_scan_handles_minimal_yaml() {
        let yaml = r#"
TrackDisplayName: Sample Circuit
SessionType: Race
SessionLaps: 45
"#;
        let meta = SessionMetadata::parse(yaml, 2);
        assert_eq!(meta.track_name.as_deref(), Some("Sample Circuit"));
        assert_eq!(meta.session_type.as_deref(), Some("Race"));
        assert_eq!(meta.session_laps.as_deref(), Some("45"));
    }

    #[test]
    fn parse_track_length_km() {
        assert_eq!(parse_track_length_meters("3.70 km"), Some(3700.0));
        assert_eq!(parse_track_length_meters("\"5.84 km\""), Some(5840.0));
    }

    #[test]
    fn parse_track_length_miles() {
        let m = parse_track_length_meters("0.90 mi").expect("parse");
        assert!((m - 1448.41).abs() < 0.1, "got {m}");
    }

    #[test]
    fn parse_track_length_rejects_garbage() {
        assert_eq!(parse_track_length_meters(""), None);
        assert_eq!(parse_track_length_meters("unknown"), None);
        assert_eq!(parse_track_length_meters("0.0 km"), None);
    }

    #[test]
    fn session_metadata_parses_track_length() {
        let yaml = r#"
WeekendInfo:
    TrackName: test_track
    TrackLength: 3.70 km
SessionInfo:
    Sessions:
        - SessionType: Race
"#;
        let meta = SessionMetadata::parse(yaml, 0);
        assert_eq!(meta.track_length_m, Some(3700.0));
    }
}
