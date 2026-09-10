// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use mcp_core::metadata::finalize_tool_result;
use mcp_core::{JsonRpcRequest, JsonRpcResponse, McpHandler, SnapshotMeta, ToolCapability};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{
    default_exe_path, normalize_fingerprint, validate_ingest_url, PublisherConfig,
    PublisherConfigStore,
};
use crate::secret::{default_token_store, SecretString, TokenStore};
use crate::supervisor::{ExeLauncher, LaunchSpec, PublisherState, Supervisor, SupervisorStatus};

pub const MUTATING_TOOLS: &[&str] = &["publisher_configure", "publisher_start", "publisher_stop"];
pub const READ_ONLY_TOOLS: &[&str] = &["publisher_status", "get_capabilities"];

pub struct PublisherMcpHandler {
    pub(crate) supervisor: Arc<Supervisor>,
    pub(crate) config: Arc<dyn PublisherConfigStore>,
    pub(crate) tokens: Arc<dyn TokenStore>,
}

impl PublisherMcpHandler {
    pub fn new(
        supervisor: Arc<Supervisor>,
        config: Arc<dyn PublisherConfigStore>,
        tokens: Arc<dyn TokenStore>,
    ) -> Self {
        Self {
            supervisor,
            config,
            tokens,
        }
    }

    pub fn with_defaults(config: Arc<dyn PublisherConfigStore>) -> Self {
        let exe = config
            .load()
            .ok()
            .and_then(|config| config.exe_path)
            .unwrap_or_else(default_exe_path);
        Self::new(
            Arc::new(Supervisor::new(Box::new(ExeLauncher::new(exe)))),
            config,
            default_token_store(),
        )
    }

    fn error(id: Option<Value>, code: i32, message: impl Into<String>) -> JsonRpcResponse {
        JsonRpcResponse::err(id, code, message)
    }

    fn tool_result(id: Option<Value>, data: Value, is_error: bool) -> JsonRpcResponse {
        let payload = if is_error {
            json!({
                "ok": false,
                "data": data,
                "warnings": [],
                "error": null
            })
        } else {
            json!({
                "ok": true,
                "data": data,
                "warnings": [],
                "error": null
            })
        };
        let text = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string());
        JsonRpcResponse::ok(
            id,
            json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": payload,
                "isError": is_error
            }),
        )
    }

    fn finalize(mut response: JsonRpcResponse, started: Instant) -> JsonRpcResponse {
        if let Some(result) = response.result.as_mut() {
            finalize_tool_result(
                result,
                SnapshotMeta::unavailable(),
                started.elapsed().as_millis() as u64,
            );
        }
        response
    }

    fn status_data(
        &self,
        config: PublisherConfig,
        token_configured: bool,
        supervisor: SupervisorStatus,
    ) -> Value {
        let exe_path = self.supervisor.exe_path().to_path_buf();
        json!({
            "state": supervisor.state,
            "pid": supervisor.pid,
            "uptimeSeconds": supervisor.uptime_seconds,
            "lastExitCode": supervisor.last_exit_code,
            "ingestUrl": config.ingest_url,
            "certFingerprint": config.cert_fingerprint,
            "driverDisplayName": config.driver_display_name,
            "exePath": exe_path,
            "exePresent": exe_path.is_file(),
            "tokenConfigured": token_configured,
            "publisherVersion": null,
            "lastHeartbeat": null
        })
    }

    fn current_status(&self) -> Result<Value, JsonRpcResponse> {
        let config = self.config.load().map_err(|error| {
            Self::error(
                None,
                -32603,
                format!("failed to load publisher config: {error}"),
            )
        })?;
        let token_configured = self
            .tokens
            .load()
            .map_err(|error| {
                Self::error(
                    None,
                    -32603,
                    format!("failed to load publisher token: {error}"),
                )
            })?
            .is_some_and(|token| !token.is_empty());
        Ok(self.status_data(config, token_configured, self.supervisor.status()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InMemoryConfigStore;
    use crate::secret::InMemoryTokenStore;
    use crate::supervisor::ExeLauncher;
    use std::path::PathBuf;

    fn handler() -> PublisherMcpHandler {
        PublisherMcpHandler::new(
            Arc::new(Supervisor::new(Box::new(ExeLauncher::new(PathBuf::from(
                "publisher.exe",
            ))))),
            Arc::new(InMemoryConfigStore::default()),
            Arc::new(InMemoryTokenStore::default()),
        )
    }

    fn request(name: &str, arguments: Value) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(json!(1)),
            method: "tools/call".to_string(),
            params: json!({"name": name, "arguments": arguments}),
        }
    }

    #[tokio::test]
    async fn status_and_configure_never_return_token() {
        let handler = handler();
        let token = "do-not-return-this-token";
        let response = handler
            .handle(request(
                "publisher_configure",
                json!({
                    "ingestUrl": "https://director.example.com",
                    "token": token,
                    "certFingerprint": "AA:bb:CC:dd:EE:ff:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99"
                }),
            ))
            .await;
        let serialized = serde_json::to_string(&response).unwrap();
        assert!(!serialized.contains(token));

        let response = handler.handle(request("publisher_status", json!({}))).await;
        let serialized = serde_json::to_string(&response).unwrap();
        assert!(!serialized.contains(token));
        assert_eq!(
            response.result.unwrap()["structuredContent"]["data"]["tokenConfigured"],
            true
        );
    }

    #[tokio::test]
    async fn invalid_configure_returns_invalid_params_without_token() {
        let handler = handler();
        let token = "do-not-return-this-token";
        let response = handler
            .handle(request(
                "publisher_configure",
                json!({"ingestUrl": "http://192.168.1.10:9000/ingest", "token": token}),
            ))
            .await;
        assert_eq!(response.error.as_ref().unwrap().code, -32602);
        assert!(!serde_json::to_string(&response).unwrap().contains(token));
    }

    #[tokio::test]
    async fn start_without_configuration_returns_invalid_params() {
        let response = handler()
            .handle(request("publisher_start", json!({})))
            .await;
        assert_eq!(response.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn tools_list_has_exactly_five_tools() {
        let response = handler()
            .handle(JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "tools/list".to_string(),
                params: Value::Null,
            })
            .await;
        assert_eq!(
            response.result.unwrap()["tools"].as_array().unwrap().len(),
            5
        );
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ConfigureArgs {
    ingest_url: String,
    token: String,
    cert_fingerprint: Option<String>,
    driver_display_name: Option<String>,
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "publisher_status",
            "description": "Returns telemetry publisher process state and configuration. Publisher version and heartbeat are null until the child exposes a status signal.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "publisher_configure",
            "description": "Stores the Director ingest endpoint, token, and optional certificate fingerprint/display name. Configuration changes do not restart a running publisher.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ingestUrl": {"type": "string"},
                    "token": {"type": "string"},
                    "certFingerprint": {"type": "string"},
                    "driverDisplayName": {"type": "string"}
                },
                "required": ["ingestUrl", "token"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "publisher_start",
            "description": "Starts the configured telemetry publisher process.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "publisher_stop",
            "description": "Stops the telemetry publisher process.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "get_capabilities",
            "description": "Returns the tools supported by the publisher role.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
    ]
}

fn capabilities() -> Vec<ToolCapability> {
    vec![
        ToolCapability::supported("publisher_status"),
        ToolCapability::supported("publisher_configure"),
        ToolCapability::supported("publisher_start"),
        ToolCapability::supported("publisher_stop"),
        ToolCapability::supported("get_capabilities"),
    ]
}

#[async_trait]
impl McpHandler for PublisherMcpHandler {
    async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        if request.jsonrpc != "2.0" {
            return Self::error(request.id, -32600, "invalid request: jsonrpc must be 2.0");
        }

        match request.method.as_str() {
            "initialize" => JsonRpcResponse::ok(
                request.id,
                json!({
                    "protocolVersion": "2025-06-18",
                    "serverInfo": {"name": "publisher-mcp", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {"tools": {"listChanged": true}}
                }),
            ),
            "tools/list" => JsonRpcResponse::ok(request.id, json!({"tools": tool_descriptors()})),
            "tools/call" => self.tools_call(request.id, request.params).await,
            _ => Self::error(request.id, -32601, "method not found"),
        }
    }
}

impl PublisherMcpHandler {
    async fn tools_call(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let started = Instant::now();
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let response = match name {
            "publisher_status" => match self.current_status() {
                Ok(data) => Self::tool_result(id, data, false),
                Err(mut error) => {
                    error.id = id.unwrap_or(Value::Null);
                    error
                }
            },
            "publisher_configure" => self.configure(id, params),
            "publisher_start" => self.start(id).await,
            "publisher_stop" => self.stop(id).await,
            "get_capabilities" => Self::tool_result(id, json!(capabilities()), false),
            _ => Self::error(id, -32602, "unknown tool name"),
        };
        Self::finalize(response, started)
    }

    fn configure(&self, id: Option<Value>, params: Value) -> JsonRpcResponse {
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        let args: ConfigureArgs = match serde_json::from_value(arguments) {
            Ok(args) => args,
            Err(error) => {
                return Self::error(
                    id,
                    -32602,
                    format!("invalid publisher_configure arguments: {error}"),
                )
            }
        };
        if args.token.is_empty() {
            return Self::error(id, -32602, "publisher token must be non-empty");
        }
        if let Err(error) = validate_ingest_url(&args.ingest_url) {
            return Self::error(id, -32602, error);
        }
        let fingerprint = match args.cert_fingerprint {
            Some(raw) => match normalize_fingerprint(&raw) {
                Ok(value) => Some(value),
                Err(error) => return Self::error(id, -32602, error),
            },
            None => None,
        };

        let mut config = match self.config.load() {
            Ok(config) => config,
            Err(error) => {
                return Self::error(
                    id,
                    -32603,
                    format!("failed to load publisher config: {error}"),
                )
            }
        };
        let restart_required = matches!(
            self.supervisor.status().state,
            PublisherState::Starting | PublisherState::Running
        );
        if let Err(error) = self.tokens.save(&args.token) {
            return Self::error(
                id,
                -32603,
                format!("failed to save publisher token: {error}"),
            );
        }
        config.ingest_url = Some(args.ingest_url);
        config.cert_fingerprint = fingerprint;
        config.driver_display_name = args.driver_display_name;
        if let Err(error) = self.config.save(&config) {
            return Self::error(
                id,
                -32603,
                format!("failed to save publisher config: {error}"),
            );
        }
        let mut data = self.status_data(config, true, self.supervisor.status());
        data["restartRequired"] = Value::Bool(restart_required);
        Self::tool_result(id, data, false)
    }

    async fn start(&self, id: Option<Value>) -> JsonRpcResponse {
        let config = match self.config.load() {
            Ok(config) => config,
            Err(error) => {
                return Self::error(
                    id,
                    -32603,
                    format!("failed to load publisher config: {error}"),
                )
            }
        };
        let token = match self.tokens.load() {
            Ok(Some(token)) if !token.is_empty() => token,
            Ok(_) => {
                return Self::error(
                    id,
                    -32602,
                    "publisher is not configured; call publisher_configure first",
                )
            }
            Err(error) => {
                return Self::error(
                    id,
                    -32603,
                    format!("failed to load publisher token: {error}"),
                )
            }
        };
        let ingest_url = match config.ingest_url.clone() {
            Some(url) => url,
            None => {
                return Self::error(
                    id,
                    -32602,
                    "publisher is not configured; call publisher_configure first",
                )
            }
        };
        let exe = self.supervisor.exe_path().to_path_buf();
        if !exe.is_file() {
            return Self::error(
                id,
                -32603,
                format!("publisher executable not found: {}", exe.display()),
            );
        }
        let spec = LaunchSpec {
            ingest_url,
            token: SecretString::new(token),
            cert_fingerprint: config.cert_fingerprint.clone(),
            driver_display_name: config.driver_display_name.clone(),
        };
        let supervisor = Arc::clone(&self.supervisor);
        match tokio::task::spawn_blocking(move || supervisor.start(&spec)).await {
            Ok(Ok(status)) => {
                let data = self.status_data(config, true, status);
                Self::tool_result(id, data, false)
            }
            Ok(Err(error)) => {
                Self::error(id, -32603, format!("failed to start publisher: {error}"))
            }
            Err(error) => Self::error(id, -32603, format!("publisher start task failed: {error}")),
        }
    }

    async fn stop(&self, id: Option<Value>) -> JsonRpcResponse {
        let supervisor = Arc::clone(&self.supervisor);
        match tokio::task::spawn_blocking(move || supervisor.stop()).await {
            Ok(status) => match self.config.load() {
                Ok(config) => Self::tool_result(
                    id,
                    self.status_data(config, self.tokens.load().ok().flatten().is_some(), status),
                    false,
                ),
                Err(error) => Self::error(
                    id,
                    -32603,
                    format!("failed to load publisher config: {error}"),
                ),
            },
            Err(error) => Self::error(id, -32603, format!("publisher stop task failed: {error}")),
        }
    }
}
