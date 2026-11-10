use std::fmt;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use mercurio_core::properties::{
    ContentType, CorrelationData, MessageExpiryInterval, PayloadFormatIndicator, ResponseTopic,
    UserProperty,
};
use mercurio_core::{protocol::ProtocolVersion, qos::QoS};
use mercurio_packets::connect::WillProperties;
use mercurio_packets::publish::PublishProperties;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::connection::Stream;
use crate::error::ClientError;

/// MQTT strings and binary data have a two-byte length (section 1.5.4).
pub(crate) fn check_length(
    field: &'static str,
    value: impl AsRef<[u8]>,
) -> crate::error::Result<()> {
    if value.as_ref().len() > u16::MAX as usize {
        return Err(ClientError::TooLong(field));
    }
    Ok(())
}

/// MQTT 5 properties of an application message. MQTT 3.x ignores them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageProperties {
    /// The payload is UTF-8 text
    pub payload_is_utf8: bool,
    /// Seconds the broker keeps the message for subscribers yet to get it
    pub message_expiry: Option<u32>,
    /// Describes the payload, such as a MIME type; free-form
    pub content_type: Option<String>,
    /// Where a request's reply should be published
    pub response_topic: Option<String>,
    /// Matches a reply to its request
    pub correlation_data: Option<Bytes>,
    /// Name and value pairs, delivered as published; a name may repeat
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

    pub(crate) fn check(&self) -> crate::error::Result<()> {
        if let Some(content_type) = &self.content_type {
            check_length("content type", content_type)?;
        }
        if let Some(response_topic) = &self.response_topic {
            check_length("response topic", response_topic)?;
        }
        if let Some(correlation_data) = &self.correlation_data {
            check_length("correlation data", correlation_data)?;
        }
        for (key, value) in &self.user_properties {
            check_length("user property", key)?;
            check_length("user property", value)?;
        }
        Ok(())
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
    fn check(&self) -> crate::error::Result<()> {
        check_length("will topic", &self.topic)?;
        check_length("will payload", &self.payload)?;
        self.properties.check()
    }

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
    /// A will publishing `payload` on `topic`: QoS 0, not retained, and
    /// without delay, until set otherwise.
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

/// The client's side of an MQTT 5 enhanced authentication method, such as
/// SCRAM. An exchange begins with `start` on every connection, reconnection
/// and re-authentication, then answers the broker's challenges until it
/// accepts or refuses the client.
pub trait Authenticator: Send {
    /// The method name the broker knows it by.
    fn method(&self) -> String;

    /// Begins an exchange, returning the data to send first.
    fn start(&mut self) -> Option<Bytes>;

    /// Answers a challenge from the broker. An error gives up, closing the
    /// connection.
    fn respond(&mut self, challenge: Option<Bytes>) -> Result<Option<Bytes>, String>;

    /// Checks the data the broker sent along with accepting the client,
    /// such as proof that the broker knows the credentials too.
    fn finish(&mut self, _data: Option<Bytes>) -> Result<(), String> {
        Ok(())
    }
}

/// An authenticator shared by every connection the client makes.
#[derive(Clone)]
pub(crate) struct SharedAuthenticator(Arc<Mutex<dyn Authenticator>>);

impl SharedAuthenticator {
    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, dyn Authenticator + 'static> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl fmt::Debug for SharedAuthenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Authenticator({})", self.lock().method())
    }
}

/// Opens the stream to the broker in place of TCP and TLS.
#[derive(Clone)]
pub(crate) struct Connector(pub Arc<dyn Fn() -> std::io::Result<Box<dyn Stream>> + Send + Sync>);

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Connector")
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
    /// Client certificate chain (PEM) to authenticate with.
    pub cert_path: Option<String>,
    /// Private key (PEM) of the client certificate.
    pub key_path: Option<String>,
}

/// Options for connecting to an MQTT broker.
#[derive(Clone)]
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
    pub(crate) reconnect_delay: Option<Duration>,
    pub(crate) receive_maximum: Option<u16>,
    pub(crate) maximum_packet_size: Option<u32>,
    pub(crate) topic_alias_maximum: u16,
    pub(crate) request_response_information: bool,
    pub(crate) request_problem_information: bool,
    pub(crate) authenticator: Option<SharedAuthenticator>,
    pub(crate) connector: Option<Connector>,
    /// Connect through this Unix socket instead of TCP
    #[cfg(unix)]
    pub(crate) unix_path: Option<PathBuf>,
}

impl std::fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_struct("ConnectOptions");
        debug
            .field("host", &self.host)
            .field("port", &self.port)
            .field("client_id", &self.client_id)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("keep_alive", &self.keep_alive)
            .field("clean_start", &self.clean_start)
            .field("session_expiry", &self.session_expiry)
            .field("will", &self.will)
            .field("protocol_version", &self.protocol_version)
            .field("connect_timeout_secs", &self.connect_timeout_secs)
            .field("tls", &self.tls)
            .field("reconnect_delay", &self.reconnect_delay)
            .field("receive_maximum", &self.receive_maximum)
            .field("maximum_packet_size", &self.maximum_packet_size)
            .field("topic_alias_maximum", &self.topic_alias_maximum)
            .field(
                "request_response_information",
                &self.request_response_information,
            )
            .field(
                "request_problem_information",
                &self.request_problem_information,
            )
            .field("authenticator", &self.authenticator)
            .field("connector", &self.connector);
        #[cfg(unix)]
        debug.field("unix_path", &self.unix_path);
        debug.finish()
    }
}

impl ConnectOptions {
    pub(crate) fn check(&self) -> crate::error::Result<()> {
        #[cfg(unix)]
        if self.unix_path.is_some() && self.tls.enabled {
            return Err(ClientError::Tls(
                "TLS isn't supported over a Unix socket".to_string(),
            ));
        }
        if let Some(client_id) = &self.client_id {
            check_length("client id", client_id)?;
        }
        if let Some(username) = &self.username {
            check_length("user name", username)?;
        }
        if let Some(password) = &self.password {
            check_length("password", password)?;
        }
        if let Some(authenticator) = &self.authenticator {
            check_length("authentication method", authenticator.lock().method())?;
        }
        self.will.as_ref().map_or(Ok(()), Will::check)
    }

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
            reconnect_delay: None,
            receive_maximum: None,
            maximum_packet_size: None,
            topic_alias_maximum: 0,
            request_response_information: false,
            request_problem_information: true,
            authenticator: None,
            connector: None,
            #[cfg(unix)]
            unix_path: None,
        }
    }

    /// Create connection options for a broker listening on the Unix socket
    /// at `path`, such as its admin socket. Reconnecting uses the same path;
    /// TLS can't be enabled on it.
    #[cfg(unix)]
    pub fn unix(path: impl Into<PathBuf>) -> Self {
        Self {
            unix_path: Some(path.into()),
            ..Self::new("localhost", 1883)
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

    /// Authenticate with a client certificate and its private key, both
    /// PEM files.
    pub fn client_cert(
        mut self,
        cert_path: impl Into<String>,
        key_path: impl Into<String>,
    ) -> Self {
        self.tls.cert_path = Some(cert_path.into());
        self.tls.key_path = Some(key_path.into());
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

    /// Reconnect after losing the connection, waiting `delay` between
    /// attempts. Combine with `clean_start(false)` and a session expiry so
    /// the broker keeps subscriptions and messages in flight meanwhile.
    pub fn reconnect(mut self, delay: Duration) -> Self {
        self.reconnect_delay = Some(delay);
        self
    }

    /// Most QoS 2 messages the broker may have in flight to this client at
    /// once. MQTT 5 only.
    pub fn receive_maximum(mut self, maximum: u16) -> Self {
        self.receive_maximum = Some(maximum);
        self
    }

    /// Largest packet the broker may send; it drops larger messages
    /// instead. MQTT 5 only.
    pub fn maximum_packet_size(mut self, bytes: u32) -> Self {
        self.maximum_packet_size = Some(bytes);
        self
    }

    /// Let the broker replace topic names with up to this many aliases,
    /// saving bandwidth on repeated topics. MQTT 5 only.
    pub fn topic_alias_maximum(mut self, maximum: u16) -> Self {
        self.topic_alias_maximum = maximum;
        self
    }

    /// Ask the broker for response information, a basis for response
    /// topics; see [`MqttClient::response_information`]. MQTT 5 only.
    ///
    /// [`MqttClient::response_information`]: crate::MqttClient::response_information
    pub fn request_response_information(mut self, request: bool) -> Self {
        self.request_response_information = request;
        self
    }

    /// With `false`, the broker leaves reason strings and user properties
    /// out of everything but PUBLISH, CONNACK and DISCONNECT. MQTT 5 only.
    pub fn request_problem_information(mut self, request: bool) -> Self {
        self.request_problem_information = request;
        self
    }

    /// Authenticate with an MQTT 5 enhanced authentication method.
    pub fn authenticator(mut self, authenticator: impl Authenticator + 'static) -> Self {
        self.authenticator = Some(SharedAuthenticator(Arc::new(Mutex::new(authenticator))));
        self
    }

    /// Open the stream to the broker with `connect`, on every connection
    /// and reconnection, instead of connecting to the host and port.
    pub fn connector<S, F>(mut self, connect: F) -> Self
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
        F: Fn() -> std::io::Result<S> + Send + Sync + 'static,
    {
        self.connector = Some(Connector(Arc::new(move || {
            connect().map(|stream| Box::new(stream) as Box<dyn Stream>)
        })));
        self
    }

    /// Set the connection timeout in seconds.
    pub fn connect_timeout(mut self, seconds: u64) -> Self {
        self.connect_timeout_secs = seconds;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_output_hides_the_password() {
        let options = ConnectOptions::new("localhost", 1883).password("hunter2");
        let shown = format!("{options:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains(r#"password: Some("<redacted>")"#), "{shown}");
        assert!(format!("{:?}", ConnectOptions::new("h", 1)).contains("password: None"));
    }

    #[test]
    fn test_overlong_values_are_refused() {
        let will = Will::new("t", "p").properties(MessageProperties {
            user_properties: vec![("k".into(), "v".repeat(65_536))],
            ..Default::default()
        });
        let options = ConnectOptions::new("localhost", 1883).will(will);
        assert!(matches!(
            options.check(),
            Err(ClientError::TooLong("user property"))
        ));
        assert!(ConnectOptions::new("localhost", 1883).check().is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn test_tls_is_refused_over_unix_sockets() {
        let options = ConnectOptions::unix("/run/mercurio/admin.sock").tls(true);
        assert!(matches!(options.check(), Err(ClientError::Tls(_))));
        assert!(ConnectOptions::unix("/run/mercurio/admin.sock")
            .check()
            .is_ok());
    }
}
