//! Configuration file handling for mercuriod.

use std::path::Path;

use serde::Deserialize;

/// Main configuration structure.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub logging: LoggingConfig,

    #[serde(default)]
    pub auth: AuthConfig,
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}

/// Server configuration.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ServerConfig {
    /// Host address to bind to
    #[serde(default = "default_host")]
    pub host: String,

    /// Port to listen on
    #[serde(default = "default_port")]
    pub port: u16,

    /// Maximum number of concurrent connections
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// TLS configuration
    #[serde(default)]
    pub tls: TlsServerConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            max_connections: default_max_connections(),
            tls: TlsServerConfig::default(),
        }
    }
}

/// TLS configuration for the server.
#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct TlsServerConfig {
    /// Enable TLS
    #[serde(default)]
    pub enabled: bool,

    /// Path to certificate file (PEM format)
    pub cert_path: Option<String>,

    /// Path to private key file (PEM format)
    pub key_path: Option<String>,

    /// Optional path to CA certificate for client authentication (mTLS)
    pub ca_path: Option<String>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    1883
}

fn default_max_connections() -> usize {
    10000
}

/// Logging configuration.
#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error)
    #[serde(default = "default_log_level")]
    pub level: String,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
        }
    }
}

fn default_log_level() -> String {
    "info".to_string()
}

/// Authentication configuration.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct AuthConfig {
    /// Enable authentication
    #[serde(default)]
    pub enabled: bool,

    /// Allow anonymous connections
    #[serde(default = "default_allow_anonymous")]
    pub allow_anonymous: bool,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_anonymous: default_allow_anonymous(),
        }
    }
}

fn default_allow_anonymous() -> bool {
    true
}
