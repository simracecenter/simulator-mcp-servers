// SPDX-License-Identifier: GPL-3.0-or-later

//! Shared state between the publisher pipeline thread and the UI thread.

use std::collections::VecDeque;
use std::time::SystemTime;

/// Maximum number of entries retained in the event log.
pub const EVENT_LOG_CAPACITY: usize = 50;

/// Written by the publisher pipeline thread; read by the UI render loop.
///
/// Wrapped in `Arc<Mutex<PublisherStatus>>`. The pipeline holds the lock
/// for a few microseconds per frame update; the UI holds it for a single
/// paint pass (~16ms at 60fps). No long-held locks.
#[derive(Default)]
pub struct PublisherStatus {
    // ── iRacing connection ────────────────────────────────────────────────
    pub iracing_connected: bool,
    pub sub_session_id: Option<i64>,
    pub track_name: Option<String>,
    pub session_type: Option<String>,
    /// "unlimited" or a lap count string.
    pub session_laps: Option<String>,
    pub current_lap: u8,
    pub session_tick: i64,
    pub session_time_secs: f64,

    // ── Race Control / transport ──────────────────────────────────────────
    pub rc_last_http_status: Option<u16>,
    pub rc_connected: bool,
    pub token_expires_at: Option<SystemTime>,
    /// Wall-clock time of the last successful batch POST.
    pub last_post_at: Option<SystemTime>,
    /// `TransportErrorKind::label()` of the last failed call.
    pub last_error_kind: Option<String>,
    /// Events buffered in the delivery queue right now.
    pub queued_events: usize,
    /// Batch files persisted in the outbox but not yet acknowledged.
    pub outbox_pending_batches: usize,
    /// Set on shutdown so `status.json` reports `stopped`.
    pub stopped: bool,

    // ── Counters ──────────────────────────────────────────────────────────
    pub events_enqueued_total: u64,
    /// Events acknowledged (accepted or spooled) by the receiver.
    pub events_delivered_total: u64,
    /// Events the receiver explicitly rejected inside an acknowledged batch.
    pub events_rejected_total: u64,
    /// Events the receiver reported as already-seen duplicates.
    pub events_duplicate_total: u64,
    /// Events permanently dropped (queue overflow, outbox bound, quarantine).
    pub events_lost_total: u64,
    pub calls_total: u64,
    pub calls_failed: u64,

    // ── Event log ─────────────────────────────────────────────────────────
    pub event_log: VecDeque<EventLogEntry>,

    // ── Config ────────────────────────────────────────────────────────────
    pub config_path: Option<String>,
}

/// One row in the rolling event log panel.
#[derive(Clone)]
pub struct EventLogEntry {
    pub session_time: f64,
    pub event_type: String,
    pub car_number: String,
    pub driver_name: String,
}

impl PublisherStatus {
    /// Append an entry to the event log, evicting the oldest if at capacity.
    pub fn push_event_log(&mut self, entry: EventLogEntry) {
        if self.event_log.len() >= EVENT_LOG_CAPACITY {
            self.event_log.pop_back();
        }
        self.event_log.push_front(entry);
    }

    /// Snapshot this status as the serialisable [`StatusFile`] written to
    /// `status.json` in headless mode.
    pub fn to_status_file(&self, destination: &str) -> crate::headless::StatusFile {
        let state = if self.stopped {
            "stopped"
        } else if self.iracing_connected {
            "connected"
        } else {
            "waiting_for_iracing"
        };
        crate::headless::StatusFile {
            state: state.to_owned(),
            destination: destination.to_owned(),
            updated_at: crate::headless::iso8601_utc(SystemTime::now()),
            last_post_at: self.last_post_at.map(crate::headless::iso8601_utc),
            last_error_kind: self.last_error_kind.clone(),
            queued_events: self.queued_events,
            outbox_pending_batches: self.outbox_pending_batches,
            sub_session_id: self.sub_session_id,
            events_enqueued_total: self.events_enqueued_total,
            events_delivered_total: self.events_delivered_total,
            events_rejected_total: self.events_rejected_total,
            events_duplicate_total: self.events_duplicate_total,
            events_lost_total: self.events_lost_total,
            calls_total: self.calls_total,
            calls_failed: self.calls_failed,
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        }
    }
}
