// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use publisher::controls::{self, CapturedBinding, ControlAction, ControlsConfig, ControlsState};
use publisher_mcp::ControlRelay;
use serde::Serialize;

#[cfg(windows)]
use publisher::controls::ControlRequest;

#[cfg(windows)]
use std::thread::JoinHandle;

#[cfg(windows)]
use std::time::Duration;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlsStatus {
    pub available: bool,
    pub listening: bool,
    pub devices_seen: usize,
    pub learning: Option<String>,
    pub bindings: Vec<BindingStatus>,
    pub last_captured: Option<CapturedStatus>,
    pub last_request: Option<RequestStatus>,
    pub last_error: Option<String>,
    pub path: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BindingStatus {
    pub action: String,
    pub label: String,
    pub device: Option<String>,
    pub button: Option<u16>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapturedStatus {
    pub action: String,
    pub device: String,
    pub button: u16,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestStatus {
    pub action: String,
    pub at_ms: i64,
}

pub struct WheelControls {
    state: Arc<Mutex<ControlsState>>,
    relay: Arc<ControlRelay>,
    running: Arc<AtomicBool>,
    path: PathBuf,
    #[cfg(windows)]
    listener_handle: Mutex<Option<JoinHandle<()>>>,
}

impl WheelControls {
    pub fn new(relay: Arc<ControlRelay>) -> Self {
        let path = crate::config::config_dir().join(controls::CONTROLS_FILE_NAME);
        let (config, last_error) = match controls::load_controls(&path) {
            Ok(config) => (config, None),
            Err(error) => (ControlsConfig::default(), Some(error)),
        };
        let mut state = ControlsState::new(config, path.clone());
        state.last_error = last_error;
        Self {
            state: Arc::new(Mutex::new(state)),
            relay,
            running: Arc::new(AtomicBool::new(false)),
            path,
            #[cfg(windows)]
            listener_handle: Mutex::new(None),
        }
    }

    pub fn relay(&self) -> &Arc<ControlRelay> {
        &self.relay
    }

    pub fn start(&self) {
        #[cfg(windows)]
        {
            let Ok(mut handle) = self.listener_handle.lock() else {
                return;
            };
            if self.running.load(Ordering::SeqCst) {
                return;
            }
            if let Some(previous) = handle.take() {
                let _ = previous.join();
            }

            self.running.store(true, Ordering::SeqCst);
            let receiver = publisher::controls_input::spawn(
                Arc::clone(&self.state),
                Arc::clone(&self.running),
            );
            let running = Arc::clone(&self.running);
            let relay = Arc::clone(&self.relay);
            let state = Arc::clone(&self.state);
            match std::thread::Builder::new()
                .name("wheel-controls-forward".to_string())
                .spawn(move || forward_requests(receiver, running, relay, state))
            {
                Ok(forwarder) => *handle = Some(forwarder),
                Err(error) => {
                    self.running.store(false, Ordering::SeqCst);
                    self.set_error(format!("could not start wheel control forwarder: {error}"));
                }
            }
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        if let Ok(mut state) = self.state.lock() {
            state.learning = None;
        }
        #[cfg(windows)]
        if let Ok(mut handle) = self.listener_handle.lock() {
            if let Some(forwarder) = handle.take() {
                let _ = forwarder.join();
            }
        }
    }

    pub fn learn(&self, action: ControlAction) {
        if let Ok(mut state) = self.state.lock() {
            state.learning = Some(action);
        }
    }

    pub fn cancel_learn(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.learning = None;
        }
    }

    pub fn clear(&self, action: ControlAction) {
        if let Ok(mut state) = self.state.lock() {
            state.clear_binding(action);
        }
    }

    pub fn status(&self) -> ControlsStatus {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let bindings = ControlAction::ALL
            .iter()
            .copied()
            .map(|action| {
                let binding = state.config.binding_for(action);
                BindingStatus {
                    action: action.as_str().to_string(),
                    label: action.label().to_string(),
                    device: binding.map(|binding| binding.device.clone()),
                    button: binding.map(|binding| binding.button),
                }
            })
            .collect();
        let last_captured = state.last_captured.as_ref().map(captured_status);
        let last_request = state.last_request.map(|(action, at_ms)| RequestStatus {
            action: action.as_str().to_string(),
            at_ms,
        });
        ControlsStatus {
            available: cfg!(windows),
            listening: state.listening,
            devices_seen: state.devices_seen,
            learning: state.learning.map(|action| action.as_str().to_string()),
            bindings,
            last_captured,
            last_request,
            last_error: state.last_error.clone(),
            path: self.path.to_string_lossy().into_owned(),
        }
    }

    #[cfg(windows)]
    fn set_error(&self, error: String) {
        if let Ok(mut state) = self.state.lock() {
            state.last_error = Some(error);
        }
    }
}

fn captured_status(capture: &CapturedBinding) -> CapturedStatus {
    CapturedStatus {
        action: capture.action.as_str().to_string(),
        device: capture.device.clone(),
        button: capture.button,
    }
}

#[cfg(windows)]
fn forward_requests(
    receiver: std::sync::mpsc::Receiver<ControlRequest>,
    running: Arc<AtomicBool>,
    relay: Arc<ControlRelay>,
    state: Arc<Mutex<ControlsState>>,
) {
    use std::sync::mpsc::RecvTimeoutError;

    while running.load(Ordering::SeqCst) {
        match receiver.recv_timeout(Duration::from_millis(100)) {
            Ok(request) => {
                let action = request.action;
                let device = request.device.clone();
                let button = request.button;
                if relay.send(request) {
                    tracing::info!(
                        action = action.as_str(),
                        %device,
                        button,
                        "relayed wheel control request"
                    );
                } else {
                    tracing::debug!(
                        action = action.as_str(),
                        %device,
                        button,
                        "dropped wheel control request because publisher engine is stopped"
                    );
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                if running.load(Ordering::SeqCst) {
                    running.store(false, Ordering::SeqCst);
                    if let Ok(mut state) = state.lock() {
                        if state.last_error.is_none() {
                            state.last_error =
                                Some("wheel input listener stopped unexpectedly".to_string());
                        }
                    }
                }
                break;
            }
        }
    }
}
