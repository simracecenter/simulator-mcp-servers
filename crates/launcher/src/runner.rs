// SPDX-License-Identifier: GPL-3.0-or-later
//! Owns the active simulator's MCP handler + transport for this run.
//! Deliberately UI-agnostic (ADR 0001 D2) — nothing here depends on
//! `crate::ui`.
//!
//! The concrete handler is wrapped by a [`SwappableHandler`] so the MCP
//! transport is wired once at startup and the inner handler can be replaced
//! when the user switches simulators via the settings UI or API.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex, RwLock},
};

use async_trait::async_trait;
use mcp_core::{JsonRpcRequest, JsonRpcResponse, McpHandler};
use tracing::error;

use crate::config::Sim;
use crate::TransportKind;
use crate::{pair_server::build_publisher_router, pairing::PairingState, tls::RigIdentity};
use tokio::task::JoinHandle;

/// A [`McpHandler`] that delegates to a swappable inner handler.
///
/// This lets the launcher start its MCP transport once and switch the active
/// simulator's handler without tearing down the transport listener (ADR 0001
/// D2, 2026-07-17 update). Only one inner handler is active at a time, so the
/// single-active-simulator constraint from ADR 0003 is preserved.
pub struct SwappableHandler {
    inner: RwLock<Arc<dyn McpHandler>>,
}

impl SwappableHandler {
    pub fn new(inner: Arc<dyn McpHandler>) -> Self {
        Self {
            inner: RwLock::new(inner),
        }
    }

    pub fn set(&self, inner: Arc<dyn McpHandler>) {
        *self.inner.write().unwrap() = inner;
    }
}

#[async_trait]
impl McpHandler for SwappableHandler {
    async fn handle(&self, request: JsonRpcRequest) -> JsonRpcResponse {
        let inner = { self.inner.read().unwrap().clone() };
        inner.handle(request).await
    }
}

/// Build the concrete [`McpHandler`] for `sim`.
pub fn build_handler(sim: Sim) -> Arc<dyn McpHandler> {
    match sim {
        Sim::Iracing => {
            let adapter = Arc::new(iracing_mcp::adapter::SdkAdapter);
            let handler: Arc<dyn McpHandler> =
                Arc::new(iracing_mcp::IracingMcpHandler::new(adapter));
            handler
        }
        Sim::Lmu => {
            let adapter = Arc::new(lmu_mcp::adapter::SdkAdapter::default());
            let handler: Arc<dyn McpHandler> = Arc::new(lmu_mcp::LmuMcpHandler::new(adapter));
            handler
        }
        Sim::Publisher => Arc::new(publisher_mcp::PublisherMcpHandler::with_config_store(
            Arc::new(crate::config::FileConfigStore),
        )),
    }
}

/// Run the configured MCP transport with `handler` until it exits.
///
/// This is separate from [`build_handler`] so the launcher can construct a
/// single [`SwappableHandler`], hand it to the transport, and swap its inner
/// handler later without restarting the listener.
#[allow(dead_code)]
pub async fn run_transport(
    handler: Arc<SwappableHandler>,
    transport: TransportKind,
    bind: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match transport {
        TransportKind::Stdio => mcp_core::transport::stdio::run_stdio(handler).await?,
        TransportKind::Http => mcp_core::transport::http::run_http(bind, handler).await?,
    }

    Ok(())
}

pub struct TransportSupervisor {
    transport: TransportKind,
    bind: String,
    handler: Arc<SwappableHandler>,
    pairing: Arc<PairingState>,
    identity: Arc<RigIdentity>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl TransportSupervisor {
    pub fn new(
        transport: TransportKind,
        bind: String,
        handler: Arc<SwappableHandler>,
        pairing: Arc<PairingState>,
        identity: Arc<RigIdentity>,
    ) -> Self {
        Self {
            transport,
            bind,
            handler,
            pairing,
            identity,
            task: Mutex::new(None),
        }
    }

    pub fn start(&self, role: Sim) {
        if let Some(task) = self.task.lock().expect("transport task").take() {
            task.abort();
        }
        let transport = self.transport;
        let bind = self.bind.clone();
        let handler = Arc::clone(&self.handler);
        let pairing = Arc::clone(&self.pairing);
        let identity = Arc::clone(&self.identity);
        let task = tokio::spawn(async move {
            let result: Result<(), Box<dyn std::error::Error>> = match transport {
                TransportKind::Stdio => mcp_core::transport::stdio::run_stdio(handler)
                    .await
                    .map_err(Into::into),
                TransportKind::Http if role == Sim::Publisher => {
                    let address: SocketAddr = match bind.parse() {
                        Ok(address) => address,
                        Err(error) => {
                            error!(%error, "invalid publisher bind address");
                            return;
                        }
                    };
                    let tls = axum_server::tls_rustls::RustlsConfig::from_config(
                        identity.server_config(),
                    );
                    axum_server::bind_rustls(address, tls)
                        .serve(build_publisher_router(handler, pairing).into_make_service())
                        .await
                        .map_err(Into::into)
                }
                TransportKind::Http => mcp_core::transport::http::run_http(&bind, handler)
                    .await
                    .map_err(Into::into),
            };
            if let Err(error) = result {
                error!(%error, "mcp server task exited");
            }
        });
        *self.task.lock().expect("transport task") = Some(task);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn http_transport_propagates_bind_errors() {
        let handler = Arc::new(SwappableHandler::new(build_handler(Sim::Iracing)));
        let error = run_transport(handler, TransportKind::Http, "")
            .await
            .unwrap_err();

        assert!(!error.to_string().is_empty());
    }

    #[tokio::test]
    async fn publisher_handler_exposes_only_publisher_tools() {
        let handler = build_handler(Sim::Publisher);
        let response = handler
            .handle(mcp_core::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(serde_json::json!(1)),
                method: "tools/list".to_string(),
                params: serde_json::Value::Null,
            })
            .await;
        let result = response.result.unwrap();
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            vec![
                "publisher_status",
                "publisher_configure",
                "publisher_start",
                "publisher_stop",
                "get_capabilities"
            ]
        );
        assert!(!names.contains(&"get_session_overview"));
    }

    #[tokio::test]
    async fn swapping_away_from_publisher_releases_the_previous_handler() {
        let publisher = build_handler(Sim::Publisher);
        let swapped = SwappableHandler::new(Arc::clone(&publisher));
        swapped.set(build_handler(Sim::Iracing));
        assert_eq!(Arc::strong_count(&publisher), 1);

        let response = swapped
            .handle(mcp_core::JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(serde_json::json!(1)),
                method: "tools/list".to_string(),
                params: serde_json::Value::Null,
            })
            .await;
        let result = response.result.unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert!(tools
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap().starts_with("publisher_")));
    }
}
