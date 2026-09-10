// SPDX-License-Identifier: GPL-3.0-or-later

#[cfg(windows)]
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::Serialize;

use crate::secret::SecretString;

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
    pub events_enqueued_total: u64,
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
}

#[cfg(windows)]
impl InProcessEngine {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                running: None,
                status: None,
                controls_tx: None,
                handle: None,
                snapshot: EngineSnapshot::default(),
            }),
        }
    }

    fn reap_finished(inner: &mut Inner) {
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
                snapshot.events_enqueued_total = status.events_enqueued_total;
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
        Self::reap_finished(&mut inner);
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
                .and_then(|mut inner| {
                    Self::reap_finished(&mut inner);
                    Some(inner.handle.is_none())
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
        Self::reap_finished(&mut inner);
        Self::copy_status(&inner)
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
