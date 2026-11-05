use std::{future::Future, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::broadcast,
    time::{self, Duration, Instant},
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use mercurio_core::{
    message::Message, protocol::ProtocolVersion, qos::QoS, reason::ReasonCode, Result,
};
use mercurio_packets::{
    connack::{ConnAckFlags, ConnAckPacket},
    connect::ConnectPacket,
    disconnect::DisconnectPacket,
    publish::PublishPacket,
    ControlPacket,
};
use mercurio_storage::{memory::MemoryStore, InflightMessage};

use crate::{
    auth::{AuthManager, CredentialValidator},
    broker::Broker,
    connection::Connection,
    session::Session,
    session_manager::{ConnectionLease, SessionManager, SessionManagerDropGuard},
    shutdown::Shutdown,
    tls::TlsConfig,
};

/// Authentication configuration for the server.
pub struct AuthConfig {
    /// Whether authentication is required.
    pub require_auth: bool,
    /// Optional credential validator for CONNECT username/password.
    pub credential_validator: Option<Arc<dyn CredentialValidator>>,
    /// Optional auth manager for MQTT 5.0 enhanced authentication.
    pub auth_manager: Option<Arc<AuthManager>>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            require_auth: false,
            credential_validator: None,
            auth_manager: None,
        }
    }
}

struct Listener {
    listener: TcpListener,
    broker: Broker<MemoryStore>,
    session_manager_holder: SessionManagerDropGuard<MemoryStore>,
    notify_shutdown: broadcast::Sender<()>,
    tls_acceptor: Option<TlsAcceptor>,
    auth_config: Arc<AuthConfig>,
}

struct Handler<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    broker: Broker<MemoryStore>,
    session_manager: SessionManager<MemoryStore>,
    connection: Connection<S>,
    shutdown: Shutdown,
    auth_config: Arc<AuthConfig>,
}

/// Run the MQTT broker with plain TCP connections.
pub async fn run(listener: TcpListener, shutdown: impl Future) {
    run_with_tls(listener, None, AuthConfig::default(), shutdown).await
}

/// Run the MQTT broker with optional TLS support and authentication.
pub async fn run_with_tls(
    listener: TcpListener,
    tls_config: Option<TlsConfig>,
    auth_config: AuthConfig,
    shutdown: impl Future,
) {
    let (notify_shutdown, _) = broadcast::channel(1);

    // Create shared storage instance for both broker and session manager
    let storage = Arc::new(MemoryStore::new());

    // Build TLS acceptor if configured
    let tls_acceptor = match tls_config {
        Some(config) => match config.build_acceptor() {
            Ok(acceptor) => {
                info!("TLS enabled");
                Some(acceptor)
            }
            Err(e) => {
                error!("Failed to configure TLS: {}", e);
                return;
            }
        },
        None => {
            debug!("TLS not configured, using plain TCP");
            None
        }
    };

    let auth_config = Arc::new(auth_config);

    let mut server = Listener {
        listener,
        broker: Broker::new(Arc::clone(&storage)),
        session_manager_holder: SessionManagerDropGuard::new(storage),
        notify_shutdown,
        tls_acceptor,
        auth_config,
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
            let peer_addr = socket.peer_addr().ok();

            info!("Got a connection: {:?}", peer_addr);

            let broker = self.broker.clone();
            let session_manager = self.session_manager_holder.session_manager();
            let shutdown = Shutdown::new(self.notify_shutdown.subscribe());
            let auth_config = Arc::clone(&self.auth_config);

            if let Some(ref tls_acceptor) = self.tls_acceptor {
                // TLS connection
                let acceptor = tls_acceptor.clone();
                tokio::spawn(async move {
                    match acceptor.accept(socket).await {
                        Ok(tls_stream) => {
                            debug!("TLS handshake completed for {:?}", peer_addr);
                            let mut handler = Handler {
                                broker,
                                session_manager,
                                connection: Connection::new_tls(tls_stream),
                                shutdown,
                                auth_config,
                            };
                            Self::handle_connection(&mut handler).await;
                        }
                        Err(e) => {
                            error!("TLS handshake failed for {:?}: {}", peer_addr, e);
                        }
                    }
                });
            } else {
                // Plain TCP connection
                tokio::spawn(async move {
                    let mut handler = Handler {
                        broker,
                        session_manager,
                        connection: Connection::new(socket),
                        shutdown,
                        auth_config,
                    };
                    Self::handle_connection(&mut handler).await;
                });
            }
        }
    }

    async fn handle_connection<S>(handler: &mut Handler<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
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

/// How long a session outlives its connection; `None` means forever.
fn session_expiry(connect_packet: &ConnectPacket) -> Option<Duration> {
    if !connect_packet.protocol_version.supports_properties() {
        // MQTT 3.x: a clean session ends with the connection, others persist
        return connect_packet.flags.clean_start.then_some(Duration::ZERO);
    }

    let interval = connect_packet
        .properties
        .as_ref()
        .and_then(|p| p.session_expiry_interval.as_ref())
        .map_or(0, |p| p.value);

    (interval != u32::MAX).then(|| Duration::from_secs(interval.into()))
}

impl<S> Handler<S>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn run(&mut self, connect_packet: ConnectPacket) -> Result<()> {
        self.connection
            .set_protocol_version(connect_packet.protocol_version);

        // Validate credentials if authentication is configured
        if self.auth_config.require_auth {
            let authenticated = match (
                &connect_packet.payload.user_name,
                &connect_packet.payload.password,
            ) {
                (Some(username), Some(password)) => {
                    if let Some(ref validator) = self.auth_config.credential_validator {
                        let valid = validator.validate(username, password);
                        if !valid {
                            warn!("Authentication failed for user `{}`", username);
                        }
                        valid
                    } else {
                        // Auth required but no validator configured — reject
                        false
                    }
                }
                _ => {
                    warn!("Authentication required but no credentials provided");
                    false
                }
            };

            if !authenticated {
                return self
                    .reject_connect(&connect_packet, ReasonCode::BadUserNameOrPassword)
                    .await;
            }
        }

        // MQTT 3.x servers must reject a zero-length client id unless the
        // session is clean, as there would be no way to resume it [MQTT-3.1.3-8]
        if connect_packet.payload.client_id.is_empty()
            && !connect_packet.flags.clean_start
            && !connect_packet.protocol_version.supports_properties()
        {
            return self
                .reject_connect(&connect_packet, ReasonCode::ClientIdentifierNotValid)
                .await;
        }

        let keepalive = connect_packet.keepalive;
        let protocol_version = connect_packet.protocol_version;
        let mut expiry = session_expiry(&connect_packet);

        let start_result = self.session_manager.start_session(connect_packet).await?;
        let mut session = start_result.session;
        let mut lease = start_result.lease;

        let result = async {
            session
                .begin(
                    &mut self.connection,
                    start_result.resume,
                    start_result.assigned_client_id,
                )
                .await?;

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

            self.handle_connection(
                &session,
                keepalive,
                &mut lease,
                protocol_version,
                &mut expiry,
            )
            .await
        }
        .await;

        // Publish will message on abnormal disconnect
        // (clean disconnect clears the will, so take_will returns None)
        if let Some(will) = session.take_will().await {
            self.publish_will(will, &session).await;
            // Delete persisted will after publishing
            let client_id = session.get_client_id().await;
            let _ = self.session_manager.delete_will(&client_id).await;
        }

        self.session_manager.end_connection(lease, expiry).await;

        result
    }

    async fn reject_connect(
        &mut self,
        connect_packet: &ConnectPacket,
        reason_code: ReasonCode,
    ) -> Result<()> {
        let connack = ConnAckPacket {
            protocol_version: connect_packet.protocol_version,
            flags: ConnAckFlags {
                session_present: false,
            },
            reason_code,
            properties: None,
        };
        let _ = self
            .connection
            .write_packet(ControlPacket::ConnAck(connack))
            .await;
        Ok(())
    }

    async fn handle_connection(
        &mut self,
        session: &Session,
        keepalive: u16,
        lease: &mut ConnectionLease,
        protocol_version: ProtocolVersion,
        expiry: &mut Option<Duration>,
    ) -> Result<()> {
        let mut session = session.clone();

        // Keep-alive timeout is 1.5x the keepalive interval; 0 disables it
        let timeout_duration =
            (keepalive > 0).then(|| Duration::from_millis(keepalive as u64 * 1500));

        let deadline = time::sleep(timeout_duration.unwrap_or_default());
        tokio::pin!(deadline);

        while !self.shutdown.is_shutdown() {
            tokio::select! {
                // Try to read and process new incoming packet
                maybe_packet = self.connection.read_packet() => {
                    let packet = match maybe_packet? {
                        None => {
                            // Connection closed without DISCONNECT - abnormal
                            return Ok(());
                        }
                        Some(ControlPacket::Disconnect(disconnect)) => {
                            let new_interval = disconnect
                                .properties
                                .as_ref()
                                .and_then(|p| p.session_expiry_interval.as_ref())
                                .map(|p| p.value);

                            if let Some(interval) = new_interval {
                                // A session that was to end with the connection can't
                                // be extended on the way out [MQTT-3.14.2-2]
                                if *expiry == Some(Duration::ZERO) && interval != 0 {
                                    let _ = self
                                        .connection
                                        .write_packet(ControlPacket::Disconnect(DisconnectPacket {
                                            reason: ReasonCode::ProtocolError,
                                            properties: None,
                                        }))
                                        .await;
                                    return Err(ReasonCode::ProtocolError.into());
                                }
                                *expiry = (interval != u32::MAX)
                                    .then(|| Duration::from_secs(interval.into()));
                            }

                            // Reason 0x04 asks for the will to be published anyway
                            if disconnect.reason != ReasonCode::DisconnectWithWillMessage {
                                session.clear_will().await;
                                let client_id = session.get_client_id().await;
                                let _ = self.session_manager.delete_will(&client_id).await;
                            }
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
                            self.auth_config.auth_manager.as_deref(),
                        ).await?;

                    // Persist subscriptions after subscribe/unsubscribe operations
                    if is_subscription_change {
                        let client_id = session.get_client_id().await;
                        let subscriptions = session.get_subscriptions().await;
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

                    // Reset keep-alive deadline after receiving any packet
                    if let Some(timeout) = timeout_duration {
                        deadline.as_mut().reset(Instant::now() + timeout);
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

                // Keep-alive timeout expired - disconnect client
                _ = &mut deadline, if timeout_duration.is_some() => {
                    let client_id = session.get_client_id().await;
                    warn!(
                        "Keep-alive timeout for client `{}` ({:?} with no packets)",
                        client_id, timeout_duration.unwrap_or_default()
                    );
                    // Return normally to trigger abnormal disconnect handling
                    // (will message will be published)
                    return Ok(());
                }

                // Another connection with the same client id replaces this one
                Ok(()) = lease.takeover.changed() => {
                    info!(
                        "Client `{}` was taken over by a new connection",
                        session.get_client_id().await
                    );
                    if protocol_version.supports_reason_codes() {
                        let _ = self
                            .connection
                            .write_packet(ControlPacket::Disconnect(DisconnectPacket {
                                reason: ReasonCode::SessionTakenOver,
                                properties: None,
                            }))
                            .await;
                    }
                    return Ok(());
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
