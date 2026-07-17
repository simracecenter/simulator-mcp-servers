mod config;
mod runner;
mod settings_server;
mod singleton;
mod ui;

use std::sync::Arc;

use clap::{Parser, ValueEnum};
use config::Sim;
use runner::{build_handler, run_transport, SwappableHandler};
use settings_server::SettingsState;
use singleton::SingletonGuard;
use tracing::{error, info};
use ui::LauncherUi;

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum TransportKind {
    Stdio,
    Http,
}

/// Sim RaceCenter launcher: a singleton runner that hosts at most one
/// simulator's MCP server at a time (ADR 0001 D2/D3).
///
/// The full headless/scripting flag surface (PowerShell, Stream Deck) is
/// tracked as its own follow-up on the project board — these are the basic
/// flags needed to make the launcher runnable today.
#[derive(Debug, Parser)]
#[command(author, version, about = "Sim RaceCenter launcher")]
struct Cli {
    /// Which simulator's MCP server to run. Overrides config.toml for this run only.
    #[arg(long, value_enum)]
    sim: Option<Sim>,

    #[arg(long, value_enum, default_value = "stdio")]
    transport: TransportKind,

    /// Address the HTTP transport binds to. Defaults to loopback so the MCP
    /// server is not reachable off-host: the transport has no authentication
    /// (see SECURITY.md), so anything that can reach it can invoke any tool.
    /// To expose it to a trusted Broadcast Agent host on the LAN, pass an
    /// explicit interface (e.g. `--bind 0.0.0.0:8765`); never port-forward it
    /// to the internet.
    #[arg(long, default_value = "127.0.0.1:8765")]
    bind: String,

    /// Address the settings HTTP server binds to. Defaults to loopback for the
    /// same reason as `--bind`: the settings server has no authentication and
    /// should not be reachable off-host.
    #[arg(long, default_value = "127.0.0.1:8766")]
    settings_bind: String,

    /// Skip the tray UI entirely (for PowerShell/Stream Deck automation).
    #[arg(long)]
    headless: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();

    let _guard = SingletonGuard::acquire("SimRaceCenterLauncher").map_err(|error| {
        error!(%error, "refusing to start a second instance");
        error
    })?;

    let file_config = config::load()?;
    let active_sim = mcp_core::config::override_with(file_config.active_sim, cli.sim);

    info!(
        ?active_sim,
        headless = cli.headless,
        settings_bind = %cli.settings_bind,
        "starting simracecenter-launcher"
    );

    let handler = Arc::new(SwappableHandler::new(build_handler(active_sim)));
    let settings_state = SettingsState::new(handler.clone(), active_sim);

    let transport = cli.transport;
    let bind = cli.bind;
    let mcp_handle = tokio::spawn(async move {
        if let Err(error) = run_transport(handler, transport, &bind).await {
            error!(%error, "mcp server task exited");
        }
    });

    let settings_bind = cli.settings_bind;
    let settings_handle = tokio::spawn(async move {
        if let Err(error) = settings_server::run(&settings_bind, settings_state).await {
            error!(%error, "settings server task exited");
        }
    });

    if cli.headless {
        tokio::select! {
            _ = mcp_handle => {},
            _ = settings_handle => {},
            _ = tokio::signal::ctrl_c() => {
                info!("received shutdown signal");
            }
        }
        Ok(())
    } else {
        let tray = ui::tray::build()?;
        tray.run()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_uses_stdio_defaults() {
        let cli = Cli::try_parse_from(["simracecenter-launcher"]).unwrap();

        assert!(cli.sim.is_none());
        assert!(matches!(cli.transport, TransportKind::Stdio));
        assert_eq!(cli.bind, "127.0.0.1:8765");
        assert_eq!(cli.settings_bind, "127.0.0.1:8766");
        assert!(!cli.headless);
    }

    #[test]
    fn cli_parses_headless_http_overrides() {
        let cli = Cli::try_parse_from([
            "simracecenter-launcher",
            "--sim",
            "iracing",
            "--transport",
            "http",
            "--bind",
            "127.0.0.1:9000",
            "--settings-bind",
            "127.0.0.1:9001",
            "--headless",
        ])
        .unwrap();

        assert_eq!(cli.sim, Some(Sim::Iracing));
        assert!(matches!(cli.transport, TransportKind::Http));
        assert_eq!(cli.bind, "127.0.0.1:9000");
        assert_eq!(cli.settings_bind, "127.0.0.1:9001");
        assert!(cli.headless);
    }
}
