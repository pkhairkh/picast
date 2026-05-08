//! PiCast Configuration
//!
//! Loads application configuration from a TOML file and/or environment
//! variables. Configuration sources are merged with the following
//! precedence (highest to lowest):
//!
//! 1. Environment variables (`PICAST_*`)
//! 2. TOML config file (`picast.toml` or path from `PICAST_CONFIG`)
//! 3. Built-in defaults
//!
//! ## Example `picast.toml`
//!
//! ```toml
//! [server]
//! http_addr = "0.0.0.0:8585"
//! ws_addr = "0.0.0.0:8586"
//! db_path = "/var/lib/picast/sessions.db"
//!
//! [tor]
//! socks_addr = "127.0.0.1:9050"
//! control_port = 9051
//! cookie_path = "/run/tor/control.authcookie"
//!
//! [display]
//! drm_device = ""
//!
//! [dlna]
//! friendly_name = "PiCast"
//!
//! [logging]
//! level = "info"
//! ```

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

// ── Top-level Config ──────────────────────────────────────────────────

/// Full application configuration.
///
/// Deserialized from a TOML file and/or populated from environment
/// variables. All fields have sensible defaults so PiCast works
/// out of the box with no config file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppConfig {
    /// Server listen addresses and database path.
    #[serde(default)]
    pub server: ServerConfig,
    /// Tor SOCKS proxy configuration.
    #[serde(default)]
    pub tor: TorConfig,
    /// Display/DRM configuration.
    #[serde(default)]
    pub display: DisplayConfig,
    /// Playback pipeline configuration.
    #[serde(default)]
    pub playback: PlaybackConfig,
    /// DLNA renderer configuration.
    #[serde(default)]
    pub dlna: DlnaConfig,
    /// Logging configuration.
    #[serde(default)]
    pub logging: LoggingConfig,
}

// ── Sub-configs ──────────────────────────────────────────────────────

/// HTTP/WS server configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// HTTP API listen address.
    #[serde(default = "default_http_addr")]
    pub http_addr: String,
    /// WebSocket listen address.
    #[serde(default = "default_ws_addr")]
    pub ws_addr: String,
    /// Session database path.
    #[serde(default = "default_db_path")]
    pub db_path: String,
    /// Path to TLS certificate (PEM). If set, HTTPS/WSS is enabled.
    #[serde(default)]
    pub tls_cert_path: String,
    /// Path to TLS private key (PEM). If set, HTTPS/WSS is enabled.
    #[serde(default)]
    pub tls_key_path: String,
}

impl ServerConfig {
    /// Returns true if both cert and key paths are set, enabling TLS.
    pub fn tls_enabled(&self) -> bool {
        !self.tls_cert_path.is_empty() && !self.tls_key_path.is_empty()
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            http_addr: default_http_addr(),
            ws_addr: default_ws_addr(),
            db_path: default_db_path(),
            tls_cert_path: String::new(),
            tls_key_path: String::new(),
        }
    }
}

/// Tor SOCKS proxy configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TorConfig {
    /// SOCKS5 proxy address (host:port).
    #[serde(default = "default_socks_addr")]
    pub socks_addr: String,
    /// Tor control port used for health checks and lifecycle control.
    #[serde(default = "default_tor_control_port")]
    pub control_port: u16,
    /// Tor control authentication cookie path.
    #[serde(default = "default_tor_cookie_path")]
    pub cookie_path: String,
}

impl Default for TorConfig {
    fn default() -> Self {
        Self {
            socks_addr: default_socks_addr(),
            control_port: default_tor_control_port(),
            cookie_path: default_tor_cookie_path(),
        }
    }
}

/// Display/DRM configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisplayConfig {
    /// DRM device path (empty = auto-detect).
    #[serde(default)]
    pub drm_device: String,
}

/// Playback pipeline configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybackConfig {
    /// ALSA device string for audio output (e.g. "plughw:1,0" for HDMI).
    /// When empty, alsasink uses the ALSA default device.
    #[serde(default)]
    pub audio_device: String,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self { audio_device: String::new() }
    }
}

/// DLNA renderer configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DlnaConfig {
    /// Friendly name advertised on the network.
    #[serde(default = "default_dlna_name")]
    pub friendly_name: String,
    /// DLNA renderer listen port.
    #[serde(default = "default_dlna_port")]
    pub port: u16,
}

impl Default for DlnaConfig {
    fn default() -> Self {
        Self { friendly_name: default_dlna_name(), port: default_dlna_port() }
    }
}

/// Logging configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    /// Log level filter (trace, debug, info, warn, error).
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self { level: default_log_level() }
    }
}

// ── Default value functions ───────────────────────────────────────────

fn default_http_addr() -> String {
    "0.0.0.0:8585".into()
}
fn default_ws_addr() -> String {
    "0.0.0.0:8586".into()
}
fn default_db_path() -> String {
    "/var/lib/picast/sessions.db".into()
}
fn default_socks_addr() -> String {
    "127.0.0.1:9050".into()
}
fn default_tor_control_port() -> u16 {
    9051
}
fn default_tor_cookie_path() -> String {
    "/run/tor/control.authcookie".into()
}
fn default_dlna_name() -> String {
    "PiCast".into()
}
fn default_dlna_port() -> u16 {
    49152
}
fn default_log_level() -> String {
    "info".into()
}

// ── Loading & Merging ─────────────────────────────────────────────────

/// Configuration file search paths (in order of priority).
const CONFIG_SEARCH_PATHS: &[&str] =
    &["picast.toml", "/etc/picast/picast.toml", "/usr/local/etc/picast/picast.toml"];

impl AppConfig {
    /// Load configuration from file (if found) and merge with env vars.
    ///
    /// The config file is searched in the following locations:
    /// 1. `PICAST_CONFIG` env var (explicit path)
    /// 2. `./picast.toml` (current directory)
    /// 3. `/etc/picast/picast.toml`
    /// 4. `/usr/local/etc/picast/picast.toml`
    ///
    /// If no config file is found, defaults are used and then overridden
    /// by any `PICAST_*` environment variables that are set.
    pub fn load() -> Result<Self> {
        let mut config = Self::load_from_file()?;
        config.merge_env();
        Ok(config)
    }

    /// Load configuration from a TOML file.
    ///
    /// Searches the standard paths unless `PICAST_CONFIG` is set.
    /// Returns default config if no file is found.
    pub fn load_from_file() -> Result<Self> {
        // Check for explicit config path.
        let explicit_path = std::env::var("PICAST_CONFIG").ok();

        let config_path = if let Some(ref path) = explicit_path {
            if !Path::new(path).exists() {
                anyhow::bail!("PICAST_CONFIG={:?} does not exist", path);
            }
            Some(path.clone())
        } else {
            // Search standard paths.
            CONFIG_SEARCH_PATHS.iter().find(|p| Path::new(p).exists()).map(|s| s.to_string())
        };

        match config_path {
            Some(path) => {
                tracing::info!(path = %path, "loading configuration file");
                let contents = std::fs::read_to_string(&path)
                    .with_context(|| format!("failed to read config file: {}", path))?;
                let config: AppConfig = toml::from_str(&contents)
                    .with_context(|| format!("failed to parse config file: {}", path))?;
                tracing::info!(path = %path, "configuration loaded from file");
                Ok(config)
            },
            None => {
                tracing::info!("no configuration file found — using defaults + env vars");
                Ok(Self::default())
            },
        }
    }

    /// Override config fields with environment variables.
    ///
    /// Environment variables take precedence over config file values.
    fn merge_env(&mut self) {
        if let Ok(v) = std::env::var("PICAST_HTTP_ADDR") {
            self.server.http_addr = v;
        }
        if let Ok(v) = std::env::var("PICAST_WS_ADDR") {
            self.server.ws_addr = v;
        }
        if let Ok(v) = std::env::var("PICAST_DB_PATH") {
            self.server.db_path = v;
        }
        if let Ok(v) = std::env::var("PICAST_TOR_SOCKS") {
            self.tor.socks_addr = v;
        }
        if let Ok(v) = std::env::var("PICAST_TOR_CONTROL_PORT") {
            if let Ok(port) = v.parse::<u16>() {
                self.tor.control_port = port;
            } else {
                tracing::warn!(value = %v, "PICAST_TOR_CONTROL_PORT is not a valid port number");
            }
        }
        if let Ok(v) = std::env::var("PICAST_TOR_COOKIE_PATH") {
            self.tor.cookie_path = v;
        }
        if let Ok(v) = std::env::var("PICAST_DRM_DEVICE") {
            self.display.drm_device = v;
        }
        if let Ok(v) = std::env::var("PICAST_AUDIO_DEVICE") {
            self.playback.audio_device = v;
        }
        if let Ok(v) = std::env::var("PICAST_DLNA_NAME") {
            self.dlna.friendly_name = v;
        }
        if let Ok(v) = std::env::var("PICAST_DLNA_PORT") {
            if let Ok(port) = v.parse::<u16>() {
                self.dlna.port = port;
            } else {
                tracing::warn!(value = %v, "PICAST_DLNA_PORT is not a valid port number");
            }
        }
        if let Ok(v) = std::env::var("PICAST_TLS_CERT") {
            self.server.tls_cert_path = v;
        }
        if let Ok(v) = std::env::var("PICAST_TLS_KEY") {
            self.server.tls_key_path = v;
        }
        if let Ok(v) = std::env::var("PICAST_LOG_LEVEL") {
            self.logging.level = v;
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_values() {
        let config = AppConfig::default();
        assert_eq!(config.server.http_addr, "0.0.0.0:8585");
        assert_eq!(config.server.ws_addr, "0.0.0.0:8586");
        assert_eq!(config.server.db_path, "/var/lib/picast/sessions.db");
        assert_eq!(config.tor.socks_addr, "127.0.0.1:9050");
        assert_eq!(config.tor.control_port, 9051);
        assert_eq!(config.tor.cookie_path, "/run/tor/control.authcookie");
        assert_eq!(config.display.drm_device, "");
        assert_eq!(config.dlna.friendly_name, "PiCast");
        assert_eq!(config.logging.level, "info");
    }

    #[test]
    fn parse_minimal_toml() {
        let toml = r#"
[server]
http_addr = "0.0.0.0:9999"
"#;
        let config: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.server.http_addr, "0.0.0.0:9999");
        // Unset fields should use defaults.
        assert_eq!(config.server.ws_addr, "0.0.0.0:8586");
        assert_eq!(config.tor.socks_addr, "127.0.0.1:9050");
        assert_eq!(config.tor.control_port, 9051);
        assert_eq!(config.tor.cookie_path, "/run/tor/control.authcookie");
    }

    #[test]
    fn parse_full_toml() {
        let toml = r#"
[server]
http_addr = "0.0.0.0:8080"
ws_addr = "0.0.0.0:8081"
db_path = "/tmp/picast-test.db"

[tor]
socks_addr = "127.0.0.1:19050"
control_port = 19051
cookie_path = "/tmp/picast-test/control_auth_cookie"

[display]
drm_device = "/dev/dri/card1"

[dlna]
friendly_name = "Living Room Pi"

[logging]
level = "debug"
"#;
        let config: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.server.http_addr, "0.0.0.0:8080");
        assert_eq!(config.server.ws_addr, "0.0.0.0:8081");
        assert_eq!(config.server.db_path, "/tmp/picast-test.db");
        assert_eq!(config.tor.socks_addr, "127.0.0.1:19050");
        assert_eq!(config.tor.control_port, 19051);
        assert_eq!(config.tor.cookie_path, "/tmp/picast-test/control_auth_cookie");
        assert_eq!(config.display.drm_device, "/dev/dri/card1");
        assert_eq!(config.dlna.friendly_name, "Living Room Pi");
        assert_eq!(config.logging.level, "debug");
    }

    #[test]
    fn parse_empty_toml() {
        let config: AppConfig = toml::from_str("").unwrap();
        // All defaults should be applied.
        assert_eq!(config.server.http_addr, "0.0.0.0:8585");
        assert_eq!(config.tor.socks_addr, "127.0.0.1:9050");
    }

    #[test]
    fn config_serialization_roundtrip() {
        let original = AppConfig::default();
        let toml_str = toml::to_string(&original).unwrap();
        let parsed: AppConfig = toml::from_str(&toml_str).unwrap();
        assert_eq!(parsed.server.http_addr, original.server.http_addr);
        assert_eq!(parsed.server.ws_addr, original.server.ws_addr);
        assert_eq!(parsed.tor.socks_addr, original.tor.socks_addr);
        assert_eq!(parsed.tor.control_port, original.tor.control_port);
        assert_eq!(parsed.tor.cookie_path, original.tor.cookie_path);
        assert_eq!(parsed.dlna.friendly_name, original.dlna.friendly_name);
        assert_eq!(parsed.logging.level, original.logging.level);
    }

    #[test]
    fn merge_env_overrides_config() {
        let mut config = AppConfig::default();
        assert_eq!(config.server.http_addr, "0.0.0.0:8585");

        // Set env var.
        std::env::set_var("PICAST_HTTP_ADDR", "0.0.0.0:9999");
        config.merge_env();
        assert_eq!(config.server.http_addr, "0.0.0.0:9999");

        // Clean up.
        std::env::remove_var("PICAST_HTTP_ADDR");
    }

    #[test]
    fn merge_env_applies_defaults_for_unset() {
        // Clear any PICAST env vars from other tests.
        std::env::remove_var("PICAST_HTTP_ADDR");
        std::env::remove_var("PICAST_WS_ADDR");
        std::env::remove_var("PICAST_TOR_SOCKS");
        std::env::remove_var("PICAST_TOR_CONTROL_PORT");
        std::env::remove_var("PICAST_TOR_COOKIE_PATH");
        std::env::remove_var("PICAST_DRM_DEVICE");
        std::env::remove_var("PICAST_DLNA_NAME");
        std::env::remove_var("PICAST_DB_PATH");
        std::env::remove_var("PICAST_LOG_LEVEL");

        let mut config = AppConfig::default();
        config.merge_env();
        assert_eq!(config.server.http_addr, "0.0.0.0:8585");
        assert_eq!(config.tor.socks_addr, "127.0.0.1:9050");
    }

    #[test]
    fn parse_partial_toml_sections() {
        // Only tor section — everything else should default.
        let toml = r#"
[tor]
socks_addr = "127.0.0.1:19051"
control_port = 19052
cookie_path = "/tmp/picast/control_auth_cookie"
"#;
        let config: AppConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.tor.socks_addr, "127.0.0.1:19051");
        assert_eq!(config.tor.control_port, 19052);
        assert_eq!(config.tor.cookie_path, "/tmp/picast/control_auth_cookie");
        assert_eq!(config.server.http_addr, "0.0.0.0:8585");
        assert_eq!(config.display.drm_device, "");
    }

    #[test]
    fn load_from_file_missing_returns_defaults() {
        // No config file exists — should return defaults without error.
        let config = AppConfig::load_from_file().unwrap();
        assert_eq!(config.server.http_addr, "0.0.0.0:8585");
    }

    #[test]
    fn load_from_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("picast.toml");
        let contents = r#"
[server]
http_addr = "0.0.0.0:7777"
[tor]
socks_addr = "127.0.0.1:19052"
"#;
        std::fs::write(&path, contents).unwrap();

        let loaded = std::fs::read_to_string(&path).unwrap();
        let config: AppConfig = toml::from_str(&loaded).unwrap();
        assert_eq!(config.server.http_addr, "0.0.0.0:7777");
        assert_eq!(config.tor.socks_addr, "127.0.0.1:19052");
    }
}
