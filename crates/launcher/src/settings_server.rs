// SPDX-License-Identifier: GPL-3.0-or-later
//! Always-on local settings HTTP server for the launcher.
//!
//! Serves a single static HTML page that lets the Driver select the active
//! simulator. Endpoints:
//! - `GET /`         — the settings UI.
//! - `GET /healthz`  — liveness check.
//! - `GET /api/status`  — active sim, connection status, and live tool names.
//! - `POST /api/sim`    — persist selection and hot-swap the in-process handler.
//!
//! The server binds loopback by default and has no authentication, matching
//! the MCP HTTP transport's trust model (SECURITY.md).

use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::Html,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use mcp_core::{JsonRpcRequest, McpHandler};

use crate::config::{self, LauncherConfig, Sim};
use crate::runner::{build_handler, SwappableHandler};

/// Shared state for the settings server.
pub struct SettingsState {
    pub current_sim: RwLock<Sim>,
    pub handler: Arc<SwappableHandler>,
}

impl SettingsState {
    pub fn new(handler: Arc<SwappableHandler>, sim: Sim) -> Arc<Self> {
        Arc::new(Self {
            current_sim: RwLock::new(sim),
            handler,
        })
    }
}

#[derive(Deserialize)]
struct SimRequest {
    sim: Sim,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    sim: String,
    connected: bool,
    tool_names: Vec<String>,
}

const INDEX_HTML: &str = include_str!("settings_page.html");

/// Start the settings server and block until it exits.
pub async fn run(bind: &str, state: Arc<SettingsState>) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(healthz))
        .route("/api/status", get(api_status))
        .route("/api/sim", post(api_sim))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn api_status(State(state): State<Arc<SettingsState>>) -> Json<Status> {
    let sim = *state.current_sim.read().await;
    Json(build_status(&state.handler, sim).await)
}

async fn api_sim(
    State(state): State<Arc<SettingsState>>,
    Json(body): Json<SimRequest>,
) -> Result<Json<Status>, (StatusCode, String)> {
    let new_sim = body.sim;
    {
        let mut sim_lock = state.current_sim.write().await;
        if *sim_lock != new_sim {
            let config = LauncherConfig {
                active_sim: new_sim,
            };
            config::save(&config)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            state.handler.set(build_handler(new_sim));
            *sim_lock = new_sim;
        }
    }
    let sim = *state.current_sim.read().await;
    Ok(Json(build_status(&state.handler, sim).await))
}

async fn build_status(handler: &SwappableHandler, sim: Sim) -> Status {
    let connected = get_connected(handler).await;
    let tool_names = get_tool_names(handler).await;
    Status {
        sim: sim.to_string(),
        connected,
        tool_names,
    }
}

async fn get_connected(handler: &SwappableHandler) -> bool {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(1)),
        method: "tools/call".to_string(),
        params: json!({"name": "get_session_overview", "arguments": {}}),
    };

    let response = handler.handle(request).await;
    response
        .result
        .and_then(|result| {
            result
                .get("structuredContent")?
                .get("data")?
                .get("connected")?
                .as_bool()
        })
        .unwrap_or(false)
}

async fn get_tool_names(handler: &SwappableHandler) -> Vec<String> {
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(2)),
        method: "tools/list".to_string(),
        params: Value::Null,
    };

    let response = handler.handle(request).await;
    if let Some(result) = response.result {
        if let Some(tools) = result.get("tools").and_then(Value::as_array) {
            return tools
                .iter()
                .filter_map(|tool| tool.get("name")?.as_str().map(String::from))
                .collect();
        }
    }

    Vec::new()
}
