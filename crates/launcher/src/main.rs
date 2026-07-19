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
use tracing::{error, info, warn};
use ui::LauncherUi;

/// Whether `bind` exposes the transport beyond the local machine. A bind is
/// treated as loopback-only when its host is an explicit loopback address
/// (`127.0.0.0/8` or `::1`); anything else — including the `0.0.0.0`/`::`
/// wildcards used by the default — is reachable from other hosts on the LAN.
fn is_lan_reachable(bind: &str) -> bool {
    use std::net::IpAddr;

    let host = match bind.rsplit_once(':') {
        Some((host, _port)) => host.trim_start_matches('[').trim_end_matches(']'),
        None => bind,
    };

    match host.parse::<IpAddr>() {
        Ok(ip) => !ip.is_loopback(),
        // A hostname (not a bare IP) can't be assumed loopback-only.
        Err(_) => true,
    }
}

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

    /// Transport for the MCP server. Defaults to `http` so a Driver can run
    /// the exe on the Rig and connect a remote Broadcast Agent without extra
    /// flags. Use `stdio` when an MCP client launches the server as a local
    /// child process on the same machine.
    #[arg(long, value_enum, default_value = "http")]
    transport: TransportKind,

    /// Address the MCP HTTP transport binds to. Defaults to `0.0.0.0:8765`,
    /// which is reachable from other hosts on the LAN: the typical deployment
    /// runs this server on the Rig while the Broadcast Agent runs on separate
    /// hardware. The transport has no authentication (see SECURITY.md), so
    /// anything that can reach it can invoke any tool — keep it on a trusted
    /// network segment and never port-forward it to the internet. To restrict
    /// it to same-machine clients, pass a loopback address
    /// (e.g. `--bind 127.0.0.1:8765`).
    #[arg(long, default_value = "0.0.0.0:8765")]
    bind: String,

    /// Address the settings HTTP server binds to. Defaults to loopback: unlike
    /// the MCP transport, the Director Console settings UI only needs to be
    /// driven from the Rig itself, and it has no authentication, so it should
    /// not be reachable off-host.
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
        ?cli.transport,
        bind = %cli.bind,
        settings_bind = %cli.settings_bind,
        "starting simracecenter-launcher"
    );

    if matches!(cli.transport, TransportKind::Http) && is_lan_reachable(&cli.bind) {
        warn!(
            bind = %cli.bind,
            "MCP transport is reachable off-host and has no authentication; \
             keep it on a trusted network segment and never port-forward it to the internet \
             (see SECURITY.md)"
        );
    }

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
    let settings_url = format!("http://{}/", settings_bind);
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
        let tray = ui::tray::build(settings_url)?;
        tray.run()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_defaults_to_lan_reachable_http() {
        let cli = Cli::try_parse_from(["simracecenter-launcher"]).unwrap();

        assert!(cli.sim.is_none());
        assert!(matches!(cli.transport, TransportKind::Http));
        assert_eq!(cli.bind, "0.0.0.0:8765");
        assert_eq!(cli.settings_bind, "127.0.0.1:8766");
        assert!(!cli.headless);
    }

    #[test]
    fn stdio_transport_is_still_selectable() {
        let cli = Cli::try_parse_from(["simracecenter-launcher", "--transport", "stdio"]).unwrap();

        assert!(matches!(cli.transport, TransportKind::Stdio));
    }

    #[test]
    fn lan_reachability_detects_off_host_binds() {
        assert!(!is_lan_reachable("127.0.0.1:8765"));
        assert!(!is_lan_reachable("127.0.0.5:8765"));
        assert!(!is_lan_reachable("[::1]:8765"));
        assert!(is_lan_reachable("0.0.0.0:8765"));
        assert!(is_lan_reachable("192.168.1.10:8765"));
        assert!(is_lan_reachable("[::]:8765"));
        assert!(is_lan_reachable("rig.local:8765"));
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
