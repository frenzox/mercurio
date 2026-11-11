use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use mercurio_core::codec::VariableByteInteger;
use mercurio_core::properties::{
    AuthenticationData, AuthenticationMethod, MaximumPacketSize, ReceiveMaximum,
    RequestProblemInformation, RequestResponseInformation, SessionExpiryInterval,
    SubscriptionIdentifier, TopicAliasMaximum,
};
use mercurio_core::qos::QoS;
use mercurio_core::reason::ReasonCode;
use mercurio_core::topic::{validate_publish_topic, validate_subscribe_filter};
use mercurio_packets::auth::{AuthPacket, AuthProperties};
use mercurio_packets::connack::ConnAckProperties;
use mercurio_packets::connect::{ConnectFlags, ConnectPacket, ConnectPayload, ConnectProperties};
use mercurio_packets::disconnect::DisconnectPacket;
use mercurio_packets::pingreq::PingReqPacket;
use mercurio_packets::puback::PubAckPacket;
use mercurio_packets::pubcomp::PubCompPacket;
use mercurio_packets::publish::PublishPacket;
use mercurio_packets::pubrec::PubRecPacket;
use mercurio_packets::pubrel::PubRelPacket;
use mercurio_packets::subscribe::{
    SubscribePacket, SubscribePayload, SubscribeProperties, SubscriptionOptions,
};
use mercurio_packets::unsubscribe::{UnsubscribePacket, UnsubscribePayload};
use mercurio_packets::ControlPacket;
use tokio::net::TcpStream;
use tokio::sync::mpsc::error::{SendError, TrySendError};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{self, timeout};
use tracing::{debug, error, info, warn};

use crate::connection::Connection;
use crate::error::{transient_refusal, ClientError, Result};
use crate::event::{AckToken, DisconnectReason, Event, SubscribeResult, UnsubscribeResult};
use crate::options::{
    check_length, Backoff, ConnectOptions, MessageProperties, SharedAuthenticator, Will,
};
use crate::tls::build_tls_connector;

/// Command sent to the client event loop.
enum Command {
    Publish(PublishRequest),
    Subscribe {
        topics: Vec<(String, SubscriptionOptions)>,
        subscription_id: Option<u32>,
        response: oneshot::Sender<Result<Vec<SubscribeResult>>>,
    },
    Unsubscribe {
        topics: Vec<String>,
        response: oneshot::Sender<Result<Vec<UnsubscribeResult>>>,
    },
    Disconnect {
        response: oneshot::Sender<Result<()>>,
    },
    Reauthenticate {
        response: oneshot::Sender<Result<()>>,
    },
    Ack(AckToken),
}

/// An MQTT client for connecting to brokers.
///
/// Dropping the client closes the connection as [`MqttClient::disconnect`]
/// does: with a normal DISCONNECT, so the broker discards the will. A
/// client dropped while reconnecting stops trying.
pub struct MqttClient {
    command_tx: mpsc::Sender<Command>,
    event_rx: Mutex<mpsc::Receiver<Event>>,
    #[allow(dead_code)]
    client_id: String,
    response_information: Option<String>,
}

impl MqttClient {
    /// Connect to an MQTT broker with the given options.
    pub async fn connect(mut options: ConnectOptions) -> Result<Self> {
        options.check()?;
        let (connection, session) = Self::open(&options).await?;
        // Reconnecting must present the same identity to find the session
        options.client_id = Some(session.client_id.clone());
        let client_id = session.client_id.clone();
        let response_information = session.response_information.clone();

        let (command_tx, command_rx) = mpsc::channel(32);
        let (event_tx, event_rx) = mpsc::channel(256);
        tokio::spawn(Self::event_loop(
            connection, session, command_rx, event_tx, options,
        ));

        Ok(MqttClient {
            command_tx,
            event_rx: Mutex::new(event_rx),
            client_id,
            response_information,
        })
    }

    /// Opens the transport and performs the CONNECT handshake.
    async fn open(options: &ConnectOptions) -> Result<(Connection, Session)> {
        if let Some(connector) = &options.connector {
            let stream = (connector.0)().map_err(ClientError::ConnectionFailed)?;
            let mut connection = Connection::new(stream, options);
            let session = Self::perform_connect(&mut connection, options).await?;
            return Ok((connection, session));
        }

        let connect_timeout = options.connect_timeout;
        #[cfg(unix)]
        if let Some(path) = &options.unix_path {
            info!("Connecting to MQTT broker at {}", path.display());
            let stream = timeout(connect_timeout, tokio::net::UnixStream::connect(path))
                .await
                .map_err(|_| ClientError::Timeout)?
                .map_err(ClientError::ConnectionFailed)?;
            let mut connection = Connection::new(stream, options);
            let session = Self::perform_connect(&mut connection, options).await?;
            return Ok((connection, session));
        }

        let addr = format!("{}:{}", options.host, options.port);
        info!("Connecting to MQTT broker at {}", addr);

        let stream = timeout(connect_timeout, TcpStream::connect(&addr))
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(ClientError::ConnectionFailed)?;

        let mut connection = if options.tls.enabled {
            info!("Establishing TLS connection");
            let (connector, server_name) = build_tls_connector(&options.tls, &options.host)?;
            let tls_stream = timeout(connect_timeout, connector.connect(server_name, stream))
                .await
                .map_err(|_| ClientError::Timeout)?
                .map_err(handshake_error)?;
            Connection::new(tls_stream, options)
        } else {
            Connection::new(stream, options)
        };

        let session = Self::perform_connect(&mut connection, options).await?;
        Ok((connection, session))
    }

    /// Perform the MQTT CONNECT handshake.
    async fn perform_connect(
        connection: &mut Connection,
        options: &ConnectOptions,
    ) -> Result<Session> {
        let connect_packet = Self::build_connect_packet(options)?;
        connection
            .write_packet(ControlPacket::Connect(connect_packet))
            .await?;

        // Answer the broker's challenges until it sends CONNACK
        let connack = loop {
            let packet = timeout(options.connect_timeout, connection.read_packet())
                .await
                .map_err(|_| ClientError::Timeout)??
                .ok_or_else(|| {
                    ClientError::ConnectionFailed(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "the broker closed the connection before answering CONNECT",
                    ))
                })?;
            let ControlPacket::Auth(auth) = packet else {
                break packet;
            };
            let (Some(authenticator), ReasonCode::ContinueAuthentication) =
                (&options.authenticator, auth.reason)
            else {
                return Err(ClientError::Protocol("unexpected AUTH".into()));
            };
            let answer = auth_answer(authenticator, auth.properties.auth_data.map(|d| d.value))
                .map_err(ClientError::AuthenticationFailed)?;
            connection.write_packet(answer).await?;
        };

        let response_information;
        let (session_present, client_id, keep_alive, limits) = match connack {
            ControlPacket::ConnAck(ack) => {
                if ack.reason_code != ReasonCode::Success {
                    // A refusal may name the server to use instead
                    let server_reference = ack
                        .properties
                        .as_ref()
                        .and_then(|p| p.server_reference.as_ref())
                        .map(|r| r.value.clone());
                    return Err(match server_reference {
                        Some(server_reference) => ClientError::Redirected {
                            reason: ack.reason_code,
                            server_reference,
                        },
                        None => ClientError::ConnectionRefused(ack.reason_code),
                    });
                }
                // A clean start leaves no session to resume [MQTT-3.2.2-4]
                if ack.flags.session_present && options.clean_start {
                    let _ = connection
                        .write_packet(ControlPacket::Disconnect(DisconnectPacket {
                            reason: ReasonCode::ProtocolError,
                            properties: None,
                        }))
                        .await;
                    return Err(ClientError::BrokerViolation(ReasonCode::ProtocolError));
                }
                let assigned_id = ack
                    .properties
                    .as_ref()
                    .and_then(|p| p.assigned_client_id.as_ref())
                    .map(|id| id.value.clone());
                let client_id =
                    assigned_id.unwrap_or_else(|| options.client_id.clone().unwrap_or_default());
                // The broker may require a different keep-alive [MQTT-3.2.2-21]
                let keep_alive = ack
                    .properties
                    .as_ref()
                    .and_then(|p| p.server_keepalive.as_ref())
                    .map_or(options.keep_alive, |k| k.value);
                if let Some(authenticator) = &options.authenticator {
                    let data = ack
                        .properties
                        .as_ref()
                        .and_then(|p| p.authentication_data.as_ref())
                        .map(|d| d.value.clone());
                    authenticator
                        .lock()
                        .finish(data)
                        .map_err(ClientError::AuthenticationFailed)?;
                }
                let limits = BrokerLimits::from_connack(ack.properties.as_ref());
                response_information = ack
                    .properties
                    .as_ref()
                    .and_then(|p| p.response_information.as_ref())
                    .map(|r| r.value.clone());
                (ack.flags.session_present, client_id, keep_alive, limits)
            }
            _ => return Err(ClientError::Protocol("Expected CONNACK packet".to_string())),
        };

        info!(
            "Connected to MQTT broker, client_id: {}, session_present: {}",
            client_id, session_present
        );

        connection.set_max_packet_size(limits.maximum_packet_size);
        connection.set_write_timeout(options.write_timeout_for(keep_alive));
        Ok(Session {
            client_id,
            keep_alive,
            limits,
            session_present,
            response_information,
        })
    }

    /// Build a CONNECT packet from options.
    fn build_connect_packet(options: &ConnectOptions) -> Result<ConnectPacket> {
        let authentication = options.authenticator.as_ref().map(|a| {
            let mut authenticator = a.lock();
            (authenticator.method(), authenticator.start())
        });
        let flags = ConnectFlags {
            user_name: options.username.is_some(),
            password: options.password.is_some(),
            will_retain: options.will.as_ref().map(|w| w.retain).unwrap_or(false),
            will_qos: options
                .will
                .as_ref()
                .map(|w| w.qos)
                .unwrap_or(QoS::AtMostOnce),
            will_flag: options.will.is_some(),
            clean_start: options.clean_start,
        };

        let payload = ConnectPayload {
            client_id: options.client_id.clone().unwrap_or_default(),
            will_properties: options.will.as_ref().map(Will::to_properties),
            will_topic: options.will.as_ref().map(|w| w.topic.clone()),
            will_payload: options.will.as_ref().map(|w| w.payload.clone()),
            user_name: options.username.clone(),
            password: options.password.clone(),
        };

        Ok(ConnectPacket {
            protocol_version: options.protocol_version,
            flags,
            keepalive: options.keep_alive,
            properties: Some(ConnectProperties {
                session_expiry_interval: (options.session_expiry > 0)
                    .then(|| SessionExpiryInterval::new(options.session_expiry)),
                receive_maximum: options.receive_maximum.map(ReceiveMaximum::new),
                maximum_packet_size: Some(MaximumPacketSize::new(options.incoming_packet_limit())),
                topic_alias_maximum: (options.topic_alias_maximum > 0)
                    .then(|| TopicAliasMaximum::new(options.topic_alias_maximum)),
                request_response_information: options
                    .request_response_information
                    .then(|| RequestResponseInformation::new(1)),
                request_problem_information: (!options.request_problem_information)
                    .then(|| RequestProblemInformation::new(0)),
                authentication_method: authentication
                    .as_ref()
                    .map(|(method, _)| AuthenticationMethod::new(method.clone())),
                authentication_data: auth_data(authentication.and_then(|(_, data)| data))?,
                ..Default::default()
            }),
            payload,
        })
    }

    /// Runs the client until it disconnects for good: with reconnection
    /// enabled, a lost connection is reopened and the session resumed.
    async fn event_loop(
        connection: Connection,
        session: Session,
        mut command_rx: mpsc::Receiver<Command>,
        event_tx: mpsc::Sender<Event>,
        options: ConnectOptions,
    ) {
        let mut state = EventLoop {
            connection,
            event_tx,
            reports_reasons: options.protocol_version.supports_reason_codes(),
            last_packet_id: 0,
            next_seq: 0,
            outgoing: HashMap::new(),
            incoming_qos2: HashSet::new(),
            subscribes: HashMap::new(),
            unsubscribes: HashMap::new(),
            limits: session.limits,
            waiting: VecDeque::new(),
            topic_aliases: HashMap::new(),
            topic_alias_maximum: options.topic_alias_maximum,
            receive_maximum: options.receive_maximum.unwrap_or(u16::MAX),
            authenticator: options.authenticator.clone(),
            reauthentication: None,
            pending_events: VecDeque::new(),
            manual_acks: options.manual_acks,
            unacknowledged: VecDeque::new(),
            next_delivery: 0,
        };
        let mut keep_alive = session.keep_alive;
        let mut backoff = options.reconnect_backoff();

        loop {
            let reason = state.run(&mut command_rx, keep_alive).await;
            let reconnect = backoff.as_mut().filter(|_| should_reconnect(&reason));
            state.deliver(Event::Disconnected { reason });
            let Some(backoff) = reconnect else { break };

            // Their acknowledgements can't go out any more; the broker
            // sends the messages again if it kept the session
            state.unacknowledged.clear();
            // Requests the broker never answered can't be answered now
            state.subscribes.clear();
            state.unsubscribes.clear();
            if let Some(response) = state.reauthentication.take() {
                let _ = response.send(Err(ClientError::Disconnected));
            }
            let (connection, session) =
                match state.reconnect(&mut command_rx, &options, backoff).await {
                    Ok(opened) => opened,
                    Err(Some(error)) => {
                        state.deliver(Event::Stopped {
                            error: Arc::new(error),
                        });
                        break;
                    }
                    Err(None) => break,
                };
            state.connection = connection;
            state.limits = session.limits;
            // Aliases only last as long as the connection [MQTT-3.3.2-7]
            state.topic_aliases.clear();
            keep_alive = session.keep_alive;
            state.deliver(Event::Reconnected {
                session_present: session.session_present,
            });
            state.resume(session.session_present).await;
        }

        // Fail every request still waiting for the broker, and refuse new
        // ones, before waiting for the application to take the last events
        let pending_events = std::mem::take(&mut state.pending_events);
        let event_tx = state.event_tx.clone();
        drop(state);
        drop(command_rx);
        for event in pending_events {
            if event_tx.send(event).await.is_err() {
                break;
            }
        }
    }

    /// Handle a disconnect command.
    async fn handle_disconnect(connection: &mut Connection) -> Result<()> {
        let disconnect = DisconnectPacket {
            reason: ReasonCode::NormalDisconnection,
            properties: None,
        };

        connection
            .write_packet(ControlPacket::Disconnect(disconnect))
            .await?;

        Ok(())
    }

    /// Publish a message to a topic. For QoS 1 and 2 this waits until the
    /// broker has acknowledged the message.
    pub async fn publish(&self, topic: &str, payload: impl Into<Bytes>, qos: QoS) -> Result<()> {
        self.publish_with_retain(topic, payload, qos, false).await
    }

    /// Publish a message to a topic with retain flag.
    pub async fn publish_with_retain(
        &self,
        topic: &str,
        payload: impl Into<Bytes>,
        qos: QoS,
        retain: bool,
    ) -> Result<()> {
        self.publish_with_properties(topic, payload, qos, retain, MessageProperties::default())
            .await
    }

    /// Publish a message with MQTT 5 properties.
    pub async fn publish_with_properties(
        &self,
        topic: &str,
        payload: impl Into<Bytes>,
        qos: QoS,
        retain: bool,
        properties: MessageProperties,
    ) -> Result<()> {
        self.start_publish(topic, payload, qos, retain, properties)
            .await?
            .await
    }

    /// Hands a message to the client and returns once it is queued, with
    /// a [`PendingPublish`] that completes as `publish_with_properties`
    /// would. Messages started one after another go out in that order, so
    /// many can be in flight at once without reordering them.
    pub async fn start_publish(
        &self,
        topic: &str,
        payload: impl Into<Bytes>,
        qos: QoS,
        retain: bool,
        properties: MessageProperties,
    ) -> Result<PendingPublish> {
        check_length("topic", topic)?;
        validate_publish_topic(topic)
            .map_err(|e| ClientError::InvalidArgument(format!("topic `{topic}`: {e}")))?;
        properties.check()?;
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Publish(PublishRequest {
                topic: topic.to_string(),
                payload: payload.into(),
                qos,
                retain,
                properties,
                response: response_tx,
            }))
            .await
            .map_err(|_| ClientError::Closed)?;
        Ok(PendingPublish(response_rx))
    }

    /// Subscribe to one or more topics.
    pub async fn subscribe(&self, topics: &[(&str, QoS)]) -> Result<Vec<SubscribeResult>> {
        let topics: Vec<_> = topics
            .iter()
            .map(|(t, qos)| {
                let options = SubscriptionOptions {
                    qos: *qos,
                    ..Default::default()
                };
                (*t, options)
            })
            .collect();
        self.subscribe_with(&topics, None).await
    }

    /// Subscribe with MQTT 5 subscription options, optionally tagging the
    /// subscription so matching messages carry `subscription_id`.
    pub async fn subscribe_with(
        &self,
        topics: &[(&str, SubscriptionOptions)],
        subscription_id: Option<u32>,
    ) -> Result<Vec<SubscribeResult>> {
        check_filters(topics.iter().map(|(t, _)| *t))?;
        // A Variable Byte Integer from 1 [MQTT-3.8.2.1.2]
        if subscription_id.is_some_and(|id| !(1..=268_435_455).contains(&id)) {
            return Err(ClientError::InvalidArgument(
                "subscription identifiers range from 1 to 268,435,455".into(),
            ));
        }
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Subscribe {
                topics: topics.iter().map(|(t, o)| (t.to_string(), *o)).collect(),
                subscription_id,
                response: response_tx,
            })
            .await
            .map_err(|_| ClientError::Closed)?;

        response_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// Unsubscribe from one or more topics. MQTT 3.x brokers report
    /// success for every topic.
    pub async fn unsubscribe(&self, topics: &[&str]) -> Result<Vec<UnsubscribeResult>> {
        check_filters(topics.iter().copied())?;
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Unsubscribe {
                topics: topics.iter().map(|t| t.to_string()).collect(),
                response: response_tx,
            })
            .await
            .map_err(|_| ClientError::Closed)?;

        response_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// Runs the enhanced authentication exchange again on the open
    /// connection, for instance to renew expiring credentials. A broker that
    /// refuses the client closes the connection.
    pub async fn reauthenticate(&self) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Reauthenticate {
                response: response_tx,
            })
            .await
            .map_err(|_| ClientError::Closed)?;

        response_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// Acknowledges a message received with
    /// [`manual_acks`](ConnectOptions::manual_acks) on: QoS 1 gets its
    /// PUBACK and QoS 2 its PUBREC, which also frees its place under the
    /// Receive Maximum this client announced. Acknowledgements go out in
    /// the order the messages came, so one waits for those of earlier
    /// messages. A token from a connection that has ended does nothing.
    pub async fn ack(&self, token: AckToken) -> Result<()> {
        self.command_tx
            .send(Command::Ack(token))
            .await
            .map_err(|_| ClientError::Closed)
    }

    /// Disconnect from the broker gracefully.
    pub async fn disconnect(&self) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Disconnect {
                response: response_tx,
            })
            .await
            .map_err(|_| ClientError::Closed)?;

        response_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// What the broker answered to `request_response_information`, often a
    /// topic prefix this client may use for response topics.
    pub fn response_information(&self) -> Option<&str> {
        self.response_information.as_deref()
    }

    /// Receive the next event from the broker.
    /// Returns None if the client is disconnected.
    ///
    /// Up to 256 events wait for the application. Once that many are
    /// waiting, the client stops reading from the broker, which holds
    /// further messages back (and with them, over TCP, its publishers),
    /// until the application takes events again. Meanwhile the client keeps
    /// sending keep-alive pings and requests, so a slow reader stays
    /// connected. While one of its requests (a QoS 1 or 2 publish, a
    /// subscription, an unsubscription, a re-authentication) waits for the
    /// broker's answer, it reads on and holds the messages that arrive
    /// before the answer, so awaiting a publish between two calls to `recv`
    /// can't deadlock. No message is dropped while the client lives. With
    /// [`manual_acks`](ConnectOptions::manual_acks), the QoS 1 and 2
    /// messages held that way stay within the Receive Maximum the client
    /// announced, as the broker sends no more before some are acknowledged.
    pub async fn recv(&self) -> Option<Event> {
        let mut rx = self.event_rx.lock().await;
        rx.recv().await
    }
}

/// SUBSCRIBE and UNSUBSCRIBE need at least one filter [MQTT-3.8.3-2,
/// MQTT-3.10.3-2], each well formed.
fn check_filters<'a>(filters: impl ExactSizeIterator<Item = &'a str>) -> Result<()> {
    if filters.len() == 0 {
        return Err(ClientError::InvalidArgument(
            "at least one topic filter is needed".into(),
        ));
    }
    for filter in filters {
        check_length("topic filter", filter)?;
        validate_subscribe_filter(filter)
            .map_err(|e| ClientError::InvalidArgument(format!("topic filter `{filter}`: {e}")))?;
    }
    Ok(())
}

fn auth_data(data: Option<Bytes>) -> Result<Option<AuthenticationData>> {
    data.map(|data| {
        check_length("authentication data", &data).map(|()| AuthenticationData::new(data))
    })
    .transpose()
}

/// AUTH answering a broker challenge, or an error if the authenticator
/// gives up.
fn auth_answer(
    authenticator: &SharedAuthenticator,
    challenge: Option<Bytes>,
) -> std::result::Result<ControlPacket, String> {
    let mut authenticator = authenticator.lock();
    let data = auth_data(authenticator.respond(challenge)?).map_err(|e| e.to_string())?;
    Ok(ControlPacket::Auth(AuthPacket {
        reason: ReasonCode::ContinueAuthentication,
        properties: AuthProperties {
            auth_method: Some(AuthenticationMethod::new(authenticator.method())),
            auth_data: data,
            ..Default::default()
        },
    }))
}

/// A publish handed to the client, completing once the broker has
/// acknowledged it (QoS 1 and 2) or it was written (QoS 0). Dropping it
/// doesn't cancel the publish.
#[must_use = "a publish's outcome is only known by awaiting it"]
pub struct PendingPublish(oneshot::Receiver<Result<()>>);

impl Future for PendingPublish {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0)
            .poll(cx)
            .map(|answer| answer.unwrap_or(Err(ClientError::Disconnected)))
    }
}

/// What CONNACK settled for a connection.
struct Session {
    client_id: String,
    keep_alive: u16,
    limits: BrokerLimits,
    session_present: bool,
    response_information: Option<String>,
}

/// Whether a connection that ended this way is worth reopening. A broker
/// that handed the session to another connection or moved the client
/// elsewhere doesn't want it back.
fn should_reconnect(reason: &DisconnectReason) -> bool {
    match reason {
        DisconnectReason::ClientInitiated => false,
        DisconnectReason::ServerInitiated { reason, .. } => !matches!(
            reason,
            ReasonCode::SessionTakenOver | ReasonCode::UseAnotherServer | ReasonCode::ServerMoved
        ),
        _ => true,
    }
}

/// Whether a failed reconnection attempt means the broker won't take the
/// client back. Refusals for want of capacity pass and are retried; bad
/// credentials, a ban, an unsupported version or a redirection don't.
fn refused_for_good(error: &ClientError) -> bool {
    match error {
        ClientError::Redirected { .. } => true,
        ClientError::ConnectionRefused(reason) => !transient_refusal(*reason),
        _ => false,
    }
}

/// A TLS handshake that failed over the certificate or the TLS settings,
/// which rustls reports as invalid data, or over the connection itself.
fn handshake_error(error: std::io::Error) -> ClientError {
    if error.kind() == std::io::ErrorKind::InvalidData {
        ClientError::tls("TLS handshake failed", Some(error.into()))
    } else {
        ClientError::ConnectionFailed(error)
    }
}

/// A QoS 1 or 2 message sent to the broker and not acknowledged yet.
struct OutgoingPublish {
    /// Kept to send again if the connection drops before the broker
    /// acknowledges it
    packet: PublishPacket,
    /// PUBREC arrived, so only the PUBREL is left to (re)send
    released: bool,
    /// Messages are resent in the order they were first sent [MQTT-4.6.0-1]
    seq: u64,
    /// On the current connection; a resumed message waits for room under
    /// Receive Maximum before it goes out again
    sent: bool,
    response: oneshot::Sender<Result<()>>,
}

/// The client's side of the session.
struct EventLoop {
    connection: Connection,
    event_tx: mpsc::Sender<Event>,
    /// MQTT 5 acknowledgements carry reason codes, MQTT 3.x ones don't
    reports_reasons: bool,
    last_packet_id: u16,
    next_seq: u64,
    /// QoS 1 and 2 messages awaiting PUBACK or PUBCOMP
    outgoing: HashMap<u16, OutgoingPublish>,
    /// QoS 2 messages received and awaiting PUBREL
    incoming_qos2: HashSet<u16>,
    subscribes: HashMap<u16, PendingSubscribe>,
    unsubscribes: HashMap<u16, PendingUnsubscribe>,
    limits: BrokerLimits,
    /// QoS 1 and 2 publishes held back by the broker's Receive Maximum
    waiting: VecDeque<PublishRequest>,
    /// Topics the broker gave aliases on this connection
    topic_aliases: HashMap<u16, String>,
    topic_alias_maximum: u16,
    /// Most QoS 2 messages the broker may have in flight to this client
    receive_maximum: u16,
    authenticator: Option<SharedAuthenticator>,
    /// The application waiting for a re-authentication to finish
    reauthentication: Option<oneshot::Sender<Result<()>>>,
    /// Events that didn't fit in the event channel, oldest first
    pending_events: VecDeque<Event>,
    /// QoS 1 and 2 messages are acknowledged when the application says
    manual_acks: bool,
    /// With manual acknowledgement, the QoS 1 and 2 messages delivered on
    /// this connection and not acknowledged to the broker yet, in the
    /// order they came
    unacknowledged: VecDeque<Unacknowledged>,
    /// Numbers deliveries, so a token never matches a later message that
    /// reuses its packet identifier
    next_delivery: u64,
}

/// A message delivered to the application, awaiting its acknowledgement.
struct Unacknowledged {
    token: AckToken,
    qos: QoS,
    /// The application acknowledged it, but an earlier message still waits
    acknowledged: bool,
}

/// A message the application asked to publish.
struct PublishRequest {
    topic: String,
    payload: Bytes,
    qos: QoS,
    retain: bool,
    properties: MessageProperties,
    response: oneshot::Sender<Result<()>>,
}

/// What the broker said it supports, in CONNACK.
#[derive(Debug, Clone, Copy)]
pub(crate) struct BrokerLimits {
    receive_maximum: u16,
    maximum_packet_size: Option<u32>,
    maximum_qos: QoS,
    retain_available: bool,
    wildcard_subscriptions: bool,
    shared_subscriptions: bool,
    subscription_identifiers: bool,
}

impl BrokerLimits {
    /// Absent properties mean the most permissive value.
    fn from_connack(properties: Option<&ConnAckProperties>) -> Self {
        let p = properties;
        BrokerLimits {
            receive_maximum: p
                .and_then(|p| p.receive_maximum.as_ref())
                .map_or(u16::MAX, |r| r.value),
            maximum_packet_size: p
                .and_then(|p| p.maximum_packet_size.as_ref())
                .map(|m| m.value),
            maximum_qos: p
                .and_then(|p| p.maximum_qos.as_ref())
                .map_or(QoS::ExactlyOnce, |q| q.value.into()),
            retain_available: p
                .and_then(|p| p.retain_available.as_ref())
                .is_none_or(|r| r.value),
            wildcard_subscriptions: p
                .and_then(|p| p.wildcard_subscription_available.as_ref())
                .is_none_or(|w| w.value),
            shared_subscriptions: p
                .and_then(|p| p.shared_subscription_available.as_ref())
                .is_none_or(|s| s.value),
            subscription_identifiers: p
                .and_then(|p| p.subscription_identifier_available.as_ref())
                .is_none_or(|s| s.value),
        }
    }
}

struct PendingUnsubscribe {
    topics: Vec<String>,
    response: oneshot::Sender<Result<Vec<UnsubscribeResult>>>,
}

struct PendingSubscribe {
    topics: Vec<(String, QoS)>,
    response: oneshot::Sender<Result<Vec<SubscribeResult>>>,
}

impl EventLoop {
    /// Serves one connection until it ends, returning why.
    async fn run(
        &mut self,
        command_rx: &mut mpsc::Receiver<Command>,
        keep_alive: u16,
    ) -> DisconnectReason {
        // Ping at half the keep-alive, so a PINGRESP has the other half to
        // arrive; a keep-alive of 0 means no pings at all
        let ping_interval =
            (keep_alive > 0).then(|| Duration::from_millis(u64::from(keep_alive) * 500));
        let period = ping_interval.unwrap_or(Duration::from_secs(3600));
        let mut ping_timer = time::interval(period);
        // Ticks missed while the loop was busy don't fire in a burst
        ping_timer.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
        ping_timer.tick().await; // Skip the first immediate tick
        let mut awaiting_pingresp = false;
        // Reading stopped at some point since the last PINGREQ, so its
        // PINGRESP may still be unread
        let mut paused_since_ping = false;
        // A permit borrowing `self` would outlive the select
        let event_tx = self.event_tx.clone();

        loop {
            if self.connection.is_broken() {
                return DisconnectReason::ConnectionLost;
            }
            let reading = self.pending_events.is_empty() || self.waits_for_broker();
            paused_since_ping |= !reading;
            tokio::select! {
                // Handle incoming packets
                maybe_packet = self.connection.read_packet(), if reading => {
                    match maybe_packet {
                        Ok(Some(ControlPacket::Disconnect(disconnect))) => {
                            // The broker is closing the connection [MQTT-3.14.4-1]
                            info!("Received DISCONNECT from server: {:?}", disconnect.reason);
                            let properties = disconnect.properties.unwrap_or_default();
                            return DisconnectReason::ServerInitiated {
                                reason: disconnect.reason,
                                reason_string: properties.reason_string.map(|r| r.value),
                                server_reference: properties.server_reference.map(|r| r.value),
                            };
                        }
                        Ok(Some(packet)) => {
                            if matches!(packet, ControlPacket::PingResp(_)) {
                                awaiting_pingresp = false;
                            }
                            match self.handle_incoming(packet).await {
                                Err(ClientError::BrokerViolation(reason)) => {
                                    return self.disconnect_for(reason).await;
                                }
                                Err(ClientError::AuthenticationFailed(e)) => {
                                    warn!("Re-authentication failed: {}", e);
                                    return self.disconnect_for(ReasonCode::NotAuthorized).await;
                                }
                                Err(e) => warn!("Error handling packet: {}", e),
                                Ok(()) => {}
                            }
                        }
                        Ok(None) => {
                            // Connection closed
                            info!("Connection closed by server");
                            return DisconnectReason::ConnectionClosed;
                        }
                        Err(ClientError::BrokerViolation(reason)) => {
                            return self.disconnect_for(reason).await;
                        }
                        Err(e) => {
                            error!("Error reading packet: {}", e);
                            return DisconnectReason::ConnectionLost;
                        }
                    }
                }

                permit = event_tx.reserve(), if !self.pending_events.is_empty() => {
                    self.hand_over(permit);
                }

                // Handle commands from the client API
                cmd = command_rx.recv() => {
                    let Some(cmd) = cmd else {
                        // Every handle is gone: nobody can read events or
                        // send requests any more
                        let _ = MqttClient::handle_disconnect(&mut self.connection).await;
                        return DisconnectReason::ClientInitiated;
                    };
                    match cmd {
                        Command::Publish(request) => self.publish(request).await,
                        Command::Subscribe { topics, subscription_id, response } => {
                            self.subscribe(topics, subscription_id, response).await;
                        }
                        Command::Unsubscribe { topics, response } => {
                            self.unsubscribe(topics, response).await;
                        }
                        Command::Reauthenticate { response } => {
                            self.reauthenticate(response).await;
                        }
                        Command::Ack(token) => self.acknowledge(token).await,
                        Command::Disconnect { response } => {
                            let result = MqttClient::handle_disconnect(&mut self.connection).await;
                            let _ = response.send(result);
                            return DisconnectReason::ClientInitiated;
                        }
                    }
                }

                // Send PINGREQ for keep-alive
                scheduled = ping_timer.tick(), if ping_interval.is_some() => {
                    // A late tick means the loop itself was stalled, by a
                    // slow authenticator or a starved runtime, and couldn't
                    // read the PINGRESP that may be waiting
                    let stalled = scheduled.elapsed() > period / 2;
                    // The previous PINGREQ went unanswered: the broker or the
                    // network is gone
                    if awaiting_pingresp && !paused_since_ping && !stalled {
                        warn!("No PINGRESP from the broker, closing the connection");
                        return DisconnectReason::KeepAliveTimeout;
                    }
                    awaiting_pingresp = true;
                    paused_since_ping = false;
                    debug!("Sending PINGREQ");
                    if let Err(e) = self.connection.write_packet(
                        ControlPacket::PingReq(PingReqPacket {})
                    ).await {
                        error!("Failed to send PINGREQ: {}", e);
                        return DisconnectReason::KeepAliveTimeout;
                    }
                }
            }
        }
    }

    /// Hands an event to the application, or holds it while the event
    /// channel is full.
    fn deliver(&mut self, event: Event) {
        if self.pending_events.is_empty() {
            match self.event_tx.try_send(event) {
                Ok(()) | Err(TrySendError::Closed(_)) => {}
                Err(TrySendError::Full(event)) => self.pending_events.push_back(event),
            }
        } else {
            self.pending_events.push_back(event);
        }
    }

    /// Moves the oldest held event into the room `permit` made in the
    /// event channel.
    fn hand_over(&mut self, permit: std::result::Result<mpsc::Permit<'_, Event>, SendError<()>>) {
        match permit {
            Ok(permit) => {
                if let Some(event) = self.pending_events.pop_front() {
                    permit.send(event);
                }
            }
            // The application is gone
            Err(_) => self.pending_events.clear(),
        }
    }

    /// Whether a request of the application waits for the broker's answer.
    /// The answer may be queued behind messages, so they must be read even
    /// though the application isn't taking events.
    fn waits_for_broker(&self) -> bool {
        self.outgoing.values().any(|o| o.sent)
            || !self.subscribes.is_empty()
            || !self.unsubscribes.is_empty()
            || self.reauthentication.is_some()
    }

    /// Starts re-authenticating [MQTT-4.12.1-1].
    async fn reauthenticate(&mut self, response: oneshot::Sender<Result<()>>) {
        let Some(authenticator) = &self.authenticator else {
            let _ = response.send(Err(ClientError::InvalidArgument(
                "no authenticator configured".into(),
            )));
            return;
        };
        if self.reauthentication.is_some() {
            let _ = response.send(Err(ClientError::Protocol(
                "re-authentication already in progress".into(),
            )));
            return;
        }
        let packet = {
            let mut authenticator = authenticator.lock();
            let auth_data = match auth_data(authenticator.start()) {
                Ok(data) => data,
                Err(e) => {
                    let _ = response.send(Err(e));
                    return;
                }
            };
            ControlPacket::Auth(AuthPacket {
                reason: ReasonCode::ReAuthenticate,
                properties: AuthProperties {
                    auth_method: Some(AuthenticationMethod::new(authenticator.method())),
                    auth_data,
                    ..Default::default()
                },
            })
        };
        match self.connection.write_packet(packet).await {
            Ok(()) => self.reauthentication = Some(response),
            Err(e) => {
                let _ = response.send(Err(e));
            }
        }
    }

    /// Continues or completes a re-authentication.
    async fn handle_auth(&mut self, auth: AuthPacket) -> Result<()> {
        let (Some(authenticator), true) = (&self.authenticator, self.reauthentication.is_some())
        else {
            return Err(ClientError::BrokerViolation(ReasonCode::ProtocolError));
        };
        let data = auth.properties.auth_data.map(|d| d.value);
        let result = match auth.reason {
            ReasonCode::ContinueAuthentication => match auth_answer(authenticator, data) {
                Ok(answer) => return self.connection.write_packet(answer).await,
                Err(e) => Err(e),
            },
            ReasonCode::Success => authenticator.lock().finish(data),
            _ => return Err(ClientError::BrokerViolation(ReasonCode::ProtocolError)),
        };
        let response = self.reauthentication.take().expect("checked above");
        match result {
            Ok(()) => {
                let _ = response.send(Ok(()));
                Ok(())
            }
            Err(e) => {
                let _ = response.send(Err(ClientError::AuthenticationFailed(e.clone())));
                Err(ClientError::AuthenticationFailed(e))
            }
        }
    }

    /// Closes the connection over a broker's protocol violation, telling it
    /// why [MQTT-4.13.1-1].
    async fn disconnect_for(&mut self, reason: ReasonCode) -> DisconnectReason {
        warn!("Broker violated the protocol: {:?}", reason);
        let _ = self
            .connection
            .write_packet(ControlPacket::Disconnect(DisconnectPacket {
                reason,
                properties: None,
            }))
            .await;
        DisconnectReason::ProtocolError(reason.to_string())
    }

    /// Waits as `backoff` says between attempts to reopen the connection,
    /// starting it over once one succeeds. Meanwhile
    /// QoS 1 and 2 publishes queue up, and other requests fail. Gives up
    /// with the error when the broker refuses the client for good, or
    /// without one when the application disconnects.
    async fn reconnect(
        &mut self,
        command_rx: &mut mpsc::Receiver<Command>,
        options: &ConnectOptions,
        backoff: &mut Backoff,
    ) -> std::result::Result<(Connection, Session), Option<ClientError>> {
        let event_tx = self.event_tx.clone();
        loop {
            let wait = time::sleep(backoff.next_delay());
            tokio::pin!(wait);
            loop {
                tokio::select! {
                    _ = &mut wait => break,
                    permit = event_tx.reserve(), if !self.pending_events.is_empty() => {
                        self.hand_over(permit);
                    }
                    command = command_rx.recv() => match command.ok_or(None)? {
                        Command::Publish(request) if request.qos != QoS::AtMostOnce => {
                            self.waiting.push_back(request);
                        }
                        Command::Publish(request) => {
                            let _ = request.response.send(Err(ClientError::Disconnected));
                        }
                        Command::Subscribe { response, .. } => {
                            let _ = response.send(Err(ClientError::Disconnected));
                        }
                        Command::Unsubscribe { response, .. } => {
                            let _ = response.send(Err(ClientError::Disconnected));
                        }
                        Command::Reauthenticate { response } => {
                            let _ = response.send(Err(ClientError::Disconnected));
                        }
                        // Delivered on a connection that is gone
                        Command::Ack(_) => {}
                        Command::Disconnect { response } => {
                            let _ = response.send(Ok(()));
                            return Err(None);
                        }
                    },
                }
            }

            match MqttClient::open(options).await {
                Ok(opened) => {
                    backoff.reset();
                    return Ok(opened);
                }
                Err(e) if refused_for_good(&e) => {
                    error!("Broker refused to reconnect: {}", e);
                    return Err(Some(e));
                }
                Err(e) => warn!("Reconnecting failed: {}", e),
            }
        }
    }

    /// Picks up the exchanges the old connection left unfinished. If the
    /// broker kept the session, unacknowledged messages are resent with DUP
    /// and released ones get their PUBREL again [MQTT-4.4.0-1]. If it
    /// didn't, they start over as new messages.
    async fn resume(&mut self, session_present: bool) {
        if !session_present {
            self.incoming_qos2.clear();
            // The broker received these; only the PUBCOMP went missing
            let released: Vec<u16> = self
                .outgoing
                .iter()
                .filter(|(_, o)| o.released)
                .map(|(id, _)| *id)
                .collect();
            for id in released {
                self.complete_publish(id, ReasonCode::Success);
            }
        }

        // Releases go out again straight away: the broker already counts
        // their messages against Receive Maximum. Unacknowledged messages
        // wait their turn, with their packet identifier and DUP set, so a
        // broker that lowered the limit isn't sent more than it allows
        let mut releases: Vec<(u64, u16)> = Vec::new();
        for (id, outgoing) in &mut self.outgoing {
            if outgoing.released {
                releases.push((outgoing.seq, *id));
            } else {
                outgoing.sent = false;
                outgoing.packet.dup = session_present;
            }
        }
        releases.sort();
        for (_, packet_id) in releases {
            let release = ControlPacket::PubRel(PubRelPacket {
                packet_id,
                reason: ReasonCode::Success,
                properties: None,
            });
            // A failed write ends this connection too, and the next
            // reconnect resends again
            if self.connection.write_packet(release).await.is_err() {
                return;
            }
        }
        self.send_waiting().await;
    }

    /// Allocates a packet identifier not used by any exchange in progress.
    /// Identifiers are never 0 [MQTT-2.2.1-3].
    fn next_packet_id(&mut self) -> Option<u16> {
        for _ in 0..u16::MAX {
            self.last_packet_id = self.last_packet_id.checked_add(1).unwrap_or(1);
            let id = self.last_packet_id;
            if !self.outgoing.contains_key(&id)
                && !self.subscribes.contains_key(&id)
                && !self.unsubscribes.contains_key(&id)
            {
                return Some(id);
            }
        }
        None
    }

    /// Sends a PUBLISH. QoS 0 completes at once; QoS 1 and 2 complete when
    /// the broker has acknowledged them, and wait their turn if the broker's
    /// Receive Maximum is reached [MQTT-3.3.4-7].
    async fn publish(&mut self, request: PublishRequest) {
        if request.qos != QoS::AtMostOnce && self.in_flight() >= self.window() {
            self.waiting.push_back(request);
            return;
        }
        self.send_publish(request).await;
    }

    fn window(&self) -> usize {
        usize::from(self.limits.receive_maximum)
    }

    /// QoS 1 and 2 messages the broker has and hasn't fully acknowledged.
    fn in_flight(&self) -> usize {
        self.outgoing.values().filter(|o| o.sent).count()
    }

    /// Sends publishes held back by Receive Maximum while there is room,
    /// those resumed from the previous connection first.
    async fn send_waiting(&mut self) {
        while self.in_flight() < self.window() {
            let resumed = self
                .outgoing
                .iter_mut()
                .filter(|(_, o)| !o.sent)
                .min_by_key(|(_, o)| o.seq);
            if let Some((&packet_id, outgoing)) = resumed {
                outgoing.sent = true;
                let packet = ControlPacket::Publish(outgoing.packet.clone());
                match self.connection.write_packet(packet).await {
                    Ok(()) => {}
                    // The broker lowered its Maximum Packet Size since the
                    // message was first sent
                    Err(ClientError::PacketTooLarge) => {
                        if let Some(outgoing) = self.outgoing.remove(&packet_id) {
                            let _ = outgoing.response.send(Err(ClientError::PacketTooLarge));
                        }
                    }
                    Err(_) => return,
                }
                continue;
            }
            let Some(request) = self.waiting.pop_front() else {
                break;
            };
            self.send_publish(request).await;
        }
    }

    async fn send_publish(&mut self, request: PublishRequest) {
        let PublishRequest {
            topic,
            payload,
            qos,
            retain,
            properties,
            response,
        } = request;
        // Features the broker doesn't support are refused here rather than
        // sent [MQTT-3.2.2-11, MQTT-3.2.2-14]. A publish that waited may
        // meet the limits of a later connection.
        if (qos as u8) > (self.limits.maximum_qos as u8) {
            let _ = response.send(Err(ClientError::NotSupported("this QoS")));
            return;
        }
        if retain && !self.limits.retain_available {
            let _ = response.send(Err(ClientError::NotSupported("retained messages")));
            return;
        }
        let packet_id = match qos {
            QoS::AtMostOnce => None,
            _ => match self.next_packet_id() {
                Some(id) => Some(id),
                None => {
                    let _ = response.send(Err(ClientError::Protocol(
                        "no free packet identifier".into(),
                    )));
                    return;
                }
            },
        };

        let publish = PublishPacket {
            dup: false,
            qos_level: qos,
            retain,
            topic_name: topic,
            packet_id,
            properties: properties.to_publish(),
            payload: Some(payload),
        };
        if let Err(e) = self
            .connection
            .write_packet(ControlPacket::Publish(publish.clone()))
            .await
        {
            let _ = response.send(Err(e));
            return;
        }

        match packet_id {
            Some(packet_id) => {
                self.next_seq += 1;
                self.outgoing.insert(
                    packet_id,
                    OutgoingPublish {
                        packet: publish,
                        released: false,
                        seq: self.next_seq,
                        sent: true,
                        response,
                    },
                );
            }
            None => {
                let _ = response.send(Ok(()));
            }
        }
    }

    async fn subscribe(
        &mut self,
        topics: Vec<(String, SubscriptionOptions)>,
        subscription_id: Option<u32>,
        response: oneshot::Sender<Result<Vec<SubscribeResult>>>,
    ) {
        let uses_wildcards = topics
            .iter()
            .any(|(t, _)| t.split('/').any(|level| level == "+" || level == "#"));
        let uses_shared = topics.iter().any(|(t, _)| t.starts_with("$share/"));
        if (uses_wildcards && !self.limits.wildcard_subscriptions)
            || (uses_shared && !self.limits.shared_subscriptions)
        {
            let _ = response.send(Err(ClientError::NotSupported(
                "wildcard or shared subscriptions",
            )));
            return;
        }
        if subscription_id.is_some() && !self.limits.subscription_identifiers {
            let _ = response.send(Err(ClientError::NotSupported("subscription identifiers")));
            return;
        }

        let Some(packet_id) = self.next_packet_id() else {
            let _ = response.send(Err(ClientError::Protocol(
                "no free packet identifier".into(),
            )));
            return;
        };

        let payload = topics
            .iter()
            .map(|(topic, options)| SubscribePayload {
                topic_filter: topic.clone(),
                subs_opt: *options,
            })
            .collect();
        let subscribe = SubscribePacket {
            packet_id,
            properties: subscription_id.map(|id| SubscribeProperties {
                subscription_id: Some(SubscriptionIdentifier::new(VariableByteInteger(id))),
                user_property: None,
            }),
            payload,
        };
        let topics = topics.into_iter().map(|(t, o)| (t, o.qos)).collect();
        match self
            .connection
            .write_packet(ControlPacket::Subscribe(subscribe))
            .await
        {
            Ok(()) => {
                self.subscribes
                    .insert(packet_id, PendingSubscribe { topics, response });
            }
            Err(e) => {
                let _ = response.send(Err(e));
            }
        }
    }

    async fn unsubscribe(
        &mut self,
        topics: Vec<String>,
        response: oneshot::Sender<Result<Vec<UnsubscribeResult>>>,
    ) {
        let Some(packet_id) = self.next_packet_id() else {
            let _ = response.send(Err(ClientError::Protocol(
                "no free packet identifier".into(),
            )));
            return;
        };

        let unsubscribe = UnsubscribePacket {
            packet_id,
            properties: None,
            payload: topics
                .iter()
                .map(|topic_filter| UnsubscribePayload {
                    topic_filter: topic_filter.clone(),
                })
                .collect(),
        };
        match self
            .connection
            .write_packet(ControlPacket::Unsubscribe(unsubscribe))
            .await
        {
            Ok(()) => {
                self.unsubscribes
                    .insert(packet_id, PendingUnsubscribe { topics, response });
            }
            Err(e) => {
                let _ = response.send(Err(e));
            }
        }
    }

    /// Reason for acknowledging an identifier with no exchange in progress.
    fn unknown_packet_id(&self) -> ReasonCode {
        if self.reports_reasons {
            ReasonCode::PacketIdentifierNotFound
        } else {
            ReasonCode::Success
        }
    }

    /// Completes the publish waiting for `packet_id`, failing it if the
    /// broker refused the message.
    fn complete_publish(&mut self, packet_id: u16, reason: ReasonCode) {
        if let Some(outgoing) = self.outgoing.remove(&packet_id) {
            let result = if reason.get_code() >= 0x80 {
                Err(ClientError::PublishRefused(reason))
            } else {
                Ok(())
            };
            let _ = outgoing.response.send(result);
        }
    }

    async fn handle_incoming(&mut self, packet: ControlPacket) -> Result<()> {
        let acknowledges_publish = matches!(
            packet,
            ControlPacket::PubAck(_) | ControlPacket::PubRec(_) | ControlPacket::PubComp(_)
        );
        self.dispatch(packet).await?;
        // An acknowledgement may have made room under Receive Maximum
        if acknowledges_publish {
            self.send_waiting().await;
        }
        Ok(())
    }

    async fn dispatch(&mut self, packet: ControlPacket) -> Result<()> {
        match packet {
            ControlPacket::Publish(publish) => self.handle_publish(publish).await?,
            ControlPacket::PubAck(ack) => self.complete_publish(ack.packet_id, ack.reason),
            ControlPacket::PubRec(rec) => {
                // A failure ends the exchange; otherwise release the message
                if rec.reason.get_code() >= 0x80 {
                    self.complete_publish(rec.packet_id, rec.reason);
                } else {
                    let reason = match self.outgoing.get_mut(&rec.packet_id) {
                        Some(outgoing) => {
                            outgoing.released = true;
                            ReasonCode::Success
                        }
                        None => self.unknown_packet_id(),
                    };
                    self.connection
                        .write_packet(ControlPacket::PubRel(PubRelPacket {
                            packet_id: rec.packet_id,
                            reason,
                            properties: None,
                        }))
                        .await?;
                }
            }
            ControlPacket::PubComp(comp) => self.complete_publish(comp.packet_id, comp.reason),
            ControlPacket::PubRel(rel) => {
                let reason = if self.incoming_qos2.remove(&rel.packet_id) {
                    ReasonCode::Success
                } else {
                    self.unknown_packet_id()
                };
                self.connection
                    .write_packet(ControlPacket::PubComp(PubCompPacket {
                        packet_id: rel.packet_id,
                        reason,
                        properties: None,
                    }))
                    .await?;
            }
            ControlPacket::SubAck(ack) => {
                if let Some(pending) = self.subscribes.remove(&ack.packet_id) {
                    let results = pending
                        .topics
                        .into_iter()
                        .zip(ack.payload)
                        .map(|((topic, _), granted)| {
                            let code = granted.reason_code.get_code();
                            SubscribeResult {
                                topic,
                                qos: if code < 0x80 {
                                    code.into()
                                } else {
                                    QoS::AtMostOnce
                                },
                                success: code < 0x80,
                                reason: granted.reason_code,
                            }
                        })
                        .collect();
                    let _ = pending.response.send(Ok(results));
                }
            }
            ControlPacket::UnsubAck(ack) => {
                if let Some(pending) = self.unsubscribes.remove(&ack.packet_id) {
                    // MQTT 3.x UNSUBACKs carry no reason codes
                    let reasons = ack
                        .payload
                        .into_iter()
                        .map(|p| p.reason_code)
                        .chain(std::iter::repeat(ReasonCode::Success));
                    let results = pending
                        .topics
                        .into_iter()
                        .zip(reasons)
                        .map(|(topic, reason)| UnsubscribeResult {
                            topic,
                            success: reason.get_code() < 0x80,
                            reason,
                        })
                        .collect();
                    let _ = pending.response.send(Ok(results));
                }
            }
            ControlPacket::Auth(auth) => self.handle_auth(auth).await?,
            ControlPacket::PingResp(_) => debug!("Received PINGRESP"),
            other => debug!("Received packet: {:?}", other),
        }
        Ok(())
    }

    /// Fills in the topic of a PUBLISH that uses an alias, and remembers
    /// aliases the broker sets [MQTT-3.3.2-12].
    fn resolve_topic_alias(&mut self, publish: &mut PublishPacket) -> Result<()> {
        let Some(alias) = publish
            .properties
            .as_ref()
            .and_then(|p| p.topic_alias.as_ref())
            .map(|a| a.value)
        else {
            return Ok(());
        };
        if alias == 0 || alias > self.topic_alias_maximum {
            return Err(ClientError::BrokerViolation(ReasonCode::TopicAliasInvalid));
        }
        if publish.topic_name.is_empty() {
            publish.topic_name = self
                .topic_aliases
                .get(&alias)
                .cloned()
                .ok_or(ClientError::BrokerViolation(ReasonCode::ProtocolError))?;
        } else {
            self.topic_aliases.insert(alias, publish.topic_name.clone());
        }
        Ok(())
    }

    /// Acknowledges a message the application is done with, and any later
    /// ones it acknowledged before: acknowledgements go out in the order
    /// the messages came [MQTT-4.6.0-2, MQTT-4.6.0-3]. A token from an
    /// earlier connection, or used already, matches nothing.
    async fn acknowledge(&mut self, token: AckToken) {
        let Some(entry) = self.unacknowledged.iter_mut().find(|u| u.token == token) else {
            return;
        };
        entry.acknowledged = true;
        while self.unacknowledged.front().is_some_and(|u| u.acknowledged) {
            let entry = self.unacknowledged.pop_front().expect("checked above");
            // A failed write ends the connection, and the broker sends the
            // messages again
            if self
                .send_acknowledgement(entry.token.packet_id, entry.qos)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// PUBACK for QoS 1; PUBREC for QoS 2, which then awaits PUBREL.
    async fn send_acknowledgement(&mut self, packet_id: u16, qos: QoS) -> Result<()> {
        let packet = if qos == QoS::ExactlyOnce {
            self.incoming_qos2.insert(packet_id);
            ControlPacket::PubRec(PubRecPacket {
                packet_id,
                reason: ReasonCode::Success,
                properties: None,
            })
        } else {
            ControlPacket::PubAck(PubAckPacket {
                packet_id,
                reason: ReasonCode::Success,
                properties: None,
            })
        };
        self.connection.write_packet(packet).await
    }

    /// QoS 1 and 2 messages the broker has sent and this client hasn't
    /// fully acknowledged.
    fn incoming_in_flight(&self) -> usize {
        self.unacknowledged.len() + self.incoming_qos2.len()
    }

    async fn handle_publish(&mut self, mut publish: PublishPacket) -> Result<()> {
        self.resolve_topic_alias(&mut publish)?;
        debug!("Received PUBLISH on topic: {}", publish.topic_name);

        let mut ack = None;
        let deliver = match (publish.qos_level, publish.packet_id) {
            (QoS::AtLeastOnce | QoS::ExactlyOnce, Some(packet_id))
                if self
                    .unacknowledged
                    .iter()
                    .any(|u| u.token.packet_id == packet_id) =>
            {
                // Sent again before the application acknowledged it
                false
            }
            (QoS::ExactlyOnce, Some(packet_id)) if self.incoming_qos2.contains(&packet_id) => {
                // Delivered once already; only the PUBREC went missing
                self.connection
                    .write_packet(ControlPacket::PubRec(PubRecPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    }))
                    .await?;
                false
            }
            (qos @ (QoS::AtLeastOnce | QoS::ExactlyOnce), Some(packet_id)) => {
                // The broker must stay within this client's Receive Maximum
                // [MQTT-3.3.4-9]; QoS 1 messages count while unacknowledged
                let counted = self.manual_acks || qos == QoS::ExactlyOnce;
                if counted && self.incoming_in_flight() >= usize::from(self.receive_maximum) {
                    return Err(ClientError::BrokerViolation(
                        ReasonCode::ReceiveMaximumExceeded,
                    ));
                }
                if self.manual_acks {
                    self.next_delivery += 1;
                    let token = AckToken {
                        delivery: self.next_delivery,
                        packet_id,
                    };
                    self.unacknowledged.push_back(Unacknowledged {
                        token,
                        qos,
                        acknowledged: false,
                    });
                    ack = Some(token);
                } else {
                    self.send_acknowledgement(packet_id, qos).await?;
                }
                true
            }
            _ => true,
        };

        if deliver {
            let subscription_ids = publish
                .properties
                .as_ref()
                .map(|p| {
                    p.subscription_identifiers
                        .iter()
                        .map(|s| s.value.0)
                        .collect()
                })
                .unwrap_or_default();
            self.deliver(Event::Message {
                topic: publish.topic_name,
                payload: publish.payload.unwrap_or_default(),
                qos: publish.qos_level,
                retain: publish.retain,
                properties: MessageProperties::from_publish(publish.properties),
                subscription_ids,
                ack,
            });
        }
        Ok(())
    }
}
