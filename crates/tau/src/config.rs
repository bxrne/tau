//! Configuration for tau server via toml file.
//!
//! Example config.toml:
//!
//! ```toml
//! bind = "127.0.0.1:7070"
//! log_level = "info"
//! compact_threshold = 8
//!
//! [disk]
//! compression_level = 3  # zstd level 1-22; higher = better ratio, slower writes
//!
//! [wal]
//! enabled = true
//! path = "/var/lib/tau/tau.wal"
//! no_fsync_each = false
//!
//! [tls]
//! enabled = true
//! cert = "/etc/tau/cert.pem"
//! key  = "/etc/tau/key.pem"
//!
//! [auth]
//! enabled = true
//! username = "admin"
//! password = "secret"
//! # users_file = "/etc/tau/users"
//!
//! [metrics]
//! port = 9100
//!
//! [limits]
//! max_connections = 1024
//! idle_timeout_secs = 300   # omit or set to null to disable
//! ```
//!
//! Set `TAU_ENCRYPTION_KEY` (64 hex chars = 32 bytes) for AES-256-GCM
//! at-rest WAL encryption.

use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;

#[derive(Deserialize, Serialize, Debug, PartialEq, Clone)]
#[serde(default)]
pub(crate) struct Config {
    pub(crate) bind: String,
    pub(crate) log_level: String,
    pub(crate) compact_threshold: usize,
    pub(crate) wal: WalConfig,
    pub(crate) tls: TlsConfig,
    pub(crate) auth: AuthConfig,
    pub(crate) metrics: MetricsConfig,
    pub(crate) limits: LimitsConfig,
    pub(crate) disk: DiskConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:7070".to_string(),
            log_level: "info".to_string(),
            compact_threshold: 8,
            wal: WalConfig::default(),
            tls: TlsConfig::default(),
            auth: AuthConfig::default(),
            metrics: MetricsConfig::default(),
            limits: LimitsConfig::default(),
            disk: DiskConfig::default(),
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Eq, Clone)]
#[serde(rename_all = "lowercase")]
pub(crate) enum BackendChoice {
    #[default]
    Memory,
    Disk,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Clone)]
#[serde(default)]
pub(crate) struct DiskConfig {
    pub(crate) backend: BackendChoice,
    pub(crate) path: Option<std::path::PathBuf>,
    pub(crate) compression_level: i32,
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self {
            backend: BackendChoice::Memory,
            path: None,
            compression_level: libtau::storage::DEFAULT_ZSTD_LEVEL,
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Clone)]
#[serde(default)]
pub(crate) struct WalConfig {
    pub(crate) enabled: bool,
    pub(crate) path: Option<PathBuf>,
    pub(crate) no_fsync_each: bool,
    /// Soft size cap in mebibytes. When the WAL file on disk reaches this
    /// size, the next APPEND triggers a checkpoint rewrite to keep the file
    /// bounded. `None` (the default) means no cap.
    pub(crate) max_size_mb: Option<u64>,
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Clone)]
#[serde(default)]
pub(crate) struct TlsConfig {
    pub(crate) enabled: bool,
    pub(crate) cert: Option<PathBuf>,
    pub(crate) key: Option<PathBuf>,
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Clone)]
#[serde(default)]
pub(crate) struct AuthConfig {
    pub(crate) enabled: bool,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<String>,
    pub(crate) users_file: Option<PathBuf>,
}

#[derive(Deserialize, Serialize, Debug, Default, PartialEq, Clone)]
#[serde(default)]
pub(crate) struct MetricsConfig {
    pub(crate) port: Option<u16>,
}

#[derive(Deserialize, Serialize, Debug, PartialEq, Clone)]
#[serde(default)]
pub(crate) struct LimitsConfig {
    pub(crate) max_connections: usize,
    /// `None` means no timeout. `Some(n)` sets read/write timeout to `n` seconds.
    pub(crate) idle_timeout_secs: Option<u64>,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_connections: 1024,
            idle_timeout_secs: None,
        }
    }
}

pub(crate) fn load_config(cli_config: Option<PathBuf>) -> io::Result<Config> {
    match cli_config {
        Some(path) => {
            let text = std::fs::read_to_string(&path).map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("failed to read {}: {}", path.display(), e),
                )
            })?;
            toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
        None => {
            let default_path = PathBuf::from("config.toml");
            if default_path.exists() {
                let text = std::fs::read_to_string(&default_path)?;
                toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            } else {
                Ok(Config::default())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hegel::TestCase;
    use hegel::generators as gs;

    #[test]
    fn test_default_config() {
        let config = Config::default();
        assert_eq!(config.bind, "127.0.0.1:7070");
        assert_eq!(config.log_level, "info");
        assert_eq!(config.compact_threshold, 8);
        assert!(!config.wal.enabled);
        assert!(!config.tls.enabled);
        assert!(!config.auth.enabled);
        assert_eq!(config.metrics.port, None);
        assert_eq!(config.limits.max_connections, 1024);
        assert_eq!(config.limits.idle_timeout_secs, None);
        assert_eq!(config.disk.backend, BackendChoice::Memory);
        assert_eq!(
            config.disk.compression_level,
            libtau::storage::DEFAULT_ZSTD_LEVEL
        );
    }

    #[test]
    fn test_default_disk_config() {
        let disk_config = DiskConfig::default();
        assert_eq!(disk_config.backend, BackendChoice::Memory);
        assert_eq!(disk_config.path, None);
        assert_eq!(
            disk_config.compression_level,
            libtau::storage::DEFAULT_ZSTD_LEVEL
        );
    }

    #[test]
    fn test_defaults_limits_config() {
        let limits_config = LimitsConfig::default();
        assert_eq!(limits_config.max_connections, 1024);
        assert_eq!(limits_config.idle_timeout_secs, None);
    }

    fn optional_path_buf(tc: &TestCase) -> Option<PathBuf> {
        tc.draw(gs::optional(gs::text())).map(PathBuf::from)
    }

    #[hegel::composite]
    fn config_generator(tc: TestCase) -> Config {
        let backend = if tc.draw(gs::booleans()) {
            BackendChoice::Memory
        } else {
            BackendChoice::Disk
        };

        Config {
            bind: tc.draw(gs::text()),
            log_level: tc.draw(gs::text()),
            compact_threshold: tc.draw(gs::integers::<usize>().max_value(i64::MAX as usize)),
            wal: WalConfig {
                enabled: tc.draw(gs::booleans()),
                path: optional_path_buf(&tc),
                no_fsync_each: tc.draw(gs::booleans()),
                max_size_mb: tc.draw(gs::optional(
                    gs::integers::<u64>().min_value(1).max_value(65536),
                )),
            },
            tls: TlsConfig {
                enabled: tc.draw(gs::booleans()),
                cert: optional_path_buf(&tc),
                key: optional_path_buf(&tc),
            },
            auth: AuthConfig {
                enabled: tc.draw(gs::booleans()),
                username: tc.draw(gs::optional(gs::text())),
                password: tc.draw(gs::optional(gs::text())),
                users_file: optional_path_buf(&tc),
            },
            metrics: MetricsConfig {
                port: tc.draw(gs::optional(gs::integers::<u16>())),
            },
            limits: LimitsConfig {
                max_connections: tc.draw(gs::integers::<usize>().max_value(i64::MAX as usize)),
                idle_timeout_secs: tc.draw(gs::optional(
                    gs::integers::<u64>().max_value(i64::MAX as u64),
                )),
            },
            disk: DiskConfig {
                backend,
                path: optional_path_buf(&tc),
                compression_level: tc.draw(gs::integers::<i32>().min_value(1).max_value(22)),
            },
        }
    }

    #[hegel::test(test_cases = 1000)]
    fn property_toml_round_trip(tc: TestCase) {
        let original_config = tc.draw(config_generator());
        let toml_string = toml::to_string(&original_config)
            .expect("Valid structural Config setups must serialize smoothly");
        let parsed_config: Config = toml::from_str(&toml_string)
            .expect("Deserializer failed to parse its own valid structural layout back");
        assert_eq!(original_config, parsed_config);
    }
}
