// SPDX-License-Identifier: GPL-3.0-or-later

//! Publisher lifecycle events: PUBLISHER_HELLO / PUBLISHER_HEARTBEAT /
//! PUBLISHER_GOODBYE.
//!
//! [`LifecyclePublisher`] is called from the main publisher loop to generate
//! lifecycle [`RaceEvent`]s that are enqueued into the same [`PublisherTransport`]
//! as narrative events.

use std::time::{Duration, Instant};

use crate::race_event::RaceEvent;

/// `PUBLISHER_GOODBYE.reason` when the process is shutting down.
pub const GOODBYE_SHUTDOWN: &str = "shutdown";
/// `PUBLISHER_GOODBYE.reason` when iRacing moved to another sub-session and
/// this publisher instance is being re-created for it.
pub const GOODBYE_SESSION_TRANSITION: &str = "session_transition";

/// Manages publisher lifecycle events.
///
/// Create one instance at startup, call [`on_activate`] after successful
/// token acquisition, and [`on_deactivate`] on clean shutdown.
pub struct LifecyclePublisher {
    version: String,
    /// `true` until `on_activate` is called for the first time.
    /// Used by the publisher loop to detect when a HELLO is still outstanding.
    fresh: bool,
}

impl LifecyclePublisher {
    /// Create a lifecycle publisher.
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            version: version.into(),
            fresh: true,
        }
    }

    /// `true` if `on_activate` has not yet been called on this instance.
    /// The publisher loop uses this to know when to emit `PUBLISHER_HELLO`.
    pub fn is_fresh(&self) -> bool {
        self.fresh
    }

    /// Emit `PUBLISHER_HELLO` — call once after successful registration.
    pub fn on_activate(&mut self, lap: u8, session_time: f32) -> RaceEvent {
        self.fresh = false;
        RaceEvent::PublisherHello {
            lap,
            session_time,
            version: self.version.clone(),
            scope: "driver".to_owned(),
        }
    }

    /// Emit `PUBLISHER_GOODBYE` — call on clean shutdown.
    pub fn on_deactivate(&self, lap: u8, session_time: f32) -> RaceEvent {
        RaceEvent::PublisherGoodbye {
            lap,
            session_time,
            reason: GOODBYE_SHUTDOWN.to_owned(),
            previous_sub_session_id: None,
        }
    }

    /// Emit `PUBLISHER_GOODBYE` for a sub-session the publisher is leaving
    /// because iRacing advanced to another one. The caller stamps the
    /// envelope with the *live* sub-session so the consumer does not discard
    /// the one transition signal it needs; the departed id rides in the payload.
    pub fn on_session_transition(
        &self,
        lap: u8,
        session_time: f32,
        previous_sub_session_id: i64,
    ) -> RaceEvent {
        RaceEvent::PublisherGoodbye {
            lap,
            session_time,
            reason: GOODBYE_SESSION_TRANSITION.to_owned(),
            previous_sub_session_id: Some(previous_sub_session_id),
        }
    }

    /// Emit `PUBLISHER_HEARTBEAT` — call on the heartbeat timer while connected.
    pub fn heartbeat(&self, lap: u8, session_time: f32, events_enqueued_total: u64) -> RaceEvent {
        RaceEvent::PublisherHeartbeat {
            lap,
            session_time,
            version: self.version.clone(),
            events_enqueued_total,
        }
    }
}

/// Wall-clock scheduler for periodic emissions — `PUBLISHER_HEARTBEAT` and
/// `DRIVER_MATERIAL`.
///
/// An interval of `0` disables the emission entirely. The first firing is
/// due one full interval after the first [`due`] call (HELLO covers startup).
pub struct IntervalScheduler {
    interval: Option<Duration>,
    last: Option<Instant>,
}

/// Historic name of [`IntervalScheduler`], kept because the publisher's
/// heartbeat is its original caller.
pub type HeartbeatScheduler = IntervalScheduler;

impl IntervalScheduler {
    /// Create a scheduler firing every `interval_ms` milliseconds; `0` disables.
    pub fn new(interval_ms: u64) -> Self {
        Self {
            interval: (interval_ms > 0).then(|| Duration::from_millis(interval_ms)),
            last: None,
        }
    }

    /// `true` when an emission is due at `now`; advances the timer when it fires.
    pub fn due(&mut self, now: Instant) -> bool {
        self.due_elapsed(now).is_some()
    }

    /// Elapsed time since the previous firing when one is due at `now`, so the
    /// emitted event can state its own cadence.
    pub fn due_elapsed(&mut self, now: Instant) -> Option<Duration> {
        let interval = self.interval?;
        match self.last {
            None => {
                self.last = Some(now);
                None
            }
            Some(last) if now.duration_since(last) >= interval => {
                self.last = Some(now);
                Some(now.duration_since(last))
            }
            Some(_) => None,
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn on_activate_emits_hello() {
        let mut lc = LifecyclePublisher::new("0.1.0");
        let event = lc.on_activate(1, 10.0);
        assert!(
            matches!(event, RaceEvent::PublisherHello { version, scope, .. }
                if version == "0.1.0" && scope == "driver")
        );
    }

    #[test]
    fn on_deactivate_emits_goodbye() {
        let lc = LifecyclePublisher::new("0.1.0");
        let event = lc.on_deactivate(5, 250.0);
        assert!(matches!(
            event,
            RaceEvent::PublisherGoodbye { lap: 5, reason, previous_sub_session_id: None, .. }
                if reason == GOODBYE_SHUTDOWN
        ));
    }

    #[test]
    fn session_transition_goodbye_names_the_departed_sub_session() {
        let lc = LifecyclePublisher::new("0.1.0");
        let event = lc.on_session_transition(5, 250.0, 88_429_240);
        assert!(matches!(
            event,
            RaceEvent::PublisherGoodbye { reason, previous_sub_session_id: Some(88_429_240), .. }
                if reason == GOODBYE_SESSION_TRANSITION
        ));
    }

    #[test]
    fn heartbeat_emits_heartbeat_event() {
        let lc = LifecyclePublisher::new("0.1.0");
        let event = lc.heartbeat(7, 321.5, 42);
        assert!(
            matches!(event, RaceEvent::PublisherHeartbeat { lap: 7, version, events_enqueued_total: 42, .. }
                if version == "0.1.0")
        );
    }

    #[test]
    fn heartbeat_scheduler_fires_after_interval() {
        let mut hb = HeartbeatScheduler::new(1000);
        let t0 = Instant::now();
        assert!(!hb.due(t0));
        assert!(!hb.due(t0 + Duration::from_millis(500)));
        assert!(hb.due(t0 + Duration::from_millis(1000)));
        assert!(!hb.due(t0 + Duration::from_millis(1500)));
        assert!(hb.due(t0 + Duration::from_millis(2000)));
    }

    #[test]
    fn due_elapsed_reports_the_gap_since_the_previous_firing() {
        let mut sched = IntervalScheduler::new(20_000);
        let t0 = Instant::now();
        assert_eq!(sched.due_elapsed(t0), None);
        assert_eq!(sched.due_elapsed(t0 + Duration::from_secs(10)), None);
        assert_eq!(
            sched.due_elapsed(t0 + Duration::from_secs(25)),
            Some(Duration::from_secs(25))
        );
        assert_eq!(
            sched.due_elapsed(t0 + Duration::from_secs(46)),
            Some(Duration::from_secs(21))
        );
    }

    #[test]
    fn heartbeat_scheduler_zero_interval_disables() {
        let mut hb = HeartbeatScheduler::new(0);
        let t0 = Instant::now();
        assert!(!hb.due(t0));
        assert!(!hb.due(t0 + Duration::from_secs(3600)));
    }

    #[test]
    fn is_fresh_is_true_before_activate() {
        let mut lc = LifecyclePublisher::new("0.1.0");
        assert!(lc.is_fresh());
        let _ = lc.on_activate(1, 0.0);
        assert!(!lc.is_fresh());
    }
}
