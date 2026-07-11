//! Generic TOML config load/save + CLI-override merge helpers (ADR 0001 D4).
//!
//! Each `<sim>-mcp` crate (and the launcher) defines its own config struct;
//! this module owns the file I/O and the "CLI flag wins over file value"
//! merge rule so that logic is written once.

use std::path::Path;

use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("failed to read config file {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to write config file {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to parse config file {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error("failed to serialize config: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Loads `T` from `path`, or returns `T::default()` if the file doesn't exist
/// yet (e.g. first run before the user has saved anything from the UI).
pub fn load_or_default<T: Default + DeserializeOwned>(path: &Path) -> Result<T, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(contents) => toml::from_str(&contents).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        }),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(source) => Err(ConfigError::Read {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Serializes `config` as TOML and writes it to `path`, creating parent
/// directories as needed.
pub fn save<T: Serialize>(path: &Path, config: &T) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: path.display().to_string(),
            source,
        })?;
    }

    let encoded = toml::to_string_pretty(config)?;
    std::fs::write(path, encoded).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })
}

/// A CLI-provided value always wins over the value loaded from the config
/// file; `None` means "not passed on the command line this run."
pub fn override_with<T>(from_file: T, from_cli: Option<T>) -> T {
    from_cli.unwrap_or(from_file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
    struct ExampleConfig {
        active_sim: String,
    }

    #[test]
    fn load_or_default_returns_default_when_file_missing() {
        let path = Path::new("/nonexistent/simulator-mcp-servers-test/config.toml");
        let config: ExampleConfig = load_or_default(path).unwrap();
        assert_eq!(config, ExampleConfig::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = std::env::temp_dir().join(format!("smcp-test-{}", std::process::id()));
        let path = dir.join("config.toml");
        let written = ExampleConfig {
            active_sim: "iracing".to_string(),
        };

        save(&path, &written).unwrap();
        let read_back: ExampleConfig = load_or_default(&path).unwrap();

        assert_eq!(written, read_back);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cli_override_wins_over_file_value() {
        let from_file = "iracing".to_string();
        let from_cli = Some("lmu".to_string());
        assert_eq!(override_with(from_file, from_cli), "lmu");
    }

    #[test]
    fn missing_cli_override_keeps_file_value() {
        let from_file = "iracing".to_string();
        assert_eq!(override_with(from_file.clone(), None), from_file);
    }
}
