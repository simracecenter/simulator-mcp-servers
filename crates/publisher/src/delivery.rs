// SPDX-License-Identifier: GPL-3.0-or-later

//! Delivery worker — owns the HTTP transport and durable outbox on a dedicated
//! thread so the sampling loop never performs network or disk I/O beyond a
//! bounded in-memory push.
//!
//! Delivery model:
//!
//! 1. The sampling thread calls [`DeliveryService::enqueue`] — a bounded,
//!    non-blocking push. A full queue drops the *oldest* event and increments
//!    `events_lost_total`; loss is always counted, never silent.
//! 2. The worker forms batches, persists each to the [`Outbox`], and POSTs the
//!    oldest pending file. A batch file is deleted only after a 2xx response.
//! 3. On restart the outbox is recovered and pending files are re-delivered
//!    first; the receiver deduplicates by event `id`.
//! 4. `shutdown` persists everything still queued and delivers on a bounded
//!    best-effort pass; whatever remains waits for the next launch.

use std::collections::VecDeque;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use serde_json::json;

use crate::outbox::Outbox;
use crate::publisher_event::PublisherEvent;
use crate::transport::{BatchReceipt, PublisherTransport, BATCH_LIMIT};
use crate::{log_info, log_warn};

/// In-memory queue bound between the sampling thread and the worker. At
/// 20 events per 500 ms batch this is over a minute of headroom; beyond it
/// oldest events drop and are counted rather than growing memory unbounded.
const MAX_QUEUED_EVENTS: usize = 4096;

/// Default cap on persisted (unacknowledged) batch files — about 10 000
/// events. Override per deployment in tests or unusual recovery windows.
pub const DEFAULT_MAX_OUTBOX_BATCHES: usize = 512;

/// Wait between redelivery attempts after a failed POST. The per-request
/// retry schedule already ran inside the failed attempt; this spaces out
/// whole-batch retries so a dead receiver is not hammered.
const RESEND_DELAY: Duration = Duration::from_secs(5);

/// Bounded budget for the shutdown drain: pending outbox files are posted
/// single-attempt until this elapses, then left for the next launch.
const SHUTDOWN_DRAIN_BUDGET: Duration = Duration::from_secs(4);

/// Session stamp applied to the batch envelope — the latest frame values the
/// sampling thread reported via `tick`/`request_flush`.
#[derive(Clone, Copy, Default)]
struct SessionEnvelope {
    session_time: f64,
    session_tick: i64,
    sub_session_id: i64,
}

enum Control {
    Tick(SessionEnvelope),
    Flush(SessionEnvelope),
    Shutdown(SessionEnvelope),
}

/// Live delivery counters shared with the publisher status surface. Counters
/// are written only by the worker thread.
#[derive(Default)]
pub struct DeliveryStats {
    /// Events waiting in the in-memory queue to be batched.
    pub queued_events: AtomicUsize,
    /// Batch files persisted but not yet acknowledged.
    pub outbox_pending_batches: AtomicUsize,
    /// Events permanently dropped (queue overflow, outbox bound, store or
    /// quarantine failures). Always counted — loss is never silent.
    pub events_lost_total: AtomicU64,
    /// Events acknowledged as accepted or spooled by the receiver.
    pub events_delivered_total: AtomicU64,
    /// Events the receiver explicitly rejected inside an acknowledged batch.
    pub events_rejected_total: AtomicU64,
    /// Events the receiver reported as already-seen duplicates.
    pub events_duplicate_total: AtomicU64,
    pub calls_total: AtomicU64,
    pub calls_failed: AtomicU64,
    /// Last POST succeeded (true) or failed (false).
    pub connected: AtomicBool,
    pub last_post_at: Mutex<Option<SystemTime>>,
    pub last_http_status: Mutex<Option<u16>>,
    pub last_error_kind: Mutex<Option<String>>,
    pub token_expires_at: Mutex<Option<SystemTime>>,
}

impl DeliveryStats {
    /// Point-in-time copy for status surfaces.
    pub fn snapshot(&self) -> DeliverySnapshot {
        DeliverySnapshot {
            queued_events: self.queued_events.load(Ordering::SeqCst),
            outbox_pending_batches: self.outbox_pending_batches.load(Ordering::SeqCst),
            events_lost_total: self.events_lost_total.load(Ordering::SeqCst),
            events_delivered_total: self.events_delivered_total.load(Ordering::SeqCst),
            events_rejected_total: self.events_rejected_total.load(Ordering::SeqCst),
            events_duplicate_total: self.events_duplicate_total.load(Ordering::SeqCst),
            calls_total: self.calls_total.load(Ordering::SeqCst),
            calls_failed: self.calls_failed.load(Ordering::SeqCst),
            connected: self.connected.load(Ordering::SeqCst),
            last_post_at: *self.last_post_at.lock().unwrap(),
            last_http_status: *self.last_http_status.lock().unwrap(),
            last_error_kind: self.last_error_kind.lock().unwrap().clone(),
            token_expires_at: *self.token_expires_at.lock().unwrap(),
        }
    }
}

/// Plain-data copy of [`DeliveryStats`].
#[derive(Debug, Default, Clone)]
pub struct DeliverySnapshot {
    pub queued_events: usize,
    pub outbox_pending_batches: usize,
    pub events_lost_total: u64,
    pub events_delivered_total: u64,
    pub events_rejected_total: u64,
    pub events_duplicate_total: u64,
    pub calls_total: u64,
    pub calls_failed: u64,
    pub connected: bool,
    pub last_post_at: Option<SystemTime>,
    pub last_http_status: Option<u16>,
    pub last_error_kind: Option<String>,
    pub token_expires_at: Option<SystemTime>,
}

struct Shared {
    queue: Mutex<VecDeque<PublisherEvent>>,
    stats: DeliveryStats,
}

/// Sampling-side handle to the delivery worker.
pub struct DeliveryService {
    shared: Arc<Shared>,
    tx: Sender<Control>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl DeliveryService {
    /// Open the outbox under `outbox_dir` and spawn the delivery worker.
    ///
    /// An unopenable outbox fails startup: without it the publisher cannot
    /// honour durable delivery, and pretending otherwise is worse than
    /// refusing to run.
    pub fn start(
        transport: PublisherTransport,
        outbox_dir: &Path,
        max_outbox_batches: usize,
    ) -> io::Result<Self> {
        Self::start_with_resend(transport, outbox_dir, max_outbox_batches, RESEND_DELAY)
    }

    /// Like [`Self::start`], with a caller-chosen resend delay. Tests use a
    /// short delay; production callers should use [`Self::start`].
    pub fn start_with_resend(
        transport: PublisherTransport,
        outbox_dir: &Path,
        max_outbox_batches: usize,
        resend_delay: Duration,
    ) -> io::Result<Self> {
        // Dry-run is a local inspection mode: batches are printed, nothing is
        // persisted and no durable guarantees are implied.
        let outbox = if transport.is_dry_run() {
            None
        } else {
            Some(Outbox::open(outbox_dir, max_outbox_batches)?)
        };

        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            stats: DeliveryStats::default(),
        });
        let pending = outbox.as_ref().map_or(0, Outbox::pending_len);
        shared
            .stats
            .outbox_pending_batches
            .store(pending, Ordering::SeqCst);

        let (tx, rx) = mpsc::channel();
        let worker_shared = Arc::clone(&shared);
        let join = std::thread::Builder::new()
            .name("publisher-delivery".to_string())
            .spawn(move || worker_loop(transport, outbox, worker_shared, rx, resend_delay))
            .map_err(io::Error::other)?;

        Ok(Self {
            shared,
            tx,
            join: Mutex::new(Some(join)),
        })
    }

    /// Buffer one event for batching. Never blocks on I/O; when the queue is
    /// full the oldest buffered event is dropped and counted as lost.
    pub fn enqueue(&self, event: PublisherEvent) {
        let mut queue = self.shared.queue.lock().unwrap();
        if queue.len() >= MAX_QUEUED_EVENTS {
            queue.pop_front();
            let lost = self
                .shared
                .stats
                .events_lost_total
                .fetch_add(1, Ordering::SeqCst)
                + 1;
            if lost == 1 || lost.is_multiple_of(64) {
                log_warn!(
                    "[delivery] queue full — dropped oldest event (events_lost_total={lost})"
                );
            }
        }
        queue.push_back(event);
        self.shared
            .stats
            .queued_events
            .store(queue.len(), Ordering::SeqCst);
    }

    /// Update the session stamp used for the next batch envelope.
    pub fn tick(&self, session_time: f64, session_tick: i64, sub_session_id: i64) {
        let _ = self.tx.send(Control::Tick(SessionEnvelope {
            session_time,
            session_tick,
            sub_session_id,
        }));
    }

    /// Ask the worker to persist and deliver queued events immediately rather
    /// than waiting for the batch interval.
    pub fn request_flush(&self, session_time: f64, session_tick: i64, sub_session_id: i64) {
        let _ = self.tx.send(Control::Flush(SessionEnvelope {
            session_time,
            session_tick,
            sub_session_id,
        }));
    }

    /// Latest delivery counters for status reporting.
    pub fn stats(&self) -> DeliverySnapshot {
        self.shared.stats.snapshot()
    }

    /// Stop the worker: persist everything still queued, then attempt pending
    /// deliveries on a bounded best-effort pass. Files left unacknowledged are
    /// delivered on the next start.
    pub fn shutdown(&self, session_time: f64, session_tick: i64, sub_session_id: i64) {
        let _ = self.tx.send(Control::Shutdown(SessionEnvelope {
            session_time,
            session_tick,
            sub_session_id,
        }));
        if let Some(join) = self.join.lock().unwrap().take() {
            let _ = join.join();
        }
    }
}

impl Drop for DeliveryService {
    fn drop(&mut self) {
        // If shutdown() never ran, still signal drain-and-exit rather than
        // leaking the worker thread.
        let _ = self.tx.send(Control::Shutdown(SessionEnvelope::default()));
    }
}

struct WorkerState {
    envelope: SessionEnvelope,
    flush_now: bool,
    shutting: bool,
    deadline: Instant,
}

impl WorkerState {
    fn handle(&mut self, transport: &mut PublisherTransport, msg: Control) {
        match msg {
            Control::Tick(env) => self.envelope = env,
            Control::Flush(env) => {
                self.envelope = env;
                self.flush_now = true;
            }
            Control::Shutdown(env) => {
                self.envelope = env;
                self.shutting = true;
                self.deadline = Instant::now() + SHUTDOWN_DRAIN_BUDGET;
                // Single-attempt posts: the drain must stay inside its
                // budget, so the multi-second retry schedule is off.
                transport.set_retry_delays(&[]);
            }
        }
    }
}

fn worker_loop(
    mut transport: PublisherTransport,
    mut outbox: Option<Outbox>,
    shared: Arc<Shared>,
    rx: Receiver<Control>,
    resend_delay: Duration,
) {
    let interval = transport.batch_interval();
    let mut state = WorkerState {
        envelope: SessionEnvelope::default(),
        flush_now: false,
        shutting: false,
        deadline: Instant::now(),
    };
    // Gate on re-posting after a failure: whole-batch retries are spaced by
    // `resend_delay` on top of the per-attempt retry schedule inside post_body.
    let mut resend_after = Instant::now();
    // First batch fires immediately after startup rather than one interval late.
    let mut last_batch_at = Instant::now()
        .checked_sub(interval)
        .unwrap_or_else(Instant::now);
    // Events the outbox has already counted as lost (bound drops at open,
    // drops during store, quarantines) — folded into stats as a delta so one
    // counter reports all loss.
    let mut outbox_loss_seen = outbox.as_ref().map_or(0, |o| o.dropped_events_total());
    if outbox_loss_seen > 0 {
        shared
            .stats
            .events_lost_total
            .fetch_add(outbox_loss_seen, Ordering::SeqCst);
    }

    loop {
        // Drain pending control messages first — flush/shutdown must not sit
        // behind a queue of ticks.
        loop {
            match rx.try_recv() {
                Ok(msg) => state.handle(&mut transport, msg),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    state.shutting = true;
                    state.deadline = Instant::now() + SHUTDOWN_DRAIN_BUDGET;
                    transport.set_retry_delays(&[]);
                }
            }
        }
        let now = Instant::now();

        // Persist due events from the in-memory queue into the outbox. Events
        // are neither posted nor stored while the session id is unresolved — a
        // subSessionId of 0 would persist events against a ghost session. The
        // shutdown drain is exempt: the outbox is the honest landing place.
        let queued = shared.queue.lock().unwrap().len();
        let batch_due = state.flush_now
            || state.shutting
            || queued >= BATCH_LIMIT
            || (queued > 0 && now.duration_since(last_batch_at) >= interval);
        if queued > 0 && batch_due && (state.envelope.sub_session_id != 0 || state.shutting) {
            let batch: Vec<PublisherEvent> = {
                let mut queue = shared.queue.lock().unwrap();
                let n = queue.len().min(BATCH_LIMIT);
                queue.drain(..n).collect()
            };
            let body = json!({
                "subSessionId": state.envelope.sub_session_id,
                "sessionTime": state.envelope.session_time,
                "sessionTick": state.envelope.session_tick,
                "events": batch,
            });
            match &mut outbox {
                Some(o) => {
                    if let Err(e) = o.store(&body) {
                        // Store failed (e.g. disk full) — the batch is lost and
                        // counted; the queue keeps its bound.
                        shared
                            .stats
                            .events_lost_total
                            .fetch_add(batch.len() as u64, Ordering::SeqCst);
                        *shared.stats.last_error_kind.lock().unwrap() =
                            Some("outbox_store".to_string());
                        log_warn!(
                            "[delivery] outbox store failed — {} event(s) lost: {e}",
                            batch.len()
                        );
                    }
                    // A full outbox drops its oldest file inside store() —
                    // surface that loss on the shared counter too.
                    let dropped = o.dropped_events_total();
                    if dropped > outbox_loss_seen {
                        shared
                            .stats
                            .events_lost_total
                            .fetch_add(dropped - outbox_loss_seen, Ordering::SeqCst);
                        outbox_loss_seen = dropped;
                    }
                    shared
                        .stats
                        .outbox_pending_batches
                        .store(o.pending_len(), Ordering::SeqCst);
                }
                // Dry-run: post directly (prints to stdout).
                None => {
                    if let Err(e) = transport.post_body(&body) {
                        log_warn!("[delivery] post error: {e}");
                    }
                }
            }
            shared
                .stats
                .queued_events
                .store(shared.queue.lock().unwrap().len(), Ordering::SeqCst);
            last_batch_at = Instant::now();
            state.flush_now = false;
            continue;
        }

        // Deliver the oldest pending outbox file. Backlog goes first, so send
        // order matches store order end to end.
        if let Some(o) = &mut outbox {
            if o.pending_len() > 0 && (state.shutting || now >= resend_after) {
                if state.shutting && Instant::now() >= state.deadline {
                    break;
                }
                match o.read_front() {
                    Ok(body) => {
                        shared.stats.calls_total.fetch_add(1, Ordering::SeqCst);
                        match transport.post_body(&body) {
                            Ok(receipt) => {
                                o.ack_front();
                                record_receipt(&shared.stats, receipt);
                                *shared.stats.last_error_kind.lock().unwrap() = None;
                                resend_after = Instant::now();
                            }
                            Err(e) => {
                                shared.stats.calls_failed.fetch_add(1, Ordering::SeqCst);
                                shared.stats.connected.store(false, Ordering::SeqCst);
                                *shared.stats.last_error_kind.lock().unwrap() =
                                    Some(e.kind.label());
                                log_warn!("[delivery] delivery failed, will retry: {e}");
                                resend_after = Instant::now() + resend_delay;
                                if state.shutting {
                                    // Failed posts return quickly (e.g.
                                    // refused connection); avoid a hot spin
                                    // against the drain deadline.
                                    std::thread::sleep(Duration::from_millis(20));
                                }
                            }
                        }
                        shared
                            .stats
                            .outbox_pending_batches
                            .store(o.pending_len(), Ordering::SeqCst);
                        shared
                            .stats
                            .token_expires_at
                            .lock()
                            .unwrap()
                            .clone_from(&transport.token_expires_at());
                    }
                    Err(e) => {
                        log_warn!("[delivery] unreadable outbox batch, quarantining: {e}");
                        o.quarantine_front();
                        let dropped = o.dropped_events_total();
                        if dropped > outbox_loss_seen {
                            shared
                                .stats
                                .events_lost_total
                                .fetch_add(dropped - outbox_loss_seen, Ordering::SeqCst);
                            outbox_loss_seen = dropped;
                        }
                        shared
                            .stats
                            .outbox_pending_batches
                            .store(o.pending_len(), Ordering::SeqCst);
                    }
                }
                continue;
            }
        }

        if state.shutting {
            break;
        }

        // Sleep until the next batch cadence or the resend deadline, waking
        // early on a control message.
        let mut wait = interval
            .checked_sub(now.duration_since(last_batch_at))
            .unwrap_or(Duration::ZERO);
        if let Some(o) = &outbox {
            if o.pending_len() > 0 && resend_after > now {
                wait = wait.min(resend_after - now);
            }
        }
        if wait.is_zero() {
            wait = Duration::from_millis(1);
        }
        match rx.recv_timeout(wait) {
            Ok(msg) => state.handle(&mut transport, msg),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                state.shutting = true;
                state.deadline = Instant::now() + SHUTDOWN_DRAIN_BUDGET;
                transport.set_retry_delays(&[]);
            }
        }
    }

    log_info!("[delivery] worker stopped");
}

fn record_receipt(stats: &DeliveryStats, receipt: BatchReceipt) {
    stats
        .events_delivered_total
        .fetch_add(receipt.accepted as u64, Ordering::SeqCst);
    stats
        .events_rejected_total
        .fetch_add(receipt.rejected as u64, Ordering::SeqCst);
    stats
        .events_duplicate_total
        .fetch_add(receipt.duplicate as u64, Ordering::SeqCst);
    stats.connected.store(true, Ordering::SeqCst);
    *stats.last_post_at.lock().unwrap() = Some(SystemTime::now());
    *stats.last_http_status.lock().unwrap() = Some(receipt.http_status);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publisher_event::PublisherIdentity;
    use crate::race_event::EventScope;
    use serde_json::json;
    use std::path::PathBuf;
    use std::time::UNIX_EPOCH;

    fn test_event(n: u64) -> PublisherEvent {
        PublisherEvent {
            id: format!("evt-{n}"),
            race_session_id: "7".to_owned(),
            rig_id: "rig-test".to_owned(),
            event_type: "RACE_GREEN".to_owned(),
            timestamp: 1_700_000_000_000,
            emitted_at: "2023-11-14T22:13:20.000Z".to_owned(),
            publisher_run_id: "run-test".to_owned(),
            session_time: 10.0,
            session_tick: 100,
            contract_version: 2,
            sequence: n,
            event_key: format!("key-{n}"),
            scope: EventScope::SessionScoped,
            publisher: PublisherIdentity {
                rig_id: "rig-test".to_owned(),
                rig_label: "rig-test".to_owned(),
                car_idx: 0,
                car_number: "7".to_owned(),
                driver_id: "d1".to_owned(),
                driver_name: "Test Driver".to_owned(),
            },
            subject: None,
            car: None,
            payload: json!({}),
            context: None,
        }
    }

    fn temp_outbox(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dnc-delivery-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A URL nothing is listening on: bind then drop an ephemeral port.
    fn dead_url() -> String {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        format!("http://127.0.0.1:{port}")
    }

    fn service(url: &str, dir: &std::path::Path, max: usize) -> DeliveryService {
        let mut t = PublisherTransport::new_local(url, "tok", None, 10).unwrap();
        t.set_retry_delays(&[]);
        DeliveryService::start_with_resend(t, dir, max, Duration::from_millis(50)).unwrap()
    }

    /// Poll a predicate until it holds or `ms` elapse.
    fn wait_for(ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cond()
    }

    #[test]
    fn delivered_batch_is_acked_and_outbox_empties() {
        let dir = temp_outbox("ack");
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(202)
            .with_body(r#"{"accepted":2,"rejected":0,"duplicate":0,"spooled":false}"#)
            .expect(1)
            .create();

        let delivery = service(&server.url(), &dir, 16);
        delivery.tick(10.0, 100, 7);
        delivery.enqueue(test_event(1));
        delivery.enqueue(test_event(2));
        delivery.request_flush(10.0, 100, 7);

        assert!(wait_for(3000, || delivery.stats().events_delivered_total == 2));
        assert!(wait_for(1000, || delivery.stats().outbox_pending_batches == 0));
        mock.assert();
        let s = delivery.stats();
        assert!(s.connected);
        assert_eq!(s.last_http_status, Some(202));
        assert_eq!(s.calls_total, 1);
        assert_eq!(s.calls_failed, 0);
        assert_eq!(s.events_lost_total, 0);

        delivery.shutdown(10.0, 100, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dead_receiver_persists_and_redelivers_on_restart() {
        let dir = temp_outbox("restart");

        // Phase 1 — receiver down: events must land in the outbox and stay.
        {
            let delivery = service(&dead_url(), &dir, 16);
            delivery.tick(10.0, 100, 7);
            for n in 0..3 {
                delivery.enqueue(test_event(n));
            }
            delivery.request_flush(10.0, 100, 7);
            assert!(wait_for(2000, || delivery.stats().outbox_pending_batches == 1));
            // Sampling kept enqueueing — nothing blocked and nothing was lost.
            assert_eq!(delivery.stats().events_lost_total, 0);
            assert!(!delivery.stats().connected);
            delivery.shutdown(10.0, 100, 7);
        }
        assert_eq!(delivery_files(&dir), 1, "batch must survive the kill");

        // Phase 2 — restart against a live receiver: pending batch redelivers.
        let mut server = mockito::Server::new();
        let mock = server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(202)
            .with_body(r#"{"accepted":3}"#)
            .match_body(mockito::Matcher::PartialJson(
                json!({"subSessionId": 7, "events": [{"id": "evt-0"}]}),
            ))
            .expect(1)
            .create();

        let delivery = service(&server.url(), &dir, 16);
        assert!(wait_for(3000, || delivery.stats().events_delivered_total == 3));
        assert_eq!(delivery.stats().outbox_pending_batches, 0);
        mock.assert();
        delivery.shutdown(10.0, 100, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn outbox_bound_drops_oldest_and_counts_loss() {
        let dir = temp_outbox("bound");
        let delivery = service(&dead_url(), &dir, 1);
        delivery.tick(10.0, 100, 7);
        // Three batches' worth of events against a dead receiver; bound of 1
        // means the two older batch files are dropped and counted.
        for n in 0..(BATCH_LIMIT * 3) {
            delivery.enqueue(test_event(n as u64));
        }
        delivery.request_flush(10.0, 100, 7);

        assert!(wait_for(3000, || delivery.stats().events_lost_total
            >= (BATCH_LIMIT * 2) as u64));
        assert_eq!(delivery.stats().outbox_pending_batches, 1);
        delivery.shutdown(10.0, 100, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn events_wait_for_session_resolution() {
        let dir = temp_outbox("sid0");
        let mut server = mockito::Server::new();
        server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(202)
            .expect(0) // nothing may post while sid == 0
            .create();

        let delivery = service(&server.url(), &dir, 16);
        // Events with sid still 0 must not persist or post.
        delivery.enqueue(test_event(1));
        delivery.request_flush(10.0, 100, 0);
        std::thread::sleep(Duration::from_millis(120));
        assert_eq!(delivery.stats().outbox_pending_batches, 0);
        assert_eq!(delivery.stats().queued_events, 1);
        server.reset();

        // Resolving the session releases the held event.
        let mock = server
            .mock("POST", "/api/publisher/v2/ingest")
            .with_status(202)
            .with_body(r#"{"accepted":1}"#)
            .expect(1)
            .create();
        delivery.tick(11.0, 101, 7);
        delivery.request_flush(11.0, 101, 7);
        assert!(wait_for(2000, || delivery.stats().events_delivered_total == 1));
        mock.assert();
        delivery.shutdown(11.0, 101, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn delivery_files(dir: &std::path::Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("batch-")
                    && e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".json")
            })
            .count()
    }
}
