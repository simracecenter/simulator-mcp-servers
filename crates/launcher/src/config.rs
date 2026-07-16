use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which simulator's MCP server the launcher hosts. The runner is a
/// singleton (ADR 0001 D2/D3): exactly one of these is active at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sim {
    Iracing,
    Lmu,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherConfig {
    pub active_sim: Sim,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            active_sim: Sim::Iracing,
        }
    }
}

/// `%APPDATA%\SimRaceCenter\config.toml` (ADR 0001 D4). Falls back to the
/// system temp dir when `APPDATA` isn't set, which only happens off Windows
/// (Linux devcontainer / `cargo test`) — the launcher itself only ships for
/// Windows.
pub fn config_path() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("SimRaceCenter").join("config.toml")
}

pub fn load() -> Result<LauncherConfig, mcp_core::config::ConfigError> {
    mcp_core::config::load_or_default(&config_path())
}

// Used by the tray UI's settings window once it can edit config (ADR 0001 D4);
// not yet called from the CLI-only skeleton.
#[allow(dead_code)]
pub fn save(config: &LauncherConfig) -> Result<(), mcp_core::config::ConfigError> {
    mcp_core::config::save(&config_path(), config)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn default_config_uses_iracing() {
        assert_eq!(LauncherConfig::default().active_sim, Sim::Iracing);
    }

    #[test]
    fn config_path_load_and_save_use_appdata() {
        let _guard = ENV_LOCK.lock().unwrap();
        let appdata = std::env::temp_dir().join(format!(
            "simracecenter-launcher-config-test-{}",
            std::process::id()
        ));
        std::env::set_var("APPDATA", &appdata);

        let path = config_path();
        assert_eq!(path, appdata.join("SimRaceCenter").join("config.toml"));
        assert_eq!(load().unwrap().active_sim, Sim::Iracing);

        let config = LauncherConfig {
            active_sim: Sim::Iracing,
        };
        save(&config).unwrap();
        assert_eq!(load().unwrap().active_sim, Sim::Iracing);

        std::fs::remove_dir_all(&appdata).ok();
        std::env::remove_var("APPDATA");
    }
}
