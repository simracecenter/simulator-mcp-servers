// SPDX-License-Identifier: GPL-3.0-or-later

//! Third-party battle pair tracking.
//!
//! The legacy classifier in [`crate::battle_state`] only answers "is *my* car
//! threatened". This module widens that to every pair of cars racing near the
//! publisher's car, using the same per-frame field data
//! (`car_idx_lap_dist_pct` / `car_idx_position` / `car_idx_on_pit_road`) the
//! gap finder already reads, and gives each fight a durable identity so a
//! consumer can hold coverage on it across many cycles.
//!
//! Lifecycle per pair, all correlated by one `battle_id`:
//!
//! ```text
//! (gap < 1.5 s for 5 frames) ──► ENGAGED ──► CLOSING* ──► BROKEN
//! ```
//!
//! `CLOSING` repeats while the behind car keeps taking time out of the gap;
//! `BROKEN` fires once the gap opens past the break threshold and stays there,
//! or one car pits / leaves the world. A pair that breaks and re-forms is a
//! new battle with a new id.

use std::collections::{HashMap, VecDeque};

use crate::battle_state::{
    CLOSE_APPROACH_MIN_FRAMES, CLOSE_APPROACH_THRESH_S, MAX_BATTLE_GAP_S, SCAN_FIELD_POSITIONS,
};
use crate::race_event::{BattleBreakReason, BattleIdentity, BattlePhase};
use crate::telemetry_frame::TelemetryFrame;

// ── Constants ────────────────────────────────────────────────────────────────

/// Gap below which two cars are engaged, once held for `PAIR_ENGAGE_MIN_FRAMES`.
pub const PAIR_ENGAGE_GAP_S: f32 = CLOSE_APPROACH_THRESH_S;
pub const PAIR_ENGAGE_MIN_FRAMES: u32 = CLOSE_APPROACH_MIN_FRAMES;
/// Gap above which an engaged pair is breaking. Wider than the engage gap so a
/// fight that oscillates around 1.5 s does not flicker.
pub const PAIR_BREAK_GAP_S: f32 = 2.5;
/// Frames the gap must stay past `PAIR_BREAK_GAP_S` (or the pair must be
/// unobservable) before `BROKEN` fires — half a second at 60 Hz.
pub const PAIR_BREAK_MIN_FRAMES: u32 = 30;
/// A pair is tracked while either car is within this many race positions of
/// the publisher's car. When the publisher's car has no race position (pit
/// stall, spectating) the whole field is in scope.
pub const PAIR_SCAN_POSITIONS: u8 = SCAN_FIELD_POSITIONS as u8;
/// Minimum closing rate for a `CLOSING` update, seconds per lap.
pub const PAIR_CLOSING_RATE_S_PER_LAP: f32 = 0.5;
/// The gap must have shrunk by at least this much since the previous
/// `ENGAGED`/`CLOSING` event before another `CLOSING` fires.
pub const PAIR_CLOSING_MIN_GAP_DROP_S: f32 = 0.2;
/// Minimum spacing between two `CLOSING` events for the same battle.
pub const PAIR_CLOSING_MIN_INTERVAL_S: f32 = 10.0;
/// Window over which the closing rate is estimated.
const GAP_HISTORY_WINDOW_S: f32 = 10.0;
/// Minimum span of gap history before a closing rate is reported.
const GAP_HISTORY_MIN_SPAN_S: f32 = 3.0;
/// Samples at which confidence saturates (one second at 60 Hz).
const CONFIDENCE_FULL_SAMPLES: f32 = 60.0;
const CONFIDENCE_FLOOR: f32 = 0.35;
/// Discount applied while the lap time used to scale gaps is a fallback.
const CONFIDENCE_NO_LAP_TIME_FACTOR: f32 = 0.6;

// ── Types ────────────────────────────────────────────────────────────────────

/// A lifecycle transition the tracker observed on one frame.
#[derive(Clone, Debug, PartialEq)]
pub enum PairTransition {
    Engaged(BattleIdentity),
    Closing(BattleIdentity),
    Broken(BattleIdentity),
}

impl PairTransition {
    pub fn identity(&self) -> &BattleIdentity {
        match self {
            Self::Engaged(id) | Self::Closing(id) | Self::Broken(id) => id,
        }
    }
}

/// Per-frame facts needed to describe a battle: the clock, the lap time used
/// to scale gaps, whether that lap time is measured, and which car is the rig.
#[derive(Clone, Copy, Debug)]
pub struct FrameContext {
    pub session_time: f32,
    pub lap_time_s: f32,
    pub lap_time_known: bool,
    pub observer: u8,
}

impl FrameContext {
    pub fn new(frame: &TelemetryFrame, lap_time_s: f32, lap_time_known: bool) -> Self {
        Self {
            session_time: frame.session_time,
            lap_time_s,
            lap_time_known,
            observer: frame.player_car_idx,
        }
    }
}

/// Unordered pair key: `(min, max)` of the two car indices.
type PairKey = (u8, u8);

fn pair_key(a: u8, b: u8) -> PairKey {
    (a.min(b), a.max(b))
}

#[derive(Clone, Copy, Debug)]
struct ObservedPair {
    ahead: u8,
    behind: u8,
    gap_s: f32,
}

#[derive(Debug)]
struct ActiveBattle {
    id: String,
    engaged_at: f32,
    ahead: u8,
    behind: u8,
    last_gap_s: Option<f32>,
    /// `(session_time, gap_s)` samples inside `GAP_HISTORY_WINDOW_S`.
    history: VecDeque<(f32, f32)>,
    samples: u32,
    far_frames: u32,
    last_closing_t: f32,
    gap_at_last_closing: f32,
}

impl ActiveBattle {
    fn record(&mut self, t: f32, gap_s: f32) {
        self.history.push_back((t, gap_s));
        while self
            .history
            .front()
            .is_some_and(|(t0, _)| t - *t0 > GAP_HISTORY_WINDOW_S)
        {
            self.history.pop_front();
        }
        self.last_gap_s = Some(gap_s);
        self.samples = self.samples.saturating_add(1);
    }

    /// Seconds per lap the behind car is closing (positive = closing).
    fn closing_rate_s_per_lap(&self, lap_time_s: f32) -> Option<f32> {
        let (t0, g0) = *self.history.front()?;
        let (t1, g1) = *self.history.back()?;
        let span = t1 - t0;
        if span < GAP_HISTORY_MIN_SPAN_S || lap_time_s <= 0.0 {
            return None;
        }
        Some((g0 - g1) / span * lap_time_s)
    }

    fn confidence(&self, lap_time_known: bool) -> f32 {
        let growth = (self.samples as f32 / CONFIDENCE_FULL_SAMPLES).min(1.0);
        let base = CONFIDENCE_FLOOR + (1.0 - CONFIDENCE_FLOOR) * growth;
        if lap_time_known {
            base
        } else {
            base * CONFIDENCE_NO_LAP_TIME_FACTOR
        }
    }

    fn identity(
        &self,
        phase: BattlePhase,
        ctx: FrameContext,
        break_reason: Option<BattleBreakReason>,
    ) -> BattleIdentity {
        BattleIdentity {
            battle_id: self.id.clone(),
            battle_phase: phase,
            ahead_car_idx: self.ahead,
            behind_car_idx: self.behind,
            engaged_at: self.engaged_at,
            battle_age_s: (ctx.session_time - self.engaged_at).max(0.0),
            current_gap_s: self.last_gap_s,
            closing_rate_s_per_lap: self.closing_rate_s_per_lap(ctx.lap_time_s),
            battle_confidence: self.confidence(ctx.lap_time_known),
            battle_involves_publisher: self.ahead == ctx.observer || self.behind == ctx.observer,
            battle_break_reason: break_reason,
        }
    }
}

/// Tracks every close pair of cars near the publisher's car across frames.
#[derive(Debug, Default)]
pub struct BattlePairTracker {
    battles: HashMap<PairKey, ActiveBattle>,
    /// Consecutive frames a not-yet-engaged pair has been inside the engage gap.
    candidates: HashMap<PairKey, u32>,
    /// Frame this tracker's most recent `ENGAGED` was minted on, so two
    /// battles engaged on the same frame still get distinct ids.
    minted: u32,
}

impl BattlePairTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of battles currently engaged.
    pub fn active_count(&self) -> usize {
        self.battles.len()
    }

    /// Identity of the tracked battle between two cars, if any, as it stands
    /// at session time `t`. Used to stamp the legacy player-threat events so
    /// they correlate with the pair tracker's own events.
    pub fn identity_for(
        &self,
        a: u8,
        b: u8,
        phase: BattlePhase,
        ctx: FrameContext,
    ) -> Option<BattleIdentity> {
        self.battles
            .get(&pair_key(a, b))
            .map(|battle| battle.identity(phase, ctx, None))
    }

    /// Frames of evidence behind the tracked battle between two cars (0 if none).
    pub fn sample_count(&self, a: u8, b: u8) -> u32 {
        self.battles
            .get(&pair_key(a, b))
            .map_or(0, |battle| battle.samples)
    }

    /// Advance the tracker by one frame.
    ///
    /// `lap_time_s` scales track-distance differences into seconds, exactly as
    /// the gap finder does; `lap_time_known` says whether it is a measured lap
    /// or the fallback, which only affects confidence.
    pub fn update(
        &mut self,
        frame: &TelemetryFrame,
        lap_time_s: f32,
        lap_time_known: bool,
    ) -> Vec<PairTransition> {
        let t = frame.session_time;
        let ctx = FrameContext::new(frame, lap_time_s, lap_time_known);
        let observed = observe_pairs(frame, lap_time_s);
        let mut out = Vec::new();

        // Existing battles: refresh, then decide CLOSING / BROKEN.
        let mut broken: Vec<PairKey> = Vec::new();
        for (key, battle) in self.battles.iter_mut() {
            match observed.get(key) {
                Some(pair) => {
                    battle.ahead = pair.ahead;
                    battle.behind = pair.behind;
                    battle.record(t, pair.gap_s);
                    if pair.gap_s > PAIR_BREAK_GAP_S {
                        battle.far_frames += 1;
                    } else {
                        battle.far_frames = 0;
                    }
                }
                None => {
                    battle.far_frames += 1;
                    battle.samples = battle.samples.saturating_add(1);
                }
            }

            if battle.far_frames >= PAIR_BREAK_MIN_FRAMES {
                let reason = break_reason(frame, battle.ahead, battle.behind);
                let mut id = battle.identity(BattlePhase::Broken, ctx, Some(reason));
                // An unobservable pair has no gap to report.
                if !observed.contains_key(key) {
                    id.current_gap_s = None;
                }
                out.push(PairTransition::Broken(id));
                broken.push(*key);
                continue;
            }

            if let Some(pair) = observed.get(key) {
                let rate = battle.closing_rate_s_per_lap(lap_time_s);
                let closing = rate.is_some_and(|r| r >= PAIR_CLOSING_RATE_S_PER_LAP)
                    && pair.gap_s <= battle.gap_at_last_closing - PAIR_CLOSING_MIN_GAP_DROP_S
                    && (t - battle.last_closing_t) >= PAIR_CLOSING_MIN_INTERVAL_S;
                if closing {
                    battle.last_closing_t = t;
                    battle.gap_at_last_closing = pair.gap_s;
                    out.push(PairTransition::Closing(battle.identity(
                        BattlePhase::Closing,
                        ctx,
                        None,
                    )));
                }
            }
        }
        for key in broken {
            self.battles.remove(&key);
        }

        // Candidates: pairs inside the engage gap that are not yet battles.
        self.candidates.retain(|key, _| {
            observed
                .get(key)
                .is_some_and(|p| p.gap_s < PAIR_ENGAGE_GAP_S)
        });
        for (key, pair) in &observed {
            if self.battles.contains_key(key) || pair.gap_s >= PAIR_ENGAGE_GAP_S {
                continue;
            }
            let frames = self.candidates.entry(*key).or_insert(0);
            *frames += 1;
            if *frames < PAIR_ENGAGE_MIN_FRAMES {
                continue;
            }
            self.minted = self.minted.wrapping_add(1);
            let mut battle = ActiveBattle {
                id: format!(
                    "btl-{:02}-{:02}-{}-{}",
                    pair.ahead,
                    pair.behind,
                    (t * 1000.0).round() as i64,
                    self.minted
                ),
                engaged_at: t,
                ahead: pair.ahead,
                behind: pair.behind,
                last_gap_s: None,
                history: VecDeque::new(),
                samples: *frames,
                far_frames: 0,
                last_closing_t: t,
                gap_at_last_closing: pair.gap_s,
            };
            battle.record(t, pair.gap_s);
            out.push(PairTransition::Engaged(battle.identity(
                BattlePhase::Engaged,
                ctx,
                None,
            )));
            self.battles.insert(*key, battle);
        }
        self.candidates
            .retain(|key, _| !self.battles.contains_key(key));

        out
    }
}

/// Whether a car slot is racing: classified, on track and in the world.
fn car_is_racing(frame: &TelemetryFrame, idx: usize) -> bool {
    let position = frame.car_idx_position.get(idx).copied().unwrap_or(0);
    let ldp = frame.car_idx_lap_dist_pct.get(idx).copied().unwrap_or(-1.0);
    let on_pit = frame.car_idx_on_pit_road.get(idx).copied().unwrap_or(false);
    let surface = frame.car_idx_track_surface.get(idx).copied().unwrap_or(0);
    position > 0 && ldp >= -0.5 && !on_pit && surface >= 0
}

/// Every pair of racing cars near the observer within `MAX_BATTLE_GAP_S`,
/// with the car ahead in race order as `ahead`.
fn observe_pairs(frame: &TelemetryFrame, lap_time_s: f32) -> HashMap<PairKey, ObservedPair> {
    let observer_pos = frame
        .car_idx_position
        .get(frame.player_car_idx as usize)
        .copied()
        .filter(|p| *p > 0);
    let in_scope = |pos: u8| {
        observer_pos
            .is_none_or(|op| (pos as i16 - op as i16).unsigned_abs() as u8 <= PAIR_SCAN_POSITIONS)
    };

    let racing: Vec<(u8, u8, f32)> = frame
        .car_idx_position
        .iter()
        .enumerate()
        .filter(|(idx, _)| car_is_racing(frame, *idx))
        .map(|(idx, &pos)| (idx as u8, pos, frame.car_idx_lap_dist_pct[idx]))
        .collect();

    let mut pairs = HashMap::new();
    for (i, &(idx_a, pos_a, ldp_a)) in racing.iter().enumerate() {
        for &(idx_b, pos_b, ldp_b) in &racing[i + 1..] {
            if pos_a == pos_b || (!in_scope(pos_a) && !in_scope(pos_b)) {
                continue;
            }
            let (ahead, behind, ldp_ahead, ldp_behind) = if pos_a < pos_b {
                (idx_a, idx_b, ldp_a, ldp_b)
            } else {
                (idx_b, idx_a, ldp_b, ldp_a)
            };
            let mut diff = ldp_ahead - ldp_behind;
            if diff < 0.0 {
                diff += 1.0; // S/F line wrap
            }
            let gap_s = diff * lap_time_s;
            if !(0.0..=MAX_BATTLE_GAP_S).contains(&gap_s) {
                continue;
            }
            pairs.insert(
                pair_key(ahead, behind),
                ObservedPair {
                    ahead,
                    behind,
                    gap_s,
                },
            );
        }
    }
    pairs
}

fn break_reason(frame: &TelemetryFrame, a: u8, b: u8) -> BattleBreakReason {
    let pitted = |idx: u8| {
        frame
            .car_idx_on_pit_road
            .get(idx as usize)
            .copied()
            .unwrap_or(false)
    };
    let gone = |idx: u8| {
        let i = idx as usize;
        frame.car_idx_position.get(i).copied().unwrap_or(0) == 0
            || frame.car_idx_lap_dist_pct.get(i).copied().unwrap_or(-1.0) < -0.5
            || frame.car_idx_track_surface.get(i).copied().unwrap_or(0) < 0
    };
    if pitted(a) || pitted(b) {
        BattleBreakReason::CarPitted
    } else if gone(a) || gone(b) {
        BattleBreakReason::CarLeftWorld
    } else {
        BattleBreakReason::GapOpened
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const LAP_S: f32 = 100.0;

    /// Frame with the observer (car 0) at position 1, ldp 0.50, plus the given
    /// `(car_idx, position, lap_dist_pct)` cars.
    fn frame(t: f32, cars: &[(u8, u8, f32)]) -> TelemetryFrame {
        let n = cars
            .iter()
            .map(|c| c.0 as usize + 1)
            .max()
            .unwrap_or(1)
            .max(1);
        let mut f = TelemetryFrame {
            lap: 3,
            session_time: t,
            lap_dist_pct: 0.5,
            player_car_idx: 0,
            player_car_position: 1,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct: vec![-1.0; n],
            car_idx_position: vec![0; n],
            car_idx_on_pit_road: vec![false; n],
            car_idx_track_surface: vec![0; n],
            lap_last_lap_time: LAP_S,
            session_info_update: 0,
            session_tick: (t * 60.0) as i64,
            session_state: 4,
            session_num: 0,
            session_time_remain: None,
            session_laps_remain: None,
            player_incident_count: 0,
            car_idx_lap_completed: vec![3; n],
            lf_temp_m: 0.0,
            rf_temp_m: 0.0,
            lr_temp_m: 0.0,
            rr_temp_m: 0.0,
            fuel_level: 0.0,
            throttle: 0.0,
            brake: 0.0,
            speed: 0.0,
        };
        f.car_idx_lap_dist_pct[0] = 0.5;
        f.car_idx_position[0] = 1;
        for &(idx, pos, ldp) in cars {
            f.car_idx_lap_dist_pct[idx as usize] = ldp;
            f.car_idx_position[idx as usize] = pos;
        }
        f
    }

    /// Third-party pair: car 3 (P2) leads car 5 (P3) by `gap_s`, both a long
    /// way behind the observer.
    fn third_party(t: f32, gap_s: f32) -> TelemetryFrame {
        let behind_ldp = 0.20 - gap_s / LAP_S;
        frame(t, &[(3, 2, 0.20), (5, 3, behind_ldp)])
    }

    fn run(
        tracker: &mut BattlePairTracker,
        frames: impl Iterator<Item = TelemetryFrame>,
    ) -> Vec<PairTransition> {
        frames
            .flat_map(|f| tracker.update(&f, LAP_S, true))
            .collect()
    }

    #[test]
    fn third_party_pair_engages_without_the_observer() {
        let mut tracker = BattlePairTracker::new();
        let events = run(
            &mut tracker,
            (0..PAIR_ENGAGE_MIN_FRAMES).map(|i| third_party(i as f32 / 60.0, 0.8)),
        );
        assert_eq!(events.len(), 1, "exactly one ENGAGED: {events:?}");
        let PairTransition::Engaged(id) = &events[0] else {
            panic!("expected ENGAGED, got {:?}", events[0]);
        };
        assert_eq!(id.battle_phase, BattlePhase::Engaged);
        assert_eq!(id.ahead_car_idx, 3);
        assert_eq!(id.behind_car_idx, 5);
        assert!(!id.battle_involves_publisher);
        assert!((id.current_gap_s.unwrap() - 0.8).abs() < 0.01);
        assert_eq!(id.battle_age_s, 0.0);
        assert!(id.battle_id.starts_with("btl-03-05-"));
        assert!(id.battle_confidence > 0.0 && id.battle_confidence <= 1.0);
    }

    #[test]
    fn pair_involving_the_observer_is_flagged() {
        let mut tracker = BattlePairTracker::new();
        // Car 2 at P2, 0.5 s behind the observer.
        let events = run(
            &mut tracker,
            (0..PAIR_ENGAGE_MIN_FRAMES).map(|i| frame(i as f32 / 60.0, &[(2, 2, 0.495)])),
        );
        assert_eq!(events.len(), 1);
        let id = events[0].identity();
        assert!(id.battle_involves_publisher);
        assert_eq!((id.ahead_car_idx, id.behind_car_idx), (0, 2));
    }

    #[test]
    fn a_brief_close_approach_does_not_engage() {
        let mut tracker = BattlePairTracker::new();
        let events = run(
            &mut tracker,
            (0..PAIR_ENGAGE_MIN_FRAMES - 1).map(|i| third_party(i as f32 / 60.0, 0.8)),
        );
        assert!(events.is_empty());
        // Gap opens before the threshold is met: the candidate is dropped.
        assert!(tracker
            .update(&third_party(1.0, 3.0), LAP_S, true)
            .is_empty());
        assert!(tracker.candidates.is_empty());
    }

    #[test]
    fn battle_id_is_stable_for_the_life_of_the_engagement_and_changes_when_it_reforms() {
        let mut tracker = BattlePairTracker::new();
        let mut t = 0.0;
        let mut step = |tracker: &mut BattlePairTracker, gap: f32| {
            t += 1.0 / 60.0;
            tracker.update(&third_party(t, gap), LAP_S, true)
        };

        let mut first_id = None;
        for _ in 0..PAIR_ENGAGE_MIN_FRAMES {
            for ev in step(&mut tracker, 0.9) {
                first_id = Some(ev.identity().battle_id.clone());
            }
        }
        let first_id = first_id.expect("engaged");

        // Gap wobbles between 0.9 s and 1.1 s for 20 s: no new events, id unchanged.
        for i in 0..1200 {
            let gap = if i % 2 == 0 { 0.9 } else { 1.1 };
            assert!(
                step(&mut tracker, gap).is_empty(),
                "no lifecycle change on wobble"
            );
        }
        // A short excursion past the break gap does not break the battle.
        for _ in 0..PAIR_BREAK_MIN_FRAMES - 1 {
            assert!(step(&mut tracker, 3.0).is_empty());
        }
        assert!(step(&mut tracker, 1.0).is_empty());
        assert_eq!(tracker.battles.values().next().unwrap().id, first_id);

        // Gap opens past the break threshold and stays: BROKEN with the same id.
        let mut broken = None;
        for _ in 0..PAIR_BREAK_MIN_FRAMES {
            for ev in step(&mut tracker, 4.0) {
                broken = Some(ev);
            }
        }
        let PairTransition::Broken(id) = broken.expect("broken") else {
            panic!("expected BROKEN");
        };
        assert_eq!(id.battle_id, first_id);
        assert_eq!(id.battle_phase, BattlePhase::Broken);
        assert_eq!(id.battle_break_reason, Some(BattleBreakReason::GapOpened));
        assert!(id.battle_age_s > 20.0);
        assert_eq!(tracker.active_count(), 0);

        // Re-forms: a new battle with a new id.
        let mut second_id = None;
        for _ in 0..PAIR_ENGAGE_MIN_FRAMES {
            for ev in step(&mut tracker, 0.9) {
                second_id = Some(ev.identity().battle_id.clone());
            }
        }
        let second_id = second_id.expect("re-engaged");
        assert_ne!(first_id, second_id);
    }

    #[test]
    fn closing_updates_are_correlated_and_rate_limited() {
        let mut tracker = BattlePairTracker::new();
        let mut events = Vec::new();
        // Gap shrinks steadily from 1.4 s to 0.2 s over 30 s (4 s/lap closing rate).
        for i in 0..1800 {
            let t = i as f32 / 60.0;
            let gap = 1.4 - 1.2 * (t / 30.0);
            events.extend(tracker.update(&third_party(t, gap), LAP_S, true));
        }
        let engaged: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, PairTransition::Engaged(_)))
            .collect();
        let closing: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, PairTransition::Closing(_)))
            .collect();
        assert_eq!(engaged.len(), 1);
        assert!(!closing.is_empty(), "expected CLOSING updates: {events:?}");
        assert!(
            closing.len() <= 3,
            "rate limited to one per {PAIR_CLOSING_MIN_INTERVAL_S}s: {}",
            closing.len()
        );
        let battle_id = &engaged[0].identity().battle_id;
        for ev in &closing {
            let id = ev.identity();
            assert_eq!(&id.battle_id, battle_id);
            assert_eq!(id.battle_phase, BattlePhase::Closing);
            assert!(id.closing_rate_s_per_lap.unwrap() > PAIR_CLOSING_RATE_S_PER_LAP);
            assert!(id.battle_age_s >= PAIR_CLOSING_MIN_INTERVAL_S - 0.1);
        }
        assert!(!events
            .iter()
            .any(|e| matches!(e, PairTransition::Broken(_))));
    }

    #[test]
    fn a_car_pitting_breaks_the_battle_with_the_reason() {
        let mut tracker = BattlePairTracker::new();
        for i in 0..PAIR_ENGAGE_MIN_FRAMES {
            tracker.update(&third_party(i as f32 / 60.0, 0.8), LAP_S, true);
        }
        assert_eq!(tracker.active_count(), 1);
        let mut broken = None;
        for i in 0..PAIR_BREAK_MIN_FRAMES {
            let mut f = third_party(1.0 + i as f32 / 60.0, 0.8);
            f.car_idx_on_pit_road[5] = true;
            for ev in tracker.update(&f, LAP_S, true) {
                broken = Some(ev);
            }
        }
        let PairTransition::Broken(id) = broken.expect("broken") else {
            panic!("expected BROKEN");
        };
        assert_eq!(id.battle_break_reason, Some(BattleBreakReason::CarPitted));
        assert_eq!(id.current_gap_s, None);
    }

    #[test]
    fn roles_swap_on_overtake_without_changing_the_id() {
        let mut tracker = BattlePairTracker::new();
        for i in 0..PAIR_ENGAGE_MIN_FRAMES {
            tracker.update(&third_party(i as f32 / 60.0, 0.8), LAP_S, true);
        }
        // Car 5 is now P2, car 3 P3, 0.3 s apart.
        let f = frame(1.0, &[(5, 2, 0.20), (3, 3, 0.197)]);
        let ctx = FrameContext::new(&f, LAP_S, true);
        let before = tracker
            .identity_for(3, 5, BattlePhase::Engaged, ctx)
            .unwrap();
        assert!(tracker.update(&f, LAP_S, true).is_empty());
        let after = tracker
            .identity_for(3, 5, BattlePhase::Engaged, ctx)
            .unwrap();
        assert_eq!(before.battle_id, after.battle_id);
        assert_eq!((after.ahead_car_idx, after.behind_car_idx), (5, 3));
    }

    #[test]
    fn pairs_outside_the_scan_window_are_ignored() {
        let mut tracker = BattlePairTracker::new();
        // Observer P1; cars at P9/P10 fight far away.
        let f = |t: f32| frame(t, &[(3, 9, 0.20), (5, 10, 0.195)]);
        let events = run(
            &mut tracker,
            (0..PAIR_ENGAGE_MIN_FRAMES * 2).map(|i| f(i as f32 / 60.0)),
        );
        assert!(events.is_empty());
    }

    #[test]
    fn whole_field_is_in_scope_when_the_observer_has_no_position() {
        let mut tracker = BattlePairTracker::new();
        let f = |t: f32| {
            let mut f = frame(t, &[(3, 9, 0.20), (5, 10, 0.195)]);
            f.car_idx_position[0] = 0;
            f.player_car_position = 0;
            f
        };
        let events = run(
            &mut tracker,
            (0..PAIR_ENGAGE_MIN_FRAMES).map(|i| f(i as f32 / 60.0)),
        );
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn confidence_is_discounted_without_a_measured_lap_time() {
        let mut a = BattlePairTracker::new();
        let mut b = BattlePairTracker::new();
        let mut known = None;
        let mut unknown = None;
        for i in 0..PAIR_ENGAGE_MIN_FRAMES {
            let f = third_party(i as f32 / 60.0, 0.8);
            for ev in a.update(&f, LAP_S, true) {
                known = Some(ev.identity().battle_confidence);
            }
            for ev in b.update(&f, LAP_S, false) {
                unknown = Some(ev.identity().battle_confidence);
            }
        }
        assert!(unknown.unwrap() < known.unwrap());
    }
}
