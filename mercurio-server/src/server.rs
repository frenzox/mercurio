use std::{
    future::{poll_fn, Future},
    net::SocketAddr,
    sync::Arc,
    task::Poll,
};

use crate::error::ServerError;
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream},
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, Semaphore},
    time::{self, Duration, Instant},
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};

use mercurio_core::{
    error::Error,
    message::Message,
    properties::{AuthenticationData, AuthenticationMethod, ReasonString, ServerReference},
    protocol::ProtocolVersion,
    reason::ReasonCode,
    Result,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connack::{ConnAckFlags, ConnAckPacket, ConnAckProperties},
    connect::ConnectPacket,
    disconnect::{DisconnectPacket, DisconnectProperties},
    ControlPacket,
};
use mercurio_storage::{memory::MemoryStore, MqttStore};

use crate::{
    acl::{Access, Authorizer, ClientIdentity},
    auth::{AuthManager, AuthResult, CredentialValidator},
    broker::Broker,
    connection::{Connection, DEFAULT_MAX_PACKET_SIZE},
    queue::QueueLimits,
    session::{effective_keepalive, Session, WillMessage},
    session_manager::{session_expiry, ConnectionLease, SessionManager, SessionManagerDropGuard},
    shutdown::Shutdown,
    sys::{self, ConnectedGuard, Stats},
    tls::TlsConfig,
    websocket,
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
    /// Decides which topics each client may publish and subscribe to; all
    /// of them when unset.
    pub authorizer: Option<Arc<dyn Authorizer>>,
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
    /// Highest topic alias MQTT 5 clients may use; 0 disables aliases.
    pub topic_alias_maximum: u16,
    /// QoS 2 messages a client may have awaiting PUBREL at once.
    pub receive_maximum: u16,
    /// Longest keep-alive MQTT 5 clients may use; those asking for more, or
    /// for none, are told to use this one instead.
    pub max_keepalive: Option<u16>,
    /// Replace repeated topics with aliases for clients that accept them.
    /// Off by default: some client libraries announce support they don't
    /// actually have.
    pub send_topic_aliases: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            queue: QueueLimits::default(),
            max_connections: None,
            topic_alias_maximum: 10,
            receive_maximum: u16::MAX,
            max_keepalive: None,
            send_topic_aliases: false,
        }
    }
}

/// Broker configuration.
pub struct ServerConfig {
    /// Serve TLS instead of plain TCP on the main listener when set.
    pub tls: Option<TlsConfig>,
    pub auth: AuthConfig,
    pub limits: Limits,
    pub policy: ConnectPolicy,
    /// Listeners to serve besides the main one.
    pub listeners: Vec<ListenerConfig>,
    /// Accept in-process connections from the paired `LocalConnector`.
    pub local_listener: Option<LocalListener>,
    /// How often to publish statistics under `$SYS/broker/`; never if unset.
    pub sys_interval: Option<Duration>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tls: None,
            auth: AuthConfig::default(),
            limits: Limits::default(),
            policy: ConnectPolicy::default(),
            listeners: Vec::new(),
            local_listener: None,
            sys_interval: Some(Duration::from_secs(10)),
        }
    }
}

/// A listener with its own transport and TLS settings.
pub struct ListenerConfig {
    pub listener: TcpListener,
    pub transport: Transport,
    /// Serve TLS instead of plain TCP when set.
    pub tls: Option<TlsConfig>,
}

/// Opens in-process connections to a broker, for parts of the same program
/// such as bridges. These connections skip authentication and access
/// control.
#[derive(Clone)]
pub struct LocalConnector(mpsc::UnboundedSender<DuplexStream>);

/// The broker's end of a `LocalConnector`, given in `ServerConfig`.
pub struct LocalListener(mpsc::UnboundedReceiver<DuplexStream>);

impl LocalConnector {
    pub fn new() -> (LocalConnector, LocalListener) {
        let (tx, rx) = mpsc::unbounded_channel();
        (LocalConnector(tx), LocalListener(rx))
    }

    /// A new connection to the broker, which fails once the broker stopped.
    pub fn connect(&self) -> std::io::Result<DuplexStream> {
        let (client, server) = tokio::io::duplex(64 * 1024);
        self.0.send(server).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::ConnectionRefused, "broker stopped")
        })?;
        Ok(client)
    }
}

/// A connection waiting to be served.
enum Accepted {
    Tcp(TcpStream, usize),
    Local(DuplexStream),
}

/// How MQTT is carried on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Mqtt,
    WebSocket,
}

/// A listener ready to accept connections.
struct Endpoint {
    listener: TcpListener,
    transport: Transport,
    tls_acceptor: Option<TlsAcceptor>,
}

impl Endpoint {
    fn new(config: ListenerConfig) -> std::result::Result<Self, ServerError> {
        let tls_acceptor = config.tls.map(|tls| tls.build_acceptor()).transpose()?;
        Ok(Self {
            listener: config.listener,
            transport: config.transport,
            tls_acceptor,
        })
    }
}

/// Everything a connection handler needs besides its stream.
struct HandlerParts<St: MqttStore + 'static> {
    broker: Broker<St>,
    session_manager: SessionManager<St>,
    shutdown: Shutdown,
    shutdown_complete: mpsc::Sender<()>,
    auth_config: Arc<AuthConfig>,
    policy: Arc<ConnectPolicy>,
    limits: Limits,
    stats: Arc<Stats>,
}

impl<St: MqttStore + 'static> HandlerParts<St> {
    /// Serves one client on `stream`, after the WebSocket handshake if it
    /// came in on the WebSocket listener.
    async fn serve<S>(self, stream: S, transport: Transport, peer_addr: Option<SocketAddr>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        match transport {
            Transport::Mqtt => self.handle(Connection::new(stream)).await,
            Transport::WebSocket => {
                let handshake = websocket::accept(stream);
                match time::timeout(self.limits.connect_timeout, handshake).await {
                    Ok(Ok(websocket)) => self.handle(Connection::new(websocket)).await,
                    Ok(Err(e)) => warn!("WebSocket handshake failed for {:?}: {}", peer_addr, e),
                    Err(_) => warn!("WebSocket handshake timed out for {:?}", peer_addr),
                }
            }
        }
    }

    async fn handle<S>(self, connection: Connection<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let mut handler = Handler {
            broker: self.broker,
            session_manager: self.session_manager,
            connection,
            shutdown: self.shutdown,
            auth_config: self.auth_config,
            policy: self.policy,
            limits: self.limits,
            stats: self.stats,
            _shutdown_complete: self.shutdown_complete,
        };
        Listener::<St>::handle_connection(&mut handler).await;
    }
}

/// What the broker tells clients when they connect, beyond limits.
#[derive(Debug, Clone, Default)]
pub struct ConnectPolicy {
    /// Returned to MQTT 5 clients that ask for Response Information, with
    /// `%c` replaced by their client id: a topic prefix they may use for the
    /// responses of request/response exchanges.
    pub response_information: Option<String>,
    /// Send every client to another server instead of serving it.
    pub redirect: Option<Redirect>,
}

/// Where clients are sent instead of being served.
#[derive(Debug, Clone)]
pub struct Redirect {
    /// The server clients should use, in the form MQTT 5 clients expect,
    /// such as `host:port`
    pub server_reference: String,
    /// Whether the move is permanent (Server Moved) or temporary (Use Another
    /// Server)
    pub permanent: bool,
}

struct Listener<St: MqttStore + 'static> {
    endpoints: Vec<Endpoint>,
    local_listener: Option<LocalListener>,
    /// Given to local connections instead of `auth_config`
    trusted_auth: Arc<AuthConfig>,
    /// Where the next accept starts looking, so a busy listener does not
    /// starve the others
    next_endpoint: usize,
    broker: Broker<St>,
    session_manager_holder: SessionManagerDropGuard<St>,
    notify_shutdown: broadcast::Sender<()>,
    /// Cloned into every connection handler; see `Handler::_shutdown_complete`
    shutdown_complete: mpsc::Sender<()>,
    auth_config: Arc<AuthConfig>,
    policy: Arc<ConnectPolicy>,
    stats: Arc<Stats>,
    limits: Limits,
    /// One permit per allowed concurrent connection, when limited
    connection_slots: Option<Arc<Semaphore>>,
}

struct Handler<St, S>
where
    St: MqttStore + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    broker: Broker<St>,
    session_manager: SessionManager<St>,
    connection: Connection<S>,
    shutdown: Shutdown,
    auth_config: Arc<AuthConfig>,
    policy: Arc<ConnectPolicy>,
    stats: Arc<Stats>,
    limits: Limits,
    /// Dropped when the handler finishes, so shutdown can wait for it
    _shutdown_complete: mpsc::Sender<()>,
}

/// Run the MQTT broker with plain TCP connections.
pub async fn run(
    listener: TcpListener,
    shutdown: impl Future,
) -> std::result::Result<(), ServerError> {
    run_with_config(listener, ServerConfig::default(), shutdown).await
}

/// Run the MQTT broker with the given configuration, keeping its state in
/// memory.
pub async fn run_with_config(
    listener: TcpListener,
    config: ServerConfig,
    shutdown: impl Future,
) -> std::result::Result<(), ServerError> {
    run_with_storage(listener, config, Arc::new(MemoryStore::new()), shutdown).await
}

/// Run the MQTT broker with the given configuration, keeping sessions,
/// retained messages, wills and in-flight messages in `storage`. Fails
/// without serving anyone if the configuration can't be used.
pub async fn run_with_storage<St: MqttStore + 'static>(
    listener: TcpListener,
    config: ServerConfig,
    storage: Arc<St>,
    shutdown: impl Future,
) -> std::result::Result<(), ServerError> {
    let ServerConfig {
        tls: tls_config,
        auth: auth_config,
        limits,
        policy,
        listeners,
        local_listener,
        sys_interval,
    } = config;
    let stats = Arc::new(Stats::default());
    let policy = Arc::new(policy);

    let (notify_shutdown, _) = broadcast::channel(1);
    let (shutdown_complete, mut shutdown_complete_rx) = mpsc::channel(1);

    let main = ListenerConfig {
        listener,
        transport: Transport::Mqtt,
        tls: tls_config,
    };
    let endpoints = std::iter::once(main)
        .chain(listeners)
        .map(Endpoint::new)
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let auth_config = Arc::new(auth_config);

    let mut server = Listener {
        endpoints,
        local_listener,
        trusted_auth: Arc::default(),
        next_endpoint: 0,
        broker: Broker::new(Arc::clone(&storage)),
        session_manager_holder: SessionManagerDropGuard::new(storage, limits.queue),
        connection_slots: limits
            .max_connections
            .map(|max| Arc::new(Semaphore::new(max))),
        notify_shutdown,
        shutdown_complete,
        auth_config,
        policy,
        limits,
        stats,
    };

    let session_manager = server.session_manager_holder.session_manager();
    if let Some(interval) = sys_interval {
        sys::spawn_publisher(
            server.broker.clone(),
            session_manager.clone(),
            Arc::clone(&server.stats),
            interval,
        );
    }
    for (client_id, will, due) in session_manager.restore_from_storage(&server.broker).await {
        let broker = server.broker.clone();
        let session_manager = session_manager.clone();
        tokio::spawn(async move {
            time::sleep(due).await;
            if session_manager.take_restored_will(&client_id).await {
                publish_will(&broker, will, &client_id).await;
                let _ = session_manager.delete_will(&client_id).await;
            }
        });
    }

    tokio::select! {
        _ = server.run() => {}
        _ = shutdown => {
            info!("Shutting down!");
        }
    }

    // Let connections tell their clients and finish up, for a moment
    let Listener {
        notify_shutdown,
        shutdown_complete,
        ..
    } = server;
    let _ = notify_shutdown.send(());
    drop(shutdown_complete);
    if time::timeout(Duration::from_secs(5), shutdown_complete_rx.recv())
        .await
        .is_err()
    {
        warn!("Connections still open after waiting to shut down");
    }
    Ok(())
}

impl<St: MqttStore + 'static> Listener<St> {
    async fn run(&mut self) {
        loop {
            let accepted = self.accept().await;
            let peer_addr = match &accepted {
                Accepted::Tcp(socket, _) => socket.peer_addr().ok(),
                Accepted::Local(_) => None,
            };

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

            let parts = HandlerParts {
                broker: self.broker.clone(),
                session_manager: self.session_manager_holder.session_manager(),
                shutdown: Shutdown::new(self.notify_shutdown.subscribe()),
                shutdown_complete: self.shutdown_complete.clone(),
                auth_config: Arc::clone(&self.auth_config),
                policy: Arc::clone(&self.policy),
                limits: self.limits,
                stats: Arc::clone(&self.stats),
            };
            let (socket, endpoint) = match accepted {
                Accepted::Tcp(socket, endpoint) => (socket, endpoint),
                Accepted::Local(stream) => {
                    let parts = HandlerParts {
                        auth_config: Arc::clone(&self.trusted_auth),
                        ..parts
                    };
                    tokio::spawn(async move {
                        let _slot = slot;
                        parts.serve(stream, Transport::Mqtt, None).await;
                    });
                    continue;
                }
            };
            let transport = self.endpoints[endpoint].transport;
            let tls_acceptor = self.endpoints[endpoint].tls_acceptor.clone();

            tokio::spawn(async move {
                let _slot = slot;
                let Some(acceptor) = tls_acceptor else {
                    return parts.serve(socket, transport, peer_addr).await;
                };
                match time::timeout(parts.limits.connect_timeout, acceptor.accept(socket)).await {
                    Ok(Ok(tls_stream)) => {
                        debug!("TLS handshake completed for {:?}", peer_addr);
                        parts.serve(tls_stream, transport, peer_addr).await;
                    }
                    Ok(Err(e)) => error!("TLS handshake failed for {:?}: {}", peer_addr, e),
                    Err(_) => warn!("TLS handshake timed out for {:?}", peer_addr),
                }
            });
        }
    }

    async fn handle_connection<S>(handler: &mut Handler<St, S>)
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
                match handler.run(p).await {
                    // Closed by the broker's own shutdown
                    Err(mercurio_core::error::Error::MQTTReasonCode(
                        ReasonCode::ServerShuttingDown,
                    )) => debug!("Connection closed for shutdown"),
                    Err(err) => error!(cause = ?err, "Connection error"),
                    Ok(()) => {}
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
            // MQTT 5 clients are told why their CONNECT was refused; earlier
            // versions have no reason code for it and are just disconnected
            Ok(Err(Error::MQTTReasonCode(reason)))
                if handler.connection.connect_version() == Some(ProtocolVersion::V5) =>
            {
                warn!("Refusing invalid CONNECT: {:?}", reason);
                let connack = ConnAckPacket {
                    protocol_version: ProtocolVersion::V5,
                    flags: ConnAckFlags {
                        session_present: false,
                    },
                    reason_code: reason,
                    properties: Some(ConnAckProperties {
                        reason_string: reason_string(reason, None),
                        ..Default::default()
                    }),
                };
                let _ = handler
                    .connection
                    .write_packet(ControlPacket::ConnAck(connack))
                    .await;
            }
            _ => error!("ConnectPacket expectation not met"),
        }
        handler.connection.close().await;
    }

    /// Waits for the next connection. Errors such as running out of file
    /// descriptors are usually transient, so they are retried with a backoff
    /// rather than shutting the listener down.
    async fn accept(&mut self) -> Accepted {
        let mut backoff = Duration::from_millis(10);

        loop {
            let accepted = poll_fn(|cx| {
                if let Some(LocalListener(local)) = &mut self.local_listener {
                    if let Poll::Ready(Some(stream)) = local.poll_recv(cx) {
                        return Poll::Ready(Ok(Accepted::Local(stream)));
                    }
                }
                let count = self.endpoints.len();
                for offset in 0..count {
                    let index = (self.next_endpoint + offset) % count;
                    if let Poll::Ready(accepted) = self.endpoints[index].listener.poll_accept(cx) {
                        self.next_endpoint = (index + 1) % count;
                        return Poll::Ready(
                            accepted.map(|(socket, _)| Accepted::Tcp(socket, index)),
                        );
                    }
                }
                Poll::Pending
            })
            .await;
            match accepted {
                Ok(accepted) => return accepted,
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

impl<St, S> Handler<St, S>
where
    St: MqttStore + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    async fn run(&mut self, connect_packet: ConnectPacket) -> Result<()> {
        self.connection
            .set_protocol_version(connect_packet.protocol_version);

        if let Some(redirect) = self.policy.redirect.clone() {
            return self.redirect(&connect_packet, &redirect).await;
        }

        let authentication_method = connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.authentication_method.as_ref())
            .map(|m| m.value.clone());

        // The user name access control can rely on: only one that was
        // verified, by enhanced authentication or the password check
        let mut verified_user = None;

        // MQTT 5 enhanced authentication replaces the user name and password
        // check when the client asks for it
        if let Some(method) = authentication_method {
            match self.authenticate_enhanced(&connect_packet, &method).await? {
                Some(username) => verified_user = username,
                None => return Ok(()),
            }
        } else if self.auth_config.require_auth {
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
            verified_user = connect_packet.payload.user_name.clone();
        }

        // A will is published on the client's behalf, so it needs the same
        // permission as a PUBLISH
        if let (Some(authorizer), Some(will_topic)) = (
            &self.auth_config.authorizer,
            &connect_packet.payload.will_topic,
        ) {
            let identity = ClientIdentity {
                client_id: connect_packet.payload.client_id.clone(),
                username: verified_user.clone(),
            };
            if !authorizer.authorize(&identity, will_topic, Access::Write) {
                return self
                    .reject_connect(&connect_packet, ReasonCode::NotAuthorized)
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

        let keepalive = effective_keepalive(&connect_packet, &self.limits);
        let protocol_version = connect_packet.protocol_version;
        let client_maximum_packet_size = connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.maximum_packet_size.as_ref())
            .map(|m| m.value);
        let mut expiry = session_expiry(&connect_packet);

        let start_result = self.session_manager.start_session(connect_packet).await?;
        let mut session = start_result.session;
        let mut lease = start_result.lease;

        session.set_verified_user(verified_user).await;

        let result = async {
            session
                .begin(
                    &mut self.connection,
                    start_result.resume,
                    start_result.assigned_client_id,
                    &self.limits,
                    &self.policy,
                )
                .await?;

            // Restore subscriptions if resuming a persisted session
            if !start_result.subscriptions_to_restore.is_empty() {
                session
                    .restore_subscriptions(start_result.subscriptions_to_restore, &self.broker)
                    .await?;
            }

            // Resend whatever the client hadn't acknowledged when it left
            session
                .restore_inflight(start_result.inflight_to_retry)
                .await;
            for packet in session.pending_retransmissions().await {
                self.connection.write_packet(packet).await?;
            }

            let _connected = ConnectedGuard::new(Arc::clone(&self.stats));
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
                        properties: Some(DisconnectProperties {
                            reason_string: reason_string(*reason, client_maximum_packet_size),
                            ..Default::default()
                        }),
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

    /// Runs the AUTH exchange of MQTT 5 enhanced authentication during
    /// CONNECT. Returns the user name the method verified, if any, or None
    /// having refused the connection.
    async fn authenticate_enhanced(
        &mut self,
        connect_packet: &ConnectPacket,
        method: &str,
    ) -> Result<Option<Option<String>>> {
        let exchange = self
            .auth_config
            .auth_manager
            .as_ref()
            .and_then(|manager| manager.begin(method));
        let Some(mut exchange) = exchange else {
            self.reject_connect(connect_packet, ReasonCode::BadAuthenticationMethod)
                .await?;
            return Ok(None);
        };

        let initial_data = connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.authentication_data.as_ref())
            .map(|d| &d.value);
        let mut result = exchange.step(initial_data);

        loop {
            let challenge = match result {
                AuthResult::Success { username } => return Ok(Some(username)),
                AuthResult::Failed(reason) => {
                    warn!("Enhanced authentication with `{}` failed", method);
                    self.reject_connect(connect_packet, reason).await?;
                    return Ok(None);
                }
                AuthResult::Continue(challenge) => challenge,
            };

            self.connection
                .write_packet(ControlPacket::Auth(AuthPacket {
                    reason: ReasonCode::ContinueAuthentication,
                    properties: AuthProperties {
                        auth_method: Some(AuthenticationMethod::new(method.to_string())),
                        auth_data: Some(AuthenticationData::new(challenge)),
                        ..Default::default()
                    },
                }))
                .await?;

            // The exchange counts towards the time allowed to connect
            let response =
                time::timeout(self.limits.connect_timeout, self.connection.read_packet())
                    .await
                    .map_err(|_| ReasonCode::MaximumConnectTime)??;

            // Only AUTH continuing the same method may answer a challenge
            let Some(ControlPacket::Auth(auth)) = response else {
                self.reject_connect(connect_packet, ReasonCode::ProtocolError)
                    .await?;
                return Ok(None);
            };
            let same_method = auth
                .properties
                .auth_method
                .as_ref()
                .is_some_and(|m| m.value == method);
            if auth.reason != ReasonCode::ContinueAuthentication || !same_method {
                self.reject_connect(connect_packet, ReasonCode::ProtocolError)
                    .await?;
                return Ok(None);
            }

            let data = auth.properties.auth_data.map(|d| d.value);
            result = exchange.step(data.as_ref());
        }
    }

    /// Refuses the connection, pointing MQTT 5 clients to another server.
    /// MQTT 3.x has no way to name one, so its clients learn only that this
    /// server is unavailable.
    async fn redirect(
        &mut self,
        connect_packet: &ConnectPacket,
        redirect: &Redirect,
    ) -> Result<()> {
        let connack = if connect_packet.protocol_version.supports_properties() {
            ConnAckPacket {
                protocol_version: connect_packet.protocol_version,
                flags: ConnAckFlags {
                    session_present: false,
                },
                reason_code: if redirect.permanent {
                    ReasonCode::ServerMoved
                } else {
                    ReasonCode::UseAnotherServer
                },
                properties: Some(ConnAckProperties {
                    server_reference: Some(ServerReference::new(redirect.server_reference.clone())),
                    ..Default::default()
                }),
            }
        } else {
            ConnAckPacket {
                protocol_version: connect_packet.protocol_version,
                flags: ConnAckFlags {
                    session_present: false,
                },
                reason_code: ReasonCode::ServerUnavailable,
                properties: None,
            }
        };
        let _ = self
            .connection
            .write_packet(ControlPacket::ConnAck(connack))
            .await;
        Ok(())
    }

    async fn reject_connect(
        &mut self,
        connect_packet: &ConnectPacket,
        reason_code: ReasonCode,
    ) -> Result<()> {
        let client_maximum = connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.maximum_packet_size.as_ref())
            .map(|m| m.value);
        let connack = ConnAckPacket {
            protocol_version: connect_packet.protocol_version,
            flags: ConnAckFlags {
                session_present: false,
            },
            reason_code,
            properties: connect_packet
                .protocol_version
                .supports_properties()
                .then(|| ConnAckProperties {
                    reason_string: reason_string(reason_code, client_maximum),
                    ..Default::default()
                }),
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
            // Re-evaluated after every packet, so acknowledgements reopen it
            let window_open = session.can_send_more().await;

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

                    if matches!(packet, ControlPacket::Publish(_)) {
                        Stats::add(&self.stats.messages_received);
                    }

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
                            self.auth_config.authorizer.as_deref(),
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
                // With the client's window full only QoS 0 messages can go out
                message = session.next_outgoing(window_open) => {
                    // Checked per message: a permitted subscription can still
                    // match topics the client may not read
                    if let Some(authorizer) = &self.auth_config.authorizer {
                        // Re-read each time: re-authentication can change the user
                        let identity = session.identity().await;
                        if !authorizer.authorize(&identity, &message.topic, Access::Read) {
                            continue;
                        }
                    }
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
                    Stats::add(&self.stats.messages_sent);
                }

                // Keep-alive timeout expired - disconnect client
                _ = &mut deadline, if timeout_duration.is_some() => {
                    let client_id = session.get_client_id().await;
                    warn!(
                        "Keep-alive timeout for client `{}` ({:?} with no packets)",
                        client_id, timeout_duration.unwrap_or_default()
                    );
                    // An abnormal disconnect: the will is published
                    return Err(ReasonCode::KeepAliveTimeout.into());
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
                                properties: Some(DisconnectProperties {
                                    reason_string: reason_string(
                                        ReasonCode::SessionTakenOver,
                                        session.client_maximum_packet_size().await,
                                    ),
                                    ..Default::default()
                                }),
                            }))
                            .await;
                    }
                    return Ok(());
                }

                // Exit in case a signal is received
                _ = self.shutdown.recv() => {
                    return Err(ReasonCode::ServerShuttingDown.into());
                },
            }
        }

        Ok(())
    }
}

/// Explains `reason` to the client, unless the text would make the packet
/// larger than the client accepts [MQTT-3.1.2-25].
fn reason_string(
    reason: ReasonCode,
    client_maximum_packet_size: Option<u32>,
) -> Option<ReasonString> {
    let text = reason.to_string();
    // Room for the rest of a CONNACK or DISCONNECT around the string
    let fits = client_maximum_packet_size.is_none_or(|max| text.len() + 16 <= max as usize);
    fits.then(|| ReasonString::new(text))
}

async fn publish_will<St: MqttStore>(broker: &Broker<St>, will: WillMessage, client_id: &str) {
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
        subscription_identifiers: Vec::new(),
    };

    if let Err(e) = broker.publish(&topic, message).await {
        error!("Failed to publish will message: {}", e);
    }
}
