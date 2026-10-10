// SPDX-License-Identifier: GPL-3.0-or-later
//! Always-on local settings HTTP server for the launcher.
//!
//! Serves a single static HTML page that lets the Driver select the active
//! simulator. Endpoints:
//! - `GET /`              — the settings UI.
//! - `GET /favicon.ico`   — the Sim RaceCenter logo as a Windows `.ico`.
//! - `GET /healthz`       — liveness check.
//! - `GET /api/status`    — active sim, connection status, and live tool names.
//! - `POST /api/sim`      — persist selection and hot-swap the in-process handler.
//! - `POST /api/revoke-director` — revoke one paired Director grant.
//!
//! The server binds loopback by default and has no authentication because it
//! is intended for local Driver settings control.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::{Html, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;
use uuid::Uuid;

use mcp_core::{JsonRpcRequest, McpHandler};
use publisher_mcp::{PublisherConfig, PublisherConfigStore};

use crate::config::{self, Sim};
use crate::pairing::{PairedDirector, PairingState};
use crate::runner::{build_handler, SwappableHandler, TransportSupervisor};

/// Shared state for the settings server.
pub struct SettingsState {
    pub current_sim: RwLock<Sim>,
    pub handler: Arc<SwappableHandler>,
    pub pairing: Arc<PairingState>,
    pub publisher_config: Arc<dyn PublisherConfigStore>,
    pub supervisor: Arc<TransportSupervisor>,
}

impl SettingsState {
    pub fn new(
        handler: Arc<SwappableHandler>,
        sim: Sim,
        pairing: Arc<PairingState>,
        publisher_config: Arc<dyn PublisherConfigStore>,
        supervisor: Arc<TransportSupervisor>,
    ) -> Arc<Self> {
        Arc::new(Self {
            current_sim: RwLock::new(sim),
            handler,
            pairing,
            publisher_config,
            supervisor,
        })
    }
}

#[derive(Deserialize)]
struct SimRequest {
    sim: Sim,
}

#[derive(Deserialize)]
struct RevokeDirectorRequest {
    id: Uuid,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    sim: String,
    connected: bool,
    tool_names: Vec<String>,
    pairing_code: Option<String>,
    paired: bool,
    director_name: Option<String>,
    director_names: Vec<String>,
    cert_fingerprint: Option<String>,
    device_id: Option<String>,
    directors: Vec<PairedDirector>,
    publishing_to: Option<PublishTarget>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PublishTarget {
    ingest_host: String,
    director_name: Option<String>,
}

const INDEX_HTML: &str = include_str!("settings_page.html");
const FAVICON_ICO: &[u8] = include_bytes!("../assets/logo.ico");

/// Start the settings server and block until it exits.
pub async fn run(bind: &str, state: Arc<SettingsState>) -> std::io::Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.ico", get(favicon))
        .route("/healthz", get(healthz))
        .route("/api/status", get(api_status))
        .route("/api/sim", post(api_sim))
        .route("/api/unpair", post(api_unpair))
        .route("/api/revoke-director", post(api_revoke_director))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(bind).await?;
    axum::serve(listener, app)
        .await
        .map_err(|error| std::io::Error::other(error.to_string()))
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn favicon() -> Response<Body> {
    Response::builder()
        .header(header::CONTENT_TYPE, "image/x-icon")
        .body(Body::from(FAVICON_ICO))
        .unwrap()
}

async fn healthz() -> Json<Value> {
    Json(json!({ "ok": true }))
}

async fn api_status(State(state): State<Arc<SettingsState>>) -> Json<Status> {
    let sim = *state.current_sim.read().await;
    Json(
        build_status(
            &state.handler,
            sim,
            &state.pairing,
            state.publisher_config.as_ref(),
        )
        .await,
    )
}

async fn api_sim(
    State(state): State<Arc<SettingsState>>,
    Json(body): Json<SimRequest>,
) -> Result<Json<Status>, (StatusCode, String)> {
    let new_sim = body.sim;
    {
        let mut sim_lock = state.current_sim.write().await;
        if *sim_lock != new_sim {
            let mut config = config::load()
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            config.active_sim = new_sim;
            config::save(&config)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            state.handler.set(build_handler(new_sim));
            state.supervisor.start(new_sim);
            *sim_lock = new_sim;
        }
    }
    let sim = *state.current_sim.read().await;
    Ok(Json(
        build_status(
            &state.handler,
            sim,
            &state.pairing,
            state.publisher_config.as_ref(),
        )
        .await,
    ))
}

async fn api_revoke_director(
    State(state): State<Arc<SettingsState>>,
    Json(body): Json<RevokeDirectorRequest>,
) -> Result<Json<Status>, (StatusCode, String)> {
    match state.pairing.revoke_director(body.id) {
        Ok(true) => {
            let sim = *state.current_sim.read().await;
            Ok(Json(
                build_status(
                    &state.handler,
                    sim,
                    &state.pairing,
                    state.publisher_config.as_ref(),
                )
                .await,
            ))
        }
        Ok(false) => Err((StatusCode::NOT_FOUND, "Director not found".to_string())),
        Err(error) => Err((StatusCode::INTERNAL_SERVER_ERROR, error)),
    }
}

async fn api_unpair(
    State(state): State<Arc<SettingsState>>,
) -> Result<Json<Status>, (StatusCode, String)> {
    state
        .pairing
        .unpair()
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let sim = *state.current_sim.read().await;
    Ok(Json(
        build_status(
            &state.handler,
            sim,
            &state.pairing,
            state.publisher_config.as_ref(),
        )
        .await,
    ))
}

async fn build_status(
    handler: &SwappableHandler,
    sim: Sim,
    pairing: &PairingState,
    publisher_config: &dyn PublisherConfigStore,
) -> Status {
    let connected = get_connected(handler, sim).await;
    let tool_names = get_tool_names(handler).await;
    let (
        pairing_code,
        paired,
        director_name,
        director_names,
        cert_fingerprint,
        device_id,
        directors,
        publishing_to,
    ) = if sim == Sim::Publisher {
        let directors = pairing.directors();
        let director_names = pairing.director_names();
        let publishing_to = publisher_config
            .load()
            .ok()
            .and_then(|config| publish_target(&config, &directors));
        (
            Some(pairing.pairing_code()),
            !directors.is_empty(),
            director_names.first().cloned(),
            director_names,
            Some(pairing.fingerprint().to_string()),
            pairing.device_id().map(|id| id.to_string()),
            directors,
            publishing_to,
        )
    } else {
        (None, false, None, Vec::new(), None, None, Vec::new(), None)
    };
    Status {
        sim: sim.to_string(),
        connected,
        tool_names,
        pairing_code,
        paired,
        director_name,
        director_names,
        cert_fingerprint,
        device_id,
        directors,
        publishing_to,
    }
}

fn publish_target(config: &PublisherConfig, directors: &[PairedDirector]) -> Option<PublishTarget> {
    let ingest_url = config.ingest_url.as_deref()?;
    let authority = ingest_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(ingest_url);
    let ingest_host = authority
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_string();
    let director_name = config
        .cert_fingerprint
        .as_deref()
        .and_then(|fingerprint| publisher::config::normalise_fingerprint(fingerprint).ok())
        .and_then(|fingerprint| {
            directors.iter().find_map(|director| {
                publisher::config::normalise_fingerprint(&director.fingerprint)
                    .ok()
                    .filter(|director_fingerprint| director_fingerprint == &fingerprint)
                    .map(|_| director.name.clone())
            })
        });

    Some(PublishTarget {
        ingest_host,
        director_name,
    })
}

async fn get_connected(handler: &SwappableHandler, sim: Sim) -> bool {
    let tool_name = match sim {
        Sim::Publisher => "publisher_status",
        Sim::Iracing | Sim::Lmu => "get_session_overview",
    };
    let request = JsonRpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(json!(1)),
        method: "tools/call".to_string(),
        params: json!({"name": tool_name, "arguments": {}}),
    };

    let response = handler.handle(request).await;
    response
        .result
        .and_then(|result| {
            if sim == Sim::Publisher {
                let text = result.get("content")?.get(0)?.get("text")?.as_str()?;
                let data: Value = serde_json::from_str(text).ok()?;
                Some(data.get("state")?.as_str()? == "RUNNING")
            } else {
                result
                    .get("structuredContent")?
                    .get("data")?
                    .get("connected")?
                    .as_bool()
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn director(name: &str, fingerprint: &str) -> PairedDirector {
        PairedDirector {
            id: Uuid::new_v4(),
            name: name.to_string(),
            fingerprint: fingerprint.to_string(),
            paired_at: "123".to_string(),
        }
    }

    fn publisher_config(
        ingest_url: Option<&str>,
        cert_fingerprint: Option<&str>,
    ) -> PublisherConfig {
        PublisherConfig {
            ingest_url: ingest_url.map(str::to_string),
            cert_fingerprint: cert_fingerprint.map(str::to_string),
            driver_display_name: None,
        }
    }

    #[test]
    fn publish_target_is_absent_without_ingest_url() {
        assert_eq!(publish_target(&publisher_config(None, None), &[]), None);
    }

    #[test]
    fn publish_target_matches_normalized_fingerprint() {
        let fingerprint = "ab".repeat(32);
        let colon_fingerprint = fingerprint
            .as_bytes()
            .chunks(2)
            .map(|pair| {
                String::from_utf8(pair.to_vec())
                    .unwrap()
                    .to_ascii_uppercase()
            })
            .collect::<Vec<_>>()
            .join(":");
        let config = publisher_config(
            Some("https://director.example/ingest"),
            Some(&colon_fingerprint),
        );

        let target =
            publish_target(&config, &[director("Primary Director", &fingerprint)]).unwrap();

        assert_eq!(
            target,
            PublishTarget {
                ingest_host: "director.example".to_string(),
                director_name: Some("Primary Director".to_string()),
            }
        );
    }

    #[test]
    fn publish_target_keeps_unmatched_fingerprint_unpaired() {
        let config = publisher_config(
            Some("https://director.example/ingest"),
            Some(&"aa".repeat(32)),
        );

        let target =
            publish_target(&config, &[director("Other Director", &"bb".repeat(32))]).unwrap();

        assert_eq!(target.director_name, None);
    }

    #[test]
    fn publish_target_includes_port_and_discards_path() {
        let target = publish_target(
            &publisher_config(Some("https://director.example:8443/ingest/events"), None),
            &[],
        )
        .unwrap();

        assert_eq!(target.ingest_host, "director.example:8443");
    }
}
