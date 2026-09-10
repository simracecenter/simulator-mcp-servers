// SPDX-License-Identifier: GPL-3.0-or-later

//! Publisher configuration: `publisher.toml` + environment variable overrides.
//!
//! # File lookup order
//! 1. Path supplied via `--config <path>` CLI flag (pass as `Some(path)` to [`load`])
//! 2. `publisher.toml` next to the running executable (`std::env::current_exe()`)
//! 3. `publisher.toml` in the current working directory
//!
//! # Environment variable overrides
//! Any field can be overridden by the matching env var (prefix `PUBLISHER_AUTH_`
//! or `PUBLISHER_`). Env vars take priority over the file.
//!
//! # Validation
//! `auth.tenant_id`, `auth.client_id`, and `auth.client_secret` are required.
//! A missing or empty value after env override causes [`load`] to return
//! [`ConfigError::Validation`] with a human-readable message.

use std::env;
use std::fmt;
use std::fs;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ── Public types ──────────────────────────────────────────────────────────────

/// Fully resolved and validated publisher configuration.
#[derive(Clone)]
pub struct PublisherConfig {
    pub destination: Destination,
    /// Present iff `destination == Destination::RaceControl`.
    pub auth: Option<AuthConfig>,
    /// Present iff `destination == Destination::Local`.
    pub local: Option<LocalConfig>,
    pub publisher: PublisherSection,
}

impl fmt::Debug for PublisherConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PublisherConfig")
            .field("destination", &self.destination)
            .field("auth", &self.auth)
            .field("local", &self.local)
            .field("publisher", &self.publisher)
            .finish()
    }
}

/// Where event batches are delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    /// Race Control cloud API (Azure AD client-credentials auth).
    RaceControl,
    /// A local collector reached over HTTP(S) with a static bearer token.
    Local,
}

impl Destination {
    /// Case-insensitive parse of `"racecontrol"` | `"local"`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "racecontrol" => Some(Destination::RaceControl),
            "local" => Some(Destination::Local),
            _ => None,
        }
    }

    /// Canonical lowercase string form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Destination::RaceControl => "racecontrol",
            Destination::Local => "local",
        }
    }
}

/// Settings for `destination = "local"`.
#[derive(Clone)]
pub struct LocalConfig {
    /// Base URL of the local collector (normalised: no trailing slash).
    pub url: String,
    /// Static bearer token sent as `Authorization: Bearer <token>`.
    pub token: String,
    /// Optional SHA-256 fingerprint of the expected server certificate,
    /// normalised to lowercase 64-char hex.
    pub cert_fingerprint: Option<String>,
}

impl fmt::Debug for LocalConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalConfig")
            .field("url", &self.url)
            .field("token", &"***")
            .field("cert_fingerprint", &self.cert_fingerprint)
            .finish()
    }
}

/// Azure AD client-credentials authentication parameters.
#[derive(Clone)]
pub struct AuthConfig {
    pub tenant_id: String,
    pub client_id: String,
    pub client_secret: String,
    pub scope: String,
    /// Optional Windows Certificate Store thumbprint (v1: documented, not implemented).
    pub cert_thumbprint: Option<String>,
}

impl fmt::Debug for AuthConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AuthConfig")
            .field("tenant_id", &self.tenant_id)
            .field("client_id", &self.client_id)
            .field("client_secret", &"***")
            .field("scope", &self.scope)
            .field("cert_thumbprint", &self.cert_thumbprint)
            .finish()
    }
}

/// Publisher-specific operational settings.
#[derive(Debug, Clone)]
pub struct PublisherSection {
    /// Base URL for the Race Control API (no trailing slash).
    pub rc_api_url: String,
    /// Interval between batch POST calls, in milliseconds.
    pub batch_interval_ms: u64,
    /// Interval between PUBLISHER_HEARTBEAT events, in milliseconds. `0` disables.
    pub heartbeat_interval_ms: u64,
    /// Interval between DRIVER_MATERIAL events, in milliseconds. `0` disables.
    pub driver_material_interval_ms: u64,
}

impl Default for PublisherSection {
    fn default() -> Self {
        Self {
            rc_api_url: "https://simracecenter.com".to_owned(),
            batch_interval_ms: 500,
            heartbeat_interval_ms: 15_000,
            driver_material_interval_ms: 25_000,
        }
    }
}

/// Errors produced by [`load`].
#[derive(Debug)]
pub enum ConfigError {
    /// Config file could not be read.
    Io(std::io::Error),
    /// Config file could not be parsed as TOML.
    Toml(toml::de::Error),
    /// A required field is missing or empty.
    Validation(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "config file read error: {e}"),
            ConfigError::Toml(e) => write!(f, "config file parse error: {e}"),
            ConfigError::Validation(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Toml(e) => Some(e),
            _ => None,
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Load, merge with env vars, and validate the publisher configuration.
///
/// `config_path` should be `Some(path)` when the caller supplies `--config`.
/// Pass `None` to use the default file-lookup order.
pub fn load(config_path: Option<&Path>) -> Result<PublisherConfig, ConfigError> {
    let toml_str = read_config_file(config_path)?;
    let mut raw: RawConfig = toml::from_str(&toml_str).map_err(ConfigError::Toml)?;
    apply_env_overrides(&mut raw);
    build_and_validate(raw)
}

// ── TOML raw types (allow missing required fields — env vars may supply them) ─

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawConfig {
    auth: RawAuth,
    publisher: RawPublisher,
    local: RawLocal,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawLocal {
    url: Option<String>,
    token: Option<String>,
    cert_fingerprint: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawAuth {
    tenant_id: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
    scope: Option<String>,
    cert_thumbprint: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawPublisher {
    destination: Option<String>,
    rc_api_url: Option<String>,
    batch_interval_ms: Option<u64>,
    heartbeat_interval_ms: Option<u64>,
    driver_material_interval_ms: Option<u64>,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn read_config_file(config_path: Option<&Path>) -> Result<String, ConfigError> {
    // 1. Explicit path
    if let Some(path) = config_path {
        return fs::read_to_string(path).map_err(ConfigError::Io);
    }

    // 2. Next to the executable
    if let Ok(exe) = env::current_exe() {
        let candidate = exe
            .parent()
            .unwrap_or(Path::new("."))
            .join("publisher.toml");
        if candidate.exists() {
            return fs::read_to_string(candidate).map_err(ConfigError::Io);
        }
    }

    // 3. Current working directory — return empty TOML if absent (env vars may be sufficient)
    let cwd_candidate = PathBuf::from("publisher.toml");
    if cwd_candidate.exists() {
        return fs::read_to_string(cwd_candidate).map_err(ConfigError::Io);
    }

    // No file found — return empty TOML; validation may fail if env vars are absent too
    Ok(String::new())
}

fn apply_env_overrides(raw: &mut RawConfig) {
    macro_rules! override_from_env {
        ($field:expr, $env_var:literal) => {
            if let Ok(v) = env::var($env_var) {
                if !v.is_empty() {
                    $field = Some(v);
                }
            }
        };
    }

    override_from_env!(raw.auth.tenant_id, "PUBLISHER_AUTH_TENANT_ID");
    override_from_env!(raw.auth.client_id, "PUBLISHER_AUTH_CLIENT_ID");
    override_from_env!(raw.auth.client_secret, "PUBLISHER_AUTH_CLIENT_SECRET");
    override_from_env!(raw.auth.scope, "PUBLISHER_AUTH_SCOPE");
    override_from_env!(raw.publisher.destination, "PUBLISHER_DESTINATION");
    override_from_env!(raw.publisher.rc_api_url, "PUBLISHER_RC_API_URL");
    override_from_env!(raw.local.url, "PUBLISHER_LOCAL_URL");
    override_from_env!(raw.local.token, "PUBLISHER_LOCAL_TOKEN");
    override_from_env!(
        raw.local.cert_fingerprint,
        "PUBLISHER_LOCAL_CERT_FINGERPRINT"
    );

    if let Ok(v) = env::var("PUBLISHER_BATCH_INTERVAL_MS") {
        if let Ok(n) = v.parse::<u64>() {
            raw.publisher.batch_interval_ms = Some(n);
        }
    }

    if let Ok(v) = env::var("PUBLISHER_HEARTBEAT_INTERVAL_MS") {
        if let Ok(n) = v.parse::<u64>() {
            raw.publisher.heartbeat_interval_ms = Some(n);
        }
    }

    if let Ok(v) = env::var("PUBLISHER_DRIVER_MATERIAL_INTERVAL_MS") {
        if let Ok(n) = v.parse::<u64>() {
            raw.publisher.driver_material_interval_ms = Some(n);
        }
    }
}

/// Validate a `local.url` value: `https://` is allowed for any host;
/// `http://` is allowed only for loopback (`127.0.0.1`, `localhost`, `[::1]`).
/// Returns the URL with any trailing `/` stripped.
pub fn validate_local_url(url: &str) -> Result<String, String> {
    let url = url.trim().trim_end_matches('/');

    let (scheme, rest) = url.split_once("://").ok_or_else(local_url_error)?;

    match scheme.to_ascii_lowercase().as_str() {
        "https" => {}
        "http" => {
            // Host is everything before the optional `:port` or first `/`.
            let host_port = rest.split('/').next().unwrap_or_default();
            let host = if host_port.starts_with('[') {
                // IPv6 literal — up to and including the closing bracket.
                match host_port.find(']') {
                    Some(end) => &host_port[..=end],
                    None => host_port,
                }
            } else {
                host_port.split(':').next().unwrap_or_default()
            };
            let host = host.to_ascii_lowercase();
            let ipv4_loopback = host
                .parse::<Ipv4Addr>()
                .is_ok_and(|address| address.octets()[0] == 127);
            if !ipv4_loopback && host != "localhost" && host != "[::1]" {
                return Err(local_url_error());
            }
        }
        _ => return Err(local_url_error()),
    }

    Ok(url.to_owned())
}

fn local_url_error() -> String {
    "local.url must be https, or http only for loopback (127.0.0.1/localhost)".to_owned()
}

/// Normalise a certificate fingerprint: strip `:` and whitespace, lowercase,
/// require exactly 64 hex characters (SHA-256).
pub fn normalise_fingerprint(s: &str) -> Result<String, String> {
    let clean: String = s
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .flat_map(|c| c.to_lowercase())
        .collect();
    if clean.len() == 64 && clean.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(clean)
    } else {
        Err("local.cert_fingerprint must be a SHA-256 fingerprint \
             (64 hex characters, colons and spaces allowed)"
            .to_owned())
    }
}

fn build_and_validate(raw: RawConfig) -> Result<PublisherConfig, ConfigError> {
    let require = |field: Option<String>, name: &str| -> Result<String, ConfigError> {
        match field.filter(|s| !s.is_empty()) {
            Some(v) => Ok(v),
            None => Err(ConfigError::Validation(format!(
                "[publisher] ERROR: auth.{name} is required. \
                 Set it in publisher.toml or {}.\n\
                 See: https://simracecenter.com/docs/rig-setup for Azure AD provisioning steps.",
                env_var_name(name),
            ))),
        }
    };

    let destination_raw = raw.publisher.destination.filter(|s| !s.is_empty());
    let destination = match destination_raw.as_deref() {
        None => Destination::RaceControl,
        Some(s) => Destination::parse(s).ok_or_else(|| {
            ConfigError::Validation(format!(
                "[publisher] ERROR: unknown destination '{s}'. \
             Allowed values for publisher.destination / PUBLISHER_DESTINATION: \
             'racecontrol', 'local'.",
            ))
        })?,
    };

    let auth = if destination == Destination::RaceControl {
        Some(AuthConfig {
            tenant_id: require(raw.auth.tenant_id, "tenant_id")?,
            client_id: require(raw.auth.client_id, "client_id")?,
            client_secret: require(raw.auth.client_secret, "client_secret")?,
            scope: raw.auth.scope.filter(|s| !s.is_empty()).unwrap_or_else(|| {
                "api://racecontrol-api-a780e279-1cb6-4ed0-9ef6-49029aa50a42/.default".to_owned()
            }),
            cert_thumbprint: raw.auth.cert_thumbprint.filter(|s| !s.is_empty()),
        })
    } else {
        None
    };

    let local = if destination == Destination::Local {
        let require_local =
            |field: Option<String>, key: &str, env_var: &str| -> Result<String, ConfigError> {
                match field.filter(|s| !s.is_empty()) {
                    Some(v) => Ok(v),
                    None => Err(ConfigError::Validation(format!(
                        "[publisher] ERROR: local.{key} is required when \
                     destination = \"local\". Set it in the [local] table of \
                     publisher.toml or {env_var}.",
                    ))),
                }
            };

        let url = require_local(raw.local.url, "url", "PUBLISHER_LOCAL_URL")
            .and_then(|u| validate_local_url(&u).map_err(ConfigError::Validation))?;
        let token = require_local(raw.local.token, "token", "PUBLISHER_LOCAL_TOKEN")?;
        let cert_fingerprint = raw
            .local
            .cert_fingerprint
            .filter(|s| !s.is_empty())
            .map(|fp| normalise_fingerprint(&fp).map_err(ConfigError::Validation))
            .transpose()?;

        Some(LocalConfig {
            url,
            token,
            cert_fingerprint,
        })
    } else {
        None
    };

    let defaults = PublisherSection::default();

    Ok(PublisherConfig {
        destination,
        auth,
        local,
        publisher: PublisherSection {
            rc_api_url: raw
                .publisher
                .rc_api_url
                .filter(|s| !s.is_empty())
                .unwrap_or(defaults.rc_api_url),
            batch_interval_ms: raw
                .publisher
                .batch_interval_ms
                .unwrap_or(defaults.batch_interval_ms),
            heartbeat_interval_ms: raw
                .publisher
                .heartbeat_interval_ms
                .unwrap_or(defaults.heartbeat_interval_ms),
            driver_material_interval_ms: raw
                .publisher
                .driver_material_interval_ms
                .unwrap_or(defaults.driver_material_interval_ms),
        },
    })
}

fn env_var_name(field: &str) -> String {
    let key = field.to_uppercase();
    format!("PUBLISHER_AUTH_{key}")
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Serialises tests that mutate process env vars.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn parse(toml: &str) -> PublisherConfig {
        let raw: RawConfig = toml::from_str(toml).expect("valid toml");
        // No env vars in unit tests — isolate file parsing only
        build_and_validate(raw).expect("valid config")
    }

    #[test]
    fn parse_full_config() {
        let cfg = parse(
            r#"
[auth]
tenant_id     = "tenant-123"
client_id     = "client-456"
client_secret = "secret-789"
scope         = "api://rc/.default"

[publisher]
rc_api_url            = "https://api.example.com"
batch_interval_ms     = 250
heartbeat_interval_ms = 5000
"#,
        );
        let auth = cfg.auth.as_ref().expect("racecontrol has auth");
        assert_eq!(auth.tenant_id, "tenant-123");
        assert_eq!(auth.client_id, "client-456");
        assert_eq!(auth.client_secret, "secret-789");
        assert_eq!(auth.scope, "api://rc/.default");
        assert_eq!(cfg.destination, Destination::RaceControl);
        assert!(cfg.local.is_none());
        assert_eq!(cfg.publisher.rc_api_url, "https://api.example.com");
        assert_eq!(cfg.publisher.batch_interval_ms, 250);
        assert_eq!(cfg.publisher.heartbeat_interval_ms, 5000);
    }

    #[test]
    fn defaults_applied_when_publisher_section_absent() {
        let cfg = parse(
            r#"
[auth]
tenant_id     = "t"
client_id     = "c"
client_secret = "s"
"#,
        );
        assert_eq!(
            cfg.auth.as_ref().unwrap().scope,
            "api://racecontrol-api-a780e279-1cb6-4ed0-9ef6-49029aa50a42/.default"
        );
        assert_eq!(cfg.publisher.rc_api_url, "https://simracecenter.com");
        assert_eq!(cfg.publisher.batch_interval_ms, 500);
        assert_eq!(cfg.publisher.heartbeat_interval_ms, 15_000);
    }

    #[test]
    fn heartbeat_interval_zero_accepted() {
        let cfg = parse(
            r#"
[auth]
tenant_id     = "t"
client_id     = "c"
client_secret = "s"

[publisher]
heartbeat_interval_ms = 0
"#,
        );
        assert_eq!(cfg.publisher.heartbeat_interval_ms, 0);
    }

    #[test]
    fn missing_client_id_returns_validation_error() {
        let mut raw = RawConfig::default();
        raw.auth.tenant_id = Some("t".to_owned());
        raw.auth.client_secret = Some("s".to_owned());
        // client_id intentionally absent
        let err = build_and_validate(raw).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("client_id"),
            "error message should name the missing field"
        );
        assert!(
            msg.contains("PUBLISHER_AUTH_CLIENT_ID"),
            "error message should name the env var"
        );
    }

    #[test]
    fn env_override_applied() {
        let mut raw = RawConfig {
            auth: RawAuth {
                tenant_id: Some("t".to_owned()),
                client_id: Some("c".to_owned()),
                client_secret: Some("s".to_owned()),
                scope: None,
                cert_thumbprint: None,
            },
            publisher: RawPublisher {
                destination: None,
                rc_api_url: Some("https://original.com".to_owned()),
                batch_interval_ms: Some(500),
                heartbeat_interval_ms: Some(15_000),
                driver_material_interval_ms: Some(25_000),
            },
            local: RawLocal::default(),
        };

        // Simulate env var override by directly modifying (avoids polluting test env)
        raw.publisher.rc_api_url = Some("https://override.com".to_owned());
        raw.publisher.batch_interval_ms = Some(100);

        let cfg = build_and_validate(raw).unwrap();
        assert_eq!(cfg.publisher.rc_api_url, "https://override.com");
        assert_eq!(cfg.publisher.batch_interval_ms, 100);
    }

    // ── Destination / local ────────────────────────────────────────────────

    #[test]
    fn destination_defaults_to_racecontrol() {
        let cfg = parse(
            r#"
[auth]
tenant_id     = "t"
client_id     = "c"
client_secret = "s"
"#,
        );
        assert_eq!(cfg.destination, Destination::RaceControl);
        assert!(cfg.auth.is_some());
        assert!(cfg.local.is_none());
    }

    #[test]
    fn local_destination_parses_without_auth() {
        let cfg = parse(
            r#"
[publisher]
destination = "local"

[local]
url   = "https://collector.internal:8443/"
token = "tok-abc"
cert_fingerprint = "AA:BB:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99:aa:bb:cc:dd:ee:ff:00:11:22:33:44:55:66:77:88:99"
"#,
        );
        assert_eq!(cfg.destination, Destination::Local);
        assert!(
            cfg.auth.is_none(),
            "local destination must not require auth"
        );
        let local = cfg.local.as_ref().expect("local config present");
        assert_eq!(local.url, "https://collector.internal:8443");
        assert_eq!(local.token, "tok-abc");
        assert_eq!(
            local.cert_fingerprint.as_deref(),
            Some("aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899")
        );
    }

    #[test]
    fn local_without_token_is_validation_error() {
        let raw: RawConfig = toml::from_str(
            r#"
[publisher]
destination = "local"

[local]
url = "https://collector.internal"
"#,
        )
        .expect("valid toml");
        let err = build_and_validate(raw).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        let msg = err.to_string();
        assert!(msg.contains("local.token"));
        assert!(msg.contains("PUBLISHER_LOCAL_TOKEN"));
    }

    #[test]
    fn local_without_url_is_validation_error() {
        let raw: RawConfig = toml::from_str(
            r#"
[publisher]
destination = "local"

[local]
token = "t"
"#,
        )
        .expect("valid toml");
        let err = build_and_validate(raw).unwrap_err();
        assert!(matches!(err, ConfigError::Validation(_)));
        assert!(err.to_string().contains("local.url"));
    }

    #[test]
    fn unknown_destination_is_validation_error() {
        let raw: RawConfig = toml::from_str(
            r#"
[publisher]
destination = "moon"
"#,
        )
        .expect("valid toml");
        let err = build_and_validate(raw).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("racecontrol") && msg.contains("local"));
        assert!(!msg.contains("moon") || msg.contains("'moon'"));
    }

    #[test]
    fn destination_parse_is_case_insensitive() {
        assert_eq!(Destination::parse("Local"), Some(Destination::Local));
        assert_eq!(
            Destination::parse("RACECONTROL"),
            Some(Destination::RaceControl)
        );
        assert_eq!(Destination::parse("nope"), None);
    }

    #[test]
    fn local_url_validation_table() {
        // https allowed for any host
        assert_eq!(
            validate_local_url("https://collector.example.com").unwrap(),
            "https://collector.example.com"
        );
        // trailing slash stripped
        assert_eq!(
            validate_local_url("https://collector.example.com/").unwrap(),
            "https://collector.example.com"
        );
        // http loopback allowed
        assert_eq!(
            validate_local_url("http://127.0.0.1:8443").unwrap(),
            "http://127.0.0.1:8443"
        );
        assert_eq!(
            validate_local_url("http://localhost").unwrap(),
            "http://localhost"
        );
        assert_eq!(
            validate_local_url("http://[::1]:9090").unwrap(),
            "http://[::1]:9090"
        );
        // http non-loopback rejected
        assert!(validate_local_url("http://192.168.1.5").is_err());
        assert!(validate_local_url("http://collector.internal").is_err());
        // other schemes rejected
        assert!(validate_local_url("ftp://127.0.0.1").is_err());
        assert!(validate_local_url("127.0.0.1:8080").is_err());
    }

    #[test]
    fn fingerprint_normalisation() {
        let hex = "aabbccddeeff00112233445566778899aabbccddeeff00112233445566778899";
        // colons + uppercase normalise
        let colon: String = hex
            .chars()
            .collect::<Vec<_>>()
            .chunks(2)
            .map(|c| c.iter().collect::<String>().to_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(normalise_fingerprint(&colon).unwrap(), hex);
        // whitespace stripped
        assert_eq!(normalise_fingerprint(&format!("  {hex} \n")).unwrap(), hex);
        // wrong length / non-hex rejected
        assert!(normalise_fingerprint("abcd").is_err());
        assert!(normalise_fingerprint(&"z".repeat(64)).is_err());
        assert!(normalise_fingerprint(&format!("{hex}00")).is_err());
    }

    #[test]
    fn env_overrides_win_over_file_for_local() {
        let _guard = ENV_LOCK.lock().unwrap();

        let fp_file = "aa".repeat(32);
        let fp_env = "bb".repeat(32);

        std::env::set_var("PUBLISHER_DESTINATION", "local");
        std::env::set_var("PUBLISHER_LOCAL_URL", "https://env.example.com:9443/");
        std::env::set_var("PUBLISHER_LOCAL_TOKEN", "env-token");
        std::env::set_var("PUBLISHER_LOCAL_CERT_FINGERPRINT", &fp_env);
        // Also prove file's racecontrol auth values can't leak through:
        std::env::remove_var("PUBLISHER_AUTH_TENANT_ID");
        std::env::remove_var("PUBLISHER_AUTH_CLIENT_ID");
        std::env::remove_var("PUBLISHER_AUTH_CLIENT_SECRET");
        std::env::remove_var("PUBLISHER_AUTH_SCOPE");

        let raw: RawConfig = toml::from_str(&format!(
            r#"
[publisher]
destination = "racecontrol"

[local]
url = "https://file.example.com"
token = "file-token"
cert_fingerprint = "{fp_file}"
"#
        ))
        .expect("valid toml");

        let mut raw = raw;
        apply_env_overrides(&mut raw);
        let cfg = build_and_validate(raw).unwrap();

        assert_eq!(cfg.destination, Destination::Local, "env destination wins");
        let local = cfg.local.unwrap();
        assert_eq!(local.url, "https://env.example.com:9443");
        assert_eq!(local.token, "env-token");
        assert_eq!(local.cert_fingerprint.as_deref(), Some(fp_env.as_str()));

        std::env::remove_var("PUBLISHER_DESTINATION");
        std::env::remove_var("PUBLISHER_LOCAL_URL");
        std::env::remove_var("PUBLISHER_LOCAL_TOKEN");
        std::env::remove_var("PUBLISHER_LOCAL_CERT_FINGERPRINT");
    }
}
