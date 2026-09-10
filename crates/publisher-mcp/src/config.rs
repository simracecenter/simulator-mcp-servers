// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct PublisherConfig {
    pub ingest_url: Option<String>,
    pub cert_fingerprint: Option<String>,
    pub driver_display_name: Option<String>,
}

pub trait PublisherConfigStore: Send + Sync {
    fn load(&self) -> Result<PublisherConfig, String>;
    fn save(&self, config: &PublisherConfig) -> Result<(), String>;
}

pub struct DefaultConfigStore;

pub fn default_config_store() -> Arc<dyn PublisherConfigStore> {
    Arc::new(DefaultConfigStore)
}

fn config_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("SimRaceCenter")
        .join("config.toml")
}

impl PublisherConfigStore for DefaultConfigStore {
    fn load(&self) -> Result<PublisherConfig, String> {
        let text = match std::fs::read_to_string(config_path()) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Default::default())
            }
            Err(error) => return Err(error.to_string()),
        };
        let value: toml::Value = toml::from_str(&text).map_err(|error| error.to_string())?;
        value
            .get("publisher")
            .cloned()
            .map(toml::Value::try_into)
            .transpose()
            .map_err(|error| error.to_string())
            .map(|config| config.unwrap_or_default())
    }

    fn save(&self, config: &PublisherConfig) -> Result<(), String> {
        let path = config_path();
        let mut value = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text).map_err(|error| error.to_string())?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                toml::Value::Table(toml::map::Map::new())
            }
            Err(error) => return Err(error.to_string()),
        };
        value["publisher"] = toml::Value::try_from(config).map_err(|error| error.to_string())?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        std::fs::write(
            path,
            toml::to_string_pretty(&value).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())
    }
}

pub struct InMemoryConfigStore(Mutex<PublisherConfig>);

impl InMemoryConfigStore {
    pub fn new(config: PublisherConfig) -> Self {
        Self(Mutex::new(config))
    }
}

impl Default for InMemoryConfigStore {
    fn default() -> Self {
        Self::new(PublisherConfig::default())
    }
}

impl PublisherConfigStore for InMemoryConfigStore {
    fn load(&self) -> Result<PublisherConfig, String> {
        self.0
            .lock()
            .map(|config| config.clone())
            .map_err(|_| "publisher config lock is poisoned".to_string())
    }

    fn save(&self, config: &PublisherConfig) -> Result<(), String> {
        self.0
            .lock()
            .map(|mut current| *current = config.clone())
            .map_err(|_| "publisher config lock is poisoned".to_string())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ingest_url_validation_table() {
        for url in [
            "https://director.example.com",
            "https://anything/path",
            "http://localhost:9000",
            "http://127.0.0.1:9000/x",
            "http://127.255.255.255/ingest",
            "http://[::1]:9000",
        ] {
            assert!(publisher::config::validate_local_url(url).is_ok(), "{url}");
        }
        for url in [
            "",
            "director.example.com",
            "ftp://director.example.com",
            "http://192.168.1.10:9000/ingest",
            "http://[::2]:9000",
            "http://",
        ] {
            assert!(publisher::config::validate_local_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn fingerprint_normalization_table() {
        assert_eq!(
            publisher::config::normalise_fingerprint(
                "AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899"
            )
            .unwrap(),
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"
        );
        assert_eq!(
            publisher::config::normalise_fingerprint(
                "AA:bb:CC:dd:EE:ff:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99"
            )
            .unwrap(),
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"
        );
        assert!(publisher::config::normalise_fingerprint("zz").is_err());
        assert!(publisher::config::normalise_fingerprint("aa:bb").is_err());
    }
}
