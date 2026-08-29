// SPDX-License-Identifier: GPL-3.0-or-later
//! Metadata shared by simulator tool-result envelopes.

use serde::{Deserialize, Serialize};

/// Snapshot context attached to every simulator tool result.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotMeta {
    pub session_tick: Option<i32>,
    pub session_time: Option<f64>,
    pub captured_at_unix_ms: Option<u64>,
    pub age_ms: Option<u64>,
    pub stale: Option<bool>,
    pub session_key: Option<String>,
    pub session_revision: Option<u64>,
    pub server_elapsed_ms: u64,
}

impl SnapshotMeta {
    pub fn unavailable() -> Self {
        Self {
            session_tick: None,
            session_time: None,
            captured_at_unix_ms: None,
            age_ms: None,
            stale: None,
            session_key: None,
            session_revision: None,
            server_elapsed_ms: 0,
        }
    }
}

/// A read value and the snapshot metadata from the same observation.
#[derive(Debug, Clone)]
pub struct Read<T> {
    pub data: T,
    pub meta: SnapshotMeta,
}

#[cfg(test)]
mod tests {
    use super::SnapshotMeta;
    use serde_json::Value;

    #[test]
    fn unavailable_metadata_serializes_every_field_as_null_except_elapsed() {
        let value = serde_json::to_value(SnapshotMeta::unavailable()).unwrap();
        let object = value.as_object().unwrap();

        assert_eq!(object.len(), 8);
        for key in [
            "sessionTick",
            "sessionTime",
            "capturedAtUnixMs",
            "ageMs",
            "stale",
            "sessionKey",
            "sessionRevision",
        ] {
            assert_eq!(object.get(key), Some(&Value::Null));
        }
        assert_eq!(object.get("serverElapsedMs"), Some(&Value::from(0)));
    }
}
