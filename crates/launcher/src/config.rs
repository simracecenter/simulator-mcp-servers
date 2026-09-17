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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pairings: Vec<PairingRecord>,
    #[serde(default, skip_serializing)]
    pub pairing: Option<PairingRecord>,
}

impl LauncherConfig {
    pub fn migrate_pairings(&mut self) {
        if self.pairings.is_empty() {
            if let Some(pairing) = self.pairing.take() {
                self.pairings.push(pairing);
            }
        } else {
            self.pairing = None;
        }
    }
}

impl Default for LauncherConfig {
    fn default() -> Self {
        Self {
            active_sim: Sim::Iracing,
            publisher: publisher_mcp::PublisherConfig::default(),
            device_id: None,
            pairings: Vec::new(),
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
    let mut config: LauncherConfig = mcp_core::config::load_or_default(&config_path())?;
    config.migrate_pairings();
    Ok(config)
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
            pairings: vec![PairingRecord {
                credential_id: Uuid::new_v4(),
                credential_sha256: "ab".repeat(32),
                director_name: "Director".to_string(),
                director_fingerprint: "AA:BB".to_string(),
                paired_at: "123".to_string(),
            }],
            ..LauncherConfig::default()
        };
        save(&config).unwrap();
        let loaded = load().unwrap();
        assert_eq!(loaded.active_sim, Sim::Publisher);
        assert_eq!(loaded.device_id, Some(device_id));
        assert_eq!(loaded.pairings, config.pairings);

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

    #[test]
    fn legacy_single_pairing_migrates_into_pairings() {
        let path = std::env::temp_dir().join(format!(
            "simracecenter-launcher-pairing-migration-test-{}.toml",
            std::process::id()
        ));
        let credential_id = Uuid::new_v4();
        std::fs::write(
            &path,
            format!(
                "active_sim = \"publisher\"\n\n[pairing]\ncredential_id = \"{credential_id}\"\ncredential_sha256 = \"{}\"\ndirector_name = \"Legacy Director\"\npaired_at = \"123\"\n",
                "ab".repeat(32)
            ),
        )
        .unwrap();
        let mut config: LauncherConfig = mcp_core::config::load_or_default(&path).unwrap();
        config.migrate_pairings();
        assert_eq!(config.pairings.len(), 1);
        assert_eq!(config.pairings[0].director_name, "Legacy Director");
        assert!(config.pairings[0].director_fingerprint.is_empty());
        assert!(config.pairing.is_none());
        std::fs::remove_file(path).ok();
    }
}
