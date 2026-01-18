use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mercurio_core::qos::QoS;
use mercurio_core::reason::ReasonCode;
use mercurio_packets::connect::{ConnectFlags, ConnectPacket, ConnectPayload, WillProperties};
use mercurio_packets::disconnect::DisconnectPacket;
use mercurio_packets::pingreq::PingReqPacket;
use mercurio_packets::publish::PublishPacket;
use mercurio_packets::subscribe::{SubscribePacket, SubscribePayload, SubscriptionOptions};
use mercurio_packets::unsubscribe::{UnsubscribePacket, UnsubscribePayload};
use mercurio_packets::ControlPacket;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::{self, timeout};
use tracing::{debug, error, info, warn};

use crate::connection::Connection;
use crate::error::{ClientError, Result};
use crate::event::{DisconnectReason, Event, SubscribeResult};
use crate::options::ConnectOptions;

/// Command sent to the client event loop.
enum Command {
    Publish {
        topic: String,
        payload: Bytes,
        qos: QoS,
        retain: bool,
        response: oneshot::Sender<Result<Option<u16>>>,
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

        let mut connection = Connection::new(stream);

        // Set the protocol version for version-aware packet parsing
        connection.set_protocol_version(options.protocol_version);

        // Build CONNECT packet
        let connect_packet = Self::build_connect_packet(&options);
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

        // Create channels for communication
        let (command_tx, command_rx) = mpsc::channel(32);
        let (event_tx, event_rx) = mpsc::channel(256);

        // Spawn the client event loop
        let keep_alive = options.keep_alive;
        tokio::spawn(async move {
            if let Err(e) = Self::event_loop(connection, command_rx, event_tx, keep_alive).await {
                error!("Client event loop error: {}", e);
            }
        });

        Ok(MqttClient {
            command_tx,
            event_rx: Mutex::new(event_rx),
            client_id,
        })
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
    async fn event_loop(
        mut connection: Connection,
        mut command_rx: mpsc::Receiver<Command>,
        event_tx: mpsc::Sender<Event>,
        keep_alive: u16,
    ) -> Result<()> {
        let packet_id_counter = Arc::new(AtomicU16::new(1));

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
                maybe_packet = connection.read_packet() => {
                    match maybe_packet {
                        Ok(Some(packet)) => {
                            if let Err(e) = Self::handle_incoming_packet(
                                packet,
                                &mut connection,
                                &event_tx,
                            ).await {
                                warn!("Error handling packet: {}", e);
                            }
                        }
                        Ok(None) => {
                            // Connection closed
                            info!("Connection closed by server");
                            let _ = event_tx.send(Event::Disconnected {
                                reason: DisconnectReason::ServerInitiated,
                            }).await;
                            break;
                        }
                        Err(e) => {
                            error!("Error reading packet: {}", e);
                            let _ = event_tx.send(Event::Disconnected {
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
                            let result = Self::handle_publish(
                                &mut connection,
                                &packet_id_counter,
                                topic,
                                payload,
                                qos,
                                retain,
                            ).await;
                            let _ = response.send(result);
                        }
                        Command::Subscribe { topics, response } => {
                            let result = Self::handle_subscribe(
                                &mut connection,
                                &packet_id_counter,
                                topics,
                            ).await;
                            let _ = response.send(result);
                        }
                        Command::Unsubscribe { topics, response } => {
                            let result = Self::handle_unsubscribe(
                                &mut connection,
                                &packet_id_counter,
                                topics,
                            ).await;
                            let _ = response.send(result);
                        }
                        Command::Disconnect { response } => {
                            let result = Self::handle_disconnect(&mut connection).await;
                            let _ = response.send(result);
                            let _ = event_tx.send(Event::Disconnected {
                                reason: DisconnectReason::ClientInitiated,
                            }).await;
                            break;
                        }
                    }
                }

                // Send PINGREQ for keep-alive
                _ = ping_timer.tick() => {
                    debug!("Sending PINGREQ");
                    if let Err(e) = connection.write_packet(
                        ControlPacket::PingReq(PingReqPacket {})
                    ).await {
                        error!("Failed to send PINGREQ: {}", e);
                        let _ = event_tx.send(Event::Disconnected {
                            reason: DisconnectReason::KeepAliveTimeout,
                        }).await;
                        break;
                    }
                }
            }
        }

        Ok(())
    }

    /// Handle an incoming packet from the broker.
    async fn handle_incoming_packet(
        packet: ControlPacket,
        connection: &mut Connection,
        event_tx: &mpsc::Sender<Event>,
    ) -> Result<()> {
        match packet {
            ControlPacket::Publish(publish) => {
                debug!("Received PUBLISH on topic: {}", publish.topic_name);

                // Send PUBACK for QoS 1
                if publish.qos_level == QoS::AtLeastOnce {
                    if let Some(packet_id) = publish.packet_id {
                        let puback = mercurio_packets::puback::PubAckPacket {
                            packet_id,
                            reason: ReasonCode::Success,
                            properties: None,
                        };
                        connection
                            .write_packet(ControlPacket::PubAck(puback))
                            .await?;
                    }
                }

                // TODO: Handle QoS 2 (PUBREC/PUBREL/PUBCOMP flow)

                // Send event to the application
                let _ = event_tx
                    .send(Event::Message {
                        topic: publish.topic_name,
                        payload: publish.payload.unwrap_or_default(),
                        qos: publish.qos_level,
                        retain: publish.retain,
                    })
                    .await;
            }
            ControlPacket::PingResp(_) => {
                debug!("Received PINGRESP");
            }
            ControlPacket::Disconnect(_) => {
                info!("Received DISCONNECT from server");
                let _ = event_tx
                    .send(Event::Disconnected {
                        reason: DisconnectReason::ServerInitiated,
                    })
                    .await;
            }
            ControlPacket::PubAck(ack) => {
                debug!("Received PUBACK for packet_id: {}", ack.packet_id);
                // TODO: Track inflight messages and mark as complete
            }
            _ => {
                debug!("Received packet: {:?}", packet);
            }
        }
        Ok(())
    }

    /// Handle a publish command.
    async fn handle_publish(
        connection: &mut Connection,
        packet_id_counter: &AtomicU16,
        topic: String,
        payload: Bytes,
        qos: QoS,
        retain: bool,
    ) -> Result<Option<u16>> {
        let packet_id = if qos != QoS::AtMostOnce {
            Some(packet_id_counter.fetch_add(1, Ordering::SeqCst))
        } else {
            None
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

        connection
            .write_packet(ControlPacket::Publish(publish))
            .await?;

        // TODO: For QoS 1/2, wait for acknowledgment
        Ok(packet_id)
    }

    /// Handle a subscribe command.
    async fn handle_subscribe(
        connection: &mut Connection,
        packet_id_counter: &AtomicU16,
        topics: Vec<(String, QoS)>,
    ) -> Result<Vec<SubscribeResult>> {
        let packet_id = packet_id_counter.fetch_add(1, Ordering::SeqCst);

        let payload: Vec<SubscribePayload> = topics
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

        connection
            .write_packet(ControlPacket::Subscribe(subscribe))
            .await?;

        // TODO: Wait for SUBACK and return actual results
        // For now, return optimistic results
        Ok(topics
            .into_iter()
            .map(|(topic, qos)| SubscribeResult {
                topic,
                qos,
                success: true,
            })
            .collect())
    }

    /// Handle an unsubscribe command.
    async fn handle_unsubscribe(
        connection: &mut Connection,
        packet_id_counter: &AtomicU16,
        topics: Vec<String>,
    ) -> Result<()> {
        let packet_id = packet_id_counter.fetch_add(1, Ordering::SeqCst);

        let payload: Vec<UnsubscribePayload> = topics
            .iter()
            .map(|topic| UnsubscribePayload {
                topic_filter: topic.clone(),
            })
            .collect();

        let unsubscribe = UnsubscribePacket {
            packet_id,
            properties: None,
            payload,
        };

        connection
            .write_packet(ControlPacket::Unsubscribe(unsubscribe))
            .await?;

        // TODO: Wait for UNSUBACK
        Ok(())
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

    /// Publish a message to a topic.
    pub async fn publish(
        &self,
        topic: &str,
        payload: impl Into<Bytes>,
        qos: QoS,
    ) -> Result<Option<u16>> {
        self.publish_with_retain(topic, payload, qos, false).await
    }

    /// Publish a message to a topic with retain flag.
    pub async fn publish_with_retain(
        &self,
        topic: &str,
        payload: impl Into<Bytes>,
        qos: QoS,
        retain: bool,
    ) -> Result<Option<u16>> {
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
