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
        }
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
