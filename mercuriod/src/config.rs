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
use mercurio_storage::postgres::ConnectOptions;

use serde::{Deserialize, Serialize, Serializer};

/// Main configuration structure.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

    /// The admin socket; on with its defaults when absent, on Linux
    pub admin: Option<AdminConfig>,

    /// Directory of extra `*.toml` files merged into this configuration
    pub include_dir: Option<String>,
}

/// A connection to another broker, as an MQTT 5 client.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

    #[serde(serialize_with = "redact_some")]
    pub password: Option<String>,

    #[serde(default)]
    pub tls: BridgeTlsConfig,

    pub topics: Vec<BridgeTopic>,
}

impl BridgeConfig {
    /// Client id on both brokers.
    pub fn client_id(&self) -> String {
        self.client_id
            .clone()
            .unwrap_or_else(|| format!("mercurio-bridge-{}", self.name))
    }
}

/// TLS to the remote broker. A CA or a client certificate turns it on
/// whatever `enabled` says, as `--cafile` does for recado and mercurioctl,
/// so credentials never cross the network in the clear by mistake.
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BridgeTlsConfig {
    /// TLS verified with the system's root certificates, unless the keys
    /// below set otherwise
    #[serde(default)]
    pub enabled: bool,

    /// CA certificate to verify the remote broker with, instead of the
    /// system's root certificates; turns TLS on
    pub ca_path: Option<String>,

    /// Client certificate and key (PEM) to authenticate with, both or
    /// neither; turn TLS on
    pub cert_path: Option<String>,
    pub key_path: Option<String>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
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
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ListenerConfig {
    #[serde(default = "default_host")]
    pub host: String,

    /// TCP port to listen on; either this or `path`
    pub port: Option<u16>,

    /// Unix domain socket to listen on; either this or `port`
    pub path: Option<String>,

    /// With `path`, the socket's mode in octal, such as `"0660"`; 0660 with
    /// `group` and 0600 without by default (Linux)
    pub mode: Option<String>,

    /// With `path`, the group the socket belongs to (Linux)
    pub group: Option<String>,

    #[serde(default)]
    pub protocol: Protocol,

    #[serde(default)]
    pub tls: TlsServerConfig,

    /// Connections start with a PROXY protocol header from a load balancer
    #[serde(default)]
    pub proxy_protocol: bool,

    /// Addresses or CIDR blocks of the proxies allowed to send the header;
    /// required with `proxy_protocol` on TCP, `["0.0.0.0/0", "::/0"]` for
    /// any peer
    #[serde(default)]
    pub proxy_from: Vec<String>,

    /// Prefixed to every topic this listener's clients use
    pub mount_point: Option<String>,

    /// Concurrent connections allowed on this listener
    pub max_connections: Option<usize>,

    /// With `protocol = "websocket"`, serve the files of this directory to
    /// plain HTTP requests
    pub http_dir: Option<String>,

    /// With `protocol = "websocket"`, answer `GET /healthz`
    #[serde(default)]
    pub health: bool,

    /// With `protocol = "websocket"`, answer `GET /metrics` with the
    /// statistics in the Prometheus text format
    #[serde(default)]
    pub metrics: bool,

    /// With `protocol = "websocket"`, the origins of the web pages that
    /// may connect, such as `https://app.example.com`; any when unset
    pub allowed_origins: Option<Vec<String>>,

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
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
// Without WebAssembly support only the table's presence is checked
#[cfg_attr(not(feature = "wasm"), allow(dead_code))]
pub struct PluginConfig {
    pub path: PathBuf,

    /// Defaults to the file name without `.wasm`
    pub name: Option<String>,

    /// Passed to the plugin's `init`
    #[serde(default, serialize_with = "redact")]
    pub config: BTreeMap<String, String>,

    /// Hosts it may send HTTP requests to, `host` (HTTPS) or `host:port`
    #[serde(default)]
    pub allow_http: Vec<String>,

    /// Addresses it may open sockets to, `address:port`
    #[serde(default)]
    pub allow_sockets: Vec<String>,

    /// Copies run at once; defaults to the number of CPUs
    pub instances: Option<usize>,

    /// Longest one call may run; defaults to, and may not exceed, `[server]
    /// plugin_timeout`
    pub timeout_ms: Option<u64>,

    /// Memory each copy may use
    #[serde(default = "default_plugin_memory_mb")]
    pub memory_mb: usize,
}

impl ListenerConfig {
    /// `mode`, as permission bits.
    pub fn socket_mode(&self) -> Result<Option<u32>, String> {
        let Some(mode) = &self.mode else {
            return Ok(None);
        };
        match u32::from_str_radix(mode, 8) {
            Ok(bits) if bits <= 0o777 => Ok(Some(bits)),
            _ => Err(format!(
                "[[listeners]] `mode` = \"{mode}\" is not permission bits in octal, such as \"0660\""
            )),
        }
    }
}

impl PluginConfig {
    pub fn name(&self) -> Cow<'_, str> {
        match &self.name {
            Some(name) => name.into(),
            None => self.path.file_stem().unwrap_or_default().to_string_lossy(),
        }
    }
}

/// What `getConfig` shows in place of a secret.
const REDACTED: &str = "<redacted>";

fn redact<T, S: Serializer>(_: &T, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(REDACTED)
}

fn redact_some<S: Serializer>(value: &Option<String>, serializer: S) -> Result<S::Ok, S::Error> {
    match value {
        Some(_) => serializer.serialize_str(REDACTED),
        None => serializer.serialize_none(),
    }
}

fn redact_url<S: Serializer>(url: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&redacted_url(url))
}

fn redact_http_url<S: Serializer>(url: &str, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&redacted_http_url(url))
}

fn redact_some_url<S: Serializer>(url: &Option<String>, serializer: S) -> Result<S::Ok, S::Error> {
    match url {
        Some(url) => redact_http_url(url, serializer),
        None => serializer.serialize_none(),
    }
}

/// `url` with the password of `scheme://user:password@host` redacted, or
/// redacted whole when it passes a secret as a parameter, as PostgreSQL
/// URLs and connection strings may (`password=`, `sslpassword=`, `token=`).
/// PostgreSQL URLs take their credentials up to the first `@`, so a
/// password with an unescaped `/`, `?` or `#` is covered too. Secrets in the
/// path of a URL are not recognised.
pub fn redacted_url(url: &str) -> Cow<'_, str> {
    const SECRET_PARAMS: [&str; 6] = ["password", "passwd", "pwd", "secret", "token", "apikey"];
    let query = if url.contains("://") {
        url.split_once('?').map_or("", |(_, query)| query)
    } else {
        url
    };
    let lower = query.to_ascii_lowercase().replace(['_', '-'], "");
    if SECRET_PARAMS.iter().any(|p| lower.contains(p)) {
        return REDACTED.into();
    }
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.into();
    };
    let (user_info, host_and_tail) = if matches!(scheme, "postgres" | "postgresql") {
        let Some(split) = rest.split_once('@') else {
            return url.into();
        };
        split
    } else {
        let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
        let Some((user_info, host)) = authority.rsplit_once('@') else {
            return url.into();
        };
        (user_info, &rest[authority.len() - host.len()..])
    };
    let Some((user, _)) = user_info.split_once(':') else {
        return url.into();
    };
    format!("{scheme}://{user}:{REDACTED}@{host_and_tail}").into()
}

/// `url` of an HTTP auth service with any user info and query redacted, as
/// ureq sends the first, as it is, in a basic `Authorization` header and the
/// second may carry a key; the
/// whole URL when an `@` appears elsewhere than in a clean authority, and
/// secrets in the path are not recognised.
pub fn redacted_http_url(url: &str) -> Cow<'_, str> {
    let Some((scheme, rest)) = url.split_once("://") else {
        return REDACTED.into();
    };
    if rest.contains('@') {
        return REDACTED.into();
    }
    match rest.find(['?', '#']) {
        Some(at) => format!("{scheme}://{}?{REDACTED}", &rest[..at]).into(),
        None => url.into(),
    }
}

/// The names of the broker's own plugins, which `$CONTROL/<name>/...`
/// topics and logs already use.
const BUILT_IN_PLUGINS: [&str; 6] = [
    "dynamic-security",
    "password-file",
    "http-auth",
    "acl-file",
    "plain",
    "credentials",
];

fn default_plugin_memory_mb() -> usize {
    64
}

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
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
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "backend", rename_all = "lowercase", deny_unknown_fields)]
pub enum StorageConfig {
    /// Lost on restart. A struct variant, as serde lets a unit variant of an
    /// internally tagged enum ignore unknown keys
    Memory {},
    /// Kept in a SQLite database file
    Sqlite { path: String },
    /// Kept in a PostgreSQL database, such as
    /// `postgres://user:password@localhost/mercurio`, whose certificate is
    /// verified against `ca_path` or the Mozilla root certificates
    Postgres {
        #[serde(serialize_with = "redact_url")]
        url: String,
        ca_path: Option<PathBuf>,
        /// When another broker holds the database, wait and take over once
        /// it stops, instead of exiting
        #[serde(default)]
        standby: bool,
        /// Connections the broker keeps to the database, besides the one
        /// holding it
        #[serde(default = "default_pool_size")]
        pool_size: usize,
        /// Seconds a write waits for a free connection before it is tried
        /// again
        #[serde(default = "default_pool_timeout")]
        pool_timeout: u64,
    },
}

fn default_pool_size() -> usize {
    ConnectOptions::default().pool_size
}

fn default_pool_timeout() -> u64 {
    ConnectOptions::default().pool_timeout.as_secs()
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self::Memory {}
    }
}

impl StorageConfig {
    /// How to connect to PostgreSQL, with `backend = "postgres"`.
    pub fn postgres_options(&self) -> Result<Option<ConnectOptions>, String> {
        let StorageConfig::Postgres {
            ca_path,
            pool_size,
            pool_timeout,
            ..
        } = self
        else {
            return Ok(None);
        };
        if *pool_size == 0 {
            return Err("pool_size must be at least 1".into());
        }
        if *pool_timeout == 0 {
            return Err("pool_timeout must be at least 1 second".into());
        }
        let mut options = ConnectOptions::default();
        options.ca_path = ca_path.clone();
        options.pool_size = *pool_size;
        options.pool_timeout = Duration::from_secs(*pool_timeout);
        Ok(Some(options))
    }
}

impl Config {
    /// Load configuration from a TOML file, and from the `*.toml` files of
    /// its `include_dir`, in name order. Their tables are merged into the
    /// file's, their arrays, such as `[[listeners]]`, appended, and their
    /// other values replace the file's.
    pub fn from_file(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut table = read_table(path)?;
        let mut source = path.display().to_string();
        if let Some(dir) = table.get("include_dir").and_then(|d| d.as_str()) {
            // Merged values no longer tell which file they came from
            source = format!("{source} or a file of {dir}");
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
        let config = Config::deserialize(table)
            .map_err(|e| format!("{source}: {}", e.to_string().trim_end().replace('\n', " ")))?;
        config.check()?;
        Ok(config)
    }
}

impl Config {
    /// Refuses settings that parse but would be unsafe.
    fn check(&self) -> Result<(), String> {
        for listener in &self.listeners {
            listener.socket_mode()?;
            if listener.path.is_none() {
                if let Some(key) = [("mode", &listener.mode), ("group", &listener.group)]
                    .into_iter()
                    .find_map(|(key, value)| value.as_ref().map(|_| key))
                {
                    return Err(format!(
                        "[[listeners]] `{key}` applies to a Unix socket `path`, not to a TCP `port`"
                    ));
                }
            }
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
            // The name is a topic level of `$CONTROL/<name>/...`
            if name.is_empty() || name.contains(['/', '+', '#']) {
                return Err(format!(
                    "[[plugins]] name `{name}` must be a non-empty topic level, without `/`, `+` or `#`"
                ));
            }
            if BUILT_IN_PLUGINS.contains(&&*name) {
                return Err(format!(
                    "[[plugins]] name `{name}` is taken by a built-in plugin; set a different `name`"
                ));
            }
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
        self.check_plugin_timeouts()?;
        self.check_bridges()
    }

    /// Refuses timeouts the broker's own limit on a plugin call would cut
    /// short, so a slow answer is never taken for a denial in silence.
    fn check_plugin_timeouts(&self) -> Result<(), String> {
        let limit = self.server.plugin_timeout();
        if limit.is_zero() {
            return Err("[server] plugin_timeout must be at least 1 second".into());
        }
        if self.server.plugin_tick_interval == 0 {
            return Err("[server] plugin_tick_interval must be at least 1 second".into());
        }
        let http = std::iter::once(("[auth.http]", &self.auth))
            .chain(
                self.listeners
                    .iter()
                    .filter_map(|l| l.auth.as_ref())
                    .map(|auth| ("[listeners.auth.http]", auth)),
            )
            .filter_map(|(table, auth)| Some((table, auth.http.as_ref()?)));
        for (table, http) in http {
            if Duration::from_secs(http.timeout_seconds) > limit {
                return Err(format!(
                    "{table} `timeout_seconds` = {} is longer than [server] `plugin_timeout` = {}, which would cut the request short; lower the first or raise the second",
                    http.timeout_seconds, self.server.plugin_timeout
                ));
            }
        }
        for plugin in &self.plugins {
            if let Some(ms) = plugin
                .timeout_ms
                .filter(|ms| Duration::from_millis(*ms) > limit)
            {
                return Err(format!(
                    "[[plugins]] `{}`: `timeout_ms` = {ms} is longer than [server] `plugin_timeout` = {} seconds, which would cut its calls short; lower the first or raise the second",
                    plugin.name(),
                    self.server.plugin_timeout
                ));
            }
        }
        Ok(())
    }

    fn check_bridges(&self) -> Result<(), String> {
        let mut names = HashSet::new();
        let mut client_ids = HashSet::new();
        for bridge in &self.bridges {
            let name = &bridge.name;
            if !names.insert(name) {
                return Err(format!(
                    "Two [[bridges]] are named `{name}`; give one a different `name`"
                ));
            }
            // The same client id makes each bridge take over the other's
            // session on both brokers
            let client_id = bridge.client_id();
            if !client_ids.insert(client_id.clone()) {
                return Err(format!(
                    "Two [[bridges]] use the client id `{client_id}`; set a different `client_id` or `name`"
                ));
            }
            // The id is a topic level of `$SYS/broker/connection/<id>/state`
            if bridge.notifications
                && (client_id.is_empty() || client_id.contains(['/', '+', '#', '\0']))
            {
                return Err(format!(
                    "Bridge {name}: client id `{client_id}` must be a non-empty topic level, without `/`, `+` or `#`, for its state topic; set another `client_id` or `notifications = false`"
                ));
            }
            if bridge.tls.cert_path.is_some() != bridge.tls.key_path.is_some() {
                return Err(format!(
                    "Bridge {name}: tls.cert_path and tls.key_path go together"
                ));
            }
        }
        Ok(())
    }
}

fn read_table(path: &Path) -> Result<toml::Table, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
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
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

    /// Largest CONNECT, and AUTH during enhanced authentication, accepted
    /// from a client, in bytes; they come before it is authenticated
    #[serde(default = "default_max_connect_packet_size")]
    pub max_connect_packet_size: usize,

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

    /// Longest topic filter, in bytes, a SUBSCRIBE may name
    #[serde(default = "default_max_topic_filter_length")]
    pub max_topic_filter_length: usize,

    /// Seconds a new connection has, in all, to complete its handshakes and
    /// send CONNECT
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,

    /// Seconds writing one packet to a client may take before its connection
    /// is dropped as stalled; also how long a connection taking over a client
    /// id waits for the previous one
    #[serde(default = "default_write_timeout")]
    pub write_timeout: u64,

    /// Highest topic alias MQTT 5 clients may use; 0 disables aliases
    #[serde(default = "default_max_topic_alias")]
    pub max_topic_alias: u16,

    /// QoS 2 messages a client may have awaiting PUBREL at once
    #[serde(default = "default_receive_maximum")]
    pub receive_maximum: u16,

    /// QoS 1 and 2 messages sent to one client and not yet acknowledged, at
    /// most, whatever Receive Maximum the client announces
    #[serde(default = "default_max_inflight")]
    pub max_inflight: u16,

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
    /// `$SYS` topics not counted; unbounded when unset
    pub max_retained_messages: Option<usize>,

    /// Connections one client address may hold at once, the address a PROXY
    /// header gives counted and IPv6 clients counted by /64; unbounded when
    /// unset
    pub max_connections_per_ip: Option<usize>,

    /// Seconds one call of a plugin hook may take, built-in and WebAssembly
    /// plugins alike; one that takes longer denies, or drops the message
    #[serde(default = "default_plugin_timeout")]
    pub plugin_timeout: u64,

    /// Seconds between calls of plugins' `on_tick`
    #[serde(default = "default_plugin_tick_interval")]
    pub plugin_tick_interval: u64,

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
            max_connect_packet_size: default_max_connect_packet_size(),
            max_queued_messages: default_max_queued_messages(),
            max_queued_qos0_messages: default_max_queued_qos0_messages(),
            max_queued_bytes: default_max_queued_bytes(),
            upgrade_outgoing_qos: false,
            check_retain_source: false,
            max_subscriptions: default_max_subscriptions(),
            max_topic_filter_length: default_max_topic_filter_length(),
            connect_timeout: default_connect_timeout(),
            write_timeout: default_write_timeout(),
            max_topic_alias: default_max_topic_alias(),
            receive_maximum: default_receive_maximum(),
            max_inflight: default_max_inflight(),
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
            plugin_timeout: default_plugin_timeout(),
            plugin_tick_interval: default_plugin_tick_interval(),
            tls: TlsServerConfig::default(),
        }
    }
}

/// TLS configuration for the server.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

    /// With `ca_path`, refuse clients without a certificate; when false,
    /// they log in as on a listener without `ca_path`
    #[serde(default = "default_true")]
    pub require_certificate: bool,

    /// Revocation lists to check client certificates against
    pub crl_path: Option<String>,

    /// Check only the client's own certificate against `crl_path`, not
    /// the intermediate CAs that issued it
    #[serde(default)]
    pub crl_end_entity_only: bool,

    /// Accept certificates whose issuer has no CRL in `crl_path`
    #[serde(default)]
    pub crl_allow_unknown: bool,

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

#[derive(Debug, Default, Clone, Copy, Deserialize, Serialize)]
pub enum MinTlsVersion {
    #[default]
    #[serde(rename = "1.2")]
    Tls12,
    #[serde(rename = "1.3")]
    Tls13,
}

impl Default for TlsServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            cert_path: None,
            key_path: None,
            ca_path: None,
            require_certificate: true,
            crl_path: None,
            crl_end_entity_only: false,
            crl_allow_unknown: false,
            min_version: MinTlsVersion::default(),
            identity_as_username: false,
            subject_as_username: false,
        }
    }
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
        tls.require_certificate = self.require_certificate;
        tls.crl_path = self.crl_path.clone();
        tls.crl_end_entity_only = self.crl_end_entity_only;
        tls.crl_allow_unknown = self.crl_allow_unknown;
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

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

fn default_max_connect_packet_size() -> usize {
    Limits::default().max_connect_packet_size
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

fn default_max_topic_filter_length() -> usize {
    mercurio_server::server::DEFAULT_MAX_TOPIC_FILTER_LENGTH
}

fn default_connect_timeout() -> u64 {
    Limits::default().connect_timeout.as_secs()
}

fn default_write_timeout() -> u64 {
    Limits::default().write_timeout.as_secs()
}

fn default_max_topic_alias() -> u16 {
    Limits::default().topic_alias_maximum
}

fn default_receive_maximum() -> u16 {
    Limits::default().receive_maximum
}

fn default_max_inflight() -> u16 {
    Limits::default().max_inflight
}

fn default_plugin_timeout() -> u64 {
    mercurio_server::server::ServerConfig::default()
        .plugin_timeout
        .as_secs()
}

fn default_plugin_tick_interval() -> u64 {
    mercurio_server::server::ServerConfig::default()
        .plugin_tick_interval
        .as_secs()
}

impl ServerConfig {
    pub fn plugin_timeout(&self) -> Duration {
        Duration::from_secs(self.plugin_timeout)
    }

    pub fn plugin_tick_interval(&self) -> Duration {
        Duration::from_secs(self.plugin_tick_interval)
    }

    pub fn limits(&self) -> Result<Limits, String> {
        let maximum_qos = QoS::from(self.max_qos);
        if maximum_qos == QoS::Invalid {
            return Err(format!("max_qos must be 0, 1 or 2, not {}", self.max_qos));
        }
        if self.max_inflight == 0 {
            return Err("max_inflight must be at least 1".to_string());
        }
        if self.max_connect_packet_size == 0 {
            return Err("max_connect_packet_size must be at least 1".into());
        }
        if self.write_timeout == 0 {
            return Err("write_timeout must be at least 1 second".into());
        }
        let mut limits = Limits::default();
        limits.connect_timeout = Duration::from_secs(self.connect_timeout);
        limits.write_timeout = Duration::from_secs(self.write_timeout);
        limits.max_packet_size = self.max_packet_size;
        limits.max_connect_packet_size = self.max_connect_packet_size;
        limits.queue.max_qos0 = self.max_queued_qos0_messages;
        limits.queue.max_messages = self.max_queued_messages;
        limits.queue.max_bytes = Some(self.max_queued_bytes);
        limits.queue.upgrade_qos = self.upgrade_outgoing_qos;
        limits.max_connections = Some(self.max_connections);
        limits.topic_alias_maximum = self.max_topic_alias;
        limits.receive_maximum = self.receive_maximum;
        limits.max_inflight = self.max_inflight;
        limits.max_keepalive = self.max_keepalive;
        limits.send_topic_aliases = self.send_topic_aliases;
        limits.maximum_qos = maximum_qos;
        limits.retain_available = self.retain_available;
        limits.persistent_client_expiration =
            self.persistent_client_expiration.map(Duration::from_secs);
        limits.check_retain_source = self.check_retain_source;
        limits.max_subscriptions = Some(self.max_subscriptions);
        limits.max_topic_filter_length = self.max_topic_filter_length;
        limits.max_session_expiry = self.max_session_expiry.map(Duration::from_secs);
        limits.max_retained_messages = self.max_retained_messages;
        limits.max_connections_per_ip = self.max_connections_per_ip;
        Ok(limits)
    }
}

/// Logging configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

/// The Unix socket administrators on this host reach the broker through,
/// with no credentials or access control.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct AdminConfig {
    /// Path of the socket; empty turns it off
    pub socket: String,

    /// Group whose members may use it, besides root and the broker's own
    /// user; nobody else when empty
    pub group: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            socket: "/run/mercurio/admin.sock".to_string(),
            group: "mercurio".to_string(),
        }
    }
}

/// Authentication configuration.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HttpAuthConfig {
    /// Checks user names and passwords, in place of `password_file`
    #[serde(serialize_with = "redact_some_url")]
    pub user_url: Option<String>,

    /// Checks topic access, in place of `acl_file`
    #[serde(serialize_with = "redact_some_url")]
    pub acl_url: Option<String>,

    /// Names superusers, whom no `acl_url` check restricts and who may
    /// administer the broker
    #[serde(serialize_with = "redact_some_url")]
    pub superuser_url: Option<String>,

    #[serde(default = "default_http_timeout")]
    pub timeout_seconds: u64,

    /// How long ACL answers are reused
    #[serde(default = "default_http_cache")]
    pub cache_seconds: u64,

    /// PEM certificates of the CAs that HTTPS endpoints' certificates must
    /// chain to, in place of the bundled roots
    pub ca_path: Option<String>,

    /// PEM certificate chain presented to HTTPS endpoints that ask for one
    pub cert_path: Option<String>,

    /// PEM private key of `cert_path`
    pub key_path: Option<String>,
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
    use std::time::Duration;

    use serde::Deserialize;

    use super::{redacted_http_url, redacted_url, AdminConfig, Config, Protocol, StorageConfig};

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
        assert!(mercurio_server::tls::Tls::new(tls).is_ok());
    }

    #[test]
    fn test_client_certificates_are_required_unless_turned_off() {
        let tls = |extra: &str| {
            let config: Config = toml::from_str(&format!(
                "[server.tls]\nenabled = true\ncert_path = \"c.pem\"\nkey_path = \"k.pem\"\n\
                 ca_path = \"ca.pem\"\n{extra}"
            ))
            .unwrap();
            config.server.tls.build().unwrap().unwrap()
        };
        assert!(tls("").require_certificate);
        assert!(!tls("require_certificate = false").require_certificate);
    }

    #[test]
    fn test_crls_cover_the_whole_chain_unless_relaxed() {
        let tls = |extra: &str| {
            let config: Config = toml::from_str(&format!(
                "[server.tls]\nenabled = true\ncert_path = \"c.pem\"\nkey_path = \"k.pem\"\n\
                 ca_path = \"ca.pem\"\ncrl_path = \"crl.pem\"\n{extra}"
            ))
            .unwrap();
            config.server.tls.build().unwrap().unwrap()
        };
        let strict = tls("");
        assert!(!strict.crl_end_entity_only && !strict.crl_allow_unknown);
        let relaxed = tls("crl_end_entity_only = true\ncrl_allow_unknown = true");
        assert!(relaxed.crl_end_entity_only && relaxed.crl_allow_unknown);
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
    fn test_bridges_are_checked() {
        let check = |bridges: &[&str]| -> Result<(), String> {
            let toml: String = bridges
                .iter()
                .map(|extra| format!("[[bridges]]\nhost = \"h\"\ntopics = []\n{extra}\n"))
                .collect();
            toml::from_str::<Config>(&toml).unwrap().check()
        };
        assert!(check(&["name = \"a\"", "name = \"b\""]).is_ok());
        // Each bridge's session takes over the other's
        let error = check(&["name = \"a\"", "name = \"a\""]).unwrap_err();
        assert!(error.contains("`a`"), "{error}");
        let error = check(&[
            "name = \"a\"\nclient_id = \"edge\"",
            "name = \"b\"\nclient_id = \"edge\"",
        ])
        .unwrap_err();
        assert!(error.contains("`edge`"), "{error}");
        assert!(check(&[
            "name = \"a\"",
            "name = \"b\"\nclient_id = \"mercurio-bridge-a\""
        ])
        .is_err());

        // Not a single level of the state topic
        for invalid in [
            "name = \"a/b\"",
            "name = \"a\"\nclient_id = \"c+\"",
            "name = \"#\"",
        ] {
            assert!(check(&[invalid]).is_err(), "{invalid}");
        }
        assert!(check(&["name = \"a/b\"\nnotifications = false"]).is_ok());

        // Half a client certificate
        assert!(check(&["name = \"a\"\n[bridges.tls]\ncert_path = \"c.pem\""]).is_err());
        assert!(check(&["name = \"a\"\n[bridges.tls]\nkey_path = \"k.pem\""]).is_err());
        assert!(check(&[
            "name = \"a\"\n[bridges.tls]\ncert_path = \"c.pem\"\nkey_path = \"k.pem\""
        ])
        .is_ok());
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
    fn test_plugin_names_must_be_usable_in_topics_and_not_built_in() {
        let named = |name: &str| -> Config {
            toml::from_str(&format!(
                "[[plugins]]\npath = \"a.wasm\"\nname = \"{name}\"\n"
            ))
            .unwrap()
        };
        for bad in [
            "",
            "a/b",
            "a+",
            "#",
            "dynamic-security",
            "acl-file",
            "plain",
        ] {
            let error = named(bad).check().unwrap_err();
            assert!(error.contains(&format!("`{bad}`")), "{bad}: {error}");
        }
        assert!(named("audit").check().is_ok());
        // A file name counts as the name too
        let file: Config = toml::from_str("[[plugins]]\npath = \"http-auth.wasm\"\n").unwrap();
        assert!(file.check().is_err());
    }

    #[test]
    fn test_unix_listeners_take_an_octal_mode_and_a_group() {
        let config = |extra: &str| -> Config {
            toml::from_str(&format!("[[listeners]]\n{extra}\n")).unwrap()
        };
        let set = config("path = \"/run/m.sock\"\nmode = \"0660\"\ngroup = \"mqtt\"");
        assert!(set.check().is_ok());
        assert_eq!(set.listeners[0].socket_mode().unwrap(), Some(0o660));
        assert_eq!(set.listeners[0].group.as_deref(), Some("mqtt"));
        assert_eq!(
            config("path = \"/run/m.sock\"").listeners[0].socket_mode(),
            Ok(None)
        );
        for (extra, key) in [
            ("path = \"/run/m.sock\"\nmode = \"0999\"", "mode"),
            ("path = \"/run/m.sock\"\nmode = \"1777\"", "mode"),
            ("path = \"/run/m.sock\"\nmode = \"rw\"", "mode"),
            ("port = 1884\nmode = \"0660\"", "mode"),
            ("port = 1884\ngroup = \"mqtt\"", "group"),
        ] {
            let error = config(extra).check().unwrap_err();
            assert!(error.contains(&format!("`{key}`")), "{extra}: {error}");
        }
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

    #[test]
    fn test_admin_socket_is_on_by_default_and_can_be_turned_off() {
        let config: Config = toml::from_str("").unwrap();
        assert!(config.admin.is_none());
        let admin = AdminConfig::default();
        assert_eq!(
            (admin.socket.as_str(), admin.group.as_str()),
            ("/run/mercurio/admin.sock", "mercurio")
        );

        let config: Config = toml::from_str("[admin]\ngroup = \"wheel\"\n").unwrap();
        let admin = config.admin.unwrap();
        assert_eq!(
            (admin.socket.as_str(), admin.group.as_str()),
            ("/run/mercurio/admin.sock", "wheel")
        );
        let config: Config = toml::from_str("[admin]\nsocket = \"\"\n").unwrap();
        assert!(config.admin.unwrap().socket.is_empty());
        assert!(toml::from_str::<Config>("[admin]\nsockets = \"/x\"\n").is_err());
    }

    #[test]
    fn test_max_inflight_reaches_the_limits_and_rejects_zero() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(
            config.server.limits().unwrap().max_inflight,
            mercurio_server::server::Limits::default().max_inflight
        );
        let config: Config = toml::from_str("[server]\nmax_inflight = 5\n").unwrap();
        assert_eq!(config.server.limits().unwrap().max_inflight, 5);
        let config: Config = toml::from_str("[server]\nmax_inflight = 0\n").unwrap();
        assert!(config.server.limits().is_err());
    }

    #[test]
    fn test_max_connect_packet_size_reaches_the_limits_and_rejects_zero() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(
            config.server.limits().unwrap().max_connect_packet_size,
            mercurio_server::server::DEFAULT_MAX_CONNECT_PACKET_SIZE
        );
        let config: Config = toml::from_str("[server]\nmax_connect_packet_size = 512\n").unwrap();
        assert_eq!(config.server.limits().unwrap().max_connect_packet_size, 512);
        let config: Config = toml::from_str("[server]\nmax_connect_packet_size = 0\n").unwrap();
        assert!(config.server.limits().is_err());
    }

    #[test]
    fn test_plugin_timeout_and_tick_interval_are_configurable() {
        let config: Config = toml::from_str("").unwrap();
        let defaults = mercurio_server::server::ServerConfig::default();
        assert_eq!(config.server.plugin_timeout(), defaults.plugin_timeout);
        assert_eq!(
            config.server.plugin_tick_interval(),
            defaults.plugin_tick_interval
        );
        let config: Config =
            toml::from_str("[server]\nplugin_timeout = 12\nplugin_tick_interval = 3\n").unwrap();
        assert!(config.check().is_ok());
        assert_eq!(config.server.plugin_timeout(), Duration::from_secs(12));
        assert_eq!(config.server.plugin_tick_interval(), Duration::from_secs(3));
        for zero in ["plugin_timeout = 0", "plugin_tick_interval = 0"] {
            let config: Config = toml::from_str(&format!("[server]\n{zero}\n")).unwrap();
            assert!(config.check().is_err(), "{zero}");
        }
    }

    #[test]
    fn test_timeouts_longer_than_plugin_timeout_are_refused() {
        let check = |toml: &str| toml::from_str::<Config>(toml).unwrap().check();
        for (toml, key) in [
            ("[auth.http]\nacl_url = \"http://h/acl\"\ntimeout_seconds = 10\n", "timeout_seconds"),
            (
                "[[listeners]]\nport = 1884\n[listeners.auth.http]\nacl_url = \"http://h/acl\"\ntimeout_seconds = 6\n",
                "timeout_seconds",
            ),
            ("[[plugins]]\npath = \"a.wasm\"\ntimeout_ms = 5001\n", "timeout_ms"),
        ] {
            let error = check(toml).unwrap_err();
            assert!(
                error.contains(&format!("`{key}`")) && error.contains("`plugin_timeout`"),
                "{toml}: {error}"
            );
            let raised = format!("[server]\nplugin_timeout = 10\n{toml}");
            assert!(check(&raised).is_ok(), "{raised}");
        }
        assert!(check("[auth.http]\nacl_url = \"http://h/acl\"\ntimeout_seconds = 5\n").is_ok());
        assert!(check("[[plugins]]\npath = \"a.wasm\"\ntimeout_ms = 5000\n").is_ok());
    }

    #[test]
    fn test_shipped_config_parses() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/contrib/config.toml");
        let config = Config::from_file(std::path::Path::new(path)).unwrap();
        let admin = config.admin.expect("the shipped config has [admin]");
        assert_eq!(admin.socket, "/run/mercurio/admin.sock");
        assert_eq!(admin.group, "mercurio");
    }

    #[test]
    fn test_postgres_pool_settings() {
        let options = |toml: &str| {
            let config: Config = toml::from_str(toml).unwrap();
            config.storage.postgres_options().unwrap().unwrap()
        };
        let defaults = options("[storage]\nbackend = \"postgres\"\nurl = \"postgres://h/db\"\n");
        assert_eq!(defaults.pool_size, 16);
        assert_eq!(defaults.pool_timeout, std::time::Duration::from_secs(5));

        let set = options(
            "[storage]\nbackend = \"postgres\"\nurl = \"postgres://h/db\"\n\
             ca_path = \"ca.pem\"\npool_size = 64\npool_timeout = 30\n",
        );
        assert_eq!(set.pool_size, 64);
        assert_eq!(set.pool_timeout, std::time::Duration::from_secs(30));
        assert_eq!(set.ca_path.as_deref(), Some(std::path::Path::new("ca.pem")));

        let config: Config = toml::from_str(
            "[storage]\nbackend = \"postgres\"\nurl = \"postgres://h/db\"\npool_size = 0\n",
        )
        .unwrap();
        assert!(config.storage.postgres_options().is_err());
    }

    #[test]
    fn test_unknown_keys_are_refused_in_every_table() {
        for (toml, key) in [
            ("includedir = \"/x\"\n", "includedir"),
            ("[listener]\nport = 1884\n", "listener"),
            ("[server]\nprot = 1\n", "prot"),
            ("[server.tls]\nenable = true\n", "enable"),
            (
                "[server.redirect]\nserver_reference = \"h\"\npermanent_ = true\n",
                "permanent_",
            ),
            ("[logging]\nlvl = \"debug\"\n", "lvl"),
            ("[auth]\nenable = true\n", "enable"),
            ("[auth.http]\nuser_ur = \"http://h\"\n", "user_ur"),
            ("[admin]\nsockets = \"/x\"\n", "sockets"),
            ("[storage]\nbackend = \"memory\"\npath = \"x\"\n", "path"),
            (
                "[storage]\nbackend = \"sqlite\"\npath = \"x\"\nstandby = true\n",
                "standby",
            ),
            (
                "[storage]\nbackend = \"postgres\"\nurl = \"postgres://h/db\"\nca_file = \"ca.pem\"\n",
                "ca_file",
            ),
            (
                "[[listeners]]\nport = 1884\nproxy_form = [\"10.0.0.0/8\"]\n",
                "proxy_form",
            ),
            (
                "[[listeners]]\nport = 8883\n[listeners.tls]\nenabled = true\nca_file = \"ca.pem\"\n",
                "ca_file",
            ),
            (
                "[[listeners]]\nport = 1885\n[listeners.auth]\nenabled = true\npasswd_file = \"p\"\n",
                "passwd_file",
            ),
            (
                "[[bridges]]\nname = \"b\"\nhost = \"h\"\nusername_ = \"u\"\n[[bridges.topics]]\npattern = \"#\"\n",
                "username_",
            ),
            (
                "[[bridges]]\nname = \"b\"\nhost = \"h\"\n[bridges.tls]\nca_file = \"ca.pem\"\n[[bridges.topics]]\npattern = \"#\"\n",
                "ca_file",
            ),
            (
                "[[bridges]]\nname = \"b\"\nhost = \"h\"\n[[bridges.topics]]\npattern = \"#\"\ndirecton = \"in\"\n",
                "directon",
            ),
            ("[[plugins]]\npath = \"a.wasm\"\nmemory = 1\n", "memory"),
        ] {
            let error = toml::from_str::<Config>(toml).unwrap_err().to_string();
            assert!(error.contains(&format!("`{key}`")), "{toml}: {error}");
        }
    }

    #[test]
    fn test_unknown_keys_in_included_files_are_refused() {
        let dir = std::env::temp_dir().join(format!("mercuriod-unknown-{}", std::process::id()));
        let conf_d = dir.join("conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        let main = dir.join("main.toml");
        std::fs::write(&main, format!("include_dir = \"{}\"\n", conf_d.display())).unwrap();
        std::fs::write(conf_d.join("auth.toml"), "[auth]\nenable = true\n").unwrap();
        let error = Config::from_file(&main).unwrap_err();
        std::fs::remove_dir_all(&dir).unwrap();
        // As main prints it
        let shown = error.to_string();
        assert!(shown.contains("`enable`"), "{shown}");
        assert!(shown.contains("main.toml"), "{shown}");
        assert!(shown.contains("auth"), "{shown}");
        assert!(!shown.contains("TomlError"), "{shown}");
    }

    /// `sample` with every commented-out `key = value` line and table header
    /// turned on.
    fn uncommented(sample: &str) -> toml::Table {
        let setting = |line: &str| {
            line.starts_with('[')
                || line
                    .split_once(" = ")
                    .is_some_and(|(key, _)| key.chars().all(|c| c.is_ascii_lowercase() || c == '_'))
        };
        let text: String = sample
            .lines()
            .map(|line| match line.strip_prefix("# ") {
                Some(rest) if setting(rest) => rest,
                _ => line,
            })
            .map(|line| format!("{line}\n"))
            .collect();
        toml::from_str(&text).unwrap()
    }

    #[test]
    fn test_every_key_of_the_shipped_sample_and_readme_parses() {
        let dir = env!("CARGO_MANIFEST_DIR");
        let sample = std::fs::read_to_string(format!("{dir}/contrib/config.toml")).unwrap();
        let readme = std::fs::read_to_string(format!("{dir}/README.md")).unwrap();
        let (_, readme) = readme.split_once("```toml\n").unwrap();
        let (readme, _) = readme.split_once("```").unwrap();
        check_every_key_parses(uncommented(&sample));
        check_every_key_parses(uncommented(readme));
    }

    fn check_every_key_parses(mut table: toml::Table) {
        // The storage keys belong to different backends
        let storage = table.remove("storage").unwrap();
        let config = Config::deserialize(table).unwrap();
        assert!(config.include_dir.is_some());
        assert!(config.server.redirect.is_some());
        let http = config.auth.http.as_ref().unwrap();
        assert!(http.ca_path.is_some() && http.cert_path.is_some() && http.key_path.is_some());
        assert!(http.superuser_url.is_some());
        assert!(config.listeners.iter().any(|l| l.auth.is_some()));
        assert!(config.listeners.iter().any(|l| l.health && l.metrics));
        assert!(config.bridges.iter().any(|b| !b.topics.is_empty()));

        let storage = storage.as_table().unwrap();
        for (backend, keys) in [
            ("memory", &[][..]),
            ("sqlite", &["path"][..]),
            (
                "postgres",
                &["url", "ca_path", "standby", "pool_size", "pool_timeout"][..],
            ),
        ] {
            let mut table = toml::Table::new();
            table.insert("backend".into(), backend.into());
            for key in keys {
                table.insert(key.to_string(), storage[*key].clone());
            }
            StorageConfig::deserialize(table).unwrap();
        }
        let covered = [
            "backend",
            "path",
            "url",
            "ca_path",
            "standby",
            "pool_size",
            "pool_timeout",
        ];
        assert!(storage.keys().all(|k| covered.contains(&k.as_str())));
    }

    #[test]
    fn test_serialized_config_hides_secrets() {
        let config: Config = toml::from_str(
            r#"
            [server.tls]
            enabled = true
            cert_path = "cert.pem"
            key_path = "key.pem"

            [storage]
            backend = "postgres"
            url = "postgres://mercurio:hunter2@db.example.com/mercurio"

            [auth.http]
            user_url = "https://svc:pa55@auth.example.com/user"
            acl_url = "https://auth.example.com/acl"

            [[bridges]]
            name = "cloud"
            host = "cloud.example.com"
            username = "edge"
            password = "s3cret"
            [[bridges.topics]]
            pattern = "a/#"

            [[plugins]]
            path = "audit.wasm"
            config = { token = "tok3n" }
            "#,
        )
        .unwrap();
        let json = serde_json::to_value(&config).unwrap();
        let text = json.to_string();
        for secret in ["hunter2", "pa55", "s3cret", "tok3n"] {
            assert!(!text.contains(secret), "{secret} in {text}");
        }
        assert_eq!(
            json["storage"]["url"],
            "postgres://mercurio:<redacted>@db.example.com/mercurio"
        );
        assert_eq!(json["storage"]["backend"], "postgres");
        assert_eq!(json["auth"]["http"]["user_url"], "<redacted>");
        assert_eq!(
            json["auth"]["http"]["acl_url"],
            "https://auth.example.com/acl"
        );
        assert_eq!(json["bridges"][0]["password"], "<redacted>");
        assert_eq!(json["bridges"][0]["username"], "edge");
        assert_eq!(json["plugins"][0]["config"], "<redacted>");
        assert_eq!(json["server"]["tls"]["key_path"], "key.pem");
        assert_eq!(json["server"]["port"], 1883);
        assert_eq!(json["logging"]["level"], "info");

        let plain: Config = toml::from_str(
            "[[bridges]]\nname = \"b\"\nhost = \"h\"\n[[bridges.topics]]\npattern = \"#\"\n",
        )
        .unwrap();
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json["bridges"][0]["password"].is_null());
        assert_eq!(json["storage"]["backend"], "memory");
    }

    #[test]
    fn test_no_secret_bearing_field_is_shown() {
        let config: Config = toml::from_str(
            r#"
            [storage]
            backend = "postgres"
            url = "host=db user=u password=SECRET-pg sslmode=require"

            [auth]
            enabled = true
            [auth.http]
            user_url = "https://svc:SECRET-user@auth.example.com/user?token=SECRET-tok"
            acl_url = "https://auth.example.com/acl?api_key=SECRET-key"
            superuser_url = "https://svc:SECRET-su@auth.example.com/su"

            [[listeners]]
            port = 1884
            [listeners.auth.http]
            user_url = "https://SECRET-l1@auth.example.com/user"
            acl_url = "https://h/acl?key=SECRET-l2"

            [[bridges]]
            name = "cloud"
            host = "cloud.example.com"
            password = "SECRET-bridge"
            [[bridges.topics]]
            pattern = "a/#"

            [[plugins]]
            path = "audit.wasm"
            config = { token = "SECRET-plugin", other = "SECRET-other" }
            "#,
        )
        .unwrap();
        let text = serde_json::to_string(&config).unwrap();
        assert!(!text.contains("SECRET-"), "{text}");

        let url: Config = toml::from_str(
            "[storage]\nbackend = \"postgres\"\nurl = \"postgres://u:SECRET-a@h/db?sslpassword=SECRET-b\"\n",
        )
        .unwrap();
        assert!(!serde_json::to_string(&url).unwrap().contains("SECRET-"));
    }

    #[test]
    fn test_http_urls_hide_user_info_and_queries() {
        for url in [
            "https://TOKEN@auth.example.com/user",
            "https://svc:SECRET-x@auth.example.com/user",
            "https://h/user?key=SECRET-x",
            "https://h/user?sig=SECRET-x#SECRET-y",
            "https://svc:SECRET-x/y@h/user",
        ] {
            let shown = redacted_http_url(url);
            assert!(
                !shown.contains("TOKEN") && !shown.contains("SECRET-"),
                "{shown}"
            );
        }
        assert_eq!(
            redacted_http_url("https://h/user?a=1"),
            "https://h/user?<redacted>"
        );
        assert_eq!(
            redacted_http_url("https://h:8080/acl"),
            "https://h:8080/acl"
        );
        assert_eq!(redacted_http_url("not a url"), "<redacted>");
    }

    #[test]
    fn test_url_passwords_are_redacted() {
        assert_eq!(
            redacted_url("postgres://u:p@h:5432/db"),
            "postgres://u:<redacted>@h:5432/db"
        );
        assert_eq!(redacted_url("postgres://u@h/db"), "postgres://u@h/db");
        for url in [
            "postgres://u:pa/ss@h/db",
            "postgres://u:p?ss@h/db",
            "postgres://u:p#ss@h/db",
            "postgresql://u:pa/ss@h/db",
        ] {
            let shown = redacted_url(url);
            assert!(!shown.contains("ss@") && !shown.contains("pa/"), "{shown}");
            assert!(!shown.contains("p?") && !shown.contains("p#"), "{shown}");
            assert!(shown.ends_with("@h/db"), "{shown}");
        }
        assert_eq!(redacted_url("https://h/path@x"), "https://h/path@x");
        assert_eq!(redacted_url("postgres://h/db?password=p"), "<redacted>");
        assert_eq!(redacted_url("postgres://h/db?sslPassword=p"), "<redacted>");
        assert_eq!(redacted_url("https://h/a?api_key=k"), "<redacted>");
        assert_eq!(redacted_url("host=h password=p"), "<redacted>");
        assert_eq!(redacted_url("host=h PASSWORD=p"), "<redacted>");
        assert_eq!(
            redacted_url("https://auth.example.com/token"),
            "https://auth.example.com/token"
        );
        assert_eq!(
            redacted_url("https://auth.example.com/acl"),
            "https://auth.example.com/acl"
        );
    }
}
