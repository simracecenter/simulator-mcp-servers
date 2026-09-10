// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use mcp_core::transport::http::access::CredentialRegistry;
use rand::Rng;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::config::{self, LauncherConfig};

pub const PUBLISHER_SCOPE: &[&str] = &[
    "publisher_status",
    "publisher_configure",
    "publisher_start",
    "publisher_stop",
    "get_capabilities",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PairingRecord {
    pub credential_id: Uuid,
    pub credential_sha256: String,
    pub director_name: String,
    pub paired_at: String,
}

pub trait PairingStore: Send + Sync {
    fn load(&self) -> Result<LauncherConfig, String>;
    fn save(&self, config: &LauncherConfig) -> Result<(), String>;
}

pub struct FilePairingStore;

impl PairingStore for FilePairingStore {
    fn load(&self) -> Result<LauncherConfig, String> {
        config::load().map_err(|error| error.to_string())
    }

    fn save(&self, config: &LauncherConfig) -> Result<(), String> {
        config::save(config).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
pub struct InMemoryPairingStore(Mutex<LauncherConfig>);

#[cfg(test)]
impl InMemoryPairingStore {
    pub fn new(config: LauncherConfig) -> Self {
        Self(Mutex::new(config))
    }

    pub fn config(&self) -> LauncherConfig {
        self.0.lock().expect("pairing config").clone()
    }
}

#[cfg(test)]
impl PairingStore for InMemoryPairingStore {
    fn load(&self) -> Result<LauncherConfig, String> {
        Ok(self.config())
    }

    fn save(&self, config: &LauncherConfig) -> Result<(), String> {
        *self.0.lock().expect("pairing config") = config.clone();
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct DirectorInfo {
    pub name: String,
    pub ingest_url: String,
    pub ingest_fingerprint: String,
}

#[derive(Debug, Clone)]
pub struct PairRequest {
    pub pairing_code: String,
    pub director: DirectorInfo,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub device_id: Uuid,
    pub display_name: String,
    pub credential: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairError {
    InvalidCode,
    AlreadyPaired,
    RateLimited { retry_after_secs: u64 },
    Internal(String),
}

struct Strikes {
    count: u8,
    locked_until: Option<Instant>,
}

pub struct PairingState {
    code: Mutex<String>,
    strikes: Mutex<Strikes>,
    pub credentials: Arc<CredentialRegistry>,
    store: Arc<dyn PairingStore>,
    display_name: String,
    fingerprint: String,
}

impl PairingState {
    pub fn new(
        credentials: Arc<CredentialRegistry>,
        store: Arc<dyn PairingStore>,
        display_name: String,
        fingerprint: String,
    ) -> Result<Self, String> {
        let mut config = store.load()?;
        if config.device_id.is_none() {
            config.device_id = Some(Uuid::new_v4());
            store.save(&config)?;
        }
        if let Some(pairing) = &config.pairing {
            let digest = decode_digest(&pairing.credential_sha256)?;
            credentials
                .restore(
                    pairing.credential_id,
                    digest,
                    PUBLISHER_SCOPE.iter().copied(),
                )
                .map_err(str::to_string)?;
        }
        Ok(Self {
            code: Mutex::new(generate_code()),
            strikes: Mutex::new(Strikes {
                count: 0,
                locked_until: None,
            }),
            credentials,
            store,
            display_name,
            fingerprint,
        })
    }

    pub fn pairing_code(&self) -> String {
        self.code.lock().expect("pairing code").clone()
    }

    pub fn is_paired(&self) -> bool {
        self.store
            .load()
            .map(|config| config.pairing.is_some())
            .unwrap_or(false)
    }

    pub fn device_id(&self) -> Option<Uuid> {
        self.store.load().ok().and_then(|config| config.device_id)
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn pair(&self, request: PairRequest) -> Result<PairResponse, PairError> {
        let now = Instant::now();
        {
            let mut strikes = self.strikes.lock().expect("pairing strikes");
            if let Some(until) = strikes.locked_until {
                if until > now {
                    return Err(PairError::RateLimited {
                        retry_after_secs: until.saturating_duration_since(now).as_secs().max(1),
                    });
                }
                strikes.locked_until = None;
                strikes.count = 0;
            }
            if strikes.count >= 5 {
                strikes.locked_until = Some(now + Duration::from_secs(60));
                return Err(PairError::RateLimited {
                    retry_after_secs: 60,
                });
            }
        }

        let mut config = self.store.load().map_err(PairError::Internal)?;
        if config.pairing.is_some() {
            return Err(PairError::AlreadyPaired);
        }
        if request.pairing_code != self.pairing_code() {
            let mut strikes = self.strikes.lock().expect("pairing strikes");
            strikes.count = strikes.count.saturating_add(1);
            return Err(PairError::InvalidCode);
        }

        let credential = self
            .credentials
            .issue_persistent(PUBLISHER_SCOPE.iter().copied())
            .map_err(|error| PairError::Internal(error.to_string()))?;
        let fingerprint =
            publisher::config::normalise_fingerprint(&request.director.ingest_fingerprint)
                .unwrap_or_else(|_| request.director.ingest_fingerprint.trim().to_string());
        config.publisher.ingest_url = Some(request.director.ingest_url);
        config.publisher.cert_fingerprint = Some(fingerprint);
        config.pairing = Some(PairingRecord {
            credential_id: credential.id(),
            credential_sha256: hex_digest(credential.digest()),
            director_name: request.director.name,
            paired_at: format!("{}", unix_timestamp()),
        });
        if let Err(error) = self.store.save(&config) {
            self.credentials.revoke(credential.id());
            return Err(PairError::Internal(error));
        }
        *self.strikes.lock().expect("pairing strikes") = Strikes {
            count: 0,
            locked_until: None,
        };
        rotate_code(&self.code);
        Ok(PairResponse {
            device_id: config
                .device_id
                .ok_or_else(|| PairError::Internal("launcher device id is missing".to_string()))?,
            display_name: self.display_name.clone(),
            credential: credential.token().to_string(),
            expires_at: None,
        })
    }

    pub fn unpair(&self) -> Result<(), String> {
        let mut config = self.store.load()?;
        if let Some(pairing) = config.pairing.take() {
            self.credentials.revoke(pairing.credential_id);
            self.store.save(&config)?;
        }
        rotate_code(&self.code);
        Ok(())
    }
}

fn generate_code() -> String {
    format!("{:03}", rand::rng().random_range(0..=999))
}

fn rotate_code(code: &Mutex<String>) {
    let mut next = generate_code();
    let mut current = code.lock().expect("pairing code");
    while next == *current {
        next = generate_code();
    }
    *current = next;
}

fn unix_timestamp() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn hex_digest(digest: [u8; 32]) -> String {
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_digest(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err("credential digest must be 64 hexadecimal characters".to_string());
    }
    let mut digest = [0; 32];
    for (index, pair) in value.as_bytes().chunks(2).enumerate() {
        let high = (pair[0] as char)
            .to_digit(16)
            .ok_or_else(|| "credential digest is not hexadecimal".to_string())?;
        let low = (pair[1] as char)
            .to_digit(16)
            .ok_or_else(|| "credential digest is not hexadecimal".to_string())?;
        digest[index] = ((high << 4) | low) as u8;
    }
    Ok(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(
        store: Arc<InMemoryPairingStore>,
        credentials: Arc<CredentialRegistry>,
    ) -> PairingState {
        PairingState::new(credentials, store, "rig1".to_string(), "AA:BB".to_string()).unwrap()
    }

    fn request(code: impl Into<String>) -> PairRequest {
        PairRequest {
            pairing_code: code.into(),
            director: DirectorInfo {
                name: "Director".to_string(),
                ingest_url: "https://director.example/ingest".to_string(),
                ingest_fingerprint: "not-a-fingerprint".to_string(),
            },
        }
    }

    #[test]
    fn pairing_persists_and_rotates_code() {
        let store = Arc::new(InMemoryPairingStore::new(LauncherConfig::default()));
        let credentials = Arc::new(CredentialRegistry::new());
        let pairing = state(store.clone(), credentials);
        let old_code = pairing.pairing_code();
        let response = pairing.pair(request(old_code.clone())).unwrap();
        assert_eq!(response.display_name, "rig1");
        let new_code = pairing.pairing_code();
        assert_ne!(new_code, old_code);
        assert!(pairing.is_paired());
        assert_eq!(
            store.config().publisher.ingest_url.as_deref(),
            Some("https://director.example/ingest")
        );
    }

    #[test]
    fn restored_credential_is_revoked_by_unpair() {
        let store = Arc::new(InMemoryPairingStore::new(LauncherConfig::default()));
        let credentials = Arc::new(CredentialRegistry::new());
        let pairing = state(store.clone(), credentials.clone());
        let response = pairing.pair(request(pairing.pairing_code())).unwrap();
        let restored_credentials = Arc::new(CredentialRegistry::new());
        let restored = state(store.clone(), restored_credentials.clone());
        assert!(restored.is_paired());
        restored.unpair().unwrap();
        assert!(!restored.is_paired());
        assert_eq!(response.credential.len(), 64);
        assert!(credentials.issue_persistent(["publisher_status"]).is_ok());
    }
}
