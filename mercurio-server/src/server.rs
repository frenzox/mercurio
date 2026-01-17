use std::{future::Future, sync::Arc};

use tokio::{
    net::{TcpListener, TcpStream},
    sync::broadcast,
    time::{self, Duration},
};
use tracing::{error, info};

use mercurio_core::{message::Message, qos::QoS, Result};
use mercurio_packets::{connect::ConnectPacket, publish::PublishPacket, ControlPacket};
use mercurio_storage::{memory::MemoryStore, InflightMessage};

use crate::{
    broker::Broker,
    connection::Connection,
    session::Session,
    session_manager::{SessionManager, SessionManagerDropGuard},
    shutdown::Shutdown,
};

struct Listener {
    listener: TcpListener,
    broker: Broker<MemoryStore>,
    session_manager_holder: SessionManagerDropGuard<MemoryStore>,
    notify_shutdown: broadcast::Sender<()>,
}

struct Handler {
    broker: Broker<MemoryStore>,
    session_manager: SessionManager<MemoryStore>,
    connection: Connection,
    shutdown: Shutdown,
}

pub async fn run(listener: TcpListener, shutdown: impl Future) {
    let (notify_shutdown, _) = broadcast::channel(1);

    // Create shared storage instance for both broker and session manager
    let storage = Arc::new(MemoryStore::new());

    let mut server = Listener {
        listener,
        broker: Broker::new(Arc::clone(&storage)),
        session_manager_holder: SessionManagerDropGuard::new(storage),
        notify_shutdown,
    };

    tokio::select! {
        result = server.run() => {
            if result.is_err() {
                error!("Failed to accept new connection");
            }
        }
        _ = shutdown => {
            info!("Shutting down!");
        }
    }
}

impl Listener {
    async fn run(&mut self) -> Result<()> {
        loop {
            let socket = self.accept().await?;

            info!("Got a connection: {:#?}", socket.peer_addr());

            let mut handler = Handler {
                broker: self.broker.clone(),
                session_manager: self.session_manager_holder.session_manager(),
                connection: Connection::new(socket),
                shutdown: Shutdown::new(self.notify_shutdown.subscribe()),
            };

            tokio::spawn(async move {
                match handler.connection.read_packet().await {
                    // [MQTT-3.1.0-1]
                    // After a Network Connection is established by a Client
                    // to a Server, the first packet sent from the Client to
                    // the Server MUST be a CONNECT packet.
                    Ok(Some(ControlPacket::Connect(p))) => {
                        if let Err(err) = handler.run(p).await {
                            error!(cause = ?err, "Connection error");
                        }
                    }
                    _ => error!("ConnectPacket expectation not met"),
                }
            });
        }
    }

    async fn accept(&mut self) -> Result<TcpStream> {
        let mut backoff = 1;

        loop {
            match self.listener.accept().await {
                Ok((socket, _)) => return Ok(socket),
                Err(err) => {
                    if backoff > 64 {
                        return Err(err.into());
                    }
                }
            }

            time::sleep(Duration::from_secs(backoff)).await;

            backoff *= 2;
        }
    }
}

impl Handler {
    async fn run(&mut self, connect_packet: ConnectPacket) -> Result<()> {
        let start_result = self
            .session_manager
            .start_session(&mut self.connection, connect_packet)
            .await?;

        let mut session = start_result.session;

        // Restore subscriptions if resuming a persisted session
        if !start_result.subscriptions_to_restore.is_empty() {
            session
                .restore_subscriptions(start_result.subscriptions_to_restore, &self.broker)
                .await?;
        }

        // Restore will message if resuming a persisted session
        if let Some(will) = start_result.will_to_restore {
            session.set_will(will).await;
        }

        // Retry inflight messages (QoS 1/2 awaiting acknowledgment)
        for inflight in start_result.inflight_to_retry {
            let publish = PublishPacket {
                dup: true, // Mark as duplicate since this is a retry
                qos_level: inflight.qos,
                retain: false,
                topic_name: inflight.topic,
                packet_id: Some(inflight.packet_id),
                properties: None,
                payload: inflight.payload,
            };
            if let Err(e) = self
                .connection
                .write_packet(ControlPacket::Publish(publish))
                .await
            {
                error!("Failed to retry inflight message: {}", e);
            }
        }

        let result = self.handle_connection(&session).await;

        // Publish will message on abnormal disconnect
        // (clean disconnect clears the will, so take_will returns None)
        if let Some(will) = session.take_will().await {
            self.publish_will(will, &session).await;
            // Delete persisted will after publishing
            let client_id = session.get_client_id().await;
            let _ = self.session_manager.delete_will(&client_id).await;
        }

        result
    }

    async fn handle_connection(&mut self, session: &Session) -> Result<()> {
        let mut session = session.clone();

        while !self.shutdown.is_shutdown() {
            tokio::select! {
                // Try to read and process new incoming packet
                maybe_packet = self.connection.read_packet() => {
                    let packet = match maybe_packet? {
                        None => {
                            // Connection closed without DISCONNECT - abnormal
                            return Ok(());
                        }
                        Some(ControlPacket::Disconnect(_)) => {
                            // Clean disconnect - clear the will (in-memory and storage)
                            session.clear_will().await;
                            let client_id = session.get_client_id().await;
                            let _ = self.session_manager.delete_will(&client_id).await;
                            return Ok(());
                        }
                        Some(packet) => packet,
                    };

                    let is_subscription_change = matches!(
                        packet,
                        ControlPacket::Subscribe(_) | ControlPacket::Unsubscribe(_)
                    );

                    // Check if this is an ack that completes an inflight message
                    let inflight_completed = match &packet {
                        ControlPacket::PubAck(ack) => Some(ack.packet_id),
                        ControlPacket::PubComp(comp) => Some(comp.packet_id),
                        _ => None,
                    };

                    let maybe_res = session
                        .process_incoming(
                            packet,
                            &self.broker,
                            None, // TODO: Add auth_manager when configurable auth is implemented
                        ).await?;

                    // Persist subscriptions after subscribe/unsubscribe operations
                    if is_subscription_change {
                        let client_id = session.get_client_id().await;
                        let subscriptions = session.get_subscription_filters().await;
                        let _ = self
                            .session_manager
                            .save_subscriptions(&client_id, subscriptions)
                            .await;
                    }

                    // Remove inflight message after acknowledgment
                    if let Some(packet_id) = inflight_completed {
                        let client_id = session.get_client_id().await;
                        let _ = self
                            .session_manager
                            .remove_inflight(&client_id, packet_id)
                            .await;
                    }

                    if let Some(res) = maybe_res {
                        tracing::debug!("Sending response packet:{:#?} to client {:?}", res, session.get_client_id().await);
                        self.connection.write_packet(res).await?;
                    }
                }

                // Try to send outgoing packet
                Some(packet) = session.process_outgoing() => {
                    tracing::debug!("Sending outgoing packet: {:#?} to client {:?}", packet, session.get_client_id().await);

                    // Store inflight for QoS 1/2 messages
                    if let ControlPacket::Publish(ref publish) = packet {
                        if let Some(packet_id) = publish.packet_id {
                            if matches!(publish.qos_level, QoS::AtLeastOnce | QoS::ExactlyOnce) {
                                let client_id = session.get_client_id().await;
                                let inflight = InflightMessage {
                                    packet_id,
                                    topic: publish.topic_name.clone(),
                                    payload: publish.payload.clone(),
                                    qos: publish.qos_level,
                                    dup: publish.dup,
                                };
                                let _ = self
                                    .session_manager
                                    .store_inflight(&client_id, packet_id, &inflight)
                                    .await;
                            }
                        }
                    }

                    self.connection.write_packet(packet).await?;
                }

                // Exit in case a signal is received
                _ = self.shutdown.recv() => {
                    return Ok(());
                },
            }
        }

        Ok(())
    }

    async fn publish_will(&self, will: crate::session::WillMessage, session: &Session) {
        let client_id = session.get_client_id().await;
        info!(
            "Publishing will message for client `{}` on topic `{}`",
            client_id, will.topic
        );

        let topic: Arc<str> = Arc::from(will.topic.as_str());
        let message = Message {
            packet_id: None, // Will messages don't have packet IDs
            topic: Arc::clone(&topic),
            dup: false,
            qos: will.qos,
            retain: will.retain,
            payload: Some(will.payload),
        };

        if let Err(e) = self.broker.publish(&topic, message).await {
            error!("Failed to publish will message: {}", e);
        }
    }
}
