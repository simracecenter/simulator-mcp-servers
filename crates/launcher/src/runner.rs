//! Owns the active simulator's MCP handler + transport for this run.
//! Deliberately UI-agnostic (ADR 0001 D2) — nothing here depends on
//! `crate::ui`.

use std::sync::Arc;

use crate::config::Sim;
use crate::TransportKind;

pub async fn run(
    sim: Sim,
    transport: TransportKind,
    bind: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    match sim {
        Sim::Iracing => {
            let handler = Arc::new(iracing_mcp::IracingMcpHandler);
            match transport {
                TransportKind::Stdio => mcp_core::transport::stdio::run_stdio(handler).await?,
                TransportKind::Http => mcp_core::transport::http::run_http(bind, handler).await?,
            }
        }
    }

    Ok(())
}
