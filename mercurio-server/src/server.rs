use std::{future::Future, sync::Arc};

use tokio::{
    net::{TcpListener, TcpStream},
    sync::broadcast,
    time::{self, Duration},
};
use tracing::{error, info};

use mercurio_core::{message::Message, Result};
use mercurio_packets::{connect::ConnectPacket, ControlPacket};

use crate::{
    broker::Broker,
    connection::Connection,
    session::Session,
    session_manager::{SessionManager, SessionManagerDropGuard},
    shutdown::Shutdown,
};

struct Listener {
    listener: TcpListener,
    broker: Broker,
    session_manager_holder: SessionManagerDropGuard,
    notify_shutdown: broadcast::Sender<()>,
}

struct Handler {
    broker: Broker,
    session_manager: SessionManager,
    connection: Connection,
    shutdown: Shutdown,
}

pub async fn run(listener: TcpListener, shutdown: impl Future) {
    let (notify_shutdown, _) = broadcast::channel(1);

    let mut server = Listener {
        listener,
        broker: Broker::new(),
        session_manager_holder: SessionManagerDropGuard::new(),
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
        let session = self
            .session_manager
            .start_session(&mut self.connection, connect_packet)
            .await?;

        let result = self.handle_connection(&session).await;

        // Publish will message on abnormal disconnect
        // (clean disconnect clears the will, so take_will returns None)
        if let Some(will) = session.take_will().await {
            self.publish_will(will, &session).await;
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
                            // Clean disconnect - clear the will
                            session.clear_will().await;
                            return Ok(());
                        }
                        Some(packet) => packet,
                    };

                    let maybe_res = session
                        .process_incoming(
                            packet,
                            &self.broker,
                        ).await?;

                    if let Some(res) = maybe_res {
                        tracing::debug!("Sending response packet:{:#?} to client {:?}", res, session.get_client_id().await);
                        self.connection.write_packet(res).await?;
                    }
                }

                // Try to send outgoing packet
                Some(packet) = session.process_outgoing() => {
                    tracing::debug!("Sending outgoing packet: {:#?} to client {:?}", packet, session.get_client_id().await);

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

        if let Err(e) = self.broker.publish(&topic, message) {
            error!("Failed to publish will message: {}", e);
        }
    }
}
