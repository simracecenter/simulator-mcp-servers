// SPDX-License-Identifier: GPL-3.0-or-later
//! Owns the active simulator's MCP handler + transport for this run.
//! Deliberately UI-agnostic (ADR 0001 D2) — nothing here depends on
//! `crate::ui`.
//!
//! The concrete handler is wrapped by a [`SwappableHandler`] so the MCP
//! transport is wired once at startup and the inner handler can be replaced
//! when the user switches simulators via the settings UI or API.

use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use mcp_core::{JsonRpcRequest, JsonRpcResponse, McpHandler};

use crate::config::Sim;
use crate::TransportKind;

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
    }
}

/// Run the configured MCP transport with `handler` until it exits.
///
/// This is separate from [`build_handler`] so the launcher can construct a
/// single [`SwappableHandler`], hand it to the transport, and swap its inner
/// handler later without restarting the listener.
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
}
