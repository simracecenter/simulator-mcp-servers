// SPDX-License-Identifier: GPL-3.0-or-later

use std::sync::Mutex;

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
