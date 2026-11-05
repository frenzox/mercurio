use bytes::Bytes;
use mercurio_core::properties::{
    ContentType, CorrelationData, MessageExpiryInterval, PayloadFormatIndicator, ResponseTopic,
    UserProperty,
};
use mercurio_core::{protocol::ProtocolVersion, qos::QoS};
use mercurio_packets::connect::WillProperties;
use mercurio_packets::publish::PublishProperties;

/// MQTT 5 properties of an application message. MQTT 3.x ignores them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageProperties {
    /// The payload is UTF-8 text
    pub payload_is_utf8: bool,
    /// Seconds the broker keeps the message for subscribers yet to get it
    pub message_expiry: Option<u32>,
    pub content_type: Option<String>,
    /// Where a request's reply should be published
    pub response_topic: Option<String>,
    /// Matches a reply to its request
    pub correlation_data: Option<Bytes>,
    pub user_properties: Vec<(String, String)>,
}

impl MessageProperties {
    pub(crate) fn to_publish(&self) -> Option<PublishProperties> {
        if *self == MessageProperties::default() {
            return None;
        }
        Some(PublishProperties {
            payload_format_indicator: self.payload_is_utf8.then(|| PayloadFormatIndicator::new(1)),
            message_expiry_interval: self.message_expiry.map(MessageExpiryInterval::new),
            content_type: self.content_type.clone().map(ContentType::new),
            response_topic: self.response_topic.clone().map(ResponseTopic::new),
            correlation_data: self.correlation_data.clone().map(CorrelationData::new),
            user_property: (!self.user_properties.is_empty()).then(|| {
                self.user_properties
                    .iter()
                    .map(|(k, v)| UserProperty::new(k.clone(), v.clone()))
                    .collect()
            }),
            ..Default::default()
        })
    }

    pub(crate) fn from_publish(properties: Option<PublishProperties>) -> Self {
        let p = properties.unwrap_or_default();
        MessageProperties {
            payload_is_utf8: p.payload_format_indicator.is_some_and(|f| f.value == 1),
            message_expiry: p.message_expiry_interval.map(|e| e.value),
            content_type: p.content_type.map(|c| c.value),
            response_topic: p.response_topic.map(|r| r.value),
            correlation_data: p.correlation_data.map(|c| c.value),
            user_properties: p
                .user_property
                .unwrap_or_default()
                .into_iter()
                .map(|u| (u.key, u.value))
                .collect(),
        }
    }
}

/// Will message configuration.
#[derive(Debug, Clone)]
pub struct Will {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
    /// Seconds the broker waits after the connection drops before
    /// publishing the will
    pub delay: u32,
    pub properties: MessageProperties,
}

impl Will {
    pub(crate) fn to_properties(&self) -> WillProperties {
        let p = self.properties.to_publish().unwrap_or_default();
        WillProperties {
            will_delay_interval: (self.delay > 0)
                .then(|| mercurio_core::properties::WillDelayInterval::new(self.delay)),
            payload_format_indicator: p.payload_format_indicator,
            message_expiry_interval: p.message_expiry_interval,
            content_type: p.content_type,
            response_topic: p.response_topic,
            correlation_data: p.correlation_data,
            user_property: p.user_property,
        }
    }
}

impl Will {
    pub fn new(topic: impl Into<String>, payload: impl Into<Bytes>) -> Self {
        Self {
            topic: topic.into(),
            payload: payload.into(),
            qos: QoS::AtMostOnce,
            retain: false,
            delay: 0,
            properties: MessageProperties::default(),
        }
    }

    /// Delay publishing the will, so a quick reconnect can cancel it.
    pub fn delay(mut self, seconds: u32) -> Self {
        self.delay = seconds;
        self
    }

    pub fn properties(mut self, properties: MessageProperties) -> Self {
        self.properties = properties;
        self
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
    pub(crate) session_expiry: u32,
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
            session_expiry: 0,
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

    /// Keep the session this many seconds after the connection closes;
    /// `u32::MAX` keeps it forever. MQTT 5 only.
    pub fn session_expiry(mut self, seconds: u32) -> Self {
        self.session_expiry = seconds;
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
