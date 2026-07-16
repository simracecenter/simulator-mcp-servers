// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared tool-capability-status type.
//!
//! Every `<sim>-mcp` handler's `tools/list` is a fixed surface shared across
//! simulators for agent-facing consistency (ADR 0001), but not every tool is
//! equally well-supported by every adapter — e.g. `lmu-mcp`'s `camera_focus`
//! works, while `replay_seek_session_time` doesn't (see
//! `docs/adr/0002-lmu-adapter-design.md`'s Amendment). Before this module
//! existed, an agent's only way to learn that was to call the tool and get a
//! `not_supported`/`not_yet_implemented` error back — wasting a turn.
//!
//! Each `<sim>-mcp` crate exposes a `get_capabilities` tool (no arguments)
//! returning `Vec<ToolCapability>` for its own tool set, so an agent can
//! check `tools/list` against `get_capabilities` once up front and plan
//! accordingly, instead of discovering gaps by trial and error.

use serde::Serialize;

/// How well a `tools/list` entry is actually supported by the *currently
/// active* adapter (not the trait/tool surface in the abstract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    /// Fully implemented; expected to work every time it's called.
    Supported,
    /// Implemented, but with known caveats (e.g. weak verification for some
    /// inputs, or partial data) — usable, just not fully trustworthy in
    /// every case. See the accompanying `reason`.
    Degraded,
    /// Present in `tools/list` for surface parity with another simulator's
    /// adapter, but calling it will always fail. See the accompanying
    /// `reason` for why, and where the gap is tracked.
    Unsupported,
}

/// One `tools/list` entry's real-world support status for the active
/// adapter. Returned in bulk by each `<sim>-mcp` crate's `get_capabilities`
/// tool.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCapability {
    pub name: &'static str,
    pub status: CapabilityStatus,
    /// Required whenever `status` isn't `Supported` — explains the gap and,
    /// where applicable, which issue/ADR section tracks it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
}

impl ToolCapability {
    pub fn supported(name: &'static str) -> Self {
        Self {
            name,
            status: CapabilityStatus::Supported,
            reason: None,
        }
    }

    pub fn degraded(name: &'static str, reason: &'static str) -> Self {
        Self {
            name,
            status: CapabilityStatus::Degraded,
            reason: Some(reason),
        }
    }

    pub fn unsupported(name: &'static str, reason: &'static str) -> Self {
        Self {
            name,
            status: CapabilityStatus::Unsupported,
            reason: Some(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_with_snake_case_status_and_omits_absent_reason() {
        let supported = ToolCapability::supported("get_standings");
        let value = serde_json::to_value(&supported).unwrap();
        assert_eq!(value["status"], "supported");
        assert!(value.get("reason").is_none());

        let unsupported = ToolCapability::unsupported("replay_seek_session_time", "see #9");
        let value = serde_json::to_value(&unsupported).unwrap();
        assert_eq!(value["status"], "unsupported");
        assert_eq!(value["reason"], "see #9");
    }
}
