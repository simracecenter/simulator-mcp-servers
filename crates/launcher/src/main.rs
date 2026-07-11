mod config;
mod runner;
mod singleton;
mod ui;

use clap::{Parser, ValueEnum};
use config::Sim;
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

    #[arg(long, default_value = "0.0.0.0:8765")]
    bind: String,

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
        "starting simracecenter-launcher"
    );

    if cli.headless {
        return runner::run(active_sim, cli.transport, &cli.bind).await;
    }

    let transport = cli.transport;
    let bind = cli.bind;
    tokio::spawn(async move {
        if let Err(error) = runner::run(active_sim, transport, &bind).await {
            error!(%error, "mcp server task exited");
        }
    });

    let tray = ui::tray::build()?;
    tray.run()?;
    Ok(())
}
