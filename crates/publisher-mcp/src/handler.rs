// SPDX-License-Identifier: GPL-3.0-or-later

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use mcp_core::{JsonRpcRequest, JsonRpcResponse, McpHandler, ToolCapability};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config::{PublisherConfig, PublisherConfigStore};
use crate::engine::{EngineState, LaunchSpec, PublisherEngine};
use crate::secret::{default_token_store, SecretString, TokenStore};

pub const MUTATING_TOOLS: &[&str] = &["publisher_configure", "publisher_start", "publisher_stop"];
pub const READ_ONLY_TOOLS: &[&str] = &["publisher_status", "get_capabilities"];

pub struct PublisherMcpHandler {
    pub(crate) engine: Arc<dyn PublisherEngine>,
    pub(crate) config_store: Arc<dyn PublisherConfigStore>,
    pub(crate) tokens: Arc<dyn TokenStore>,
}

impl fmt::Debug for PublisherMcpHandler {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublisherMcpHandler")
            .field("engine", &"<PublisherEngine>")
            .field("config_store", &"<PublisherConfigStore>")
            .finish()
    }
}

impl PublisherMcpHandler {
    pub fn new(
        engine: Arc<dyn PublisherEngine>,
        config_store: Arc<dyn PublisherConfigStore>,
    ) -> Self {
        Self::with_token_store(engine, config_store, default_token_store())
    }

    pub fn with_token_store(
        engine: Arc<dyn PublisherEngine>,
        config_store: Arc<dyn PublisherConfigStore>,
        tokens: Arc<dyn TokenStore>,
    ) -> Self {
        Self {
            engine,
            config_store,
            tokens,
        }
    }

    pub fn with_config_store(config_store: Arc<dyn PublisherConfigStore>) -> Self {
        #[cfg(windows)]
        let engine: Arc<dyn PublisherEngine> = Arc::new(crate::engine::InProcessEngine::default());
        #[cfg(not(windows))]
        let engine: Arc<dyn PublisherEngine> = Arc::new(crate::engine::UnsupportedEngine);
        Self::new(engine, config_store)
    }

    fn error(id: Option<Value>, code: i32, message: impl Into<String>) -> JsonRpcResponse {
        JsonRpcResponse::err(id, code, message)
    }

    fn tool_result(id: Option<Value>, data: Value, is_error: bool) -> JsonRpcResponse {
        let text = serde_json::to_string(&data).unwrap_or_else(|_| "{}".to_string());
        JsonRpcResponse::ok(
            id,
            json!({
                "content": [{"type": "text", "text": text}],
                "isError": is_error
            }),
        )
    }

    fn timestamp(value: Option<SystemTime>) -> Option<String> {
        value.map(publisher::headless::iso8601_utc)
    }

    fn status_data(&self, config: PublisherConfig, token_configured: bool) -> Value {
        let snapshot = self.engine.snapshot();
        let state = match snapshot.state {
            EngineState::Starting | EngineState::Running => "RUNNING",
            EngineState::Failed | EngineState::Unknown => "ERROR",
            EngineState::Stopped => "STOPPED",
        };
        let configured = token_configured && config.ingest_url.is_some();
        let mut data = serde_json::Map::from_iter([
            ("state".to_string(), json!(state)),
            ("configured".to_string(), json!(configured)),
            (
                "lastBatchAt".to_string(),
                json!(Self::timestamp(snapshot.last_post_at)),
            ),
        ]);
        if let Some(value) = config.driver_display_name {
            data.insert("driverDisplayName".to_string(), json!(value));
        }
        if let Some(value) = config.ingest_url {
            data.insert("ingestUrl".to_string(), json!(value));
        }
        let detail = match snapshot.state {
            EngineState::Starting => Some("waiting_for_iracing".to_string()),
            EngineState::Running => Some(format!(
                "connected sub_session={} queued={} calls={}/{}",
                snapshot
                    .sub_session_id
                    .map_or_else(|| "none".to_string(), |id| id.to_string()),
                snapshot.queued_events,
                snapshot.calls_total,
                snapshot.calls_failed
            )),
            EngineState::Failed | EngineState::Unknown => {
                snapshot.last_error.or(snapshot.last_error_kind)
            }
            EngineState::Stopped => None,
        };
        if let Some(detail) = detail {
            data.insert("detail".to_string(), json!(detail));
        }
        Value::Object(data)
    }

    fn current_status(&self) -> Result<Value, JsonRpcResponse> {
        let config = self.config_store.load().map_err(|error| {
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
        Ok(self.status_data(config, token_configured))
    }
}

impl Drop for PublisherMcpHandler {
    fn drop(&mut self) {
        self.engine.stop();
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
struct ConfigureArgs {
    ingest_url: String,
    token: String,
    cert_fingerprint: String,
    #[serde(default)]
    driver_display_name: Option<String>,
}

fn tool_descriptors() -> Vec<Value> {
    vec![
        json!({
            "name": "publisher_status",
            "description": "Returns in-process telemetry publisher state, connectivity, counters, and configuration.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "publisher_configure",
            "description": "Stores the local Director ingest endpoint, token, certificate fingerprint, and optional display name. Configuration changes do not restart a running publisher.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "ingest_url": {"type": "string"},
                    "token": {"type": "string", "minLength": 1},
                    "cert_fingerprint": {"type": "string"},
                    "driver_display_name": {"type": "string", "maxLength": 64}
                },
                "required": ["ingest_url", "token", "cert_fingerprint"],
                "additionalProperties": false
            }
        }),
        json!({
            "name": "publisher_start",
            "description": "Starts the configured in-process telemetry publisher engine.",
            "inputSchema": {"type": "object", "properties": {}, "additionalProperties": false}
        }),
        json!({
            "name": "publisher_stop",
            "description": "Stops the in-process telemetry publisher engine.",
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
        response
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
        if args
            .driver_display_name
            .as_ref()
            .is_some_and(|name| name.chars().count() > 64)
        {
            return Self::error(
                id,
                -32602,
                "driver_display_name must be at most 64 characters",
            );
        }
        let ingest_url = match publisher::config::validate_local_url(&args.ingest_url) {
            Ok(url) => url,
            Err(error) => return Self::error(id, -32602, error),
        };
        let fingerprint = match publisher::config::normalise_fingerprint(&args.cert_fingerprint) {
            Ok(value) => value,
            Err(error) => return Self::error(id, -32602, error),
        };
        let mut config = match self.config_store.load() {
            Ok(config) => config,
            Err(error) => {
                return Self::error(
                    id,
                    -32603,
                    format!("failed to load publisher config: {error}"),
                )
            }
        };
        config.ingest_url = Some(ingest_url);
        config.cert_fingerprint = Some(fingerprint);
        config.driver_display_name = args.driver_display_name;
        if let Err(error) = self.config_store.save(&config) {
            return Self::error(
                id,
                -32603,
                format!("failed to save publisher config: {error}"),
            );
        }
        if let Err(error) = self.tokens.save(&args.token) {
            return Self::error(
                id,
                -32603,
                format!("failed to save publisher token: {error}"),
            );
        }
        Self::tool_result(id, json!({"configured": true}), false)
    }

    async fn start(&self, id: Option<Value>) -> JsonRpcResponse {
        let config = match self.config_store.load() {
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
                return Self::tool_result(id, json!({"error": "Publisher is not configured"}), true)
            }
            Err(error) => {
                return Self::error(
                    id,
                    -32603,
                    format!("failed to load publisher token: {error}"),
                )
            }
        };
        let Some(ingest_url) = config.ingest_url.clone() else {
            return Self::tool_result(id, json!({"error": "Publisher is not configured"}), true);
        };
        let ingest_url = strip_ingest_suffix(&ingest_url);
        let spec = LaunchSpec {
            ingest_url,
            token: SecretString::new(token),
            cert_fingerprint: config.cert_fingerprint.clone(),
        };
        let engine = Arc::clone(&self.engine);
        match tokio::task::spawn_blocking(move || engine.start(spec)).await {
            Ok(Ok(())) => Self::tool_result(id, json!({"state": "RUNNING"}), false),
            Ok(Err(error)) => Self::tool_result(id, json!({"error": error}), true),
            Err(error) => Self::tool_result(
                id,
                json!({"error": format!("publisher start task failed: {error}")}),
                true,
            ),
        }
    }

    async fn stop(&self, id: Option<Value>) -> JsonRpcResponse {
        let engine = Arc::clone(&self.engine);
        match tokio::task::spawn_blocking(move || engine.stop()).await {
            Ok(()) => Self::tool_result(id, json!({"state": "STOPPED"}), false),
            Err(error) => Self::tool_result(
                id,
                json!({"error": format!("publisher stop task failed: {error}")}),
                true,
            ),
        }
    }
}

fn strip_ingest_suffix(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    trimmed
        .strip_suffix("/api/publisher/v2/ingest")
        .unwrap_or(trimmed)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::InMemoryConfigStore;
    use crate::secret::InMemoryTokenStore;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeEngine {
        starts: Mutex<Vec<LaunchSpec>>,
        snapshot: Mutex<crate::engine::EngineSnapshot>,
        stops: Mutex<usize>,
        failure: Mutex<Option<String>>,
    }

    impl PublisherEngine for FakeEngine {
        fn start(&self, spec: LaunchSpec) -> Result<(), String> {
            if let Some(error) = self.failure.lock().unwrap().clone() {
                let mut snapshot = self.snapshot.lock().unwrap();
                snapshot.state = EngineState::Failed;
                snapshot.last_error = Some(error.clone());
                return Err(error);
            }
            if self.snapshot.lock().unwrap().state == EngineState::Running {
                return Ok(());
            }
            self.starts.lock().unwrap().push(spec);
            self.snapshot.lock().unwrap().state = EngineState::Running;
            Ok(())
        }

        fn stop(&self) {
            *self.stops.lock().unwrap() += 1;
            self.snapshot.lock().unwrap().state = EngineState::Stopped;
        }

        fn snapshot(&self) -> crate::engine::EngineSnapshot {
            self.snapshot.lock().unwrap().clone()
        }
    }

    fn handler(engine: Arc<FakeEngine>) -> PublisherMcpHandler {
        PublisherMcpHandler::with_token_store(
            engine,
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
        let handler = handler(Arc::new(FakeEngine::default()));
        let token = "do-not-return-this-token";
        let response = handler
            .handle(request(
                "publisher_configure",
                json!({"ingest_url": "https://director.example.com/api/publisher/v2/ingest", "token": token, "cert_fingerprint": "aa".repeat(32)}),
            ))
            .await;
        assert!(!serde_json::to_string(&response).unwrap().contains(token));
        assert_eq!(
            response.result.unwrap()["content"][0]["text"],
            "{\"configured\":true}"
        );
        let response = handler.handle(request("publisher_status", json!({}))).await;
        assert!(!serde_json::to_string(&response).unwrap().contains(token));
    }

    #[tokio::test]
    async fn invalid_configuration_is_rejected() {
        let handler = handler(Arc::new(FakeEngine::default()));
        let response = handler
            .handle(request(
                "publisher_configure",
                json!({"ingest_url": "http://192.168.1.10:9000", "token": "secret", "cert_fingerprint": "aa".repeat(32)}),
            ))
            .await;
        assert_eq!(response.error.unwrap().code, -32602);
    }

    #[tokio::test]
    async fn start_without_configuration_returns_invalid_params() {
        let response = handler(Arc::new(FakeEngine::default()))
            .handle(request("publisher_start", json!({})))
            .await;
        let text = response.result.unwrap()["content"][0]["text"].clone();
        assert_eq!(text, "{\"error\":\"Publisher is not configured\"}");
    }

    #[tokio::test]
    async fn start_passes_secret_to_engine_and_maps_running() {
        let engine = Arc::new(FakeEngine::default());
        let handler = handler(Arc::clone(&engine));
        handler
            .handle(request(
                "publisher_configure",
                json!({"ingest_url": "https://director.example.com/api/publisher/v2/ingest", "token": "secret", "cert_fingerprint": "aa".repeat(32)}),
            ))
            .await;
        let response = handler.handle(request("publisher_start", json!({}))).await;
        assert_eq!(
            serde_json::from_str::<Value>(
                response.result.unwrap()["content"][0]["text"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap()["state"],
            "RUNNING"
        );
        assert_eq!(engine.starts.lock().unwrap()[0].token.as_str(), "secret");
    }

    #[tokio::test]
    async fn stop_maps_stopped() {
        let response = handler(Arc::new(FakeEngine::default()))
            .handle(request("publisher_stop", json!({})))
            .await;
        assert_eq!(
            response.result.unwrap()["content"][0]["text"],
            "{\"state\":\"STOPPED\"}"
        );
    }

    #[tokio::test]
    async fn failed_engine_maps_error_without_secret() {
        let engine = Arc::new(FakeEngine::default());
        *engine.failure.lock().unwrap() = Some("engine unavailable".to_string());
        let handler = handler(Arc::clone(&engine));
        handler
            .handle(request(
                "publisher_configure",
                json!({"ingest_url": "https://director.example.com", "token": "secret", "cert_fingerprint": "aa".repeat(32)}),
            ))
            .await;
        let response = handler.handle(request("publisher_start", json!({}))).await;
        assert_eq!(response.result.unwrap()["isError"], true);
        let response = handler.handle(request("publisher_status", json!({}))).await;
        let status: Value = serde_json::from_str(
            response.result.unwrap()["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(status["state"], "ERROR");
        assert_eq!(status["detail"], "engine unavailable");
    }

    #[tokio::test]
    async fn idempotent_engine_start_is_called_once() {
        let engine = Arc::new(FakeEngine::default());
        let handler = handler(Arc::clone(&engine));
        let args = json!({"ingest_url": "https://director.example.com", "token": "secret", "cert_fingerprint": "aa".repeat(32)});
        handler
            .handle(request("publisher_configure", args.clone()))
            .await;
        handler.handle(request("publisher_start", json!({}))).await;
        handler.handle(request("publisher_start", json!({}))).await;
        assert_eq!(engine.starts.lock().unwrap().len(), 1);
    }

    #[test]
    fn publisher_debug_redacts_local_token() {
        let local = publisher::config::LocalConfig {
            url: "https://director.example.com".to_string(),
            token: "secret".to_string(),
            cert_fingerprint: None,
        };
        assert!(!format!("{local:?}").contains("secret"));
    }

    #[test]
    fn handler_debug_is_redacted() {
        let handler = handler(Arc::new(FakeEngine::default()));
        assert!(!format!("{handler:?}").contains("secret"));
    }

    #[test]
    fn dropping_handler_stops_engine() {
        let engine = Arc::new(FakeEngine::default());
        {
            let _handler = handler(Arc::clone(&engine));
        }
        assert_eq!(*engine.stops.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn tools_list_has_exactly_five_tools() {
        let response = handler(Arc::new(FakeEngine::default()))
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

    #[test]
    fn strips_ingest_suffix_before_starting_engine() {
        assert_eq!(
            strip_ingest_suffix("https://dev.local:8780/api/publisher/v2/ingest/"),
            "https://dev.local:8780"
        );
        assert_eq!(
            strip_ingest_suffix("https://dev.local:8780"),
            "https://dev.local:8780"
        );
    }

    #[tokio::test]
    async fn status_uses_contract_keys() {
        let handler = handler(Arc::new(FakeEngine::default()));
        handler
            .handle(request(
                "publisher_configure",
                json!({"ingest_url": "https://director.example.com/api/publisher/v2/ingest", "token": "secret", "cert_fingerprint": "aa".repeat(32), "driver_display_name": "Rig"}),
            ))
            .await;
        let response = handler.handle(request("publisher_status", json!({}))).await;
        let value: Value = serde_json::from_str(
            response.result.unwrap()["content"][0]["text"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let keys = value
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        let expected = [
            "state",
            "configured",
            "driverDisplayName",
            "ingestUrl",
            "lastBatchAt",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        assert_eq!(keys, expected);
    }
}
