use std::fmt;
use std::hash::{BuildHasher, RandomState};
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

/// Largest packet accepted from the broker unless set otherwise, as
/// Mercurio's own default `max_packet_size`
const DEFAULT_MAXIMUM_PACKET_SIZE: u32 = 1024 * 1024;

const DEFAULT_RECONNECT_MAX: Duration = Duration::from_secs(60);

/// Exponential backoff with jitter: the waits between attempts to reach a
/// broker, doubling from `min` up to `max`.
#[derive(Debug, Clone)]
pub struct Backoff {
    min: Duration,
    max: Duration,
    attempt: u32,
}

impl Backoff {
    /// Waits from `min` up to `max`, or `min` if `max` is shorter.
    pub fn new(min: Duration, max: Duration) -> Self {
        Backoff {
            min,
            max: max.max(min),
            attempt: 0,
        }
    }

    /// The wait before the next attempt: somewhere between half of and the
    /// full doubled wait, and within `min` and `max`.
    pub fn next_delay(&mut self) -> Duration {
        let ceiling = self
            .min
            .saturating_mul(1 << self.attempt.min(30))
            .saturating_mul(2)
            .min(self.max);
        self.attempt = self.attempt.saturating_add(1);
        let floor = (ceiling / 2).max(self.min);
        // A fresh RandomState is seeded randomly, which is all the
        // randomness jitter needs
        let random = RandomState::new().hash_one(self.attempt);
        floor + (ceiling - floor).mul_f64(random as f64 / u64::MAX as f64)
    }

    /// Starts over from `min`, after an attempt succeeded.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

/// How long a write may take without a keep-alive to bound it
const DEFAULT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);

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
    pub(crate) connect_timeout: Duration,
    pub(crate) tls: TlsOptions,
    pub(crate) reconnect_delay: Option<Duration>,
    pub(crate) reconnect_max: Option<Duration>,
    pub(crate) write_timeout: Option<Duration>,
    pub(crate) manual_acks: bool,
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
            .field("connect_timeout", &self.connect_timeout)
            .field("tls", &self.tls)
            .field("reconnect_delay", &self.reconnect_delay)
            .field("reconnect_max", &self.reconnect_max)
            .field("write_timeout", &self.write_timeout)
            .field("manual_acks", &self.manual_acks)
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
            return Err(ClientError::tls(
                "TLS isn't supported over a Unix socket; a CA path, a client \
                 certificate or skipping verification turns it on",
                None,
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
            connect_timeout: Duration::from_secs(30),
            tls: TlsOptions::default(),
            reconnect_delay: None,
            reconnect_max: None,
            write_timeout: None,
            manual_acks: false,
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

    /// Enable TLS for the connection. Setting a CA path, a client
    /// certificate or skipping verification enables it too; the last call
    /// wins.
    pub fn tls(mut self, enabled: bool) -> Self {
        self.tls.enabled = enabled;
        self
    }

    /// Set custom CA certificate file path for TLS verification, and
    /// enable TLS.
    pub fn ca_path(mut self, path: impl Into<String>) -> Self {
        self.tls.enabled = true;
        self.tls.ca_path = Some(path.into());
        self
    }

    /// Authenticate with a client certificate and its private key, both
    /// PEM files, and enable TLS.
    pub fn client_cert(
        mut self,
        cert_path: impl Into<String>,
        key_path: impl Into<String>,
    ) -> Self {
        self.tls.enabled = true;
        self.tls.cert_path = Some(cert_path.into());
        self.tls.key_path = Some(key_path.into());
        self
    }

    /// Skip TLS certificate verification (insecure, for testing only).
    /// `true` enables TLS.
    pub fn danger_skip_tls_verify(mut self, skip: bool) -> Self {
        self.tls.enabled |= skip;
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

    /// Reconnect after losing the connection, waiting `delay` before the
    /// first attempt and doubling the wait after each failed one, up to
    /// [`reconnect_max`](Self::reconnect_max), each wait randomised by up
    /// to half so clients that lost the same broker don't all return at
    /// once. Combine with `clean_start(false)` and a session expiry so
    /// the broker keeps subscriptions and messages in flight meanwhile.
    pub fn reconnect(mut self, delay: Duration) -> Self {
        self.reconnect_delay = Some(delay);
        self
    }

    /// Longest wait between reconnection attempts, 60 seconds unless set.
    pub fn reconnect_max(mut self, max: Duration) -> Self {
        self.reconnect_max = Some(max);
        self
    }

    /// The waits between reconnection attempts, if reconnecting is on.
    pub(crate) fn reconnect_backoff(&self) -> Option<Backoff> {
        let min = self.reconnect_delay?;
        Some(Backoff::new(
            min,
            self.reconnect_max.unwrap_or(DEFAULT_RECONNECT_MAX),
        ))
    }

    /// Most QoS 2 messages, and with manual acknowledgement QoS 1 ones too,
    /// the broker may have in flight to this client at once. MQTT 5 only.
    pub fn receive_maximum(mut self, maximum: u16) -> Self {
        self.receive_maximum = Some(maximum);
        self
    }

    /// Acknowledge QoS 1 and 2 messages only when the application calls
    /// [`MqttClient::ack`] with the token each `Event::Message` carries,
    /// rather than on receipt. A message counts against
    /// [`receive_maximum`](Self::receive_maximum) until acknowledged, so
    /// an MQTT 5 broker holds further messages back while the application
    /// works on as many as it allows.
    ///
    /// A message that is never acknowledged, as when its event is dropped,
    /// keeps its place under Receive Maximum and holds back the
    /// acknowledgements of every later message, which go out in order,
    /// until the connection ends. Messages unacknowledged when the
    /// connection ends are forgotten; their tokens do nothing any more,
    /// and a broker that kept the session sends them again after
    /// reconnecting, as new events with new tokens. Processing then
    /// acknowledging gives at-least-once delivery, for QoS 2 too.
    ///
    /// [`MqttClient::ack`]: crate::MqttClient::ack
    pub fn manual_acks(mut self, manual: bool) -> Self {
        self.manual_acks = manual;
        self
    }

    /// Largest packet the broker may send, 1 MiB unless set, the broker's
    /// own default limit. An MQTT 5 broker is told and drops larger
    /// messages; one that sends a larger packet anyway, as an MQTT 3.x
    /// broker may, is disconnected.
    pub fn maximum_packet_size(mut self, bytes: u32) -> Self {
        self.maximum_packet_size = Some(bytes);
        self
    }

    pub(crate) fn incoming_packet_limit(&self) -> u32 {
        self.maximum_packet_size
            .unwrap_or(DEFAULT_MAXIMUM_PACKET_SIZE)
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

    /// Longest a packet may take to write before the connection counts as
    /// lost, which with reconnection on starts a reconnect. Defaults to the
    /// keep-alive interval the broker settled on, or to 30 seconds with a
    /// keep-alive of 0.
    pub fn write_timeout(mut self, timeout: Duration) -> Self {
        self.write_timeout = Some(timeout);
        self
    }

    pub(crate) fn write_timeout_for(&self, keep_alive: u16) -> Duration {
        self.write_timeout.unwrap_or(if keep_alive > 0 {
            Duration::from_secs(keep_alive.into())
        } else {
            DEFAULT_WRITE_TIMEOUT
        })
    }

    /// Longest the connection may take to open, TLS handshake and CONNECT
    /// included, 30 seconds unless set.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_backoff_doubles_up_to_the_maximum_with_jitter() {
        let ms = Duration::from_millis;
        let mut backoff = Backoff::new(ms(100), ms(1000));
        let waits: Vec<Duration> = (0..8).map(|_| backoff.next_delay()).collect();
        assert!((ms(100)..=ms(200)).contains(&waits[0]), "{waits:?}");
        assert!((ms(200)..=ms(400)).contains(&waits[1]), "{waits:?}");
        assert!((ms(400)..=ms(800)).contains(&waits[2]), "{waits:?}");
        for wait in &waits[3..] {
            assert!((ms(500)..=ms(1000)).contains(wait), "{waits:?}");
        }
        backoff.reset();
        assert!(backoff.next_delay() <= ms(200));

        let spread: std::collections::HashSet<Duration> = (0..20)
            .map(|_| Backoff::new(ms(100), ms(1000)).next_delay())
            .collect();
        assert!(spread.len() > 1, "no jitter");
        // A maximum below the minimum gives way
        assert_eq!(Backoff::new(ms(100), ms(10)).next_delay(), ms(100));
    }

    #[test]
    fn test_connect_timeout_takes_a_duration_as_the_other_timeouts() {
        let options = ConnectOptions::new("localhost", 1883);
        assert_eq!(options.connect_timeout, Duration::from_secs(30));
        let options = options.connect_timeout(Duration::from_millis(1500));
        assert_eq!(options.connect_timeout, Duration::from_millis(1500));
    }

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

    #[test]
    fn test_tls_settings_turn_tls_on() {
        let options = || ConnectOptions::new("broker", 8883);
        assert!(!options().tls.enabled);
        assert!(options().ca_path("ca.pem").tls.enabled);
        assert!(options().client_cert("c.pem", "k.pem").tls.enabled);
        assert!(options().danger_skip_tls_verify(true).tls.enabled);
        assert!(!options().danger_skip_tls_verify(false).tls.enabled);
        // The last call wins
        assert!(!options().ca_path("ca.pem").tls(false).tls.enabled);
        assert!(options().tls(false).ca_path("ca.pem").tls.enabled);
    }

    #[cfg(unix)]
    #[test]
    fn test_tls_is_refused_over_unix_sockets() {
        let options = ConnectOptions::unix("/run/mercurio/admin.sock").tls(true);
        assert!(matches!(options.check(), Err(ClientError::Tls { .. })));
        let options = ConnectOptions::unix("/run/mercurio/admin.sock").ca_path("ca.pem");
        let Err(error @ ClientError::Tls { .. }) = options.check() else {
            panic!("a CA path over a Unix socket was accepted");
        };
        assert!(error.to_string().contains("CA"), "{error}");
        assert!(ConnectOptions::unix("/run/mercurio/admin.sock")
            .check()
            .is_ok());
    }
}
