use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use uuid::Uuid;

use crate::pairing::PairingRecord;

/// Which simulator's MCP server the launcher hosts. The runner is a
/// singleton (ADR 0001 D2/D3): exactly one of these is active at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sim {
    Iracing,
    Lmu,
    Publisher,
}

impl fmt::Display for Sim {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sim::Iracing => write!(f, "iracing"),
            Sim::Lmu => write!(f, "lmu"),
            Sim::Publisher => write!(f, "publisher"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LauncherConfig {
    pub active_sim: Sim,
    #[serde(default)]
    pub publisher: publisher_mcp::PublisherConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<PairingRecord>,
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            active_sim: Sim::Iracing,
            publisher: publisher_mcp::PublisherConfig::default(),
            device_id: None,
            pairing: None,
        }
    }
}

/// `%APPDATA%\SimRaceCenter\config.toml` (ADR 0001 D4). Falls back to the
/// system temp dir when `APPDATA` isn't set, which only happens off Windows
/// (Linux devcontainer / `cargo test`) — the launcher itself only ships for
/// Windows.
pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn config_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("SimRaceCenter")
}

pub fn load() -> Result<LauncherConfig, mcp_core::config::ConfigError> {
    mcp_core::config::load_or_default(&config_path())
}

// Used by the tray UI's settings window and the settings HTTP server (ADR 0001 D4).
pub fn save(config: &LauncherConfig) -> Result<(), mcp_core::config::ConfigError> {
    mcp_core::config::save(&config_path(), config)
}

pub struct FileConfigStore;

impl publisher_mcp::PublisherConfigStore for FileConfigStore {
    fn load(&self) -> Result<publisher_mcp::PublisherConfig, String> {
        load()
            .map(|config| config.publisher)
            .map_err(|error| error.to_string())
    }

    fn save(&self, publisher: &publisher_mcp::PublisherConfig) -> Result<(), String> {
        let mut config = load().map_err(|error| error.to_string())?;
        config.publisher = publisher.clone();
        save(&config).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use publisher_mcp::PublisherConfigStore;

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
            ..LauncherConfig::default()
        };
        save(&config).unwrap();
        assert_eq!(load().unwrap().active_sim, Sim::Iracing);

        std::fs::remove_dir_all(&appdata).ok();
        std::env::remove_var("APPDATA");
    }

    #[test]
    fn publisher_active_sim_round_trips_through_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let appdata = std::env::temp_dir().join(format!(
            "simracecenter-launcher-publisher-config-test-{}",
            std::process::id()
        ));
        std::env::set_var("APPDATA", &appdata);

        let device_id = Uuid::new_v4();
        let config = LauncherConfig {
            active_sim: Sim::Publisher,
            device_id: Some(device_id),
            pairing: Some(PairingRecord {
                credential_id: Uuid::new_v4(),
                credential_sha256: "ab".repeat(32),
                director_name: "Director".to_string(),
                paired_at: "123".to_string(),
            }),
            ..LauncherConfig::default()
        };
        save(&config).unwrap();
        let loaded = load().unwrap();
        assert_eq!(loaded.active_sim, Sim::Publisher);
        assert_eq!(loaded.device_id, Some(device_id));
        assert_eq!(loaded.pairing, config.pairing);

        std::fs::remove_dir_all(&appdata).ok();
        std::env::remove_var("APPDATA");
    }

    #[test]
    fn publisher_store_preserves_active_sim() {
        let _guard = ENV_LOCK.lock().unwrap();
        let appdata = std::env::temp_dir().join(format!(
            "simracecenter-launcher-publisher-preserve-test-{}",
            std::process::id()
        ));
        std::env::set_var("APPDATA", &appdata);
        save(&LauncherConfig {
            active_sim: Sim::Lmu,
            ..LauncherConfig::default()
        })
        .unwrap();

        let store = FileConfigStore;
        store
            .save(&publisher_mcp::PublisherConfig {
                ingest_url: Some("https://director.example.com".to_string()),
                ..Default::default()
            })
            .unwrap();
        let config = load().unwrap();
        assert_eq!(config.active_sim, Sim::Lmu);
        assert_eq!(
            config.publisher.ingest_url.as_deref(),
            Some("https://director.example.com")
        );

        std::fs::remove_dir_all(&appdata).ok();
        std::env::remove_var("APPDATA");
    }

    #[test]
    fn publisher_table_is_optional_when_parsing_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        let path = std::env::temp_dir().join(format!(
            "simracecenter-launcher-publisher-parse-test-{}.toml",
            std::process::id()
        ));
        std::fs::write(&path, "active_sim = \"publisher\"\n").unwrap();
        let config: LauncherConfig = mcp_core::config::load_or_default(&path).unwrap();
        assert_eq!(config.active_sim, Sim::Publisher);
        assert_eq!(config.publisher, publisher_mcp::PublisherConfig::default());
        std::fs::remove_file(path).ok();
    }
}
