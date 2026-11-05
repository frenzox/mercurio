use std::collections::{HashMap, HashSet};
use std::time::Duration;

use bytes::Bytes;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_core::qos::QoS;
use mercurio_core::reason::ReasonCode;
use mercurio_packets::connect::{ConnectFlags, ConnectPacket, ConnectPayload, WillProperties};
use mercurio_packets::disconnect::DisconnectPacket;
use mercurio_packets::pingreq::PingReqPacket;
use mercurio_packets::puback::PubAckPacket;
use mercurio_packets::pubcomp::PubCompPacket;
use mercurio_packets::publish::PublishPacket;
use mercurio_packets::pubrec::PubRecPacket;
use mercurio_packets::pubrel::PubRelPacket;
use mercurio_packets::subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions};
use mercurio_packets::unsubscribe::{UnsubscribePacket, UnsubscribePayload};
use mercurio_packets::ControlPacket;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{self, timeout};
use tracing::{debug, error, info, warn};

use crate::connection::Connection;
use crate::error::{ClientError, Result};
use crate::event::{DisconnectReason, Event, SubscribeResult};
use crate::options::ConnectOptions;
use crate::tls::build_tls_connector;

/// Command sent to the client event loop.
enum Command {
    Publish {
        topic: String,
        payload: Bytes,
        qos: QoS,
        retain: bool,
        response: oneshot::Sender<Result<()>>,
    },
    Subscribe {
        topics: Vec<(String, QoS)>,
        response: oneshot::Sender<Result<Vec<SubscribeResult>>>,
    },
    Unsubscribe {
        topics: Vec<String>,
        response: oneshot::Sender<Result<()>>,
    },
    Disconnect {
        response: oneshot::Sender<Result<()>>,
    },
}

/// An MQTT client for connecting to brokers.
pub struct MqttClient {
    command_tx: mpsc::Sender<Command>,
    event_rx: Mutex<mpsc::Receiver<Event>>,
    #[allow(dead_code)]
    client_id: String,
}

impl MqttClient {
    /// Connect to an MQTT broker with the given options.
    pub async fn connect(options: ConnectOptions) -> Result<Self> {
        let addr = format!("{}:{}", options.host, options.port);
        info!("Connecting to MQTT broker at {}", addr);

        // Connect with timeout
        let stream = timeout(
            Duration::from_secs(options.connect_timeout_secs),
            TcpStream::connect(&addr),
        )
        .await
        .map_err(|_| ClientError::Timeout)?
        .map_err(|e| ClientError::ConnectionFailed(e.to_string()))?;

        // Create channels for communication
        let (command_tx, command_rx) = mpsc::channel(32);
        let (event_tx, event_rx) = mpsc::channel(256);

        if options.tls.enabled {
            // TLS connection
            info!("Establishing TLS connection");
            let (connector, server_name) = build_tls_connector(&options.tls, &options.host)?;

            let tls_stream = timeout(
                Duration::from_secs(options.connect_timeout_secs),
                connector.connect(server_name, stream),
            )
            .await
            .map_err(|_| ClientError::Timeout)?
            .map_err(|e| ClientError::Tls(format!("TLS handshake failed: {}", e)))?;

            let mut connection = Connection::new_tls(tls_stream);
            connection.set_protocol_version(options.protocol_version);

            let (client_id, keep_alive) = Self::perform_connect(&mut connection, &options).await?;

            tokio::spawn(async move {
                if let Err(e) = Self::event_loop(
                    connection,
                    command_rx,
                    event_tx,
                    keep_alive,
                    options.protocol_version,
                )
                .await
                {
                    error!("Client event loop error: {}", e);
                }
            });

            Ok(MqttClient {
                command_tx,
                event_rx: Mutex::new(event_rx),
                client_id,
            })
        } else {
            // Plain TCP connection
            let mut connection = Connection::new(stream);
            connection.set_protocol_version(options.protocol_version);

            let (client_id, keep_alive) = Self::perform_connect(&mut connection, &options).await?;

            tokio::spawn(async move {
                if let Err(e) = Self::event_loop(
                    connection,
                    command_rx,
                    event_tx,
                    keep_alive,
                    options.protocol_version,
                )
                .await
                {
                    error!("Client event loop error: {}", e);
                }
            });

            Ok(MqttClient {
                command_tx,
                event_rx: Mutex::new(event_rx),
                client_id,
            })
        }
    }

    /// Perform the MQTT CONNECT handshake.
    async fn perform_connect<S>(
        connection: &mut Connection<S>,
        options: &ConnectOptions,
    ) -> Result<(String, u16)>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        // Build CONNECT packet
        let connect_packet = Self::build_connect_packet(options);
        connection
            .write_packet(ControlPacket::Connect(connect_packet))
            .await?;

        // Wait for CONNACK with timeout
        let connack = timeout(
            Duration::from_secs(options.connect_timeout_secs),
            connection.read_packet(),
        )
        .await
        .map_err(|_| ClientError::Timeout)??
        .ok_or_else(|| ClientError::ConnectionFailed("Connection closed".into()))?;

        let (session_present, client_id) = match connack {
            ControlPacket::ConnAck(ack) => {
                if ack.reason_code != ReasonCode::Success {
                    return Err(ClientError::ConnectionRefused(ack.reason_code));
                }
                let assigned_id = ack
                    .properties
                    .as_ref()
                    .and_then(|p| p.assigned_client_id.as_ref())
                    .map(|id| id.value.clone());
                let client_id =
                    assigned_id.unwrap_or_else(|| options.client_id.clone().unwrap_or_default());
                (ack.flags.session_present, client_id)
            }
            _ => return Err(ClientError::Protocol("Expected CONNACK packet".to_string())),
        };

        info!(
            "Connected to MQTT broker, client_id: {}, session_present: {}",
            client_id, session_present
        );

        Ok((client_id, options.keep_alive))
    }

    /// Build a CONNECT packet from options.
    fn build_connect_packet(options: &ConnectOptions) -> ConnectPacket {
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
            will_properties: options.will.as_ref().map(|_| WillProperties::default()),
            will_topic: options.will.as_ref().map(|w| w.topic.clone()),
            will_payload: options.will.as_ref().map(|w| w.payload.clone()),
            user_name: options.username.clone(),
            password: options.password.clone(),
        };

        ConnectPacket {
            protocol_version: options.protocol_version,
            flags,
            keepalive: options.keep_alive,
            properties: None,
            payload,
        }
    }

    /// The main event loop that handles incoming packets and outgoing commands.
    async fn event_loop<S>(
        connection: Connection<S>,
        mut command_rx: mpsc::Receiver<Command>,
        event_tx: mpsc::Sender<Event>,
        keep_alive: u16,
        protocol_version: ProtocolVersion,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut state = EventLoop {
            connection,
            event_tx,
            reports_reasons: protocol_version.supports_reason_codes(),
            last_packet_id: 0,
            outgoing: HashMap::new(),
            incoming_qos2: HashSet::new(),
            subscribes: HashMap::new(),
            unsubscribes: HashMap::new(),
        };

        // Keep-alive interval (send PINGREQ at half the keep-alive time)
        let ping_interval = if keep_alive > 0 {
            Duration::from_secs((keep_alive / 2).max(1) as u64)
        } else {
            Duration::from_secs(u64::MAX / 2) // Effectively disabled
        };
        let mut ping_timer = time::interval(ping_interval);
        ping_timer.tick().await; // Skip the first immediate tick

        loop {
            tokio::select! {
                // Handle incoming packets
                maybe_packet = state.connection.read_packet() => {
                    match maybe_packet {
                        Ok(Some(packet)) => {
                            if let Err(e) = state.handle_incoming(packet).await {
                                warn!("Error handling packet: {}", e);
                            }
                        }
                        Ok(None) => {
                            // Connection closed
                            info!("Connection closed by server");
                            let _ = state.event_tx.send(Event::Disconnected {
                                reason: DisconnectReason::ServerInitiated,
                            }).await;
                            break;
                        }
                        Err(e) => {
                            error!("Error reading packet: {}", e);
                            let _ = state.event_tx.send(Event::Disconnected {
                                reason: DisconnectReason::ConnectionLost,
                            }).await;
                            break;
                        }
                    }
                }

                // Handle commands from the client API
                Some(cmd) = command_rx.recv() => {
                    match cmd {
                        Command::Publish { topic, payload, qos, retain, response } => {
                            state.publish(topic, payload, qos, retain, response).await;
                        }
                        Command::Subscribe { topics, response } => {
                            state.subscribe(topics, response).await;
                        }
                        Command::Unsubscribe { topics, response } => {
                            state.unsubscribe(topics, response).await;
                        }
                        Command::Disconnect { response } => {
                            let result = Self::handle_disconnect(&mut state.connection).await;
                            let _ = response.send(result);
                            let _ = state.event_tx.send(Event::Disconnected {
                                reason: DisconnectReason::ClientInitiated,
                            }).await;
                            break;
                        }
                    }
                }

                // Send PINGREQ for keep-alive
                _ = ping_timer.tick() => {
                    debug!("Sending PINGREQ");
                    if let Err(e) = state.connection.write_packet(
                        ControlPacket::PingReq(PingReqPacket {})
                    ).await {
                        error!("Failed to send PINGREQ: {}", e);
                        let _ = state.event_tx.send(Event::Disconnected {
                            reason: DisconnectReason::KeepAliveTimeout,
                        }).await;
                        break;
                    }
                }
            }
        }

        // Dropping the state fails every request still waiting for the broker
        Ok(())
    }

    /// Handle a disconnect command.
    async fn handle_disconnect<S>(connection: &mut Connection<S>) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
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
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Publish {
                topic: topic.to_string(),
                payload: payload.into(),
                qos,
                retain,
                response: response_tx,
            })
            .await
            .map_err(|_| ClientError::SendError)?;

        response_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// Subscribe to one or more topics.
    pub async fn subscribe(&self, topics: &[(&str, QoS)]) -> Result<Vec<SubscribeResult>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Subscribe {
                topics: topics.iter().map(|(t, q)| (t.to_string(), *q)).collect(),
                response: response_tx,
            })
            .await
            .map_err(|_| ClientError::SendError)?;

        response_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// Unsubscribe from one or more topics.
    pub async fn unsubscribe(&self, topics: &[&str]) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Unsubscribe {
                topics: topics.iter().map(|t| t.to_string()).collect(),
                response: response_tx,
            })
            .await
            .map_err(|_| ClientError::SendError)?;

        response_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// Disconnect from the broker gracefully.
    pub async fn disconnect(&self) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.command_tx
            .send(Command::Disconnect {
                response: response_tx,
            })
            .await
            .map_err(|_| ClientError::SendError)?;

        response_rx.await.map_err(|_| ClientError::Disconnected)?
    }

    /// Receive the next event from the broker.
    /// Returns None if the client is disconnected.
    pub async fn recv(&self) -> Option<Event> {
        let mut rx = self.event_rx.lock().await;
        rx.recv().await
    }
}

/// A QoS 1 or 2 message sent to the broker and not acknowledged yet.
struct OutgoingPublish {
    response: oneshot::Sender<Result<()>>,
}

/// The state of one connection to the broker.
struct EventLoop<S: AsyncRead + AsyncWrite + Unpin> {
    connection: Connection<S>,
    event_tx: mpsc::Sender<Event>,
    /// MQTT 5 acknowledgements carry reason codes, MQTT 3.x ones don't
    reports_reasons: bool,
    last_packet_id: u16,
    /// QoS 1 and 2 messages awaiting PUBACK or PUBCOMP
    outgoing: HashMap<u16, OutgoingPublish>,
    /// QoS 2 messages received and awaiting PUBREL
    incoming_qos2: HashSet<u16>,
    subscribes: HashMap<u16, PendingSubscribe>,
    unsubscribes: HashMap<u16, oneshot::Sender<Result<()>>>,
}

struct PendingSubscribe {
    topics: Vec<(String, QoS)>,
    response: oneshot::Sender<Result<Vec<SubscribeResult>>>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> EventLoop<S> {
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
    /// the broker has acknowledged them.
    async fn publish(
        &mut self,
        topic: String,
        payload: Bytes,
        qos: QoS,
        retain: bool,
        response: oneshot::Sender<Result<()>>,
    ) {
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
            properties: None,
            payload: Some(payload),
        };
        if let Err(e) = self
            .connection
            .write_packet(ControlPacket::Publish(publish))
            .await
        {
            let _ = response.send(Err(e));
            return;
        }

        match packet_id {
            Some(packet_id) => {
                self.outgoing
                    .insert(packet_id, OutgoingPublish { response });
            }
            None => {
                let _ = response.send(Ok(()));
            }
        }
    }

    async fn subscribe(
        &mut self,
        topics: Vec<(String, QoS)>,
        response: oneshot::Sender<Result<Vec<SubscribeResult>>>,
    ) {
        let Some(packet_id) = self.next_packet_id() else {
            let _ = response.send(Err(ClientError::Protocol(
                "no free packet identifier".into(),
            )));
            return;
        };

        let payload = topics
            .iter()
            .map(|(topic, qos)| SubscribePayload {
                topic_filter: topic.clone(),
                subs_opt: SubscriptionOptions {
                    qos: *qos,
                    ..Default::default()
                },
            })
            .collect();
        let subscribe = SubscribePacket {
            packet_id,
            properties: None,
            payload,
        };
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

    async fn unsubscribe(&mut self, topics: Vec<String>, response: oneshot::Sender<Result<()>>) {
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
                .into_iter()
                .map(|topic_filter| UnsubscribePayload { topic_filter })
                .collect(),
        };
        match self
            .connection
            .write_packet(ControlPacket::Unsubscribe(unsubscribe))
            .await
        {
            Ok(()) => {
                self.unsubscribes.insert(packet_id, response);
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
        match packet {
            ControlPacket::Publish(publish) => self.handle_publish(publish).await?,
            ControlPacket::PubAck(ack) => self.complete_publish(ack.packet_id, ack.reason),
            ControlPacket::PubRec(rec) => {
                // A failure ends the exchange; otherwise release the message
                if rec.reason.get_code() >= 0x80 {
                    self.complete_publish(rec.packet_id, rec.reason);
                } else {
                    let reason = if self.outgoing.contains_key(&rec.packet_id) {
                        ReasonCode::Success
                    } else {
                        self.unknown_packet_id()
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
                            }
                        })
                        .collect();
                    let _ = pending.response.send(Ok(results));
                }
            }
            ControlPacket::UnsubAck(ack) => {
                if let Some(response) = self.unsubscribes.remove(&ack.packet_id) {
                    let _ = response.send(Ok(()));
                }
            }
            ControlPacket::PingResp(_) => debug!("Received PINGRESP"),
            ControlPacket::Disconnect(_) => {
                info!("Received DISCONNECT from server");
                let _ = self
                    .event_tx
                    .send(Event::Disconnected {
                        reason: DisconnectReason::ServerInitiated,
                    })
                    .await;
            }
            other => debug!("Received packet: {:?}", other),
        }
        Ok(())
    }

    async fn handle_publish(&mut self, publish: PublishPacket) -> Result<()> {
        debug!("Received PUBLISH on topic: {}", publish.topic_name);

        let deliver = match (publish.qos_level, publish.packet_id) {
            (QoS::AtLeastOnce, Some(packet_id)) => {
                self.connection
                    .write_packet(ControlPacket::PubAck(PubAckPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    }))
                    .await?;
                true
            }
            (QoS::ExactlyOnce, Some(packet_id)) => {
                // A retransmission before PUBREL was already delivered once
                let first_delivery = self.incoming_qos2.insert(packet_id);
                self.connection
                    .write_packet(ControlPacket::PubRec(PubRecPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    }))
                    .await?;
                first_delivery
            }
            _ => true,
        };

        if deliver {
            let _ = self
                .event_tx
                .send(Event::Message {
                    topic: publish.topic_name,
                    payload: publish.payload.unwrap_or_default(),
                    qos: publish.qos_level,
                    retain: publish.retain,
                })
                .await;
        }
        Ok(())
    }
}
