//! Configuration file handling for mercuriod.

use std::{
    borrow::Cow,
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};

use mercurio_core::qos::QoS;
use mercurio_server::{
    server::{Limits, QueueLimits, Transport},
    tls::{CertificateName, TlsConfig, TlsVersion},
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

    /// WebAssembly plugins, run in this order after the built-in
    /// authentication and access control
    #[serde(default)]
    pub plugins: Vec<PluginConfig>,

    /// Directory of extra `*.toml` files merged into this configuration
    pub include_dir: Option<String>,
}

/// A connection to another broker, as an MQTT 5 client.
#[derive(Debug, Deserialize)]
pub struct BridgeConfig {
    /// Names the bridge in logs and in its default client id
    pub name: String,

    pub host: Option<String>,

    #[serde(default = "default_port")]
    pub port: u16,

    /// Remote brokers as `host:port`, tried in turn when one is unreachable;
    /// replaces `host` and `port`
    #[serde(default)]
    pub addresses: Vec<String>,

    /// MQTT version spoken to the remote broker
    #[serde(default)]
    pub protocol_version: BridgeProtocol,

    /// Publish the bridge's state, retained, to
    /// `$SYS/broker/connection/<client id>/state` on this broker
    #[serde(default = "default_true")]
    pub notifications: bool,

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

    /// Client certificate and key (PEM) to authenticate with
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BridgeProtocol {
    #[default]
    Mqttv5,
    Mqttv311,
}

fn default_true() -> bool {
    true
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

    /// TCP port to listen on; either this or `path`
    pub port: Option<u16>,

    /// Unix domain socket to listen on; either this or `port`
    pub path: Option<String>,

    #[serde(default)]
    pub protocol: Protocol,

    #[serde(default)]
    pub tls: TlsServerConfig,

    /// Connections start with a PROXY protocol header from a load balancer
    #[serde(default)]
    pub proxy_protocol: bool,

    /// Addresses or CIDR blocks of the proxies allowed to send the header;
    /// any when empty
    #[serde(default)]
    pub proxy_from: Vec<String>,

    /// Prefixed to every topic this listener's clients use
    pub mount_point: Option<String>,

    /// Concurrent connections allowed on this listener
    pub max_connections: Option<usize>,

    /// With `protocol = "websocket"`, serve the files of this directory to
    /// plain HTTP requests
    pub http_dir: Option<String>,

    /// Only accept connections arriving on this network interface (Linux)
    pub bind_interface: Option<String>,

    /// Authentication and access control for this listener's clients,
    /// replacing `[auth]`
    pub auth: Option<AuthConfig>,

    /// Names of the `[[plugins]]` this listener's clients go through; all
    /// of them when unset
    pub plugins: Option<Vec<String>>,
}

/// A WebAssembly plugin, run after the built-in authentication and access
/// control, in the order the tables appear. A reload on SIGHUP rereads its
/// file; its other settings only change on restart.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
// Without WebAssembly support only the table's presence is checked
#[cfg_attr(not(feature = "wasm"), allow(dead_code))]
pub struct PluginConfig {
    pub path: PathBuf,

    /// Defaults to the file name without `.wasm`
    pub name: Option<String>,

    /// Passed to the plugin's `init`
    #[serde(default)]
    pub config: BTreeMap<String, String>,

    /// Hosts it may send HTTP requests to, `host` (HTTPS) or `host:port`
    #[serde(default)]
    pub allow_http: Vec<String>,

    /// Addresses it may open sockets to, `address:port`
    #[serde(default)]
    pub allow_sockets: Vec<String>,

    /// Copies run at once; defaults to the number of CPUs
    pub instances: Option<usize>,

    /// Longest one call may run; defaults to `plugin_timeout`'s 5 seconds
    pub timeout_ms: Option<u64>,

    /// Memory each copy may use
    #[serde(default = "default_plugin_memory_mb")]
    pub memory_mb: usize,
}

impl PluginConfig {
    pub fn name(&self) -> Cow<'_, str> {
        match &self.name {
            Some(name) => name.into(),
            None => self.path.file_stem().unwrap_or_default().to_string_lossy(),
        }
    }
}

fn default_plugin_memory_mb() -> usize {
    64
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
    /// Kept in a PostgreSQL database, such as
    /// `postgres://user:password@localhost/mercurio`, whose certificate is
    /// verified against `ca_path` or the Mozilla root certificates
    Postgres {
        url: String,
        ca_path: Option<PathBuf>,
        /// When another broker holds the database, wait and take over once
        /// it stops, instead of exiting
        #[serde(default)]
        standby: bool,
    },
}

impl Config {
    /// Load configuration from a TOML file, and from the `*.toml` files of
    /// its `include_dir`, in name order. Their tables are merged into the
    /// file's, their arrays, such as `[[listeners]]`, appended, and their
    /// other values replace the file's.
    pub fn from_file(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut table = read_table(path)?;
        if let Some(dir) = table.get("include_dir").and_then(|d| d.as_str()) {
            let mut files: Vec<_> = std::fs::read_dir(dir)
                .map_err(|e| format!("Failed to read include_dir {dir}: {e}"))?
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            files.sort();
            for file in files {
                merge(&mut table, read_table(&file)?);
            }
        }
        let config = Config::deserialize(table)?;
        config.check()?;
        Ok(config)
    }
}

impl Config {
    /// Refuses settings that parse but would be unsafe.
    fn check(&self) -> Result<(), String> {
        for listener in &self.listeners {
            // `tenantA` would also let clients reach `tenantAB/...`
            if let Some(mount_point) = &listener.mount_point {
                if !mount_point.ends_with('/') {
                    return Err(format!(
                        "mount_point `{mount_point}` must end with `/`, or its clients reach the topics of every mount point it is a prefix of"
                    ));
                }
            }
        }
        let mut names = HashSet::new();
        for plugin in &self.plugins {
            let name = plugin.name();
            if !names.insert(name.clone()) {
                return Err(format!(
                    "Two [[plugins]] are named `{name}`; give one a different `name`"
                ));
            }
        }
        for name in self
            .listeners
            .iter()
            .flat_map(|l| l.plugins.iter().flatten())
        {
            if !names.contains(name.as_str()) {
                return Err(format!("No [[plugins]] entry is named `{name}`"));
            }
        }
        Ok(())
    }
}

fn read_table(path: &Path) -> Result<toml::Table, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)?;
    toml::from_str(&content).map_err(|e| format!("{}: {e}", path.display()).into())
}

fn merge(into: &mut toml::Table, from: toml::Table) {
    for (key, value) in from {
        match (into.get_mut(&key), value) {
            (Some(toml::Value::Table(into)), toml::Value::Table(from)) => merge(into, from),
            (Some(toml::Value::Array(into)), toml::Value::Array(from)) => into.extend(from),
            (_, value) => {
                into.insert(key, value);
            }
        }
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

    /// Only accept connections arriving on this network interface (Linux)
    pub bind_interface: Option<String>,

    /// Also serve MQTT over WebSocket on this port
    pub websocket_port: Option<u16>,

    /// Serve the files of this directory to plain HTTP requests on
    /// `websocket_port`
    pub http_dir: Option<String>,

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

    /// Payload bytes that may wait for one client before further messages
    /// are dropped
    #[serde(default = "default_max_queued_bytes")]
    pub max_queued_bytes: usize,

    /// Send messages at the QoS of the subscription even when published
    /// with a lower one
    #[serde(default)]
    pub upgrade_outgoing_qos: bool,

    /// Send retained messages only while their publisher may still publish
    /// to their topic
    #[serde(default)]
    pub check_retain_source: bool,

    /// Subscriptions one session may hold
    #[serde(default = "default_max_subscriptions")]
    pub max_subscriptions: usize,

    /// Seconds a new connection has, in all, to complete its handshakes and
    /// send CONNECT
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

    /// Most seconds any session may outlive its connection; MQTT 5 clients
    /// asking for longer are told to use this. Unbounded when unset
    pub max_session_expiry: Option<u64>,

    /// Topics that may hold a retained message at once, the broker's own
    /// `$SYS` topics included; unbounded when unset
    pub max_retained_messages: Option<usize>,

    /// Connections one client address may hold at once, the address a PROXY
    /// header gives counted; unbounded when unset
    pub max_connections_per_ip: Option<usize>,

    /// TLS configuration
    #[serde(default)]
    pub tls: TlsServerConfig,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            bind_interface: None,
            websocket_port: None,
            http_dir: None,
            sys_interval: default_sys_interval(),
            max_connections: default_max_connections(),
            max_packet_size: default_max_packet_size(),
            max_queued_messages: default_max_queued_messages(),
            max_queued_qos0_messages: default_max_queued_qos0_messages(),
            max_queued_bytes: default_max_queued_bytes(),
            upgrade_outgoing_qos: false,
            check_retain_source: false,
            max_subscriptions: default_max_subscriptions(),
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
            max_session_expiry: None,
            max_retained_messages: None,
            max_connections_per_ip: None,
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

    /// Use the client certificate's whole subject as the user name
    #[serde(default)]
    pub subject_as_username: bool,
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
        tls.username_from_certificate = match (self.identity_as_username, self.subject_as_username)
        {
            (false, false) => None,
            (true, false) => Some(CertificateName::CommonName),
            (false, true) => Some(CertificateName::Subject),
            (true, true) => {
                return Err("Set identity_as_username or subject_as_username, not both".to_string())
            }
        };
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

fn default_max_queued_bytes() -> usize {
    QueueLimits::default().max_bytes.unwrap_or(usize::MAX)
}

fn default_max_subscriptions() -> usize {
    mercurio_server::server::DEFAULT_MAX_SUBSCRIPTIONS
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
        let mut limits = Limits::default();
        limits.connect_timeout = Duration::from_secs(self.connect_timeout);
        limits.max_packet_size = self.max_packet_size;
        limits.queue.max_qos0 = self.max_queued_qos0_messages;
        limits.queue.max_messages = self.max_queued_messages;
        limits.queue.max_bytes = Some(self.max_queued_bytes);
        limits.queue.upgrade_qos = self.upgrade_outgoing_qos;
        limits.max_connections = Some(self.max_connections);
        limits.topic_alias_maximum = self.max_topic_alias;
        limits.receive_maximum = self.receive_maximum;
        limits.max_keepalive = self.max_keepalive;
        limits.send_topic_aliases = self.send_topic_aliases;
        limits.maximum_qos = maximum_qos;
        limits.retain_available = self.retain_available;
        limits.persistent_client_expiration =
            self.persistent_client_expiration.map(Duration::from_secs);
        limits.check_retain_source = self.check_retain_source;
        limits.max_subscriptions = Some(self.max_subscriptions);
        limits.max_session_expiry = self.max_session_expiry.map(Duration::from_secs);
        limits.max_retained_messages = self.max_retained_messages;
        limits.max_connections_per_ip = self.max_connections_per_ip;
        Ok(limits)
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

    /// Send logs to the local syslog daemon instead
    #[serde(default)]
    pub syslog: bool,

    /// Log clients connecting and disconnecting
    #[serde(default = "default_true")]
    pub connection_messages: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
            syslog: false,
            connection_messages: true,
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

    /// Path to the dynamic security state, managed over
    /// `$CONTROL/dynamic-security/v1`; replaces the password and ACL files
    pub dynamic_security: Option<String>,

    /// Checks delegated to HTTP endpoints
    pub http: Option<HttpAuthConfig>,
}

#[derive(Debug, Deserialize)]
pub struct HttpAuthConfig {
    /// Checks user names and passwords, in place of `password_file`
    pub user_url: Option<String>,

    /// Checks topic access, in place of `acl_file`
    pub acl_url: Option<String>,

    #[serde(default = "default_http_timeout")]
    pub timeout_seconds: u64,

    /// How long ACL answers are reused
    #[serde(default = "default_http_cache")]
    pub cache_seconds: u64,
}

fn default_http_timeout() -> u64 {
    5
}

fn default_http_cache() -> u64 {
    30
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            allow_anonymous: default_allow_anonymous(),
            password_file: None,
            acl_file: None,
            dynamic_security: None,
            http: None,
        }
    }
}

/// Enabling authentication refuses clients without credentials unless
/// they are explicitly allowed, as in mosquitto 2.
fn default_allow_anonymous() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::{Config, Protocol};

    #[test]
    fn test_tls_acceptor_builds_with_every_dependency_linked() {
        // Two rustls crypto providers enabled by dependencies once made
        // building any TLS acceptor panic
        let fixtures = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../mercurio-server/tests/fixtures"
        );
        let config: Config = toml::from_str(&format!(
            r#"
            [server.tls]
            enabled = true
            cert_path = "{fixtures}/cert.pem"
            key_path = "{fixtures}/key.pem"
            "#
        ))
        .unwrap();
        let tls = config.server.tls.build().unwrap().unwrap();
        assert!(tls.build_acceptor().is_ok());
    }

    #[test]
    fn test_include_dir_merges_files_in_order() {
        let dir = std::env::temp_dir().join(format!("mercuriod-include-{}", std::process::id()));
        let conf_d = dir.join("conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        let main = dir.join("main.toml");
        std::fs::write(
            &main,
            format!(
                "include_dir = \"{}\"\n[server]\nport = 1\nmax_qos = 1\n[[listeners]]\nport = 2\n",
                conf_d.display()
            ),
        )
        .unwrap();
        std::fs::write(
            conf_d.join("a.toml"),
            "[server]\nport = 3\n[[listeners]]\nport = 4\n",
        )
        .unwrap();
        std::fs::write(conf_d.join("b.toml"), "[server]\nport = 5\n").unwrap();
        std::fs::write(conf_d.join("ignored.txt"), "not toml").unwrap();

        let config = Config::from_file(&main).unwrap();
        assert_eq!(config.server.port, 5);
        assert_eq!(config.server.max_qos, 1);
        let ports: Vec<_> = config.listeners.iter().map(|l| l.port).collect();
        assert_eq!(ports, [Some(2), Some(4)]);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_mount_points_must_end_a_level() {
        let config = |mount_point: &str| -> Config {
            toml::from_str(&format!(
                "[[listeners]]\nport = 1884\nmount_point = \"{mount_point}\"\n"
            ))
            .unwrap()
        };
        assert!(config("tenantA").check().is_err());
        assert!(config("tenantA/").check().is_ok());
    }

    #[test]
    fn test_parse_plugins() {
        let config: Config = toml::from_str(
            r#"
            [[plugins]]
            path = "/etc/mercurio/plugins/audit.wasm"
            config = { webhook = "https://audit.example.com/mqtt" }
            allow_http = ["audit.example.com"]
            allow_sockets = ["10.0.0.5:5432"]
            instances = 4
            timeout_ms = 500
            memory_mb = 32

            [[plugins]]
            path = "deny.wasm"
            name = "deny"

            [[listeners]]
            port = 1884
            plugins = ["deny"]
            "#,
        )
        .unwrap();
        assert_eq!(config.plugins.len(), 2);
        assert_eq!(config.plugins[0].name(), "audit");
        assert_eq!(config.plugins[0].instances, Some(4));
        assert_eq!(config.plugins[1].name(), "deny");
        assert_eq!(
            config.listeners[0].plugins.as_deref(),
            Some(&["deny".to_string()][..])
        );
        assert!(config.check().is_ok());
    }

    #[test]
    fn test_plugin_config_values_are_strings() {
        let error =
            toml::from_str::<Config>("[[plugins]]\npath = \"a.wasm\"\nconfig = { retries = 3 }\n")
                .unwrap_err()
                .to_string();
        assert!(error.contains("retries"), "{error}");
    }

    #[test]
    fn test_plugin_names_must_be_unique_and_known() {
        let config = |toml: &str| -> Config { toml::from_str(toml).unwrap() };
        let duplicate =
            config("[[plugins]]\npath = \"a/deny.wasm\"\n[[plugins]]\npath = \"b/deny.wasm\"\n");
        assert!(duplicate.check().unwrap_err().contains("`deny`"));
        let unknown = config("[[plugins]]\npath = \"deny.wasm\"\n[[listeners]]\nport = 1884\nplugins = [\"audit\"]\n");
        assert!(unknown.check().unwrap_err().contains("`audit`"));
    }

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
        assert_eq!((tls.host.as_str(), tls.port), ("127.0.0.1", Some(8883)));
        assert!(matches!(tls.protocol, Protocol::Mqtt));
        assert!(tls.tls.build().unwrap().is_some());
        assert_eq!(
            (websocket.host.as_str(), websocket.port),
            ("0.0.0.0", Some(8080))
        );
        assert!(matches!(websocket.protocol, Protocol::Websocket));
        assert!(websocket.tls.build().unwrap().is_none());
    }
}
