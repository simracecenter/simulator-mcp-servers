// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(windows)]
use std::sync::Arc;
use std::sync::{mpsc::Sender, Mutex};
use std::time::SystemTime;

use serde::Serialize;

use crate::secret::SecretString;

pub struct ControlRelay {
    tx: Mutex<Option<Sender<publisher::controls::ControlRequest>>>,
}

impl ControlRelay {
    pub fn new() -> Self {
        Self {
            tx: Mutex::new(None),
        }
    }

    pub fn attach(&self, tx: Sender<publisher::controls::ControlRequest>) {
        if let Ok(mut slot) = self.tx.lock() {
            *slot = Some(tx);
        }
    }

    pub fn detach(&self) {
        if let Ok(mut slot) = self.tx.lock() {
            *slot = None;
        }
    }

    pub fn send(&self, request: publisher::controls::ControlRequest) -> bool {
        let Ok(mut slot) = self.tx.lock() else {
            return false;
        };
        let Some(tx) = slot.as_ref() else {
            return false;
        };
        if tx.send(request).is_ok() {
            true
        } else {
            *slot = None;
            false
        }
    }
}

impl Default for ControlRelay {
    fn default() -> Self {
        Self::new()
    }
}

pub struct LaunchSpec {
    pub ingest_url: String,
    pub token: SecretString,
    pub cert_fingerprint: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EngineState {
    #[default]
    Stopped,
    Starting,
    Running,
    Failed,
    Unknown,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct EngineSnapshot {
    pub state: EngineState,
    pub iracing_connected: bool,
    pub rc_connected: bool,
    pub sub_session_id: Option<i64>,
    pub last_post_at: Option<SystemTime>,
    pub last_error_kind: Option<String>,
    pub last_error: Option<String>,
    pub queued_events: usize,
    pub outbox_pending_batches: usize,
    pub events_enqueued_total: u64,
    pub events_delivered_total: u64,
    pub events_rejected_total: u64,
    pub events_duplicate_total: u64,
    pub events_lost_total: u64,
    pub calls_total: u64,
    pub calls_failed: u64,
    pub started_at: Option<SystemTime>,
}

pub trait PublisherEngine: Send + Sync {
    fn start(&self, spec: LaunchSpec) -> Result<(), String>;
    fn stop(&self);
    fn snapshot(&self) -> EngineSnapshot;
}

#[cfg(windows)]
struct Inner {
    running: Option<Arc<std::sync::atomic::AtomicBool>>,
    status: Option<Arc<Mutex<publisher::publisher_status::PublisherStatus>>>,
    controls_tx: Option<std::sync::mpsc::Sender<publisher::controls::ControlRequest>>,
    handle: Option<std::thread::JoinHandle<Result<(), String>>>,
    snapshot: EngineSnapshot,
}

#[cfg(windows)]
pub struct InProcessEngine {
    inner: Mutex<Inner>,
    relay: Arc<ControlRelay>,
}

#[cfg(windows)]
impl InProcessEngine {
    pub fn new() -> Self {
        Self::with_relay(Arc::new(ControlRelay::new()))
    }

    pub fn with_relay(relay: Arc<ControlRelay>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                running: None,
                status: None,
                controls_tx: None,
                handle: None,
                snapshot: EngineSnapshot::default(),
            }),
            relay,
        }
    }

    fn reap_finished(inner: &mut Inner, relay: &ControlRelay) {
        let finished = inner
            .handle
            .as_ref()
            .is_some_and(std::thread::JoinHandle::is_finished);
        if !finished {
            return;
        }
        let handle = inner.handle.take().expect("finished handle exists");
        match handle.join() {
            Ok(Ok(())) => {
                inner.snapshot.state = EngineState::Stopped;
                inner.snapshot.last_error = None;
            }
            Ok(Err(error)) => {
                inner.snapshot.state = EngineState::Failed;
                inner.snapshot.last_error = Some(error);
            }
            Err(_) => {
                inner.snapshot.state = EngineState::Failed;
                inner.snapshot.last_error = Some("engine thread panicked".to_string());
            }
        }
        inner.running = None;
        inner.status = None;
        inner.controls_tx = None;
        relay.detach();
    }

    fn copy_status(inner: &Inner) -> EngineSnapshot {
        let mut snapshot = inner.snapshot.clone();
        if let Some(status) = &inner.status {
            if let Ok(status) = status.lock() {
                snapshot.iracing_connected = status.iracing_connected;
                snapshot.rc_connected = status.rc_connected;
                snapshot.sub_session_id = status.sub_session_id;
                snapshot.last_post_at = status.last_post_at;
                snapshot.last_error_kind = status.last_error_kind.clone();
                snapshot.queued_events = status.queued_events;
                snapshot.outbox_pending_batches = status.outbox_pending_batches;
                snapshot.events_enqueued_total = status.events_enqueued_total;
                snapshot.events_delivered_total = status.events_delivered_total;
                snapshot.events_rejected_total = status.events_rejected_total;
                snapshot.events_duplicate_total = status.events_duplicate_total;
                snapshot.events_lost_total = status.events_lost_total;
                snapshot.calls_total = status.calls_total;
                snapshot.calls_failed = status.calls_failed;
                snapshot.state = if status.iracing_connected {
                    EngineState::Running
                } else {
                    EngineState::Starting
                };
            }
        }
        snapshot
    }
}

#[cfg(windows)]
impl Default for InProcessEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl PublisherEngine for InProcessEngine {
    fn start(&self, spec: LaunchSpec) -> Result<(), String> {
        use std::sync::atomic::AtomicBool;

        let mut inner = self
            .inner
            .lock()
            .map_err(|_| "publisher engine lock is poisoned".to_string())?;
        Self::reap_finished(&mut inner, &self.relay);
        if inner.handle.is_some() {
            return Ok(());
        }

        let local = publisher::config::LocalConfig {
            url: spec.ingest_url,
            token: spec.token.into_inner(),
            cert_fingerprint: spec.cert_fingerprint,
        };
        let config = publisher::config::PublisherConfig {
            destination: publisher::config::Destination::Local,
            auth: None,
            local: Some(local),
            publisher: publisher::config::PublisherSection::default(),
        };
        let running = Arc::new(AtomicBool::new(true));
        let status = Arc::new(Mutex::new(
            publisher::publisher_status::PublisherStatus::default(),
        ));
        let (controls_tx, controls_rx) = std::sync::mpsc::channel();
        let thread_running = Arc::clone(&running);
        let thread_status = Arc::clone(&status);
        let handle = std::thread::Builder::new()
            .name("publisher-engine".to_string())
            .spawn(move || {
                publisher::pipeline::run_pipeline(
                    &config,
                    thread_running,
                    thread_status,
                    controls_rx,
                )
            })
            .map_err(|error| format!("failed to spawn publisher engine: {error}"))?;

        self.relay.attach(controls_tx.clone());
        inner.running = Some(running);
        inner.status = Some(status);
        inner.controls_tx = Some(controls_tx);
        inner.snapshot = EngineSnapshot {
            state: EngineState::Starting,
            started_at: Some(SystemTime::now()),
            ..EngineSnapshot::default()
        };
        inner.handle = Some(handle);
        Ok(())
    }

    fn stop(&self) {
        self.relay.detach();
        let running = self
            .inner
            .lock()
            .ok()
            .and_then(|inner| inner.running.clone());
        if let Some(running) = running {
            running.store(false, std::sync::atomic::Ordering::SeqCst);
        }

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let finished = self
                .inner
                .lock()
                .ok()
                .map(|mut inner| {
                    Self::reap_finished(&mut inner, &self.relay);
                    inner.handle.is_none()
                })
                .unwrap_or(true);
            if finished || std::time::Instant::now() >= deadline {
                if !finished {
                    if let Ok(mut inner) = self.inner.lock() {
                        inner.snapshot.state = EngineState::Unknown;
                        inner.snapshot.last_error =
                            Some("stop timed out waiting for engine thread".to_string());
                    }
                }
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn snapshot(&self) -> EngineSnapshot {
        let Ok(mut inner) = self.inner.lock() else {
            return EngineSnapshot {
                state: EngineState::Unknown,
                last_error: Some("publisher engine lock is poisoned".to_string()),
                ..EngineSnapshot::default()
            };
        };
        Self::reap_finished(&mut inner, &self.relay);
        Self::copy_status(&inner)
    }
}

#[cfg(test)]
mod control_relay_tests {
    use super::ControlRelay;
    use publisher::controls::{simulated_request, ControlAction};

    #[test]
    fn send_without_an_attached_engine_returns_false() {
        assert!(!ControlRelay::new().send(simulated_request(ControlAction::ApproveYes, 1)));
    }

    #[test]
    fn attached_engine_receives_requests() {
        let relay = ControlRelay::new();
        let (tx, rx) = std::sync::mpsc::channel();
        relay.attach(tx);
        let request = simulated_request(ControlAction::ApproveYes, 42);

        assert!(relay.send(request.clone()));
        assert_eq!(rx.recv().unwrap(), request);
    }

    #[test]
    fn detach_drops_subsequent_requests() {
        let relay = ControlRelay::new();
        let (tx, _rx) = std::sync::mpsc::channel();
        relay.attach(tx);
        relay.detach();

        assert!(!relay.send(simulated_request(ControlAction::ApproveNo, 42)));
    }

    #[test]
    fn disconnected_receiver_drops_requests_and_detaches() {
        let relay = ControlRelay::new();
        let (tx, rx) = std::sync::mpsc::channel();
        relay.attach(tx);
        drop(rx);

        assert!(!relay.send(simulated_request(ControlAction::ApproveNo, 42)));
        assert!(!relay.send(simulated_request(ControlAction::ApproveNo, 43)));
    }
}

#[cfg(not(windows))]
pub struct UnsupportedEngine;

#[cfg(not(windows))]
impl PublisherEngine for UnsupportedEngine {
    fn start(&self, _spec: LaunchSpec) -> Result<(), String> {
        Err("publisher engine requires Windows (iRacing shared memory)".to_string())
    }

    fn stop(&self) {}

    fn snapshot(&self) -> EngineSnapshot {
        EngineSnapshot::default()
    }
}
