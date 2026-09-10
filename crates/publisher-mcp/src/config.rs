// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "snake_case")]
pub struct PublisherConfig {
    pub exe_path: Option<PathBuf>,
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

pub fn validate_ingest_url(url: &str) -> Result<(), String> {
    let (scheme, remainder) = url
        .split_once("://")
        .ok_or_else(|| "ingestUrl must use https:// or loopback http://".to_string())?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return Err("ingestUrl must use https:// or loopback http://".to_string());
    }

    let authority = remainder.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest
            .split_once(']')
            .ok_or_else(|| "ingestUrl has an invalid host".to_string())?;
        if !suffix.is_empty() && !suffix.starts_with(':') {
            return Err("ingestUrl has an invalid port".to_string());
        }
        host
    } else {
        authority
            .split_once(':')
            .map_or(authority, |(host, _)| host)
    };

    if host.is_empty() {
        return Err("ingestUrl must include a host".to_string());
    }
    if scheme == "https" {
        return Ok(());
    }

    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.octets()[0] == 127)
            .unwrap_or(false)
        || host == "::1";
    if loopback {
        Ok(())
    } else {
        Err("http ingestUrl is only permitted for loopback hosts".to_string())
    }
}

pub fn normalize_fingerprint(raw: &str) -> Result<String, String> {
    let chars: Vec<char> = raw.chars().collect();
    let valid_hex = |c: char| c.is_ascii_hexdigit();
    let normalized = if chars.len() == 64 && chars.iter().all(|c| valid_hex(*c)) {
        chars
    } else {
        let parts: Vec<&str> = raw.split(':').collect();
        if parts.len() != 32
            || parts
                .iter()
                .any(|part| part.len() != 2 || !part.chars().all(valid_hex))
        {
            return Err(
                "certFingerprint must be 64 hex characters or 32 colon-separated pairs".to_string(),
            );
        }
        parts.join("").chars().collect()
    };

    Ok(normalized
        .into_iter()
        .map(|c| c.to_ascii_lowercase())
        .collect())
}

pub fn default_exe_path() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.join("publisher.exe")))
        .unwrap_or_else(|| PathBuf::from("publisher.exe"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(validate_ingest_url(url).is_ok(), "{url}");
        }
        for url in [
            "",
            "director.example.com",
            "ftp://director.example.com",
            "http://192.168.1.10:9000/ingest",
            "http://[::2]:9000",
            "http://",
        ] {
            assert!(validate_ingest_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn fingerprint_normalization_table() {
        assert_eq!(
            normalize_fingerprint(
                "AABBCCDDEEFF00112233445566778899AABBCCDDEEFF00112233445566778899"
            )
            .unwrap(),
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"
        );
        assert_eq!(
            normalize_fingerprint(
                "AA:bb:CC:dd:EE:ff:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99"
            )
            .unwrap(),
            "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899"
        );
        assert!(normalize_fingerprint("zz").is_err());
        assert!(normalize_fingerprint("aa:bb").is_err());
    }
}
