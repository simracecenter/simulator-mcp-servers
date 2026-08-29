// SPDX-License-Identifier: GPL-3.0-or-later
//! Metadata shared by simulator tool-result envelopes.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

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

/// Finalizes the metadata in an MCP tool result and keeps its text mirror in
/// sync with `structuredContent`.
pub fn finalize_tool_result(
    result: &mut Value,
    fallback_meta: SnapshotMeta,
    server_elapsed_ms: u64,
) {
    let Some(payload) = result
        .get_mut("structuredContent")
        .and_then(Value::as_object_mut)
    else {
        return;
    };

    let payload_text = {
        let meta = match payload.get_mut("meta") {
            Some(Value::Object(meta)) => meta,
            _ => {
                payload.insert("meta".to_string(), json!(fallback_meta));
                payload
                    .get_mut("meta")
                    .and_then(Value::as_object_mut)
                    .expect("serialized SnapshotMeta is an object")
            }
        };
        meta.insert("serverElapsedMs".to_string(), json!(server_elapsed_ms));
        serde_json::to_string(payload).unwrap_or_default()
    };

    if let Some(content) = result.get_mut("content").and_then(Value::as_array_mut) {
        if let Some(item) = content.iter_mut().find(|item| item.get("text").is_some()) {
            item["text"] = Value::String(payload_text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{finalize_tool_result, SnapshotMeta};
    use serde_json::{json, Value};

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

    #[test]
    fn finalizer_preserves_existing_metadata_and_syncs_text() {
        let mut result = json!({
            "content": [{"type": "text", "text": "stale"}],
            "structuredContent": {
                "ok": true,
                "data": {"value": 7},
                "meta": {
                    "sessionTick": 42,
                    "sessionTime": 12.5,
                    "capturedAtUnixMs": 1000,
                    "ageMs": 4,
                    "stale": false,
                    "sessionKey": "session",
                    "sessionRevision": 3,
                    "serverElapsedMs": 0
                }
            }
        });

        finalize_tool_result(&mut result, SnapshotMeta::unavailable(), 17);

        let meta = &result["structuredContent"]["meta"];
        assert_eq!(meta["sessionTick"], Value::from(42));
        assert_eq!(meta["sessionTime"], Value::from(12.5));
        assert_eq!(meta["capturedAtUnixMs"], Value::from(1000));
        assert_eq!(meta["ageMs"], Value::from(4));
        assert_eq!(meta["stale"], Value::from(false));
        assert_eq!(meta["sessionKey"], Value::from("session"));
        assert_eq!(meta["sessionRevision"], Value::from(3));
        assert_eq!(
            meta["serverElapsedMs"],
            Value::from(17),
            "only serverElapsedMs may change"
        );
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"]);
    }

    #[test]
    fn finalizer_inserts_fallback_metadata_and_syncs_text() {
        let mut result = json!({
            "content": [{"type": "text", "text": "stale"}],
            "structuredContent": {
                "ok": false,
                "data": null,
                "warnings": [],
                "error": {"code": "invalid_arguments"}
            }
        });
        let fallback = SnapshotMeta {
            session_tick: Some(5),
            session_time: Some(2.5),
            captured_at_unix_ms: Some(100),
            age_ms: Some(8),
            stale: Some(true),
            session_key: Some("fallback".to_string()),
            session_revision: Some(9),
            server_elapsed_ms: 0,
        };

        finalize_tool_result(&mut result, fallback, 23);

        assert_eq!(
            result["structuredContent"]["meta"]["sessionKey"],
            Value::from("fallback")
        );
        assert_eq!(
            result["structuredContent"]["meta"]["serverElapsedMs"],
            Value::from(23)
        );
        let text: Value =
            serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(text, result["structuredContent"]);
    }
}
