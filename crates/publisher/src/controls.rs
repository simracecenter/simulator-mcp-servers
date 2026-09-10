// SPDX-License-Identifier: GPL-3.0-or-later

//! Driver rig controls — steering-wheel button bindings and request debouncing.
//!
//! The publisher is the only software deployed on a driver's rig, so it also
//! owns the wheel buttons drivers use to talk to the broadcast agent:
//!
//! * `focus_me` — ask the auto-broadcast agent to cut to this driver's onboard.
//! * `broadcast_toggle` — pause/resume the auto-broadcast agent.
//!
//! This module holds the platform-independent half of the feature: binding
//! storage (`controls.toml`), press → request state machine (edge detection,
//! debounce, per-action cooldown) and the shared state the UI uses to bind a
//! button. The Raw Input plumbing lives in [`crate::controls_input`].

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Dwell the sandbox holds a `focus_me` shot pack on air, in milliseconds.
pub const FOCUS_DWELL_MS: u32 = 10_000;

/// File name that holds the learned bindings, alongside `publisher.toml`.
pub const CONTROLS_FILE_NAME: &str = "controls.toml";

/// Source label carried in the published payload.
pub const SOURCE_WHEEL_BUTTON: &str = "wheel_button";
/// Source label used by `--simulate`.
pub const SOURCE_SIMULATED: &str = "simulated";

// ── Actions ───────────────────────────────────────────────────────────────────

/// A control a driver can bind a wheel button to.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    /// Insert this driver's onboard into the broadcast cycle.
    FocusMe,
    /// Pause/resume the auto-broadcast agent.
    BroadcastToggle,
}

impl ControlAction {
    pub const ALL: [ControlAction; 2] = [ControlAction::FocusMe, ControlAction::BroadcastToggle];

    /// Stable identifier used in config files and CLI flags.
    pub fn as_str(self) -> &'static str {
        match self {
            ControlAction::FocusMe => "focus_me",
            ControlAction::BroadcastToggle => "broadcast_toggle",
        }
    }

    /// Label shown in the publisher window.
    pub fn label(self) -> &'static str {
        match self {
            ControlAction::FocusMe => "Focus on me",
            ControlAction::BroadcastToggle => "Pause / resume auto broadcast",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "focus_me" | "focus-me" | "focusme" => Some(ControlAction::FocusMe),
            "broadcast_toggle" | "broadcast-toggle" | "toggle" => {
                Some(ControlAction::BroadcastToggle)
            }
            _ => None,
        }
    }
}

impl fmt::Display for ControlAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── Bindings ──────────────────────────────────────────────────────────────────

/// A physical wheel button bound to a control action.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonBinding {
    pub action: ControlAction,
    /// Raw Input device identity — `VID_xxxx&PID_xxxx` for the wheel/base.
    pub device: String,
    /// HID button usage index (1-based, as reported by the device).
    pub button: u16,
}

impl ButtonBinding {
    /// A binding matches a press when the button index is identical and the
    /// device identity of the press contains the bound identity. Raw Input
    /// device paths carry an instance suffix that changes when the wheel is
    /// re-plugged, so `VID_0EB7&PID_0E04` keeps matching the same hardware.
    pub fn matches(&self, device: &str, button: u16) -> bool {
        self.button == button
            && !self.device.is_empty()
            && device
                .to_ascii_uppercase()
                .contains(&self.device.to_ascii_uppercase())
    }
}

/// Tunables + learned bindings, persisted in `controls.toml`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ControlsConfig {
    /// Master switch for wheel-button input.
    pub enabled: bool,
    /// Ignore repeat presses of the same button inside this window.
    pub debounce_ms: u64,
    /// Minimum interval between two accepted `focus_me` requests.
    pub focus_cooldown_ms: u64,
    /// Minimum interval between two accepted pause/resume requests.
    pub toggle_cooldown_ms: u64,
    pub bindings: Vec<ButtonBinding>,
}

impl Default for ControlsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            debounce_ms: 250,
            focus_cooldown_ms: 30_000,
            toggle_cooldown_ms: 3_000,
            bindings: Vec::new(),
        }
    }
}

impl ControlsConfig {
    pub fn binding_for(&self, action: ControlAction) -> Option<&ButtonBinding> {
        self.bindings.iter().find(|b| b.action == action)
    }

    /// Resolve the action bound to a press, if any.
    pub fn action_for(&self, device: &str, button: u16) -> Option<ControlAction> {
        self.bindings
            .iter()
            .find(|b| b.matches(device, button))
            .map(|b| b.action)
    }

    /// Replace the binding for `action`, keeping one binding per action.
    pub fn set_binding(&mut self, binding: ButtonBinding) {
        self.bindings.retain(|b| b.action != binding.action);
        self.bindings.push(binding);
    }

    pub fn clear_binding(&mut self, action: ControlAction) {
        self.bindings.retain(|b| b.action != action);
    }

    pub fn cooldown_ms(&self, action: ControlAction) -> u64 {
        match action {
            ControlAction::FocusMe => self.focus_cooldown_ms,
            ControlAction::BroadcastToggle => self.toggle_cooldown_ms,
        }
    }
}

/// Where the controls file lives: next to `publisher.toml` when its path is
/// known, otherwise next to the executable, otherwise the working directory.
pub fn controls_path(config_path: Option<&Path>) -> PathBuf {
    if let Some(dir) = config_path
        .and_then(|p| p.parent())
        .filter(|d| !d.as_os_str().is_empty())
    {
        return dir.join(CONTROLS_FILE_NAME);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            return dir.join(CONTROLS_FILE_NAME);
        }
    }
    PathBuf::from(CONTROLS_FILE_NAME)
}

/// Load bindings, falling back to defaults when the file is absent.
///
/// A malformed file is reported rather than silently discarded so the driver
/// can see why their button stopped working.
pub fn load_controls(path: &Path) -> Result<ControlsConfig, String> {
    let text = match fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ControlsConfig::default()),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

/// Write bindings back out. Called whenever the UI learns or clears a button.
pub fn save_controls(path: &Path, cfg: &ControlsConfig) -> Result<(), String> {
    let text = toml::to_string_pretty(cfg).map_err(|e| e.to_string())?;
    let body = format!(
        "# Driver rig controls — written by the publisher UI.\n\
         # `device` matches the Raw Input device path by substring, so a\n\
         # VID/PID pair keeps working after the wheel is re-plugged.\n\n{text}"
    );
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    fs::write(path, body).map_err(|e| format!("{}: {e}", path.display()))
}

// ── Press → request state machine ─────────────────────────────────────────────

/// A physical button transition observed by the input backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ButtonPress {
    pub device: String,
    pub button: u16,
}

/// An accepted control request, ready to be published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlRequest {
    pub action: ControlAction,
    /// Idempotency key — reused verbatim on transport retries.
    pub request_id: String,
    /// Monotonic per-process counter, so the sandbox can order two presses
    /// that land on the same simulator tick.
    pub press_seq: u64,
    pub device: String,
    pub button: u16,
    pub source: String,
    /// Wall-clock milliseconds since the Unix epoch.
    pub requested_at_ms: i64,
}

static PRESS_SEQ: AtomicU64 = AtomicU64::new(0);

fn next_press_seq() -> u64 {
    PRESS_SEQ.fetch_add(1, Ordering::SeqCst) + 1
}

/// Turns raw button transitions into control requests.
///
/// Only key-down transitions produce requests (a held button is reported by
/// Raw Input on every report, so the backend passes each report through and
/// debounce collapses them). Time is injected in milliseconds from a monotonic
/// clock so the logic is testable without sleeping.
#[derive(Debug, Default)]
pub struct ControlDispatcher {
    last_press_ms: Vec<((String, u16), u64)>,
    last_accept_ms: Vec<(ControlAction, u64)>,
}

impl ControlDispatcher {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate one button-down transition against the current bindings.
    ///
    /// Returns `None` when the button is unbound, still inside its debounce
    /// window, or the action is still cooling down.
    pub fn on_button_down(
        &mut self,
        cfg: &ControlsConfig,
        press: &ButtonPress,
        now_ms: u64,
        wall_clock_ms: i64,
    ) -> Option<ControlRequest> {
        if !cfg.enabled {
            return None;
        }
        let action = cfg.action_for(&press.device, press.button)?;

        let key = (press.device.clone(), press.button);
        if let Some(entry) = self.last_press_ms.iter_mut().find(|(k, _)| *k == key) {
            if now_ms.saturating_sub(entry.1) < cfg.debounce_ms {
                return None;
            }
            entry.1 = now_ms;
        } else {
            self.last_press_ms.push((key, now_ms));
        }

        let cooldown = cfg.cooldown_ms(action);
        if let Some(entry) = self.last_accept_ms.iter_mut().find(|(a, _)| *a == action) {
            if now_ms.saturating_sub(entry.1) < cooldown {
                return None;
            }
            entry.1 = now_ms;
        } else {
            self.last_accept_ms.push((action, now_ms));
        }

        Some(ControlRequest {
            action,
            request_id: Uuid::new_v4().to_string(),
            press_seq: next_press_seq(),
            device: press.device.clone(),
            button: press.button,
            source: SOURCE_WHEEL_BUTTON.to_owned(),
            requested_at_ms: wall_clock_ms,
        })
    }
}

/// Build a request that did not come from hardware (`--simulate <action>`).
pub fn simulated_request(action: ControlAction, wall_clock_ms: i64) -> ControlRequest {
    ControlRequest {
        action,
        request_id: Uuid::new_v4().to_string(),
        press_seq: next_press_seq(),
        device: String::new(),
        button: 0,
        source: SOURCE_SIMULATED.to_owned(),
        requested_at_ms: wall_clock_ms,
    }
}

/// Wall-clock milliseconds since the Unix epoch.
pub fn now_wall_clock_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ── Shared UI state ───────────────────────────────────────────────────────────

/// A binding captured by the input backend while in learn mode.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedBinding {
    pub action: ControlAction,
    pub device: String,
    pub button: u16,
}

/// State shared between the publisher UI (writer of `learning`), the input
/// backend (writer of everything else) and the pipeline thread.
#[derive(Debug)]
pub struct ControlsState {
    pub config: ControlsConfig,
    pub config_path: PathBuf,
    /// Set by the UI when the driver clicks a "Bind" button; cleared by the
    /// input backend once the next wheel button is captured.
    pub learning: Option<ControlAction>,
    /// Most recent capture, shown as confirmation in the UI.
    pub last_captured: Option<CapturedBinding>,
    /// Most recently accepted request (action, wall-clock ms) for the UI.
    pub last_request: Option<(ControlAction, i64)>,
    /// Number of HID devices Raw Input is listening to.
    pub devices_seen: usize,
    /// True once the input backend has registered for Raw Input. Until then a
    /// `Bind` click cannot capture anything, so the UI must not claim to be
    /// listening.
    pub listening: bool,
    /// Last input/persistence error, surfaced in the UI.
    pub last_error: Option<String>,
}

impl ControlsState {
    pub fn new(config: ControlsConfig, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            learning: None,
            last_captured: None,
            last_request: None,
            devices_seen: 0,
            listening: false,
            last_error: None,
        }
    }

    /// Store a learned binding and persist it for future sessions.
    pub fn apply_capture(&mut self, capture: CapturedBinding) {
        self.config.set_binding(ButtonBinding {
            action: capture.action,
            device: capture.device.clone(),
            button: capture.button,
        });
        self.learning = None;
        self.last_captured = Some(capture);
        self.persist();
    }

    pub fn clear_binding(&mut self, action: ControlAction) {
        self.config.clear_binding(action);
        if self.learning == Some(action) {
            self.learning = None;
        }
        self.persist();
    }

    fn persist(&mut self) {
        match save_controls(&self.config_path, &self.config) {
            Ok(()) => self.last_error = None,
            Err(e) => self.last_error = Some(format!("could not save bindings — {e}")),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const WHEEL: &str = r"\\?\HID#VID_0EB7&PID_0E04&Col01#7&1234abcd&0&0000#{4d1e55b2}";
    const ASETEK: &str = r"\\?\HID#VID_2433&PID_F300#8&2b3c4d5e&0&0000#{4d1e55b2}";

    fn cfg() -> ControlsConfig {
        let mut c = ControlsConfig::default();
        c.set_binding(ButtonBinding {
            action: ControlAction::FocusMe,
            device: "VID_0EB7&PID_0E04".to_owned(),
            button: 7,
        });
        c.set_binding(ButtonBinding {
            action: ControlAction::BroadcastToggle,
            device: "VID_2433&PID_F300".to_owned(),
            button: 3,
        });
        c
    }

    fn press(device: &str, button: u16) -> ButtonPress {
        ButtonPress {
            device: device.to_owned(),
            button,
        }
    }

    #[test]
    fn binding_matches_device_by_vid_pid_substring() {
        let c = cfg();
        assert_eq!(c.action_for(WHEEL, 7), Some(ControlAction::FocusMe));
        assert_eq!(
            c.action_for(ASETEK, 3),
            Some(ControlAction::BroadcastToggle)
        );
        // Same button index on the other device is not the bound control.
        assert_eq!(c.action_for(ASETEK, 7), None);
        assert_eq!(c.action_for(WHEEL, 8), None);
    }

    #[test]
    fn unknown_device_is_ignored() {
        let mut d = ControlDispatcher::new();
        let unknown = press(r"\\?\HID#VID_DEAD&PID_BEEF#1&0&0000", 7);
        assert!(d.on_button_down(&cfg(), &unknown, 1_000, 0).is_none());
    }

    #[test]
    fn held_button_reports_produce_one_request() {
        let mut d = ControlDispatcher::new();
        let c = cfg();
        let p = press(WHEEL, 7);
        assert!(d.on_button_down(&c, &p, 1_000, 0).is_some());
        // Raw Input repeats the report while the button is held.
        for t in [1_010, 1_050, 1_200] {
            assert!(d.on_button_down(&c, &p, t, 0).is_none(), "held at {t}");
        }
    }

    #[test]
    fn focus_requests_respect_cooldown_then_are_accepted_again() {
        let mut d = ControlDispatcher::new();
        let c = cfg();
        let p = press(WHEEL, 7);
        let first = d
            .on_button_down(&c, &p, 0, 0)
            .expect("first press accepted");
        // Past debounce but inside the 30 s cooldown.
        assert!(d.on_button_down(&c, &p, 5_000, 0).is_none());
        let second = d
            .on_button_down(&c, &p, 30_000, 0)
            .expect("accepted after cooldown");
        assert_ne!(first.request_id, second.request_id);
        assert!(second.press_seq > first.press_seq);
    }

    #[test]
    fn toggle_cooldown_is_independent_of_focus_cooldown() {
        let mut d = ControlDispatcher::new();
        let c = cfg();
        assert!(d.on_button_down(&c, &press(WHEEL, 7), 0, 0).is_some());
        let toggle = d
            .on_button_down(&c, &press(ASETEK, 3), 100, 0)
            .expect("toggle is a different action");
        assert_eq!(toggle.action, ControlAction::BroadcastToggle);
        assert!(d.on_button_down(&c, &press(ASETEK, 3), 1_000, 0).is_none());
        assert!(d.on_button_down(&c, &press(ASETEK, 3), 3_100, 0).is_some());
    }

    #[test]
    fn disabled_controls_never_dispatch() {
        let mut c = cfg();
        c.enabled = false;
        let mut d = ControlDispatcher::new();
        assert!(d.on_button_down(&c, &press(WHEEL, 7), 0, 0).is_none());
    }

    #[test]
    fn request_carries_source_and_wall_clock() {
        let mut d = ControlDispatcher::new();
        let req = d
            .on_button_down(&cfg(), &press(WHEEL, 7), 0, 1_700_000_000_000)
            .unwrap();
        assert_eq!(req.source, SOURCE_WHEEL_BUTTON);
        assert_eq!(req.requested_at_ms, 1_700_000_000_000);
        assert_eq!(req.button, 7);
        assert_eq!(req.action, ControlAction::FocusMe);
    }

    #[test]
    fn simulated_request_is_labelled() {
        let req = simulated_request(ControlAction::FocusMe, 42);
        assert_eq!(req.source, SOURCE_SIMULATED);
        assert_eq!(req.requested_at_ms, 42);
    }

    #[test]
    fn bindings_round_trip_through_toml() {
        let dir = std::env::temp_dir().join(format!("controls-{}", Uuid::new_v4()));
        let path = dir.join(CONTROLS_FILE_NAME);
        let original = cfg();
        save_controls(&path, &original).expect("save");
        let loaded = load_controls(&path).expect("load");
        assert_eq!(loaded.bindings, original.bindings);
        assert_eq!(loaded.debounce_ms, original.debounce_ms);
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn missing_file_yields_defaults() {
        let path = std::env::temp_dir().join(format!("absent-{}.toml", Uuid::new_v4()));
        let cfg = load_controls(&path).expect("missing file is not an error");
        assert!(cfg.bindings.is_empty());
        assert!(cfg.enabled);
    }

    #[test]
    fn apply_capture_replaces_binding_and_persists() {
        let dir = std::env::temp_dir().join(format!("controls-state-{}", Uuid::new_v4()));
        let path = dir.join(CONTROLS_FILE_NAME);
        let mut state = ControlsState::new(cfg(), path.clone());
        state.learning = Some(ControlAction::FocusMe);
        state.apply_capture(CapturedBinding {
            action: ControlAction::FocusMe,
            device: "VID_2433&PID_F300".to_owned(),
            button: 12,
        });
        assert_eq!(state.learning, None);
        assert_eq!(state.last_error, None);
        let binding = state.config.binding_for(ControlAction::FocusMe).unwrap();
        assert_eq!(binding.button, 12);
        assert_eq!(binding.device, "VID_2433&PID_F300");
        // One binding per action.
        assert_eq!(
            state
                .config
                .bindings
                .iter()
                .filter(|b| b.action == ControlAction::FocusMe)
                .count(),
            1
        );
        let reloaded = load_controls(&path).expect("persisted");
        assert_eq!(
            reloaded.binding_for(ControlAction::FocusMe).unwrap().button,
            12
        );
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn action_parse_accepts_cli_spellings() {
        assert_eq!(
            ControlAction::parse("focus_me"),
            Some(ControlAction::FocusMe)
        );
        assert_eq!(
            ControlAction::parse("Broadcast-Toggle"),
            Some(ControlAction::BroadcastToggle)
        );
        assert_eq!(ControlAction::parse("nope"), None);
    }
}
