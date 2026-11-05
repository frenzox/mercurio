use std::{future::Future, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::{broadcast, Semaphore},
    time::{self, Duration, Instant},
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use mercurio_core::{
    error::Error, message::Message, protocol::ProtocolVersion, reason::ReasonCode, Result,
};
use mercurio_packets::{
    connack::{ConnAckFlags, ConnAckPacket},
    connect::ConnectPacket,
    disconnect::DisconnectPacket,
    ControlPacket,
};
use mercurio_storage::memory::MemoryStore;

use crate::{
    auth::{AuthManager, CredentialValidator},
    broker::Broker,
    connection::{Connection, DEFAULT_MAX_PACKET_SIZE},
    queue::QueueLimits,
    session::{Session, WillMessage},
    session_manager::{ConnectionLease, SessionManager, SessionManagerDropGuard},
    shutdown::Shutdown,
    tls::TlsConfig,
};

/// Authentication configuration for the server.
#[derive(Default)]
pub struct AuthConfig {
    /// Whether authentication is required.
    pub require_auth: bool,
    /// With authentication required, still accept clients that send no
    /// user name. Credentials that are sent are always checked.
    pub allow_anonymous: bool,
    /// Optional credential validator for CONNECT username/password.
    pub credential_validator: Option<Arc<dyn CredentialValidator>>,
    /// Optional auth manager for MQTT 5.0 enhanced authentication.
    pub auth_manager: Option<Arc<AuthManager>>,
}

/// Tunable limits of the broker.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// How long a new connection may take to complete the TLS handshake and
    /// send CONNECT before it is closed.
    pub connect_timeout: Duration,
    /// Largest packet accepted from a client, fixed header included.
    pub max_packet_size: usize,
    /// Messages waiting to be sent to one session.
    pub queue: QueueLimits,
    /// Connections beyond this many are closed right after being accepted.
    pub max_connections: Option<usize>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            queue: QueueLimits::default(),
            max_connections: None,
        }
    }
}

/// Broker configuration.
#[derive(Default)]
pub struct ServerConfig {
    /// Serve TLS instead of plain TCP when set.
    pub tls: Option<TlsConfig>,
    pub auth: AuthConfig,
    pub limits: Limits,
}

struct Listener {
    listener: TcpListener,
    broker: Broker<MemoryStore>,
    session_manager_holder: SessionManagerDropGuard<MemoryStore>,
    notify_shutdown: broadcast::Sender<()>,
    tls_acceptor: Option<TlsAcceptor>,
    auth_config: Arc<AuthConfig>,
    limits: Limits,
    /// One permit per allowed concurrent connection, when limited
    connection_slots: Option<Arc<Semaphore>>,
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
    limits: Limits,
}

/// Run the MQTT broker with plain TCP connections.
pub async fn run(listener: TcpListener, shutdown: impl Future) {
    run_with_config(listener, ServerConfig::default(), shutdown).await
}

/// Run the MQTT broker with the given configuration.
pub async fn run_with_config(listener: TcpListener, config: ServerConfig, shutdown: impl Future) {
    let ServerConfig {
        tls: tls_config,
        auth: auth_config,
        limits,
    } = config;

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
        session_manager_holder: SessionManagerDropGuard::new(storage, limits.queue),
        connection_slots: limits
            .max_connections
            .map(|max| Arc::new(Semaphore::new(max))),
        notify_shutdown,
        tls_acceptor,
        auth_config,
        limits,
    };

    tokio::select! {
        _ = server.run() => {}
        _ = shutdown => {
            info!("Shutting down!");
        }
    }
}

impl Listener {
    async fn run(&mut self) {
        loop {
            let socket = self.accept().await;
            let peer_addr = socket.peer_addr().ok();

            // Held by the connection's task for as long as it runs
            let slot = match &self.connection_slots {
                Some(slots) => match Arc::clone(slots).try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        warn!("Connection limit reached, refusing {:?}", peer_addr);
                        continue;
                    }
                },
                None => None,
            };

            info!("Got a connection: {:?}", peer_addr);

            let broker = self.broker.clone();
            let session_manager = self.session_manager_holder.session_manager();
            let shutdown = Shutdown::new(self.notify_shutdown.subscribe());
            let auth_config = Arc::clone(&self.auth_config);
            let limits = self.limits;

            if let Some(ref tls_acceptor) = self.tls_acceptor {
                // TLS connection
                let acceptor = tls_acceptor.clone();
                tokio::spawn(async move {
                    let _slot = slot;
                    match time::timeout(limits.connect_timeout, acceptor.accept(socket)).await {
                        Ok(Ok(tls_stream)) => {
                            debug!("TLS handshake completed for {:?}", peer_addr);
                            let mut handler = Handler {
                                broker,
                                session_manager,
                                connection: Connection::new_tls(tls_stream),
                                shutdown,
                                auth_config,
                                limits,
                            };
                            Self::handle_connection(&mut handler).await;
                        }
                        Ok(Err(e)) => {
                            error!("TLS handshake failed for {:?}: {}", peer_addr, e);
                        }
                        Err(_) => {
                            warn!("TLS handshake timed out for {:?}", peer_addr);
                        }
                    }
                });
            } else {
                // Plain TCP connection
                tokio::spawn(async move {
                    let _slot = slot;
                    let mut handler = Handler {
                        broker,
                        session_manager,
                        connection: Connection::new(socket),
                        shutdown,
                        auth_config,
                        limits,
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
        handler
            .connection
            .set_max_packet_size(handler.limits.max_packet_size);

        let first_packet = time::timeout(
            handler.limits.connect_timeout,
            handler.connection.read_packet(),
        )
        .await;

        match first_packet {
            // [MQTT-3.1.0-1]
            // After a Network Connection is established by a Client
            // to a Server, the first packet sent from the Client to
            // the Server MUST be a CONNECT packet.
            Ok(Ok(Some(ControlPacket::Connect(p)))) => {
                if let Err(err) = handler.run(p).await {
                    error!(cause = ?err, "Connection error");
                }
            }
            Err(_) => warn!("No CONNECT received in time, closing connection"),
            Ok(Err(Error::MQTTReasonCode(ReasonCode::UnsupportedProtocolVersion))) => {
                // Answered in the MQTT 3.1.1 format, which every version can
                // read [MQTT-3.1.2-2]
                let connack = ConnAckPacket {
                    protocol_version: ProtocolVersion::V3_1_1,
                    flags: ConnAckFlags {
                        session_present: false,
                    },
                    reason_code: ReasonCode::UnsupportedProtocolVersion,
                    properties: None,
                };
                let _ = handler
                    .connection
                    .write_packet(ControlPacket::ConnAck(connack))
                    .await;
            }
            _ => error!("ConnectPacket expectation not met"),
        }
    }

    /// Waits for the next connection. Errors such as running out of file
    /// descriptors are usually transient, so they are retried with a backoff
    /// rather than shutting the listener down.
    async fn accept(&mut self) -> TcpStream {
        let mut backoff = Duration::from_millis(10);

        loop {
            match self.listener.accept().await {
                Ok((socket, _)) => return socket,
                Err(err) => {
                    error!(
                        "Failed to accept connection, retrying in {:?}: {}",
                        backoff, err
                    );
                }
            }

            time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(1));
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
                (None, None) if self.auth_config.allow_anonymous => true,
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
                    self.limits.max_packet_size,
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

            // Resend whatever the client hadn't acknowledged when it left
            session
                .restore_inflight(start_result.inflight_to_retry)
                .await;
            for packet in session.pending_retransmissions().await {
                self.connection.write_packet(packet).await?;
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

        // Tell MQTT 5 clients why the server is closing the connection
        if let Err(Error::MQTTReasonCode(reason)) = &result {
            if protocol_version.supports_reason_codes() && reason.get_code() >= 0x80 {
                let _ = self
                    .connection
                    .write_packet(ControlPacket::Disconnect(DisconnectPacket {
                        reason: *reason,
                        properties: None,
                    }))
                    .await;
            }
        }

        // Publish will message on abnormal disconnect
        // (clean disconnect clears the will, so take_will returns None)
        if let Some(will) = session.take_will().await {
            // Published after the will delay or when the session ends, whichever
            // comes first, unless the client reconnects before then [MQTT-3.1.3-9]
            let delay = expiry.map_or(will.delay, |expiry| expiry.min(will.delay));
            let generation = lease.generation;
            let broker = self.broker.clone();
            let session_manager = self.session_manager.clone();
            let session = session.clone();

            let publish = async move {
                if !delay.is_zero() {
                    time::sleep(delay).await;
                    if session.generation().await != generation {
                        return;
                    }
                }
                let client_id = session.get_client_id().await;
                publish_will(&broker, will, &client_id).await;
                let _ = session_manager.delete_will(&client_id).await;
            };

            if delay.is_zero() {
                publish.await;
            } else {
                tokio::spawn(publish);
            }
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
                        ControlPacket::PubRec(rec) if rec.reason.get_code() >= 0x80 => {
                            Some(rec.packet_id)
                        }
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

                    // After PUBREC only PUBREL remains to be sent for a QoS 2 message
                    if let Some(ControlPacket::PubRel(ref pubrel)) = maybe_res {
                        if pubrel.reason == ReasonCode::Success {
                            let client_id = session.get_client_id().await;
                            let _ = self
                                .session_manager
                                .release_inflight(&client_id, pubrel.packet_id)
                                .await;
                        }
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
                message = session.next_outgoing() => {
                    let Some(outgoing) = session.prepare_publish(message).await else {
                        continue;
                    };
                    tracing::debug!("Sending outgoing packet: {:#?} to client {:?}", outgoing.packet, session.get_client_id().await);

                    if let Some(inflight) = outgoing.inflight {
                        let client_id = session.get_client_id().await;
                        let _ = self
                            .session_manager
                            .store_inflight(&client_id, inflight.packet_id, &inflight)
                            .await;
                    }

                    self.connection.write_packet(outgoing.packet).await?;
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
}

async fn publish_will(broker: &Broker<MemoryStore>, will: WillMessage, client_id: &str) {
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
        expires_at: Message::expiry_deadline(&will.properties),
        properties: will.properties,
        subscription_identifier: None,
    };

    if let Err(e) = broker.publish(&topic, message).await {
        error!("Failed to publish will message: {}", e);
    }
}
