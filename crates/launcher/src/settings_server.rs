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
//! - `GET /api/controls`  — wheel input availability, bindings, and status.
//! - `POST /api/controls/learn` — learn a wheel-button binding.
//! - `POST /api/controls/cancel` — cancel wheel-button learning.
//! - `POST /api/controls/clear` — clear a wheel-button binding.
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
use publisher::controls::ControlAction;
use publisher_mcp::{PublisherConfig, PublisherConfigStore};

use crate::config::{self, Sim};
use crate::pairing::{PairedDirector, PairingState};
use crate::runner::{build_handler, SwappableHandler, TransportSupervisor};
use crate::wheel_controls::WheelControls;

/// Shared state for the settings server.
pub struct SettingsState {
    pub current_sim: RwLock<Sim>,
    pub handler: Arc<SwappableHandler>,
    pub pairing: Arc<PairingState>,
    pub publisher_config: Arc<dyn PublisherConfigStore>,
    pub supervisor: Arc<TransportSupervisor>,
    pub controls: Arc<WheelControls>,
}

impl SettingsState {
    pub fn new(
        handler: Arc<SwappableHandler>,
        sim: Sim,
        pairing: Arc<PairingState>,
        publisher_config: Arc<dyn PublisherConfigStore>,
        supervisor: Arc<TransportSupervisor>,
        controls: Arc<WheelControls>,
    ) -> Arc<Self> {
        Arc::new(Self {
            current_sim: RwLock::new(sim),
            handler,
            pairing,
            publisher_config,
            supervisor,
            controls,
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

#[derive(Deserialize)]
struct ControlActionRequest {
    action: String,
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
        .route("/api/controls", get(api_controls))
        .route("/api/controls/learn", post(api_controls_learn))
        .route("/api/controls/cancel", post(api_controls_cancel))
        .route("/api/controls/clear", post(api_controls_clear))
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

async fn api_controls(
    State(state): State<Arc<SettingsState>>,
) -> Json<crate::wheel_controls::ControlsStatus> {
    Json(state.controls.status())
}

async fn api_controls_learn(
    State(state): State<Arc<SettingsState>>,
    Json(body): Json<ControlActionRequest>,
) -> Result<Json<crate::wheel_controls::ControlsStatus>, (StatusCode, String)> {
    let action = parse_control_action(&body.action)?;
    require_publisher(&state).await?;
    state.controls.learn(action);
    Ok(Json(state.controls.status()))
}

async fn api_controls_cancel(
    State(state): State<Arc<SettingsState>>,
) -> Json<crate::wheel_controls::ControlsStatus> {
    state.controls.cancel_learn();
    Json(state.controls.status())
}

async fn api_controls_clear(
    State(state): State<Arc<SettingsState>>,
    Json(body): Json<ControlActionRequest>,
) -> Result<Json<crate::wheel_controls::ControlsStatus>, (StatusCode, String)> {
    let action = parse_control_action(&body.action)?;
    require_publisher(&state).await?;
    state.controls.clear(action);
    Ok(Json(state.controls.status()))
}

fn parse_control_action(action: &str) -> Result<ControlAction, (StatusCode, String)> {
    ControlAction::parse(action).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            format!("unknown control action: {action}"),
        )
    })
}

async fn require_publisher(state: &SettingsState) -> Result<(), (StatusCode, String)> {
    if *state.current_sim.read().await == Sim::Publisher {
        Ok(())
    } else {
        Err((
            StatusCode::CONFLICT,
            "Select Publisher to bind wheel buttons".to_string(),
        ))
    }
}

async fn api_sim(
    State(state): State<Arc<SettingsState>>,
    Json(body): Json<SimRequest>,
) -> Result<Json<Status>, (StatusCode, String)> {
    let new_sim = body.sim;
    {
        let mut sim_lock = state.current_sim.write().await;
        if *sim_lock != new_sim {
            let previous_sim = *sim_lock;
            let mut config = config::load()
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            config.active_sim = new_sim;
            config::save(&config)
                .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))?;
            if previous_sim == Sim::Publisher && new_sim != Sim::Publisher {
                state.controls.stop();
            }
            state
                .handler
                .set(build_handler(new_sim, state.controls.relay()));
            state.supervisor.start(new_sim);
            if new_sim == Sim::Publisher {
                state.controls.start();
            }
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
    use crate::config::{LauncherConfig, ENV_LOCK};
    use crate::pairing::{InMemoryPairingStore, PairingState};
    use crate::runner::{build_handler, SwappableHandler, TransportSupervisor};
    use crate::wheel_controls::WheelControls;
    use publisher::controls::{load_controls, save_controls, ButtonBinding, ControlsConfig};

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

    fn test_state(sim: Sim) -> Arc<SettingsState> {
        let relay = Arc::new(publisher_mcp::ControlRelay::new());
        let controls = Arc::new(WheelControls::new(Arc::clone(&relay)));
        let handler = Arc::new(SwappableHandler::new(build_handler(sim, &relay)));
        let pairing = Arc::new(
            PairingState::new(
                Arc::new(mcp_core::transport::http::access::CredentialRegistry::new()),
                Arc::new(InMemoryPairingStore::new(LauncherConfig::default())),
                "rig1".to_string(),
                "AA:BB".to_string(),
            )
            .unwrap(),
        );
        let supervisor = Arc::new(TransportSupervisor::new(
            crate::TransportKind::Stdio,
            "127.0.0.1:0".to_string(),
            handler.clone(),
            pairing.clone(),
        ));
        SettingsState::new(
            handler,
            sim,
            pairing,
            Arc::new(crate::config::FileConfigStore),
            supervisor,
            controls,
        )
    }

    fn isolated_appdata(label: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "simracecenter-settings-controls-{label}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&path).ok();
        std::env::set_var("APPDATA", &path);
        path
    }

    fn clean_appdata(path: &std::path::Path) {
        std::fs::remove_dir_all(path).ok();
        std::env::remove_var("APPDATA");
    }

    #[tokio::test]
    async fn learn_sets_learning_and_cancel_clears_it() {
        let _guard = ENV_LOCK.lock().unwrap();
        let appdata = isolated_appdata("learn-cancel");
        let state = test_state(Sim::Publisher);
        drop(_guard);

        let learned = api_controls_learn(
            State(Arc::clone(&state)),
            Json(ControlActionRequest {
                action: "approve_yes".to_string(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(learned.0.learning.as_deref(), Some("approve_yes"));

        let cancelled = api_controls_cancel(State(state)).await;
        assert_eq!(cancelled.0.learning, None);
        let _guard = ENV_LOCK.lock().unwrap();
        clean_appdata(&appdata);
    }

    #[tokio::test]
    async fn clear_removes_binding_and_persists_the_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let appdata = isolated_appdata("clear");
        let controls_path = appdata
            .join("SimRaceCenter")
            .join(publisher::controls::CONTROLS_FILE_NAME);
        let mut config = ControlsConfig::default();
        config.set_binding(ButtonBinding {
            action: ControlAction::ApproveYes,
            device: "VID_1234&PID_5678".to_string(),
            button: 7,
        });
        save_controls(&controls_path, &config).unwrap();
        let state = test_state(Sim::Publisher);
        drop(_guard);

        let status = api_controls_clear(
            State(state),
            Json(ControlActionRequest {
                action: "approve_yes".to_string(),
            }),
        )
        .await
        .unwrap();

        let binding = status
            .0
            .bindings
            .iter()
            .find(|binding| binding.action == "approve_yes")
            .unwrap();
        assert_eq!(binding.device, None);
        assert_eq!(binding.button, None);
        assert!(load_controls(&controls_path)
            .unwrap()
            .binding_for(ControlAction::ApproveYes)
            .is_none());
        let _guard = ENV_LOCK.lock().unwrap();
        clean_appdata(&appdata);
    }

    #[tokio::test]
    async fn unknown_action_returns_bad_request() {
        let _guard = ENV_LOCK.lock().unwrap();
        let appdata = isolated_appdata("unknown");
        let state = test_state(Sim::Publisher);
        drop(_guard);

        let error = api_controls_learn(
            State(state),
            Json(ControlActionRequest {
                action: "not_a_control".to_string(),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        let _guard = ENV_LOCK.lock().unwrap();
        clean_appdata(&appdata);
    }

    #[tokio::test]
    async fn non_publisher_role_returns_conflict_for_learning_and_clearing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let appdata = isolated_appdata("role");
        let state = test_state(Sim::Iracing);
        drop(_guard);

        let learn_error = api_controls_learn(
            State(Arc::clone(&state)),
            Json(ControlActionRequest {
                action: "approve_yes".to_string(),
            }),
        )
        .await
        .unwrap_err();
        let clear_error = api_controls_clear(
            State(state),
            Json(ControlActionRequest {
                action: "approve_yes".to_string(),
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(learn_error.0, StatusCode::CONFLICT);
        assert_eq!(learn_error.1, "Select Publisher to bind wheel buttons");
        assert_eq!(clear_error.0, StatusCode::CONFLICT);
        assert_eq!(clear_error.1, "Select Publisher to bind wheel buttons");
        let _guard = ENV_LOCK.lock().unwrap();
        clean_appdata(&appdata);
    }
}
