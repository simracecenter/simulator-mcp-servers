// SPDX-License-Identifier: GPL-3.0-or-later
//! Runtime provenance: what build is actually serving this MCP endpoint.
//!
//! The MCP `initialize` handshake only carries `serverInfo.name`/`version`,
//! and a client's pinned artifact metadata can only say what it *expected*
//! to launch. `get_runtime_provenance` lets a client (e.g. the Director
//! evidence exporter) record what the running process attests about itself:
//! the source revision and target stamped at build time, plus the SHA-256
//! of the executable image the process is running from. Fields that cannot
//! be determined are reported as `null` with an explicit reason, never
//! guessed.
//!
//! This is a self-attestation by a cooperating binary — it proves which
//! build is running, not that the host is uncompromised. Only the file name
//! of the executable is reported; its directory is deliberately omitted.

use std::fs::File;
use std::io::Read;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const TOOL_NAME: &str = "get_runtime_provenance";

const SOURCE_REVISION: &str = env!("SIMRACECENTER_SOURCE_REVISION");
const BUILD_TARGET: &str = env!("SIMRACECENTER_BUILD_TARGET");
const BUILD_PROFILE: &str = env!("SIMRACECENTER_BUILD_PROFILE");

/// Digest of the executable image the current process was started from.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ExecutableDigest {
    pub file_name: Option<String>,
    pub size_bytes: Option<u64>,
    pub sha256: Option<String>,
    /// Why `sha256` is `null`; `None` when the digest was computed.
    pub unavailable_reason: Option<String>,
}

/// Everything the running server can attest about its own build.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeProvenance {
    pub schema: &'static str,
    pub server_name: String,
    pub version: &'static str,
    pub source_revision: Option<&'static str>,
    pub source_revision_unavailable_reason: Option<&'static str>,
    pub build_target: Option<&'static str>,
    pub build_profile: Option<&'static str>,
    pub executable: ExecutableDigest,
    pub pid: u32,
    pub started_at_unix_ms: Option<u64>,
}

pub const SCHEMA: &str = "simracecenter.runtime-provenance/1";

fn non_empty(value: &'static str) -> Option<&'static str> {
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn digest_current_exe() -> ExecutableDigest {
    let path = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return ExecutableDigest {
                file_name: None,
                size_bytes: None,
                sha256: None,
                unavailable_reason: Some(format!("current_exe unavailable: {error}")),
            }
        }
    };
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    match File::open(&path) {
        Ok(mut file) => {
            let mut hasher = Sha256::new();
            let mut buffer = [0u8; 64 * 1024];
            let mut size: u64 = 0;
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        size += read as u64;
                        hasher.update(&buffer[..read]);
                    }
                    Err(error) => {
                        return ExecutableDigest {
                            file_name,
                            size_bytes: None,
                            sha256: None,
                            unavailable_reason: Some(format!("read failed: {error}")),
                        }
                    }
                }
            }
            ExecutableDigest {
                file_name,
                size_bytes: Some(size),
                sha256: Some(format!("{:x}", hasher.finalize())),
                unavailable_reason: None,
            }
        }
        Err(error) => ExecutableDigest {
            file_name,
            size_bytes: None,
            sha256: None,
            unavailable_reason: Some(format!("open failed: {error}")),
        },
    }
}

fn executable_digest() -> &'static ExecutableDigest {
    static DIGEST: OnceLock<ExecutableDigest> = OnceLock::new();
    DIGEST.get_or_init(digest_current_exe)
}

fn started_at_unix_ms() -> Option<u64> {
    static STARTED: OnceLock<Option<u64>> = OnceLock::new();
    *STARTED.get_or_init(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_millis() as u64)
    })
}

/// Pin the process start time. Call once early in `main` so the recorded
/// timestamp reflects startup rather than the first provenance request.
pub fn mark_process_start() {
    let _ = started_at_unix_ms();
}

/// Build the provenance record for the server identified by `server_name`
/// (the same string reported in `serverInfo.name`).
pub fn runtime_provenance(server_name: &str) -> RuntimeProvenance {
    let source_revision = non_empty(SOURCE_REVISION);
    RuntimeProvenance {
        schema: SCHEMA,
        server_name: server_name.to_owned(),
        version: env!("CARGO_PKG_VERSION"),
        source_revision,
        source_revision_unavailable_reason: source_revision
            .is_none()
            .then_some("built without GITHUB_SHA or a git checkout"),
        build_target: non_empty(BUILD_TARGET),
        build_profile: non_empty(BUILD_PROFILE),
        executable: executable_digest().clone(),
        pid: std::process::id(),
        started_at_unix_ms: started_at_unix_ms(),
    }
}

/// `tools/list` descriptor shared by every `<sim>-mcp` handler.
pub fn tool_descriptor() -> Value {
    json!({
        "name": TOOL_NAME,
        "description": "Returns build provenance attested by the running server process: version, source revision, build target, and the SHA-256 of the executable image it is running from. Unavailable fields are null with a reason.",
        "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provenance_hashes_the_running_executable() {
        let provenance = runtime_provenance("test-mcp");
        assert_eq!(provenance.schema, SCHEMA);
        assert_eq!(provenance.server_name, "test-mcp");
        assert_eq!(provenance.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(provenance.pid, std::process::id());
        assert!(provenance.started_at_unix_ms.is_some());
        let digest = &provenance.executable;
        assert!(digest.unavailable_reason.is_none(), "{digest:?}");
        assert_eq!(digest.sha256.as_ref().map(String::len), Some(64));
        assert!(digest.size_bytes.unwrap_or(0) > 0);
        let file_name = digest.file_name.as_deref().unwrap();
        assert!(!file_name.contains('/') && !file_name.contains('\\'));
    }

    #[test]
    fn provenance_is_stable_across_calls() {
        assert_eq!(
            runtime_provenance("a").executable,
            runtime_provenance("a").executable
        );
    }

    #[test]
    fn source_revision_is_null_only_with_a_reason() {
        let provenance = runtime_provenance("x");
        assert_eq!(
            provenance.source_revision.is_none(),
            provenance.source_revision_unavailable_reason.is_some()
        );
        if let Some(revision) = provenance.source_revision {
            assert!(
                revision.chars().all(|c| c.is_ascii_hexdigit()),
                "{revision}"
            );
        }
    }

    #[test]
    fn serialized_shape_uses_camel_case_and_explicit_nulls() {
        let value = serde_json::to_value(runtime_provenance("x")).unwrap();
        let object = value.as_object().unwrap();
        for key in [
            "schema",
            "serverName",
            "version",
            "sourceRevision",
            "sourceRevisionUnavailableReason",
            "buildTarget",
            "buildProfile",
            "executable",
            "pid",
            "startedAtUnixMs",
        ] {
            assert!(object.contains_key(key), "missing {key}");
        }
        let executable = object["executable"].as_object().unwrap();
        for key in ["fileName", "sizeBytes", "sha256", "unavailableReason"] {
            assert!(executable.contains_key(key), "missing executable.{key}");
        }
        assert_eq!(tool_descriptor()["name"], TOOL_NAME);
    }
}
