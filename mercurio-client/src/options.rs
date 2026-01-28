use bytes::Bytes;
use mercurio_core::{protocol::ProtocolVersion, qos::QoS};

/// Will message configuration.
#[derive(Debug, Clone)]
pub struct Will {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
}

impl Will {
    pub fn new(topic: impl Into<String>, payload: impl Into<Bytes>) -> Self {
        Self {
            topic: topic.into(),
            payload: payload.into(),
            qos: QoS::AtMostOnce,
            retain: false,
        }
    }

    pub fn qos(mut self, qos: QoS) -> Self {
        self.qos = qos;
        self
    }

    pub fn retain(mut self, retain: bool) -> Self {
        self.retain = retain;
        self
    }
}

/// TLS configuration for client connections.
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// Enable TLS for the connection.
    pub enabled: bool,
    /// Path to custom CA certificate file (PEM format).
    /// If not set, system root certificates are used.
    pub ca_path: Option<String>,
    /// Skip server certificate verification (insecure, for testing only).
    pub danger_skip_verify: bool,
}

/// Options for connecting to an MQTT broker.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) client_id: Option<String>,
    pub(crate) username: Option<String>,
    pub(crate) password: Option<Bytes>,
    pub(crate) keep_alive: u16,
    pub(crate) clean_start: bool,
    pub(crate) will: Option<Will>,
    pub(crate) protocol_version: ProtocolVersion,
    pub(crate) connect_timeout_secs: u64,
    pub(crate) tls: TlsOptions,
}

impl ConnectOptions {
    /// Create new connection options for the given host and port.
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            client_id: None,
            username: None,
            password: None,
            keep_alive: 60,
            clean_start: true,
            will: None,
            protocol_version: ProtocolVersion::V5,
            connect_timeout_secs: 30,
            tls: TlsOptions::default(),
        }
    }

    /// Enable TLS for the connection.
    pub fn tls(mut self, enabled: bool) -> Self {
        self.tls.enabled = enabled;
        self
    }

    /// Set custom CA certificate file path for TLS verification.
    pub fn ca_path(mut self, path: impl Into<String>) -> Self {
        self.tls.ca_path = Some(path.into());
        self
    }

    /// Skip TLS certificate verification (insecure, for testing only).
    pub fn danger_skip_tls_verify(mut self, skip: bool) -> Self {
        self.tls.danger_skip_verify = skip;
        self
    }

    /// Set the client ID. If not set, the broker will assign one.
    pub fn client_id(mut self, client_id: impl Into<String>) -> Self {
        self.client_id = Some(client_id.into());
        self
    }

    /// Set the username for authentication.
    pub fn username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Set the password for authentication.
    pub fn password(mut self, password: impl Into<Bytes>) -> Self {
        self.password = Some(password.into());
        self
    }

    /// Set the keep-alive interval in seconds.
    pub fn keep_alive(mut self, seconds: u16) -> Self {
        self.keep_alive = seconds;
        self
    }

    /// Set clean start flag. If true, the broker will discard any existing session.
    pub fn clean_start(mut self, clean: bool) -> Self {
        self.clean_start = clean;
        self
    }

    /// Set the will message to be published if the client disconnects unexpectedly.
    pub fn will(mut self, will: Will) -> Self {
        self.will = Some(will);
        self
    }

    /// Set the MQTT protocol version to use.
    pub fn protocol_version(mut self, version: ProtocolVersion) -> Self {
        self.protocol_version = version;
        self
    }

    /// Set the connection timeout in seconds.
    pub fn connect_timeout(mut self, seconds: u64) -> Self {
        self.connect_timeout_secs = seconds;
        self
    }
}
