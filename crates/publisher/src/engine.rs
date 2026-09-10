// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::{HashMap, HashSet};

use crate::anchor_sampler::AnchorSampler;
use crate::basic_incident::BasicIncidentDetector;
use crate::battle_pairs::{BattlePairTracker, FrameContext, PairTransition};
use crate::battle_state::{
    classify, BattleState, SlopeInfo, CLOSE_APPROACH_MIN_FRAMES, CLOSE_APPROACH_THRESH_S,
    MIN_PUSH_READINGS, SCAN_FIELD_POSITIONS,
};
use crate::braking_profile::BrakingProfileDetector;
use crate::car_registry::CarRegistry;
use crate::compression_zone::CompressionZoneDetector;
use crate::fuel_projection::FuelProjection;
use crate::gap_finder::{find_cars_ahead, find_cars_behind};
use crate::horizon::HorizonDetector;
use crate::incident_cluster::IncidentClusterDetector;
use crate::lap_timer::LapTimer;
use crate::lift_coast::LiftCoastDetector;
use crate::micro_sector::MicroSectorTracker;
use crate::race_event::{
    BattleIdentity, BattlePhase, DriverEffort, FlagScope, GapTrend, LifecycleOrigin, RaceEvent,
};
use crate::regression_store::RegressionStore;
use crate::session_info::SessionRoster;
use crate::telemetry_frame::TelemetryFrame;
use crate::tire_degradation::TireDegradation;
use crate::traffic_intercept::TrafficInterceptDetector;
use crate::vulnerability::VulnerabilityDetector;

const PIT_LAP_FRAME_THRESH: u32 = 20;
const CHECKERED: u32 = 0x0001;
const YELLOW_WAVE: u32 = 0x0100;
const CAUTION: u32 = 0x4000;
/// iRacing `SessionState` values used for session lifecycle transitions.
const SESSION_STATE_RACING: i32 = 4;
const SESSION_STATE_CHECKERED: i32 = 5;
const SESSION_STATE_COOLDOWN: i32 = 6;
const YELLOW_ZONES: &[(u8, f32, f32)] = &[(1, 0.625, 0.646), (2, 0.616, 0.623)];
/// Gap movement over one cadence window below this is reported as `STABLE`;
/// gap readings themselves jitter by a few hundredths between frames.
const GAP_TREND_DELTA_S: f32 = 0.15;
/// A sector at most this much slower than the driver's own best still counts as
/// pushing — sector times carry more noise than a full lap.
const PUSH_SECTOR_DELTA_S: f32 = 0.05;
/// A lap within this much of the driver's own best counts as pushing.
const PUSH_LAP_DELTA_S: f32 = 0.35;
/// Upper bound for a credible race gap. Real battle gaps are bounded by
/// `MAX_BATTLE_GAP_S` (~5 s); anything at or above this value is a sentinel.
const MAX_VALID_GAP_S: f32 = 100.0;

pub struct NarrativeEngine {
    anchor_count: usize,
    lap_timer: LapTimer,
    /// Field-wide pair tracker: every fight near the rig, not only the rig's.
    battle_pairs: BattlePairTracker,
    sampler: AnchorSampler,
    sampler_behind: AnchorSampler,
    regression: RegressionStore,
    regression_behind: RegressionStore,
    engine_state: BattleState,
    defensive_state: BattleState,
    prev_slope: Option<f32>,
    prev_slope_beh: Option<f32>,
    pit_laps: HashSet<u8>,
    dirty_laps: HashSet<u8>,
    lap_end_positions: HashMap<u8, u8>,
    /// Track position-to-car mappings at lap boundaries for overtake detection.
    /// Key is lap number, value is Vec<car_idx> indexed by position (position - 1).
    lap_car_positions: HashMap<u8, Vec<u8>>,
    lap_pit_frames: HashMap<u8, u32>,
    best_lap_time_s: Option<f32>,
    engaged_cars: HashSet<u8>,
    engaged_cars_beh: HashSet<u8>,
    consecutive_close: u32,
    last_close_t: f32,
    tracking_car: Option<u8>,
    engagement_start_t: Option<f32>,
    consecutive_close_beh: u32,
    last_close_beh_t: f32,
    tracking_car_beh: Option<u8>,
    engagement_start_beh_t: Option<f32>,
    prev_session_state: i32,
    prev_session_flags: u32,
    /// `false` until the first frame has been processed, so a session already
    /// under way at connect can be reported as inherited state.
    session_state_sampled: bool,
    checkered_emitted: bool,
    /// Gaps reported by the previous `DRIVER_MATERIAL`, for the gap trends.
    last_material_gaps: Option<MaterialGaps>,
    prev_in_car: bool,
    prev_lap: Option<u8>,
    prev_on_pit: bool,
    prev_position: Option<u8>,
    prev_anchor_bucket: Option<u8>,
    car_registry: CarRegistry,
    basic_incident: BasicIncidentDetector,
    roster: SessionRoster,
    horizon: HorizonDetector,
    tire_degradation: TireDegradation,
    fuel_projection: FuelProjection,
    lift_coast: LiftCoastDetector,
    micro_sector: MicroSectorTracker,
    braking_profile: BrakingProfileDetector,
    traffic_intercept: TrafficInterceptDetector,
    vulnerability: VulnerabilityDetector,
    incident_cluster: IncidentClusterDetector,
    compression_zone: CompressionZoneDetector,
    track_length_m: f32,
    logged_tire_suppressed: bool,
    logged_fuel_suppressed: bool,
}

impl NarrativeEngine {
    pub fn new(anchor_count: usize) -> Self {
        Self {
            anchor_count,
            lap_timer: LapTimer::new(),
            battle_pairs: BattlePairTracker::new(),
            sampler: AnchorSampler::new(anchor_count),
            sampler_behind: AnchorSampler::new(anchor_count),
            regression: RegressionStore::new(),
            regression_behind: RegressionStore::new(),
            engine_state: BattleState::Idle,
            defensive_state: BattleState::Idle,
            prev_slope: None,
            prev_slope_beh: None,
            pit_laps: HashSet::new(),
            dirty_laps: HashSet::new(),
            lap_end_positions: HashMap::new(),
            lap_car_positions: HashMap::new(),
            lap_pit_frames: HashMap::new(),
            best_lap_time_s: None,
            engaged_cars: HashSet::new(),
            engaged_cars_beh: HashSet::new(),
            consecutive_close: 0,
            last_close_t: f32::NEG_INFINITY,
            tracking_car: None,
            engagement_start_t: None,
            consecutive_close_beh: 0,
            last_close_beh_t: f32::NEG_INFINITY,
            tracking_car_beh: None,
            engagement_start_beh_t: None,
            prev_session_state: 0,
            prev_session_flags: 0,
            session_state_sampled: false,
            checkered_emitted: false,
            last_material_gaps: None,
            prev_in_car: false,
            prev_lap: None,
            prev_on_pit: false,
            prev_position: None,
            prev_anchor_bucket: None,
            car_registry: CarRegistry::new(),
            roster: SessionRoster::default(),
            horizon: HorizonDetector::new(),
            tire_degradation: TireDegradation::new(0.2),
            fuel_projection: FuelProjection::new(),
            lift_coast: LiftCoastDetector::new(),
            micro_sector: MicroSectorTracker::new(anchor_count),
            braking_profile: BrakingProfileDetector::new(),
            traffic_intercept: TrafficInterceptDetector::new(),
            vulnerability: VulnerabilityDetector::default(),
            incident_cluster: IncidentClusterDetector::new(),
            compression_zone: CompressionZoneDetector::new(),
            track_length_m: 5000.0,
            logged_tire_suppressed: false,
            logged_fuel_suppressed: false,
            basic_incident: BasicIncidentDetector::new(),
        }
    }

    /// Set the real track length parsed from the SessionInfo YAML. Values that
    /// are not credible for a race track are ignored and the previous value
    /// (default 5000 m) is retained.
    pub fn set_track_length_m(&mut self, meters: f32) {
        if meters.is_finite() && meters >= 100.0 {
            self.track_length_m = meters;
        }
    }

    /// Snapshot of the driver at the wheel of this rig, for the periodic
    /// `DRIVER_MATERIAL` event. The publisher owns the cadence and passes the
    /// wall-clock `interval_s` since the previous one.
    ///
    /// `None` while there is nothing to say about the driver: sitting on pit
    /// road (the stall included) or out of the world entirely. Proximity to
    /// other cars is deliberately not a condition — a driver alone in clean air
    /// is exactly the case this event exists for.
    pub fn driver_material(
        &mut self,
        frame: &TelemetryFrame,
        interval_s: f32,
    ) -> Option<RaceEvent> {
        let me_idx = frame.player_car_idx as usize;
        let surface = frame.car_idx_track_surface.get(me_idx).copied();
        if frame.on_pit_road || surface.is_some_and(|s| s < 0) {
            self.last_material_gaps = None;
            return None;
        }

        let lap_t = self.lap_timer.best_estimate();
        let ahead = find_cars_ahead(frame, lap_t, 1).first().copied();
        let behind = find_cars_behind(frame, lap_t, 1).first().copied();
        let me = self.car_registry.get(frame.player_car_idx);

        let last_lap_time_s = me.map(|c| c.last_lap_time_s).filter(|t| *t > 0.0);
        let best_lap_time_s = self
            .best_lap_time_s
            .or_else(|| me.map(|c| c.best_lap_time_s))
            .filter(|t| *t > 0.0);
        let delta_to_best_s = match (last_lap_time_s, best_lap_time_s) {
            (Some(last), Some(best)) => Some(last - best),
            _ => None,
        };
        let sector = self.micro_sector.last_segment_delta_vs_best();

        let previous = self.last_material_gaps.take();
        let gap_ahead_trend = gap_trend(previous.as_ref().and_then(|p| p.ahead), ahead);
        let gap_behind_trend = gap_trend(previous.as_ref().and_then(|p| p.behind), behind);
        self.last_material_gaps = Some(MaterialGaps { ahead, behind });

        let effort = if sector.is_some_and(|(_, delta)| delta <= PUSH_SECTOR_DELTA_S)
            || delta_to_best_s.is_some_and(|delta| delta <= PUSH_LAP_DELTA_S)
            || gap_ahead_trend == GapTrend::Closing
        {
            DriverEffort::Pushing
        } else {
            DriverEffort::Holding
        };

        Some(RaceEvent::DriverMaterial {
            lap: frame.lap,
            session_time: frame.session_time,
            player_car_idx: frame.player_car_idx,
            position: frame.player_car_position,
            laps_completed: frame
                .car_idx_lap_completed
                .get(me_idx)
                .copied()
                .unwrap_or(-1),
            lap_dist_pct: frame.lap_dist_pct,
            last_lap_time_s,
            best_lap_time_s,
            gap_ahead_s: ahead.map(|(_, gap)| gap),
            car_ahead_idx: ahead.map(|(idx, _)| idx),
            gap_behind_s: behind.map(|(_, gap)| gap),
            car_behind_idx: behind.map(|(idx, _)| idx),
            delta_to_best_s,
            sector_bucket: sector.map(|(bucket, _)| bucket),
            sector_delta_to_best_s: sector.map(|(_, delta)| delta),
            gap_ahead_trend,
            gap_behind_trend,
            effort,
            on_pit_road: frame.on_pit_road,
            track_surface: surface.unwrap_or(-1),
            speed_mps: if frame.speed > 0.0 {
                frame.speed
            } else {
                me.map(|c| c.speed_ema_mps).unwrap_or(0.0)
            },
            fuel_level_l: frame.fuel_level,
            incident_count: frame.player_incident_count,
            session_state: frame.session_state,
            interval_s,
        })
    }

    pub fn process_frame(&mut self, frame: &TelemetryFrame) -> Vec<RaceEvent> {
        let mut events = Vec::new();
        self.car_registry.update_from_frame(
            frame,
            &self.roster,
            frame.session_tick,
            self.anchor_count,
            self.track_length_m,
        );
        events.extend(self.basic_incident.update(
            &self.car_registry,
            frame.lap,
            frame.session_time,
            frame.session_tick,
            frame.player_car_idx,
            frame.player_incident_count,
        ));
        self.tire_degradation.update_ema(frame);
        if let Some(event) = self.lift_coast.update(frame) {
            events.push(event);
        }
        if let Some(event) = self.braking_profile.update(frame, self.anchor_count) {
            events.push(event);
        }
        let bucket = ((frame.lap_dist_pct * self.anchor_count.max(1) as f32) as usize
            % self.anchor_count.max(1)) as u8;
        if self.prev_anchor_bucket != Some(bucket) {
            self.micro_sector
                .on_anchor_crossing(frame.session_time, bucket);
            self.prev_anchor_bucket = Some(bucket);
        }

        let lap = frame.lap;
        let t = frame.session_time;
        let pos = frame.player_car_position;
        let ldp = frame.lap_dist_pct;
        let on_pit = frame.on_pit_road;
        let session_state = frame.session_state;
        let session_flags = frame.session_flags;

        // A publisher that joins a session already racing sees its first sample
        // as a transition; say so rather than reporting a green flag that never
        // fell — a Practice session at connect produced one of these per rig.
        let origin = if self.session_state_sampled {
            LifecycleOrigin::SessionStateTransition
        } else {
            LifecycleOrigin::ConnectSnapshot
        };
        if session_state == SESSION_STATE_RACING && self.prev_session_state != SESSION_STATE_RACING
        {
            events.push(RaceEvent::RaceGreen {
                lap,
                session_time: t,
                synthetic: origin.is_synthetic(),
                origin,
            });
        }
        // The checkered flag is signalled two ways and neither is reliable on
        // its own: `SessionState` can step straight from Racing to CoolDown
        // between sampled frames, and the flag bit can be set while the state
        // still reads Racing. Fire on whichever arrives first, once per session.
        let checkered_now = session_state == SESSION_STATE_CHECKERED
            || session_flags & CHECKERED != 0
            || (session_state == SESSION_STATE_COOLDOWN
                && self.prev_session_state == SESSION_STATE_RACING);
        if checkered_now && !self.checkered_emitted {
            self.checkered_emitted = true;
            events.push(RaceEvent::RaceCheckered {
                lap,
                session_time: t,
                synthetic: origin.is_synthetic(),
                origin,
            });
        }
        let in_car = session_state != 0;
        if in_car && !self.prev_in_car {
            events.push(RaceEvent::DriverEnteredCar {
                lap,
                session_time: t,
                player_car_idx: frame.player_car_idx,
            });
        } else if !in_car && self.prev_in_car {
            events.push(RaceEvent::DriverExitedCar {
                lap,
                session_time: t,
                player_car_idx: frame.player_car_idx,
            });
        }
        if session_flags & CAUTION != 0 && self.prev_session_flags & CAUTION == 0 {
            events.push(RaceEvent::FlagYellowFullCourse {
                lap,
                session_time: t,
            });
        } else if session_flags & YELLOW_WAVE != 0 && self.prev_session_flags & YELLOW_WAVE == 0 {
            // Infer scope and context from active incident clusters at the time of the yellow.
            // A cluster whose centre is within 15% of the player's current track position is
            // considered "nearby"; beyond that we default to Unknown.
            const NEARBY_DIST_THRESHOLD: f32 = 0.15;
            let player_ldp = ldp;
            let mut best_primary: Option<u8> = None;
            let mut best_bucket: Option<u8> = None;
            let mut best_dist = f32::MAX;
            for (&bucket, (_cluster_lap, cars)) in &self.incident_cluster.active_clusters {
                let cluster_center = (bucket as f32 + 0.5) / self.anchor_count.max(1) as f32;
                let raw_dist = (player_ldp - cluster_center).abs();
                // Account for track wrap-around (e.g. position 0.98 vs 0.02).
                let dist = raw_dist.min(1.0 - raw_dist);
                if dist < best_dist {
                    best_dist = dist;
                    // Use the lowest car index as the primary trigger when incident
                    // damage data is unavailable — consistent with IncidentClusterDetector.
                    best_primary = cars.iter().copied().min();
                    best_bucket = Some(bucket);
                }
            }
            let (trigger_car_idx, linked_incident_id, scope) = if best_dist <= NEARBY_DIST_THRESHOLD
            {
                (
                    best_primary,
                    best_bucket.map(|b| b as u32),
                    FlagScope::Nearby,
                )
            } else {
                (None, None, FlagScope::Unknown)
            };
            events.push(RaceEvent::FlagYellowLocal {
                lap,
                session_time: t,
                trigger_car_idx,
                lap_dist_pct: Some(ldp),
                sector: None,
                scope,
                linked_incident_id,
            });
        }
        self.prev_in_car = in_car;
        self.session_state_sampled = true;
        self.prev_session_state = session_state;
        self.prev_session_flags = session_flags;

        // Record car positions every frame so we have accurate position history
        // (even for lap 0/formation lap, as we need this for overtake detection)
        if pos > 0 {
            self.record_lap_car_positions(lap, frame);
        }

        // Pit transitions are detected before the unclassified-car guard below:
        // a car in its pit stall reports position 0, so gating pit edges on a
        // classified position swallowed every stint-opening PIT_EXIT.
        if on_pit && !self.prev_on_pit {
            events.push(RaceEvent::PitEntry {
                lap,
                session_time: t,
                player_car_idx: frame.player_car_idx,
                position: pos,
            });
        } else if !on_pit && self.prev_on_pit {
            events.push(RaceEvent::PitExit {
                lap,
                session_time: t,
                player_car_idx: frame.player_car_idx,
                position: pos,
            });
        }

        // Third-party battles are tracked whatever the rig's own car is doing:
        // the field keeps racing while the rig sits in the pit stall. Pairs the
        // rig is part of are reported by the player-threat path below (stamped
        // with the same identity), so they are not emitted twice.
        {
            let lap_t = self.lap_timer.best_estimate();
            let lap_time_known = self.lap_timer.has_completed_lap();
            for transition in self.battle_pairs.update(frame, lap_t, lap_time_known) {
                if !transition.identity().battle_involves_publisher {
                    events.push(self.pair_transition_event(transition, frame));
                }
            }
        }

        if pos == 0 || lap < 1 {
            self.prev_lap = Some(lap);
            self.prev_on_pit = on_pit;
            return events;
        }

        self.lap_timer.update(lap, t);
        let lap_t = self.lap_timer.best_estimate();
        let lap_time_known = self.lap_timer.has_completed_lap();

        let synth_flags = synthesize_flags(lap, ldp);
        let is_clean = (synth_flags & (YELLOW_WAVE | CAUTION)) == 0 && !on_pit;
        if !is_clean {
            self.dirty_laps.insert(lap);
        }
        if session_flags & (YELLOW_WAVE | CAUTION) != 0 {
            self.dirty_laps.insert(lap);
        }

        if on_pit {
            *self.lap_pit_frames.entry(lap).or_insert(0) += 1;
        }

        let cars_ahead = find_cars_ahead(frame, lap_t, SCAN_FIELD_POSITIONS);
        let cars_behind = find_cars_behind(frame, lap_t, SCAN_FIELD_POSITIONS);
        for &(car_idx, gap_s) in &cars_ahead {
            self.sampler.update(lap, ldp, gap_s, car_idx, is_clean);
        }
        for &(car_idx, gap_s) in &cars_behind {
            self.sampler_behind
                .update(lap, ldp, gap_s, car_idx, is_clean);
        }
        let nearest_ahead = cars_ahead.first().copied();
        let nearest_behind = cars_behind.first().copied();

        match nearest_ahead {
            Some((car_idx, gap)) if gap < CLOSE_APPROACH_THRESH_S => {
                self.consecutive_close += 1;
                if self.consecutive_close >= CLOSE_APPROACH_MIN_FRAMES
                    && (t - self.last_close_t) > 30.0
                    && Some(car_idx) != self.tracking_car
                {
                    self.tracking_car = Some(car_idx);
                    self.last_close_t = t;
                    self.engagement_start_t = Some(t);
                    self.engaged_cars.insert(car_idx);
                    let car_race_position = frame
                        .car_idx_position
                        .get(car_idx as usize)
                        .copied()
                        .unwrap_or(0);
                    let (prior_skirmishes, prior_attack_time_s) =
                        self.opponent_history(frame.player_car_idx, car_idx);
                    events.push(RaceEvent::BattleEngaged {
                        lap,
                        session_time: t,
                        player_car_idx: frame.player_car_idx,
                        opponent_car_idx: car_idx,
                        gap_s: gap,
                        car_race_position,
                        prior_skirmishes,
                        prior_attack_time_s,
                        engagement_started_at_session_time_s: t,
                        battle: self.player_battle_identity(
                            frame,
                            car_idx,
                            BattlePhase::Engaged,
                            lap_t,
                            lap_time_known,
                        ),
                    });
                }
            }
            other => {
                self.consecutive_close = 0;
                let current_idx = other.map(|(c, _)| c);
                if current_idx != self.tracking_car {
                    if let Some(prev_car) = self.tracking_car {
                        if self.engaged_cars.remove(&prev_car) {
                            let final_gap_sec = other.and_then(|(_, g)| sanitize_gap(g));
                            let car_race_position = frame
                                .car_idx_position
                                .get(prev_car as usize)
                                .copied()
                                .unwrap_or(0);
                            let engagement_started_at_session_time_s =
                                self.engagement_start_t.unwrap_or(t);
                            events.push(RaceEvent::BattleBroken {
                                lap,
                                session_time: t,
                                player_car_idx: frame.player_car_idx,
                                opponent_car_idx: prev_car,
                                final_gap_sec,
                                car_race_position,
                                engagement_started_at_session_time_s,
                                battle: self.player_battle_identity(
                                    frame,
                                    prev_car,
                                    BattlePhase::Broken,
                                    lap_t,
                                    lap_time_known,
                                ),
                            });
                        }
                    }
                    self.tracking_car = None;
                    self.engagement_start_t = None;
                }
            }
        }

        match nearest_behind {
            Some((car_idx, gap)) if gap < CLOSE_APPROACH_THRESH_S => {
                self.consecutive_close_beh += 1;
                if self.consecutive_close_beh >= CLOSE_APPROACH_MIN_FRAMES
                    && (t - self.last_close_beh_t) > 30.0
                    && Some(car_idx) != self.tracking_car_beh
                {
                    self.tracking_car_beh = Some(car_idx);
                    self.last_close_beh_t = t;
                    self.engagement_start_beh_t = Some(t);
                    self.engaged_cars_beh.insert(car_idx);
                    let car_race_position = frame
                        .car_idx_position
                        .get(car_idx as usize)
                        .copied()
                        .unwrap_or(0);
                    let (prior_skirmishes, prior_attack_time_s) =
                        self.opponent_history(frame.player_car_idx, car_idx);
                    events.push(RaceEvent::BattleEngaged {
                        lap,
                        session_time: t,
                        player_car_idx: frame.player_car_idx,
                        opponent_car_idx: car_idx,
                        gap_s: gap,
                        car_race_position,
                        prior_skirmishes,
                        prior_attack_time_s,
                        engagement_started_at_session_time_s: t,
                        battle: self.player_battle_identity(
                            frame,
                            car_idx,
                            BattlePhase::Engaged,
                            lap_t,
                            lap_time_known,
                        ),
                    });
                }
            }
            other => {
                self.consecutive_close_beh = 0;
                let current_idx = other.map(|(c, _)| c);
                if current_idx != self.tracking_car_beh {
                    if let Some(prev_car) = self.tracking_car_beh {
                        if self.engaged_cars_beh.remove(&prev_car) {
                            let final_gap_sec = other.and_then(|(_, g)| sanitize_gap(g));
                            let car_race_position = frame
                                .car_idx_position
                                .get(prev_car as usize)
                                .copied()
                                .unwrap_or(0);
                            let engagement_started_at_session_time_s =
                                self.engagement_start_beh_t.unwrap_or(t);
                            events.push(RaceEvent::BattleBroken {
                                lap,
                                session_time: t,
                                player_car_idx: frame.player_car_idx,
                                opponent_car_idx: prev_car,
                                final_gap_sec,
                                car_race_position,
                                engagement_started_at_session_time_s,
                                battle: self.player_battle_identity(
                                    frame,
                                    prev_car,
                                    BattlePhase::Broken,
                                    lap_t,
                                    lap_time_known,
                                ),
                            });
                        }
                    }
                    self.tracking_car_beh = None;
                    self.engagement_start_beh_t = None;
                }
            }
        }

        if let Some(prev_lap) = self.prev_lap {
            if lap != prev_lap {
                let done_lap = prev_lap;
                let pit_frames = self.lap_pit_frames.get(&done_lap).copied().unwrap_or(0);
                if pit_frames >= PIT_LAP_FRAME_THRESH {
                    self.pit_laps.insert(done_lap);
                }
                let end_pos = self.prev_position.unwrap_or(pos);
                self.lap_end_positions.insert(done_lap, end_pos);
                let lap_time_s = valid_lap_time(frame.lap_last_lap_time)
                    .or_else(|| self.lap_timer.completed(done_lap).and_then(valid_lap_time));
                if let Some(lt) = lap_time_s {
                    self.best_lap_time_s = Some(match self.best_lap_time_s {
                        Some(best) if best <= lt => best,
                        _ => lt,
                    });
                }
                events.push(RaceEvent::LapCompleted {
                    lap: done_lap,
                    session_time: t,
                    player_car_idx: frame.player_car_idx,
                    lap_time_s,
                    best_lap_time_s: self.best_lap_time_s,
                    position: end_pos,
                    pit_frames,
                });

                if let Some(&prev_pos) = self.lap_end_positions.get(&done_lap.wrapping_sub(1)) {
                    let delta = prev_pos as i16 - end_pos as i16;
                    if delta > 0 && !self.pit_laps.contains(&done_lap) {
                        let overtaken_car_idx =
                            self.find_overtaken_car(done_lap.wrapping_sub(1), prev_pos, end_pos);
                        if end_pos == 1 {
                            events.push(RaceEvent::OvertakeForLead {
                                lap: done_lap,
                                session_time: t,
                                car_idx: frame.player_car_idx,
                                overtaken_car_idx,
                                position_from: prev_pos,
                                positions_gained: delta as u8,
                            });
                        } else {
                            events.push(RaceEvent::Overtake {
                                lap: done_lap,
                                session_time: t,
                                car_idx: frame.player_car_idx,
                                overtaken_car_idx,
                                position_from: prev_pos,
                                position_to: end_pos,
                                positions_gained: delta as u8,
                            });
                        }
                    }
                }

                self.regression.ingest(&self.sampler, done_lap);
                let per_bucket = self.regression.per_bucket_slopes(MIN_PUSH_READINGS);
                let car_medians = self.regression.per_car_median_slopes(MIN_PUSH_READINGS);
                let fwd = classify(
                    &car_medians,
                    &per_bucket,
                    self.anchor_count,
                    self.prev_slope,
                    self.pit_laps.contains(&done_lap),
                );
                if fwd.state != self.engine_state {
                    if matches!(fwd.state, BattleState::Push | BattleState::AttackSetup) {
                        if let (Some(car_idx), Some(si)) = (fwd.threat_car, fwd.slope_info.clone())
                        {
                            let (prior_skirmishes, prior_attack_time_s) =
                                self.opponent_history(frame.player_car_idx, car_idx);
                            events.push(RaceEvent::BattleClosing {
                                lap: done_lap,
                                session_time: t,
                                player_car_idx: frame.player_car_idx,
                                opponent_car_idx: car_idx,
                                car_race_position: frame
                                    .car_idx_position
                                    .get(car_idx as usize)
                                    .copied()
                                    .unwrap_or(0),
                                closing_rate_sec_per_lap: si.median_slope.abs(),
                                slope_info: si,
                                prior_skirmishes,
                                prior_attack_time_s,
                                battle: self.player_battle_identity(
                                    frame,
                                    car_idx,
                                    BattlePhase::Closing,
                                    lap_t,
                                    lap_time_known,
                                ),
                            });
                        }
                    }
                    self.engine_state = fwd.state;
                }
                if let Some(si) = &fwd.slope_info {
                    self.prev_slope = Some(si.median_slope);
                }
                if let Some(car_idx) = fwd.threat_car {
                    self.car_registry.update_opponent_history(
                        frame.player_car_idx,
                        car_idx,
                        &fwd.state,
                        self.pit_laps.contains(&done_lap),
                        done_lap,
                    );
                }

                self.regression_behind
                    .ingest(&self.sampler_behind, done_lap);
                let per_bucket_beh = self.regression_behind.per_bucket_slopes(MIN_PUSH_READINGS);
                let car_medians_beh = self
                    .regression_behind
                    .per_car_median_slopes(MIN_PUSH_READINGS);
                let def = classify(
                    &car_medians_beh,
                    &per_bucket_beh,
                    self.anchor_count,
                    self.prev_slope_beh,
                    self.pit_laps.contains(&done_lap),
                );
                if def.state != self.defensive_state {
                    if matches!(def.state, BattleState::Push | BattleState::AttackSetup) {
                        if let (Some(car_idx), Some(si)) = (def.threat_car, def.slope_info.clone())
                        {
                            let (prior_skirmishes, prior_attack_time_s) =
                                self.opponent_history(frame.player_car_idx, car_idx);
                            events.push(RaceEvent::BattleClosing {
                                lap: done_lap,
                                session_time: t,
                                player_car_idx: frame.player_car_idx,
                                opponent_car_idx: car_idx,
                                car_race_position: frame
                                    .car_idx_position
                                    .get(car_idx as usize)
                                    .copied()
                                    .unwrap_or(0),
                                closing_rate_sec_per_lap: si.median_slope.abs(),
                                slope_info: si,
                                prior_skirmishes,
                                prior_attack_time_s,
                                battle: self.player_battle_identity(
                                    frame,
                                    car_idx,
                                    BattlePhase::Closing,
                                    lap_t,
                                    lap_time_known,
                                ),
                            });
                        }
                    }
                    self.defensive_state = def.state;
                }
                if let Some(si) = &def.slope_info {
                    self.prev_slope_beh = Some(si.median_slope);
                }
                if let Some(car_idx) = def.threat_car {
                    self.car_registry.update_opponent_history(
                        frame.player_car_idx,
                        car_idx,
                        &def.state,
                        self.pit_laps.contains(&done_lap),
                        done_lap,
                    );
                    if let Some(history) = self
                        .car_registry
                        .find_opponent_history_mut(frame.player_car_idx, car_idx)
                    {
                        history.last_state_defensive = def.state;
                    }
                }

                let clean_lap =
                    !self.pit_laps.contains(&done_lap) && !self.dirty_laps.contains(&done_lap);
                events.extend(self.micro_sector.on_lap_end(done_lap, t, clean_lap));
                if let Some(event) = self.tire_degradation.on_lap_crossing(
                    done_lap,
                    t,
                    self.pit_laps.contains(&done_lap),
                ) {
                    if self.tire_degradation.has_valid_data() {
                        self.logged_tire_suppressed = false;
                        events.push(event);
                    } else if !self.logged_tire_suppressed {
                        eprintln!("[engine] suppressing TIRE_DEGRADATION at lap {done_lap}: telemetry unavailable");
                        self.logged_tire_suppressed = true;
                    }
                }
                if let Some(event) = self.fuel_projection.on_lap_crossing(
                    done_lap,
                    t,
                    frame.fuel_level,
                    self.pit_laps.contains(&done_lap),
                    self.dirty_laps.contains(&done_lap),
                ) {
                    if self.fuel_projection.has_valid_data() {
                        self.logged_fuel_suppressed = false;
                        events.push(event);
                    } else if !self.logged_fuel_suppressed {
                        eprintln!("[engine] suppressing FUEL_PROJECTION at lap {done_lap}: telemetry unavailable");
                        self.logged_fuel_suppressed = true;
                    }
                }
                events.extend(self.horizon.detect(
                    &self.car_registry,
                    &car_medians,
                    done_lap,
                    t,
                    lap_time_s.unwrap_or(90.0),
                ));
                events.extend(self.traffic_intercept.detect(
                    &self.car_registry,
                    self.anchor_count,
                    self.track_length_m,
                    done_lap,
                    t,
                ));
                events.extend(self.incident_cluster.update(
                    &self.car_registry,
                    self.anchor_count,
                    done_lap,
                    t,
                    self.prev_session_flags & CAUTION != 0,
                ));
                events.extend(self.compression_zone.detect(
                    &self.car_registry,
                    self.anchor_count,
                    self.track_length_m,
                    done_lap,
                    t,
                ));

                let vulnerability_event = if let Some(attacker_idx) = def.threat_car {
                    self.vulnerability.tick(
                        self.tire_degradation.latest_max_slope(),
                        def.slope_info
                            .as_ref()
                            .map(|s| s.median_slope)
                            .unwrap_or(0.0),
                        nearest_behind.map(|(_, gap)| gap).unwrap_or(99.0),
                        self.fuel_projection.laps_remaining().unwrap_or(99.0),
                        frame.player_car_idx,
                        attacker_idx,
                        done_lap,
                        t,
                        self.pit_laps.contains(&done_lap),
                    )
                } else {
                    self.vulnerability.tick(
                        0.0,
                        0.0,
                        99.0,
                        self.fuel_projection.laps_remaining().unwrap_or(99.0),
                        frame.player_car_idx,
                        0,
                        done_lap,
                        t,
                        self.pit_laps.contains(&done_lap),
                    )
                };
                if let Some(event) = vulnerability_event {
                    events.push(event);
                }
            }
        }

        self.prev_lap = Some(lap);
        self.prev_on_pit = on_pit;
        self.prev_position = Some(pos);
        events
    }

    /// Identity of the pair tracker's battle between the rig and `opponent`,
    /// so the legacy player-threat events correlate with the pair events.
    fn player_battle_identity(
        &self,
        frame: &TelemetryFrame,
        opponent: u8,
        phase: BattlePhase,
        lap_t: f32,
        lap_time_known: bool,
    ) -> Option<BattleIdentity> {
        self.battle_pairs.identity_for(
            frame.player_car_idx,
            opponent,
            phase,
            FrameContext::new(frame, lap_t, lap_time_known),
        )
    }

    /// Express a pair-tracker transition as a battle event. The legacy
    /// `player_car_idx` / `opponent_car_idx` slots carry the behind (attacking)
    /// and ahead (defending) cars so an older consumer still derives
    /// leader/follower correctly; `lap` is the rig's lap, as on every event.
    fn pair_transition_event(
        &self,
        transition: PairTransition,
        frame: &TelemetryFrame,
    ) -> RaceEvent {
        let lap = frame.lap;
        let t = frame.session_time;
        let id = transition.identity().clone();
        let (behind, ahead) = (id.behind_car_idx, id.ahead_car_idx);
        let car_race_position = frame
            .car_idx_position
            .get(ahead as usize)
            .copied()
            .unwrap_or(0);
        let (prior_skirmishes, prior_attack_time_s) = self.opponent_history(behind, ahead);
        match transition {
            PairTransition::Engaged(_) => RaceEvent::BattleEngaged {
                lap,
                session_time: t,
                player_car_idx: behind,
                opponent_car_idx: ahead,
                gap_s: id.current_gap_s.unwrap_or(f32::NAN),
                car_race_position,
                prior_skirmishes,
                prior_attack_time_s,
                engagement_started_at_session_time_s: id.engaged_at,
                battle: Some(id),
            },
            PairTransition::Closing(_) => {
                let rate = id.closing_rate_s_per_lap.unwrap_or(0.0);
                let behind_ldp = frame
                    .car_idx_lap_dist_pct
                    .get(behind as usize)
                    .copied()
                    .unwrap_or(0.0)
                    .max(0.0);
                let samples = self.battle_pairs.sample_count(behind, ahead) as usize;
                RaceEvent::BattleClosing {
                    lap,
                    session_time: t,
                    player_car_idx: behind,
                    opponent_car_idx: ahead,
                    car_race_position,
                    closing_rate_sec_per_lap: rate.abs(),
                    slope_info: SlopeInfo {
                        median_slope: -rate,
                        anchors_qualifying: samples,
                        anchors_agreeing: samples,
                        hotspot_lap_dist_pct: behind_ldp,
                    },
                    prior_skirmishes,
                    prior_attack_time_s,
                    battle: Some(id),
                }
            }
            PairTransition::Broken(_) => RaceEvent::BattleBroken {
                lap,
                session_time: t,
                player_car_idx: behind,
                opponent_car_idx: ahead,
                final_gap_sec: id.current_gap_s.and_then(sanitize_gap),
                car_race_position,
                engagement_started_at_session_time_s: id.engaged_at,
                battle: Some(id),
            },
        }
    }

    fn opponent_history(&self, player_idx: u8, opponent_idx: u8) -> (u32, f32) {
        self.car_registry
            .get(player_idx)
            .and_then(|car| {
                car.opponent_history
                    .iter()
                    .find(|history| history.car_idx == opponent_idx)
            })
            .map(|history| (history.skirmish_count, history.time_in_attack_s))
            .unwrap_or((0, 0.0))
    }

    /// Record the current car positions from the frame at a lap boundary.
    fn record_lap_car_positions(&mut self, lap: u8, frame: &TelemetryFrame) {
        let mut positions = vec![u8::MAX; 64]; // Initialize with sentinel values
        for (car_idx, &pos) in frame.car_idx_position.iter().enumerate() {
            if pos > 0 && (car_idx as u8) < 64 {
                let position_idx = (pos as usize).saturating_sub(1);
                if position_idx < positions.len() {
                    positions[position_idx] = car_idx as u8;
                }
            }
        }
        self.lap_car_positions.insert(lap, positions);
    }

    /// Find which car was likely overtaken based on position history.
    /// Returns Some(car_idx) if we can confidently identify the overtaken car,
    /// or None if we cannot determine it reliably.
    fn find_overtaken_car(&self, prev_lap: u8, _position_from: u8, position_to: u8) -> Option<u8> {
        // Look up the previous lap's position mapping
        let prev_positions = self.lap_car_positions.get(&prev_lap)?;

        // Check if there's a car recorded at the position_to (where we moved to)
        let pos_idx = (position_to as usize).saturating_sub(1);
        if pos_idx < prev_positions.len() {
            let car_at_prev_pos = prev_positions[pos_idx];
            // Only return valid car indices (not our sentinel)
            if car_at_prev_pos != u8::MAX {
                return Some(car_at_prev_pos);
            }
        }
        None
    }
}

/// Gap readings carried between `DRIVER_MATERIAL` emissions so each one can
/// state which way its gaps are moving.
struct MaterialGaps {
    ahead: Option<(u8, f32)>,
    behind: Option<(u8, f32)>,
}

/// Direction of a gap between two cadence samples. Only comparable while the
/// neighbouring car is the same one.
fn gap_trend(previous: Option<(u8, f32)>, current: Option<(u8, f32)>) -> GapTrend {
    let (Some((prev_idx, prev_gap)), Some((idx, gap))) = (previous, current) else {
        return GapTrend::Unknown;
    };
    if prev_idx != idx {
        return GapTrend::Unknown;
    }
    let delta = gap - prev_gap;
    if delta <= -GAP_TREND_DELTA_S {
        GapTrend::Closing
    } else if delta >= GAP_TREND_DELTA_S {
        GapTrend::Opening
    } else {
        GapTrend::Stable
    }
}

fn synthesize_flags(lap: u8, ldp: f32) -> u32 {
    for &(ylap, p0, p1) in YELLOW_ZONES {
        if lap == ylap && ldp >= p0 && ldp <= p1 {
            return YELLOW_WAVE;
        }
    }
    0
}

fn valid_lap_time(v: f32) -> Option<f32> {
    // Guard against zero/negative/NaN artifacts observed in replay and reset edges.
    (v.is_finite() && v > 0.1).then_some(v)
}

/// Sanitize a gap value: return `None` if it is a sentinel (NaN, Infinite,
/// or >= `MAX_VALID_GAP_S`). Real battle gaps are bounded by `MAX_BATTLE_GAP_S`
/// (~5 s), so anything >= 100 s is definitively a missing-data sentinel.
fn sanitize_gap(v: f32) -> Option<f32> {
    if v.is_nan() || v.is_infinite() || v >= MAX_VALID_GAP_S {
        None
    } else {
        Some(v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame_with_gap(lap: u8, t: f32, session_state: i32, opponent_ldp: f32) -> TelemetryFrame {
        TelemetryFrame {
            lap,
            session_time: t,
            lap_dist_pct: 0.500,
            player_car_idx: 0,
            player_car_position: 5,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct: vec![0.500, opponent_ldp],
            car_idx_position: vec![5, 4],
            car_idx_on_pit_road: vec![false, false],
            car_idx_track_surface: vec![0, 0],
            lap_last_lap_time: 0.0,
            session_info_update: 0,
            session_tick: 0,
            session_state,
            session_num: 0,
            session_time_remain: None,
            session_laps_remain: None,
            player_incident_count: 0,
            car_idx_lap_completed: vec![lap as i32, lap as i32],
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

    fn frame_no_opponent(lap: u8, t: f32) -> TelemetryFrame {
        TelemetryFrame {
            lap,
            session_time: t,
            lap_dist_pct: 0.500,
            player_car_idx: 0,
            player_car_position: 5,
            on_pit_road: false,
            session_flags: 0,
            car_idx_lap_dist_pct: vec![0.500, -1.0],
            car_idx_position: vec![5, 0],
            car_idx_on_pit_road: vec![false, false],
            car_idx_track_surface: vec![0, 0],
            lap_last_lap_time: 0.0,
            session_info_update: 0,
            session_tick: 0,
            session_state: 4,
            session_num: 0,
            session_time_remain: None,
            session_laps_remain: None,
            player_incident_count: 0,
            car_idx_lap_completed: vec![lap as i32, lap as i32],
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
    fn battle_engaged_fires_on_lap_1() {
        let mut engine = NarrativeEngine::new(10);
        let mut all_events = Vec::new();
        for i in 0..6u8 {
            all_events.extend(engine.process_frame(&frame_with_gap(1, i as f32, 4, 0.501)));
        }
        assert!(all_events.iter().any(|e| matches!(
            e,
            RaceEvent::BattleEngaged {
                lap: 1,
                opponent_car_idx: 1,
                ..
            }
        )));
    }

    #[test]
    fn battle_broken_fires_after_engaged() {
        let mut engine = NarrativeEngine::new(10);
        let mut all_events = Vec::new();
        for i in 0..6u8 {
            all_events.extend(engine.process_frame(&frame_with_gap(1, i as f32, 4, 0.501)));
        }
        assert!(all_events.iter().any(|e| matches!(
            e,
            RaceEvent::BattleEngaged {
                opponent_car_idx: 1,
                ..
            }
        )));
        let evs = engine.process_frame(&frame_no_opponent(1, 6.0));
        assert!(evs.iter().any(|e| matches!(
            e,
            RaceEvent::BattleBroken {
                opponent_car_idx: 1,
                ..
            }
        )));
    }

    /// Rig (car 0) at P5; cars 2 (P6) and 3 (P7) fight behind it with `gap`
    /// (in lap fraction) between them.
    fn frame_third_party(lap: u8, t: f32, gap: f32) -> TelemetryFrame {
        let mut f = frame_with_gap(lap, t, 4, -1.0);
        f.car_idx_position = vec![5, 0, 6, 7];
        f.car_idx_lap_dist_pct = vec![0.500, -1.0, 0.300, 0.300 - gap];
        f.car_idx_on_pit_road = vec![false; 4];
        f.car_idx_track_surface = vec![0; 4];
        f.car_idx_lap_completed = vec![lap as i32; 4];
        f
    }

    #[test]
    fn third_party_battle_is_emitted_with_identity_and_correlated_lifecycle() {
        let mut engine = NarrativeEngine::new(10);
        let mut all_events = Vec::new();
        for i in 0..6u32 {
            all_events.extend(engine.process_frame(&frame_third_party(1, i as f32 / 60.0, 0.001)));
        }
        let engaged: Vec<_> = all_events
            .iter()
            .filter(|e| matches!(e, RaceEvent::BattleEngaged { .. }))
            .collect();
        assert_eq!(engaged.len(), 1, "{all_events:?}");
        let RaceEvent::BattleEngaged {
            player_car_idx,
            opponent_car_idx,
            battle: Some(id),
            ..
        } = engaged[0]
        else {
            panic!("expected BATTLE_ENGAGED with identity");
        };
        // Legacy slots carry behind/ahead so leader/follower still derive.
        assert_eq!((*player_car_idx, *opponent_car_idx), (3, 2));
        assert_eq!((id.ahead_car_idx, id.behind_car_idx), (2, 3));
        assert_eq!(id.battle_phase, BattlePhase::Engaged);
        assert!(!id.battle_involves_publisher);
        let battle_id = id.battle_id.clone();

        // The fight breaks: BROKEN carries the same id.
        let mut broken = None;
        for i in 0..40u32 {
            for e in engine.process_frame(&frame_third_party(1, 1.0 + i as f32 / 60.0, 0.01)) {
                if matches!(e, RaceEvent::BattleBroken { .. }) {
                    broken = Some(e);
                }
            }
        }
        let Some(RaceEvent::BattleBroken {
            battle: Some(id),
            player_car_idx: 3,
            opponent_car_idx: 2,
            ..
        }) = broken
        else {
            panic!("expected BATTLE_BROKEN with identity: {broken:?}");
        };
        assert_eq!(id.battle_id, battle_id);
        assert_eq!(id.battle_phase, BattlePhase::Broken);
    }

    #[test]
    fn third_party_battles_are_tracked_while_the_rig_is_in_the_pit_stall() {
        let mut engine = NarrativeEngine::new(10);
        let mut all_events = Vec::new();
        for i in 0..6u32 {
            let mut f = frame_third_party(0, i as f32 / 60.0, 0.001);
            f.player_car_position = 0;
            f.car_idx_position[0] = 0;
            all_events.extend(engine.process_frame(&f));
        }
        assert!(all_events.iter().any(|e| matches!(
            e,
            RaceEvent::BattleEngaged {
                battle: Some(_),
                ..
            }
        )));
    }

    #[test]
    fn legacy_player_battle_is_stamped_with_the_pair_identity() {
        let mut engine = NarrativeEngine::new(10);
        let mut all_events = Vec::new();
        for i in 0..6u8 {
            all_events.extend(engine.process_frame(&frame_with_gap(1, i as f32, 4, 0.501)));
        }
        let engaged: Vec<_> = all_events
            .iter()
            .filter(|e| matches!(e, RaceEvent::BattleEngaged { .. }))
            .collect();
        // Exactly one ENGAGED: the pair tracker does not duplicate the rig's own fight.
        assert_eq!(engaged.len(), 1, "{engaged:?}");
        let RaceEvent::BattleEngaged {
            player_car_idx: 0,
            opponent_car_idx: 1,
            battle: Some(id),
            ..
        } = engaged[0]
        else {
            panic!(
                "legacy event should carry the pair identity: {:?}",
                engaged[0]
            );
        };
        assert!(id.battle_involves_publisher);
        assert_eq!((id.ahead_car_idx, id.behind_car_idx), (1, 0));
    }

    #[test]
    fn race_green_fires_on_session_state_transition() {
        let mut engine = NarrativeEngine::new(10);
        let evs1 = engine.process_frame(&frame_with_gap(0, 0.0, 3, 0.501));
        assert!(!evs1
            .iter()
            .any(|e| matches!(e, RaceEvent::RaceGreen { .. })));
        let evs2 = engine.process_frame(&frame_with_gap(1, 1.0, 4, 0.501));
        assert!(evs2
            .iter()
            .any(|e| matches!(e, RaceEvent::RaceGreen { .. })));
        let evs3 = engine.process_frame(&frame_with_gap(1, 2.0, 4, 0.501));
        assert!(!evs3
            .iter()
            .any(|e| matches!(e, RaceEvent::RaceGreen { .. })));
    }

    #[test]
    fn pit_exit_fires_for_a_car_that_is_still_unclassified() {
        // Leaving the pit stall on an out-lap: iRacing reports position 0 until
        // the car is classified, which used to swallow the whole stint's
        // PIT_ENTRY/PIT_EXIT pair.
        let mut engine = NarrativeEngine::new(10);

        let mut in_pits = frame_no_opponent(0, 10.0);
        in_pits.player_car_position = 0;
        in_pits.on_pit_road = true;
        let entry = engine.process_frame(&in_pits);
        assert!(entry
            .iter()
            .any(|e| matches!(e, RaceEvent::PitEntry { position: 0, .. })));

        let mut leaving = frame_no_opponent(0, 40.0);
        leaving.player_car_position = 0;
        leaving.on_pit_road = false;
        let exit = engine.process_frame(&leaving);
        assert!(exit
            .iter()
            .any(|e| matches!(e, RaceEvent::PitExit { position: 0, .. })));
    }

    #[test]
    fn race_checkered_fires_on_the_flag_bit_without_a_checkered_session_state() {
        let mut engine = NarrativeEngine::new(10);
        let _ = engine.process_frame(&frame_with_gap(1, 0.0, 4, 0.501));

        let mut flagged = frame_with_gap(2, 10.0, 4, 0.501);
        flagged.session_flags = CHECKERED;
        let evs = engine.process_frame(&flagged);
        assert!(evs
            .iter()
            .any(|e| matches!(e, RaceEvent::RaceCheckered { .. })));

        // Once per session: the flag stays set for the rest of the cool-down.
        let mut still_flagged = frame_with_gap(2, 11.0, 5, 0.501);
        still_flagged.session_flags = CHECKERED;
        let repeat = engine.process_frame(&still_flagged);
        assert!(!repeat
            .iter()
            .any(|e| matches!(e, RaceEvent::RaceCheckered { .. })));
    }

    #[test]
    fn race_checkered_fires_when_the_state_jumps_straight_to_cooldown() {
        let mut engine = NarrativeEngine::new(10);
        let _ = engine.process_frame(&frame_with_gap(1, 0.0, 4, 0.501));
        let evs = engine.process_frame(&frame_with_gap(2, 10.0, 6, 0.501));
        assert!(evs
            .iter()
            .any(|e| matches!(e, RaceEvent::RaceCheckered { .. })));
    }

    #[test]
    fn driver_material_names_the_rigs_own_car_and_its_neighbours() {
        let mut engine = NarrativeEngine::new(10);
        let mut frame = frame_with_gap(3, 300.0, 4, 0.505);
        frame.speed = 61.5;
        frame.fuel_level = 42.25;
        frame.player_incident_count = 4;
        for i in 0..5u8 {
            let mut f = frame.clone();
            f.session_time = 300.0 + i as f32;
            let _ = engine.process_frame(&f);
        }

        let material = engine
            .driver_material(&frame, 25.0)
            .expect("material for a car on track");
        let RaceEvent::DriverMaterial {
            player_car_idx,
            position,
            car_ahead_idx,
            gap_ahead_s,
            speed_mps,
            fuel_level_l,
            incident_count,
            interval_s,
            ..
        } = material
        else {
            panic!("expected DRIVER_MATERIAL");
        };
        assert_eq!(player_car_idx, 0);
        assert_eq!(position, 5);
        assert_eq!(car_ahead_idx, Some(1));
        assert!(gap_ahead_s.is_some_and(|g| g > 0.0));
        assert_eq!(speed_mps, 61.5);
        assert_eq!(fuel_level_l, 42.25);
        assert_eq!(incident_count, 4);
        assert_eq!(interval_s, 25.0);
    }

    #[test]
    fn driver_material_is_emitted_even_with_an_empty_field() {
        let mut engine = NarrativeEngine::new(10);
        let material = engine
            .driver_material(&frame_no_opponent(1, 60.0), 20.0)
            .expect("a driver alone in clean air is exactly the case this exists for");
        assert!(matches!(
            material,
            RaceEvent::DriverMaterial {
                car_ahead_idx: None,
                gap_ahead_s: None,
                car_behind_idx: None,
                gap_ahead_trend: GapTrend::Unknown,
                gap_behind_trend: GapTrend::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn driver_material_is_suppressed_in_the_pit_box() {
        let mut engine = NarrativeEngine::new(10);
        let mut in_pits = frame_no_opponent(1, 60.0);
        in_pits.on_pit_road = true;
        assert!(engine.driver_material(&in_pits, 25.0).is_none());
    }

    #[test]
    fn driver_material_is_suppressed_when_the_car_is_not_in_the_world() {
        let mut engine = NarrativeEngine::new(10);
        let mut not_in_world = frame_no_opponent(1, 60.0);
        not_in_world.car_idx_track_surface[0] = -1;
        assert!(engine.driver_material(&not_in_world, 25.0).is_none());
    }

    #[test]
    fn driver_material_reports_a_closing_gap_between_cadence_ticks() {
        let mut engine = NarrativeEngine::new(10);
        let far = frame_with_gap(3, 300.0, 4, 0.508);
        let near = frame_with_gap(3, 325.0, 4, 0.502);

        let first = engine
            .driver_material(&far, 25.0)
            .expect("material for a car on track");
        assert!(matches!(
            first,
            RaceEvent::DriverMaterial {
                gap_ahead_trend: GapTrend::Unknown,
                ..
            }
        ));

        let second = engine
            .driver_material(&near, 25.0)
            .expect("material for a car on track");
        let RaceEvent::DriverMaterial {
            gap_ahead_trend,
            effort,
            ..
        } = second
        else {
            panic!("expected DRIVER_MATERIAL");
        };
        assert_eq!(gap_ahead_trend, GapTrend::Closing);
        assert_eq!(effort, DriverEffort::Pushing);
    }

    #[test]
    fn a_pit_stop_breaks_the_gap_trend_rather_than_comparing_across_it() {
        let mut engine = NarrativeEngine::new(10);
        let on_track = frame_with_gap(3, 300.0, 4, 0.508);
        let mut in_pits = on_track.clone();
        in_pits.on_pit_road = true;

        let _ = engine.driver_material(&on_track, 25.0);
        assert!(engine.driver_material(&in_pits, 25.0).is_none());
        let after = engine
            .driver_material(&frame_with_gap(3, 350.0, 4, 0.505), 25.0)
            .expect("material for a car back on track");
        assert!(matches!(
            after,
            RaceEvent::DriverMaterial {
                gap_ahead_trend: GapTrend::Unknown,
                ..
            }
        ));
    }

    #[test]
    fn driver_material_cadence_fires_once_per_window_and_never_in_the_pits() {
        use crate::lifecycle::IntervalScheduler;
        use std::time::{Duration, Instant};

        let mut engine = NarrativeEngine::new(10);
        let mut sched = IntervalScheduler::new(25_000);
        let t0 = Instant::now();
        let on_track = frame_with_gap(3, 300.0, 4, 0.508);
        let mut in_pits = on_track.clone();
        in_pits.on_pit_road = true;

        let emit = |sched: &mut IntervalScheduler,
                    engine: &mut NarrativeEngine,
                    now: Instant,
                    frame: &TelemetryFrame| {
            sched
                .due_elapsed(now)
                .and_then(|elapsed| engine.driver_material(frame, elapsed.as_secs_f32()))
        };

        // Cadence tick zero only arms the timer.
        assert!(emit(&mut sched, &mut engine, t0, &on_track).is_none());
        // Nothing inside the first cadence window.
        assert!(emit(
            &mut sched,
            &mut engine,
            t0 + Duration::from_secs(10),
            &on_track
        )
        .is_none());
        assert!(emit(
            &mut sched,
            &mut engine,
            t0 + Duration::from_secs(24),
            &on_track
        )
        .is_none());
        // Fires on schedule.
        assert!(emit(
            &mut sched,
            &mut engine,
            t0 + Duration::from_secs(25),
            &on_track
        )
        .is_some());
        // ...and does not fire twice in the same window.
        assert!(emit(
            &mut sched,
            &mut engine,
            t0 + Duration::from_secs(26),
            &on_track
        )
        .is_none());
        // A due tick with the car in the box publishes nothing.
        assert!(emit(
            &mut sched,
            &mut engine,
            t0 + Duration::from_secs(50),
            &in_pits
        )
        .is_none());
    }

    #[test]
    fn race_green_at_connect_is_marked_synthetic() {
        // A publisher joining a session already racing must not claim it saw a
        // green flag fall: the capture that motivated this had one per rig in a
        // Practice session.
        let mut engine = NarrativeEngine::new(10);
        let evs = engine.process_frame(&frame_with_gap(1, 0.0, 4, 0.501));
        let green = evs
            .iter()
            .find(|e| matches!(e, RaceEvent::RaceGreen { .. }))
            .expect("first sample of a racing session");
        assert!(matches!(
            green,
            RaceEvent::RaceGreen {
                synthetic: true,
                origin: LifecycleOrigin::ConnectSnapshot,
                ..
            }
        ));
    }

    #[test]
    fn an_observed_green_flag_is_not_marked_synthetic() {
        let mut engine = NarrativeEngine::new(10);
        let _ = engine.process_frame(&frame_with_gap(1, 0.0, 3, 0.501));
        let evs = engine.process_frame(&frame_with_gap(1, 1.0, 4, 0.501));
        let green = evs
            .iter()
            .find(|e| matches!(e, RaceEvent::RaceGreen { .. }))
            .expect("green on the observed transition");
        assert!(matches!(
            green,
            RaceEvent::RaceGreen {
                synthetic: false,
                origin: LifecycleOrigin::SessionStateTransition,
                ..
            }
        ));
    }

    #[test]
    fn lap_completed_uses_iracing_lap_time_when_timer_value_is_invalid() {
        let mut engine = NarrativeEngine::new(10);

        let mut f1 = frame_no_opponent(1, 100.0);
        f1.session_state = 4;
        f1.lap_last_lap_time = 0.0;
        let _ = engine.process_frame(&f1);

        let mut f2 = frame_no_opponent(2, 200.0);
        f2.session_state = 4;
        f2.lap_last_lap_time = 152.1;
        let _ = engine.process_frame(&f2);

        let mut f3 = frame_no_opponent(3, 10.0);
        f3.session_state = 4;
        f3.lap_last_lap_time = 153.9;
        let evs = engine.process_frame(&f3);

        let lap = evs.iter().find_map(|e| {
            if let RaceEvent::LapCompleted {
                lap,
                lap_time_s,
                best_lap_time_s,
                ..
            } = e
            {
                Some((*lap, *lap_time_s, *best_lap_time_s))
            } else {
                None
            }
        });

        let Some((done_lap, lap_time_s, best_lap_time_s)) = lap else {
            panic!("expected LAP_COMPLETED event");
        };

        assert_eq!(done_lap, 2);
        assert_eq!(lap_time_s, Some(153.9));
        assert_eq!(best_lap_time_s, Some(152.1));
    }

    #[test]
    fn tire_degradation_suppressed_until_valid() {
        let mut engine = NarrativeEngine::new(10);

        let mut f1 = frame_no_opponent(1, 60.0);
        f1.session_state = 4;
        let _ = engine.process_frame(&f1);

        let mut f2 = frame_no_opponent(2, 120.0);
        f2.session_state = 4;
        let _ = engine.process_frame(&f2);

        let mut f3 = frame_no_opponent(3, 180.0);
        f3.session_state = 4;
        let evs3 = engine.process_frame(&f3);
        assert!(!evs3
            .iter()
            .any(|e| matches!(e, RaceEvent::TireDegradation { .. })));

        let mut f4 = frame_no_opponent(4, 240.0);
        f4.session_state = 4;
        f4.lf_temp_m = 80.0;
        f4.rf_temp_m = 80.0;
        f4.lr_temp_m = 80.0;
        f4.rr_temp_m = 80.0;
        let _ = engine.process_frame(&f4);

        let mut f5 = frame_no_opponent(5, 300.0);
        f5.session_state = 4;
        f5.lf_temp_m = 81.0;
        f5.rf_temp_m = 81.0;
        f5.lr_temp_m = 81.0;
        f5.rr_temp_m = 81.0;
        let _ = engine.process_frame(&f5);

        let mut f6 = frame_no_opponent(6, 360.0);
        f6.session_state = 4;
        f6.lf_temp_m = 82.0;
        f6.rf_temp_m = 82.0;
        f6.lr_temp_m = 82.0;
        f6.rr_temp_m = 82.0;
        let evs6 = engine.process_frame(&f6);
        assert!(evs6
            .iter()
            .any(|e| matches!(e, RaceEvent::TireDegradation { .. })));
    }

    #[test]
    fn fuel_projection_suppressed_until_valid() {
        let mut engine = NarrativeEngine::new(10);

        let mut f1 = frame_no_opponent(1, 60.0);
        f1.session_state = 4;
        let _ = engine.process_frame(&f1);

        let mut f2 = frame_no_opponent(2, 120.0);
        f2.session_state = 4;
        let _ = engine.process_frame(&f2);

        let mut f3 = frame_no_opponent(3, 180.0);
        f3.session_state = 4;
        let evs3 = engine.process_frame(&f3);
        assert!(!evs3
            .iter()
            .any(|e| matches!(e, RaceEvent::FuelProjection { .. })));

        let mut f4 = frame_no_opponent(4, 240.0);
        f4.session_state = 4;
        f4.fuel_level = 50.0;
        let _ = engine.process_frame(&f4);

        let mut f5 = frame_no_opponent(5, 300.0);
        f5.session_state = 4;
        f5.fuel_level = 47.5;
        let evs5 = engine.process_frame(&f5);
        assert!(evs5
            .iter()
            .any(|e| matches!(e, RaceEvent::FuelProjection { .. })));
    }

    #[test]
    fn battle_engaged_includes_both_player_and_opponent() {
        let mut engine = NarrativeEngine::new(10);
        let mut all_events = Vec::new();
        for i in 0..6u8 {
            all_events.extend(engine.process_frame(&frame_with_gap(1, i as f32, 4, 0.501)));
        }

        let battle = all_events.iter().find_map(|e| {
            if let RaceEvent::BattleEngaged {
                player_car_idx,
                opponent_car_idx,
                ..
            } = e
            {
                Some((*player_car_idx, *opponent_car_idx))
            } else {
                None
            }
        });

        assert_eq!(
            battle,
            Some((0, 1)),
            "should include both player and opponent car indices"
        );
    }

    #[test]
    fn battle_broken_includes_both_cars() {
        let mut engine = NarrativeEngine::new(10);
        let mut all_events = Vec::new();
        for i in 0..6u8 {
            all_events.extend(engine.process_frame(&frame_with_gap(1, i as f32, 4, 0.501)));
        }
        let evs = engine.process_frame(&frame_no_opponent(1, 6.0));

        let battle_broken = evs.iter().find_map(|e| {
            if let RaceEvent::BattleBroken {
                player_car_idx,
                opponent_car_idx,
                ..
            } = e
            {
                Some((*player_car_idx, *opponent_car_idx))
            } else {
                None
            }
        });

        assert_eq!(
            battle_broken,
            Some((0, 1)),
            "should include both player and opponent car indices"
        );
    }
}
