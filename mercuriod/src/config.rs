//! Configuration file handling for mercuriod.

use std::{path::Path, time::Duration};

use mercurio_core::qos::QoS;
use mercurio_server::{
    queue::QueueLimits,
    server::{Limits, Transport},
    tls::{TlsConfig, TlsVersion},
};

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

    #[serde(default)]
    pub storage: StorageConfig,

    /// Listeners served besides the one in `[server]`
    #[serde(default)]
    pub listeners: Vec<ListenerConfig>,

    /// Connections to other brokers that topics are forwarded through
    #[serde(default)]
    pub bridges: Vec<BridgeConfig>,
}

/// A connection to another broker, as an MQTT 5 client.
#[derive(Debug, Deserialize)]
pub struct BridgeConfig {
    /// Names the bridge in logs and in its default client id
    pub name: String,

    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    /// Client id on the remote broker; `mercurio-bridge-{name}` by default
    pub client_id: Option<String>,

    pub username: Option<String>,

    pub password: Option<String>,

    #[serde(default)]
    pub tls: BridgeTlsConfig,

    pub topics: Vec<BridgeTopic>,
}

#[derive(Debug, Default, Deserialize)]
pub struct BridgeTlsConfig {
    #[serde(default)]
    pub enabled: bool,

    /// CA certificate to verify the remote broker with, instead of the
    /// system's root certificates
    pub ca_path: Option<String>,
}

/// Topics forwarded through a bridge, as in mosquitto's `topic` option:
/// `pattern` is matched below `local_prefix` on this broker and below
/// `remote_prefix` on the remote one.
#[derive(Debug, Deserialize)]
pub struct BridgeTopic {
    pub pattern: String,

    #[serde(default)]
    pub direction: Direction,

    #[serde(default)]
    pub qos: u8,

    #[serde(default)]
    pub local_prefix: String,

    #[serde(default)]
    pub remote_prefix: String,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// From this broker to the remote one
    #[default]
    Out,
    /// From the remote broker to this one
    In,
    Both,
}

/// An extra listener, with its own protocol and TLS settings.
#[derive(Debug, Deserialize)]
pub struct ListenerConfig {
    #[serde(default = "default_host")]
    pub host: String,

    pub port: u16,

    #[serde(default)]
    pub protocol: Protocol,

    #[serde(default)]
    pub tls: TlsServerConfig,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    #[default]
    Mqtt,
    Websocket,
}

impl From<Protocol> for Transport {
    fn from(protocol: Protocol) -> Self {
        match protocol {
            Protocol::Mqtt => Transport::Mqtt,
            Protocol::Websocket => Transport::WebSocket,
        }
    }
}

/// Where the broker keeps sessions, retained messages, wills and in-flight
/// messages.
#[derive(Debug, Default, Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum StorageConfig {
    /// Lost on restart
    #[default]
    Memory,
    /// Kept in a SQLite database file
    Sqlite { path: String },
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

    /// Also serve MQTT over WebSocket on this port
    pub websocket_port: Option<u16>,

    /// Seconds between updates of the $SYS statistics; 0 disables them
    #[serde(default = "default_sys_interval")]
    pub sys_interval: u64,

    /// Maximum number of concurrent connections
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,

    /// Largest packet accepted from a client, in bytes
    #[serde(default = "default_max_packet_size")]
    pub max_packet_size: usize,

    /// Messages that may wait for one client before any are dropped
    #[serde(default = "default_max_queued_messages")]
    pub max_queued_messages: usize,

    /// Queued messages beyond which QoS 0 messages are dropped
    #[serde(default = "default_max_queued_qos0_messages")]
    pub max_queued_qos0_messages: usize,

    /// Seconds a new connection has to send CONNECT
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,

    /// Highest topic alias MQTT 5 clients may use; 0 disables aliases
    #[serde(default = "default_max_topic_alias")]
    pub max_topic_alias: u16,

    /// QoS 2 messages a client may have awaiting PUBREL at once
    #[serde(default = "default_receive_maximum")]
    pub receive_maximum: u16,

    /// Longest keep-alive, in seconds, MQTT 5 clients may use
    pub max_keepalive: Option<u16>,

    /// Response topic prefix for MQTT 5 clients that ask for one; `%c` is
    /// replaced by the client id
    pub response_information: Option<String>,

    /// Send every client to another server instead of serving it
    pub redirect: Option<RedirectConfig>,

    /// Replace repeated topics with aliases for MQTT 5 clients that accept
    /// them
    #[serde(default)]
    pub send_topic_aliases: bool,

    /// Highest QoS clients may publish and subscribe with
    #[serde(default = "default_max_qos")]
    pub max_qos: u8,

    /// Whether clients may publish retained messages
    #[serde(default = "default_retain_available")]
    pub retain_available: bool,

    /// Accept clients that leave their client id for the broker to assign
    #[serde(default = "default_allow_zero_length_clientid")]
    pub allow_zero_length_clientid: bool,

    /// Starts the client ids the broker assigns
    #[serde(default)]
    pub auto_id_prefix: String,

    /// Only accept client ids starting with one of these, when set
    #[serde(default)]
    pub clientid_prefixes: Vec<String>,

    /// Seconds the persistent session of an MQTT 3.x client outlives its
    /// connection; forever when unset
    pub persistent_client_expiration: Option<u64>,

    /// TLS configuration
    #[serde(default)]
    pub tls: TlsServerConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            websocket_port: None,
            sys_interval: default_sys_interval(),
            max_connections: default_max_connections(),
            max_packet_size: default_max_packet_size(),
            max_queued_messages: default_max_queued_messages(),
            max_queued_qos0_messages: default_max_queued_qos0_messages(),
            connect_timeout: default_connect_timeout(),
            max_topic_alias: default_max_topic_alias(),
            receive_maximum: default_receive_maximum(),
            max_keepalive: None,
            response_information: None,
            redirect: None,
            send_topic_aliases: false,
            max_qos: default_max_qos(),
            retain_available: default_retain_available(),
            allow_zero_length_clientid: default_allow_zero_length_clientid(),
            auto_id_prefix: String::new(),
            clientid_prefixes: Vec::new(),
            persistent_client_expiration: None,
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

    /// Revocation lists to check client certificates against
    pub crl_path: Option<String>,

    /// Oldest TLS version accepted: "1.2" or "1.3"
    #[serde(default)]
    pub min_version: MinTlsVersion,

    /// Use the client certificate's Common Name as the user name
    #[serde(default)]
    pub identity_as_username: bool,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
pub enum MinTlsVersion {
    #[default]
    #[serde(rename = "1.2")]
    Tls12,
    #[serde(rename = "1.3")]
    Tls13,
}

impl TlsServerConfig {
    /// The TLS settings to serve with, or `None` when TLS is disabled.
    pub fn build(&self) -> Result<Option<TlsConfig>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let cert_path = self
            .cert_path
            .clone()
            .ok_or("TLS enabled but cert_path not specified in configuration")?;
        let key_path = self
            .key_path
            .clone()
            .ok_or("TLS enabled but key_path not specified in configuration")?;
        let mut tls = TlsConfig::new(cert_path, key_path);
        if let Some(ca_path) = &self.ca_path {
            tls = tls.with_client_auth(ca_path.clone());
        }
        tls.crl_path = self.crl_path.clone();
        tls.min_version = match self.min_version {
            MinTlsVersion::Tls12 => TlsVersion::Tls12,
            MinTlsVersion::Tls13 => TlsVersion::Tls13,
        };
        tls.identity_as_username = self.identity_as_username;
        Ok(Some(tls))
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct RedirectConfig {
    /// The server clients should use, such as `broker2.example.com:1883`
    pub server_reference: String,

    /// Whether the move is permanent rather than temporary
    #[serde(default)]
    pub permanent: bool,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    1883
}

fn default_max_qos() -> u8 {
    2
}

fn default_retain_available() -> bool {
    true
}

fn default_allow_zero_length_clientid() -> bool {
    true
}

fn default_sys_interval() -> u64 {
    10
}

fn default_max_connections() -> usize {
    10000
}

fn default_max_packet_size() -> usize {
    Limits::default().max_packet_size
}

fn default_max_queued_messages() -> usize {
    Limits::default().queue.max_messages
}

fn default_max_queued_qos0_messages() -> usize {
    Limits::default().queue.max_qos0
}

fn default_connect_timeout() -> u64 {
    Limits::default().connect_timeout.as_secs()
}

fn default_max_topic_alias() -> u16 {
    Limits::default().topic_alias_maximum
}

fn default_receive_maximum() -> u16 {
    Limits::default().receive_maximum
}

impl ServerConfig {
    pub fn limits(&self) -> Result<Limits, String> {
        let maximum_qos = QoS::from(self.max_qos);
        if maximum_qos == QoS::Invalid {
            return Err(format!("max_qos must be 0, 1 or 2, not {}", self.max_qos));
        }
        Ok(Limits {
            connect_timeout: Duration::from_secs(self.connect_timeout),
            max_packet_size: self.max_packet_size,
            queue: QueueLimits {
                max_qos0: self.max_queued_qos0_messages,
                max_messages: self.max_queued_messages,
            },
            max_connections: Some(self.max_connections),
            topic_alias_maximum: self.max_topic_alias,
            receive_maximum: self.receive_maximum,
            max_keepalive: self.max_keepalive,
            send_topic_aliases: self.send_topic_aliases,
            maximum_qos,
            retain_available: self.retain_available,
            persistent_client_expiration: self
                .persistent_client_expiration
                .map(Duration::from_secs),
        })
    }
}

/// Logging configuration.
#[derive(Debug, Deserialize)]
pub struct LoggingConfig {
    /// Log level (trace, debug, info, warn, error)
    #[serde(default = "default_log_level")]
    pub level: String,

    /// Append logs to this file instead of writing them to standard output
    pub file: Option<String>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
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

    /// Path to password file (format: username:password per line)
    pub password_file: Option<String>,

    /// Path to a mosquitto-style ACL file restricting which topics each
    /// client may publish and subscribe to
    pub acl_file: Option<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_anonymous: default_allow_anonymous(),
            password_file: None,
            acl_file: None,
        }
    }
}

fn default_allow_anonymous() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{Config, Protocol};

    #[test]
    fn test_parse_listeners() {
        let config: Config = toml::from_str(
            r#"
            [[listeners]]
            port = 8883
            [listeners.tls]
            enabled = true
            cert_path = "cert.pem"
            key_path = "key.pem"

            [[listeners]]
            host = "0.0.0.0"
            port = 8080
            protocol = "websocket"
            "#,
        )
        .unwrap();

        let [tls, websocket] = &config.listeners[..] else {
            panic!("expected two listeners");
        };
        assert_eq!((tls.host.as_str(), tls.port), ("127.0.0.1", 8883));
        assert!(matches!(tls.protocol, Protocol::Mqtt));
        assert!(tls.tls.build().unwrap().is_some());
        assert_eq!((websocket.host.as_str(), websocket.port), ("0.0.0.0", 8080));
        assert!(matches!(websocket.protocol, Protocol::Websocket));
        assert!(websocket.tls.build().unwrap().is_none());
    }
}
