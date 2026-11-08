use std::{
    collections::HashMap,
    future::{poll_fn, Future},
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    task::Poll,
};

use crate::error::ServerError;
use crate::CONNECTIONS;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream},
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, OwnedSemaphorePermit, Semaphore},
    time::{self, Duration, Instant},
};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use mercurio_core::{
    error::Error,
    message::Message,
    properties::{AuthenticationData, AuthenticationMethod, ReasonString, ServerReference},
    protocol::ProtocolVersion,
    qos::QoS,
    reason::ReasonCode,
    topic::validate_publish_topic,
    Result,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connack::{ConnAckFlags, ConnAckPacket, ConnAckProperties},
    connect::ConnectPacket,
    disconnect::{DisconnectPacket, DisconnectProperties},
    pingresp::PingRespPacket,
    puback::PubAckPacket,
    pubcomp::PubCompPacket,
    publish::PublishPacket,
    pubrec::PubRecPacket,
    pubrel::PubRelPacket,
    ControlPacket,
};
use mercurio_storage::{memory::MemoryStore, MqttStore, SessionOwner};

use crate::{
    acl::{Access, Authorizer, ClientIdentity},
    auth::{AuthManager, AuthResult, CredentialValidator},
    broker::Broker,
    connection::Connection,
    http, proxy,
    session::{cap_session_expiry, effective_keepalive, Outgoing, Session, WillMessage},
    session_manager::{session_expiry, ConnectionLease, SessionManager, SessionManagerDropGuard},
    shutdown::Shutdown,
    sys::{self, ConnectedGuard, Stats},
    tls::{certificate_name, CertificateName, TlsConfig},
    websocket,
};

/// Authentication configuration for the server.
#[derive(Default)]
#[non_exhaustive]
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

pub use crate::{connection::DEFAULT_MAX_PACKET_SIZE, queue::QueueLimits};

/// Subscriptions a session may hold unless configured otherwise: far more
/// than clients need, far fewer than it takes to exhaust the broker.
pub const DEFAULT_MAX_SUBSCRIPTIONS: usize = 10_000;

/// Tunable limits of the broker.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Limits {
    /// How long a new connection may take, in all, to send its PROXY header,
    /// complete the TLS and WebSocket handshakes and send CONNECT, enhanced
    /// authentication included, before it is closed.
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
    /// Highest QoS clients may publish and subscribe with
    pub maximum_qos: QoS,
    /// Whether clients may publish retained messages
    pub retain_available: bool,
    /// How long the persistent session of an MQTT 3.x client outlives its
    /// connection; forever if unset, as MQTT 3.x has no expiry of its own
    pub persistent_client_expiration: Option<Duration>,
    /// Send a retained message to new subscribers only while the client
    /// that published it may still publish to its topic.
    pub check_retain_source: bool,
    /// Subscriptions one session may hold; further ones are refused with
    /// Quota Exceeded. Each costs memory and work on every message.
    pub max_subscriptions: Option<usize>,
    /// Longest a session may outlive its connection. MQTT 5 clients asking
    /// for longer, or for no expiry, are told to use this; it also bounds
    /// `persistent_client_expiration`. Unbounded when unset.
    pub max_session_expiry: Option<Duration>,
    /// Topics that may hold a retained message at once, the broker's own
    /// `$SYS` statistics included; retaining a message on another topic is
    /// then refused with Quota Exceeded. Unbounded when unset.
    pub max_retained_messages: Option<usize>,
    /// Connections one client address may hold at once, counting the
    /// address a PROXY header gives; further ones are closed. Unbounded
    /// when unset, as many devices can share an address behind NAT.
    pub max_connections_per_ip: Option<usize>,
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
            maximum_qos: QoS::ExactlyOnce,
            retain_available: true,
            persistent_client_expiration: None,
            check_retain_source: false,
            max_subscriptions: Some(DEFAULT_MAX_SUBSCRIPTIONS),
            max_session_expiry: None,
            max_retained_messages: None,
            max_connections_per_ip: None,
        }
    }
}

/// Broker configuration. Start from `ServerConfig::default()` and set
/// what differs; fields may be added in minor releases.
#[non_exhaustive]
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

/// A socket to accept connections on.
#[non_exhaustive]
pub enum Socket {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
}

impl From<TcpListener> for Socket {
    fn from(listener: TcpListener) -> Self {
        Socket::Tcp(listener)
    }
}

#[cfg(unix)]
impl From<UnixListener> for Socket {
    fn from(listener: UnixListener) -> Self {
        Socket::Unix(listener)
    }
}

/// A listener with its own transport and TLS settings.
#[non_exhaustive]
pub struct ListenerConfig {
    pub listener: Socket,
    pub transport: Transport,
    /// Serve TLS instead of plain TCP when set.
    pub tls: Option<TlsConfig>,
    /// Expect every connection to start with a PROXY protocol header, as
    /// sent by load balancers, and take the client's address from it.
    pub proxy_protocol: bool,
    /// With `proxy_protocol`, the proxies allowed to send the header; other
    /// connections are closed, as their header could claim any address.
    /// Any connection may send one when empty.
    pub proxy_from: Vec<AddressRange>,
    /// Prefixed to every topic the listener's clients use, so they only see
    /// their own part of the topic tree.
    pub mount_point: Option<String>,
    /// Replaces the broker's authentication and access control for the
    /// listener's clients.
    pub auth: Option<AuthConfig>,
    /// Connections beyond this many on this listener are closed right after
    /// being accepted, on top of the broker-wide limit.
    pub max_connections: Option<usize>,
    /// On WebSocket listeners, answer plain HTTP requests with the files of
    /// this directory.
    pub http_dir: Option<PathBuf>,
}

impl ListenerConfig {
    /// A listener without TLS, PROXY protocol, mount point or auth settings
    /// of its own.
    pub fn new(listener: impl Into<Socket>, transport: Transport) -> Self {
        Self {
            listener: listener.into(),
            transport,
            tls: None,
            proxy_protocol: false,
            proxy_from: Vec::new(),
            mount_point: None,
            auth: None,
            max_connections: None,
            http_dir: None,
        }
    }
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

/// Lets trusted connections use every topic, `$CONTROL` and `$SYS` ones
/// included.
struct AllowAll;

impl Authorizer for AllowAll {
    fn authorize(&self, _: &ClientIdentity, _: &str, _: Access) -> bool {
        true
    }

    fn allows_sys_publish(&self) -> bool {
        true
    }
}

/// A connection waiting to be served, with the index of its endpoint.
enum Accepted {
    Tcp(TcpStream, usize),
    #[cfg(unix)]
    Unix(UnixStream, usize),
    Local(DuplexStream),
}

/// How MQTT is carried on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Transport {
    Mqtt,
    WebSocket,
}

/// A listener ready to accept connections.
struct Endpoint {
    listener: Socket,
    settings: EndpointSettings,
    /// One permit per allowed concurrent connection, when limited
    connection_slots: Option<Arc<Semaphore>>,
}

/// How the connections of a listener are served.
#[derive(Clone)]
struct EndpointSettings {
    transport: Transport,
    tls_acceptor: Option<TlsAcceptor>,
    username_from_certificate: Option<CertificateName>,
    proxy_protocol: bool,
    proxy_from: Arc<[AddressRange]>,
    mount_point: Option<String>,
    auth: Option<Arc<AuthConfig>>,
    http_dir: Option<Arc<PathBuf>>,
}

impl Endpoint {
    fn new(config: ListenerConfig) -> std::result::Result<Self, ServerError> {
        let username_from_certificate = config
            .tls
            .as_ref()
            .and_then(|tls| tls.username_from_certificate);
        let tls_acceptor = config.tls.map(|tls| tls.build_acceptor()).transpose()?;
        Ok(Self {
            listener: config.listener,
            connection_slots: config
                .max_connections
                .map(|max| Arc::new(Semaphore::new(max))),
            settings: EndpointSettings {
                transport: config.transport,
                tls_acceptor,
                username_from_certificate,
                proxy_protocol: config.proxy_protocol,
                proxy_from: config.proxy_from.into(),
                mount_point: config.mount_point,
                auth: config.auth.map(Arc::new),
                http_dir: config.http_dir.map(Arc::new),
            },
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
    /// The client certificate's Common Name, when it stands in for the
    /// user name
    cert_identity: Option<String>,
    mount_point: Option<String>,
    http_dir: Option<Arc<PathBuf>>,
    /// An in-process connection, from the local listener
    local: bool,
    /// When the PROXY header, the TLS and WebSocket handshakes and CONNECT
    /// must all be done by; one deadline, so a client trickling bytes can't
    /// take the time allowed once per step
    connect_deadline: Instant,
    address_slots: Arc<AddressSlots>,
}

impl<St: MqttStore + 'static> HandlerParts<St> {
    /// Serves a connection accepted on a listener, after the PROXY header
    /// and the TLS handshake if the listener has them. `_slots` are held for
    /// as long as it runs.
    ///
    /// Everything here is boxed: the task holds this function's state for
    /// the whole connection, so it keeps only pointers, and the handshakes'
    /// state is freed once they are done.
    async fn serve_accepted<S>(
        self: Box<Self>,
        socket: S,
        settings: EndpointSettings,
        peer_addr: Option<SocketAddr>,
        _slots: [Option<OwnedSemaphorePermit>; 2],
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let transport = settings.transport;
        let Some((parts, stream, peer_addr)) =
            Box::pin(self.handshake(socket, settings, peer_addr)).await
        else {
            return;
        };
        // Counted once the PROXY header gave the client's own address
        let _address_slot = match (parts.limits.max_connections_per_ip, peer_addr) {
            (Some(max), Some(peer)) => match parts.address_slots.acquire(peer.ip(), max) {
                Some(slot) => Some(slot),
                None => {
                    return warn!("{} holds the most connections allowed, closing", peer.ip());
                }
            },
            _ => None,
        };
        match stream {
            Secured::Plain(stream) => Box::pin(parts.serve(stream, transport, peer_addr)).await,
            Secured::Tls(stream) => Box::pin(parts.serve(*stream, transport, peer_addr)).await,
        }
    }

    /// Reads the PROXY header and completes the TLS handshake the listener
    /// asks for. None if they failed, which is logged.
    async fn handshake<S>(
        mut self: Box<Self>,
        mut socket: S,
        settings: EndpointSettings,
        mut peer_addr: Option<SocketAddr>,
    ) -> Option<(Self, Secured<S>, Option<SocketAddr>)>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        if settings.proxy_protocol {
            let trusted = settings.proxy_from.is_empty()
                || peer_addr.is_some_and(|peer| {
                    settings
                        .proxy_from
                        .iter()
                        .any(|range| range.contains(peer.ip()))
                });
            if !trusted {
                warn!("{:?} may not send a PROXY header, closing", peer_addr);
                return None;
            }
            let header = proxy::read_header(&mut socket);
            match time::timeout_at(self.connect_deadline, header).await {
                Ok(Ok(source)) => {
                    debug!("{:?} is proxying {:?}", peer_addr, source);
                    peer_addr = source.or(peer_addr);
                }
                Ok(Err(e)) => {
                    warn!("Bad PROXY header from {:?}: {}", peer_addr, e);
                    return None;
                }
                Err(_) => {
                    warn!("No PROXY header from {:?} in time", peer_addr);
                    return None;
                }
            }
        }
        self.mount_point = settings.mount_point;
        self.http_dir = settings.http_dir;
        if let Some(auth) = settings.auth {
            self.auth_config = auth;
        }
        let Some(acceptor) = settings.tls_acceptor else {
            return Some((*self, Secured::Plain(socket), peer_addr));
        };
        match time::timeout_at(self.connect_deadline, acceptor.accept(socket)).await {
            Ok(Ok(tls_stream)) => {
                debug!("TLS handshake completed for {:?}", peer_addr);
                if let Some(part) = settings.username_from_certificate {
                    let certs = tls_stream.get_ref().1.peer_certificates();
                    self.cert_identity = certs
                        .and_then(|c| c.first())
                        .and_then(|cert| certificate_name(cert, part));
                    if self.cert_identity.is_none() {
                        warn!("No Common Name in the certificate of {:?}", peer_addr);
                        return None;
                    }
                }
                Some((*self, Secured::Tls(Box::new(tls_stream)), peer_addr))
            }
            Ok(Err(e)) => {
                error!("TLS handshake failed for {:?}: {}", peer_addr, e);
                None
            }
            Err(_) => {
                warn!("TLS handshake timed out for {:?}", peer_addr);
                None
            }
        }
    }

    /// Serves one client on `stream`, after the WebSocket handshake if it
    /// came in on the WebSocket listener.
    async fn serve<S>(self, stream: S, transport: Transport, peer_addr: Option<SocketAddr>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        match transport {
            Transport::Mqtt => Box::pin(self.handle(Connection::new(stream))).await,
            Transport::WebSocket => {
                let stream = match self.http_dir.clone() {
                    Some(dir) => {
                        let request = Box::pin(http::serve_or_upgrade(stream, &dir));
                        match time::timeout_at(self.connect_deadline, request).await {
                            Ok(Ok(Some(upgrade))) => upgrade,
                            Ok(Ok(None)) => return,
                            Ok(Err(e)) => {
                                return warn!("HTTP request from {:?} failed: {}", peer_addr, e)
                            }
                            Err(_) => return warn!("HTTP request from {:?} timed out", peer_addr),
                        }
                    }
                    None => http::Rewind::new(stream, Vec::new()),
                };
                // An MQTT packet may span messages, but no honest client
                // needs one larger than the largest packet
                let handshake = Box::pin(websocket::accept(stream, self.limits.max_packet_size));
                match time::timeout_at(self.connect_deadline, handshake).await {
                    Ok(Ok(websocket)) => Box::pin(self.handle(Connection::new(websocket))).await,
                    Ok(Err(e)) => warn!("WebSocket handshake failed for {:?}: {}", peer_addr, e),
                    Err(_) => warn!("WebSocket handshake timed out for {:?}", peer_addr),
                }
            }
        }
    }

    async fn handle<S>(self, mut connection: Connection<S>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        connection.set_mount_point(self.mount_point);
        connection.set_stats(Arc::clone(&self.stats));
        let mut handler = Handler {
            broker: self.broker,
            session_manager: self.session_manager,
            connection,
            shutdown: self.shutdown,
            auth_config: self.auth_config,
            policy: self.policy,
            limits: self.limits,
            stats: self.stats,
            cert_identity: self.cert_identity,
            local: self.local,
            connect_deadline: self.connect_deadline,
            _shutdown_complete: self.shutdown_complete,
        };
        Listener::<St>::handle_connection(&mut handler).await;
    }
}

/// What the broker tells clients when they connect, beyond limits.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ConnectPolicy {
    /// Returned to MQTT 5 clients that ask for Response Information, with
    /// `%c` replaced by their client id: a topic prefix they may use for the
    /// responses of request/response exchanges.
    pub response_information: Option<String>,
    /// Send every client to another server instead of serving it.
    pub redirect: Option<Redirect>,
    /// Refuse clients that leave their client id for the broker to assign.
    pub require_client_id: bool,
    /// Starts the client ids the broker assigns.
    pub auto_id_prefix: String,
    /// When not empty, only client ids starting with one of these are
    /// accepted.
    pub client_id_prefixes: Vec<String>,
}

/// Where clients are sent instead of being served.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Redirect {
    /// The server clients should use, in the form MQTT 5 clients expect,
    /// such as `host:port`
    pub server_reference: String,
    /// Whether the move is permanent (Server Moved) or temporary (Use Another
    /// Server)
    pub permanent: bool,
}

impl Redirect {
    pub fn new(server_reference: impl Into<String>, permanent: bool) -> Self {
        Redirect {
            server_reference: server_reference.into(),
            permanent,
        }
    }
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
    /// Connections per client address, for `max_connections_per_ip`
    address_slots: Arc<AddressSlots>,
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
    cert_identity: Option<String>,
    /// An in-process connection, from the local listener
    local: bool,
    connect_deadline: Instant,
    /// Dropped when the handler finishes, so shutdown can wait for it
    _shutdown_complete: mpsc::Sender<()>,
}

/// A packet for logs: its type, and for a PUBLISH its topic and size,
/// never the payload, which may carry secrets such as the passwords of
/// dynamic security commands.
fn describe(packet: &ControlPacket) -> String {
    match packet {
        ControlPacket::Publish(publish) => format!(
            "PUBLISH on `{}` ({} bytes)",
            publish.topic_name,
            publish.payload.as_ref().map_or(0, |p| p.len())
        ),
        ControlPacket::Connect(_) => "CONNECT".into(),
        ControlPacket::ConnAck(_) => "CONNACK".into(),
        ControlPacket::PubAck(_) => "PUBACK".into(),
        ControlPacket::PubRec(_) => "PUBREC".into(),
        ControlPacket::PubRel(_) => "PUBREL".into(),
        ControlPacket::PubComp(_) => "PUBCOMP".into(),
        ControlPacket::Subscribe(_) => "SUBSCRIBE".into(),
        ControlPacket::SubAck(_) => "SUBACK".into(),
        ControlPacket::Unsubscribe(_) => "UNSUBSCRIBE".into(),
        ControlPacket::UnsubAck(_) => "UNSUBACK".into(),
        ControlPacket::PingReq(_) => "PINGREQ".into(),
        ControlPacket::PingResp(_) => "PINGRESP".into(),
        ControlPacket::Disconnect(_) => "DISCONNECT".into(),
        ControlPacket::Auth(_) => "AUTH".into(),
    }
}

/// Checks credentials off the async threads, as password hashes take tens
/// of milliseconds of CPU and validators may block on I/O, and at most one
/// per core at a time, so a flood of logins queues instead of starving
/// connections.
async fn validate_credentials(
    validator: &Arc<dyn CredentialValidator>,
    username: &str,
    password: &[u8],
) -> bool {
    static CHECKS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let checks = CHECKS.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Arc::new(tokio::sync::Semaphore::new(cores))
    });
    let Ok(_permit) = checks.acquire().await else {
        return false;
    };
    let (validator, username, password) = (
        Arc::clone(validator),
        username.to_string(),
        password.to_vec(),
    );
    tokio::task::spawn_blocking(move || validator.validate(&username, &password))
        .await
        .unwrap_or(false)
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
        tls: tls_config,
        ..ListenerConfig::new(listener, Transport::Mqtt)
    };
    let endpoints = std::iter::once(main)
        .chain(listeners)
        .map(Endpoint::new)
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let auth_config = Arc::new(auth_config);

    let mut server = Listener {
        endpoints,
        local_listener,
        trusted_auth: Arc::new(AuthConfig {
            authorizer: Some(Arc::new(AllowAll)),
            ..Default::default()
        }),
        next_endpoint: 0,
        broker: Broker::new(Arc::clone(&storage)),
        session_manager_holder: SessionManagerDropGuard::new(storage, limits.queue),
        address_slots: Arc::default(),
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
    let restored_wills = session_manager.restore_from_storage(&server.broker).await?;
    for (client_id, will, due) in restored_wills {
        let broker = server.broker.clone();
        let session_manager = session_manager.clone();
        tokio::spawn(async move {
            time::sleep(due).await;
            if session_manager.take_restored_will(&client_id).await {
                publish_will(&broker, will, &client_id).await;
                session_manager.delete_will(&client_id);
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
    // What connections changed as they closed is stored before returning
    if time::timeout(Duration::from_secs(5), session_manager.flush())
        .await
        .is_err()
    {
        warn!("Storage still writing after waiting to shut down");
    }
    Ok(())
}

impl<St: MqttStore + 'static> Listener<St> {
    async fn run(&mut self) {
        loop {
            let accepted = self.accept().await;
            let peer_addr = match &accepted {
                Accepted::Tcp(socket, _) => socket.peer_addr().ok(),
                _ => None,
            };
            let endpoint = match &accepted {
                Accepted::Tcp(_, endpoint) => Some(*endpoint),
                #[cfg(unix)]
                Accepted::Unix(_, endpoint) => Some(*endpoint),
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
            let listener_slots = endpoint.and_then(|i| self.endpoints[i].connection_slots.clone());
            let listener_slot = match listener_slots {
                Some(slots) => match slots.try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        warn!(
                            "Listener connection limit reached, refusing {:?}",
                            peer_addr
                        );
                        continue;
                    }
                },
                None => None,
            };
            let slots = [slot, listener_slot];

            info!(target: CONNECTIONS, "Got a connection: {:?}", peer_addr);

            let parts = HandlerParts {
                broker: self.broker.clone(),
                session_manager: self.session_manager_holder.session_manager(),
                shutdown: Shutdown::new(self.notify_shutdown.subscribe()),
                shutdown_complete: self.shutdown_complete.clone(),
                auth_config: Arc::clone(&self.auth_config),
                policy: Arc::clone(&self.policy),
                limits: self.limits,
                stats: Arc::clone(&self.stats),
                cert_identity: None,
                mount_point: None,
                http_dir: None,
                local: false,
                connect_deadline: Instant::now() + self.limits.connect_timeout,
                address_slots: Arc::clone(&self.address_slots),
            };
            match accepted {
                Accepted::Tcp(socket, endpoint) => {
                    let settings = self.endpoints[endpoint].settings.clone();
                    tokio::spawn(
                        Box::new(parts).serve_accepted(socket, settings, peer_addr, slots),
                    );
                }
                #[cfg(unix)]
                Accepted::Unix(socket, endpoint) => {
                    let settings = self.endpoints[endpoint].settings.clone();
                    tokio::spawn(
                        Box::new(parts).serve_accepted(socket, settings, peer_addr, slots),
                    );
                }
                Accepted::Local(stream) => {
                    let parts = HandlerParts {
                        auth_config: Arc::clone(&self.trusted_auth),
                        local: true,
                        ..parts
                    };
                    tokio::spawn(async move {
                        let _slots = slots;
                        parts.serve(stream, Transport::Mqtt, None).await;
                    });
                }
            }
        }
    }

    async fn handle_connection<S>(handler: &mut Handler<St, S>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        handler
            .connection
            .set_max_packet_size(handler.limits.max_packet_size);

        let first_packet =
            time::timeout_at(handler.connect_deadline, handler.connection.read_packet()).await;

        // Boxed, as it is kept until the connection ends
        let first_packet = Box::new(first_packet);
        match *first_packet {
            // [MQTT-3.1.0-1]
            // After a Network Connection is established by a Client
            // to a Server, the first packet sent from the Client to
            // the Server MUST be a CONNECT packet.
            Ok(Ok(Some(ControlPacket::Connect(p)))) => {
                // Boxed: an async function's arguments take room in its state
                // for as long as it runs, the whole connection
                match handler.run(Box::new(p)).await {
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
        // Boxed: it runs once, at the end, but would take room in this
        // state for the whole connection
        Box::pin(handler.connection.close()).await;
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
                    let polled = match &self.endpoints[index].listener {
                        Socket::Tcp(listener) => listener
                            .poll_accept(cx)
                            .map_ok(|(socket, _)| Accepted::Tcp(socket, index)),
                        #[cfg(unix)]
                        Socket::Unix(listener) => listener
                            .poll_accept(cx)
                            .map_ok(|(socket, _)| Accepted::Unix(socket, index)),
                    };
                    if polled.is_ready() {
                        self.next_endpoint = (index + 1) % count;
                        return polled;
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
    async fn run(&mut self, mut connect_packet: Box<ConnectPacket>) -> Result<()> {
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
        if let Some(identity) = self.cert_identity.clone() {
            // Verified by the TLS handshake, in place of any credentials
            verified_user = Some(identity);
        } else if let Some(method) = authentication_method {
            // Boxed, like the rest of the connect phase, so its state isn't
            // kept for the whole connection
            match Box::pin(self.authenticate_enhanced(&connect_packet, &method)).await? {
                Some(username) => verified_user = username,
                None => return Ok(()),
            }
        } else if self.auth_config.require_auth {
            let authenticated = match (
                &connect_packet.payload.user_name,
                &connect_packet.payload.password,
            ) {
                (Some(username), Some(password)) => {
                    if let Some(validator) = &self.auth_config.credential_validator {
                        let valid = validate_credentials(validator, username, password).await;
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

        if let Some(reason) = self.refused_will(&connect_packet) {
            return self.reject_connect(&connect_packet, reason).await;
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

        let assigned_client_id = connect_packet.payload.client_id.is_empty();
        if assigned_client_id && self.policy.require_client_id {
            return self
                .reject_connect(&connect_packet, ReasonCode::ClientIdentifierNotValid)
                .await;
        }
        if assigned_client_id {
            // Unique, so such clients never share a session entry
            connect_packet.payload.client_id = format!(
                "{}{}",
                self.policy.auto_id_prefix,
                Uuid::new_v4().hyphenated()
            );
        }
        let prefixes = &self.policy.client_id_prefixes;
        if !prefixes.is_empty()
            && !prefixes.iter().any(|prefix| {
                connect_packet
                    .payload
                    .client_id
                    .starts_with(prefix.as_str())
            })
        {
            warn!(
                "Client id `{}` has none of the allowed prefixes",
                connect_packet.payload.client_id
            );
            return self
                .reject_connect(&connect_packet, ReasonCode::NotAuthorized)
                .await;
        }
        // A will is published on the client's behalf, so it needs the same
        // permission as a PUBLISH, checked once the client id is final
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

        // In-process clients don't reconnect, so a network client taking
        // over their id would end them for good
        let client_id = &connect_packet.payload.client_id;
        if !self.local && self.session_manager.is_held_locally(client_id) {
            warn!("Client id `{}` belongs to an in-process client", client_id);
            return self
                .reject_connect(&connect_packet, ReasonCode::NotAuthorized)
                .await;
        }

        let keepalive = effective_keepalive(&connect_packet, &self.limits);
        let protocol_version = connect_packet.protocol_version;
        let client_maximum_packet_size = connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.maximum_packet_size.as_ref())
            .map(|m| m.value);
        cap_session_expiry(&mut connect_packet, &self.limits);
        let mut expiry = session_expiry(&connect_packet);
        if expiry.is_none() && !protocol_version.supports_properties() {
            expiry = self.limits.persistent_client_expiration;
            if let Some(max) = self.limits.max_session_expiry {
                expiry = Some(expiry.map_or(max, |expiry| expiry.min(max)));
            }
        }

        let owner = SessionOwner {
            username: verified_user.clone(),
            mount_point: self.connection.mount_point().map(str::to_string),
        };
        let mut start_result =
            Box::pin(self.session_manager.start_session(*connect_packet, owner)).await?;
        start_result.assigned_client_id |= assigned_client_id;
        let mut session = start_result.session;
        let mut lease = start_result.lease;
        let held_locally = match self.local {
            true => Some(session.get_client_id().await),
            false => None,
        };
        if let Some(client_id) = &held_locally {
            self.session_manager.hold_locally(client_id);
        }

        session.set_verified_user(verified_user).await;

        let result = async {
            Box::pin(session.begin(
                &mut self.connection,
                start_result.resume,
                start_result.assigned_client_id,
                &self.limits,
                &self.policy,
            ))
            .await?;

            // Restore subscriptions if resuming a persisted session
            if !start_result.subscriptions_to_restore.is_empty() {
                Box::pin(
                    session
                        .restore_subscriptions(start_result.subscriptions_to_restore, &self.broker),
                )
                .await?;
            }

            // Resend whatever the client hadn't acknowledged when it left
            session
                .restore_inflight(start_result.inflight_to_retry)
                .await;
            for packet in session.pending_retransmissions().await {
                Box::pin(self.connection.write_packet(packet)).await?;
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
                let disconnect = ControlPacket::Disconnect(DisconnectPacket {
                    reason: *reason,
                    properties: Some(DisconnectProperties {
                        reason_string: reason_string(*reason, client_maximum_packet_size),
                        ..Default::default()
                    }),
                });
                let _ = Box::pin(self.connection.write_packet(disconnect)).await;
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
                session_manager.delete_will(&client_id);
            };

            if delay.is_zero() {
                Box::pin(publish).await;
            } else {
                tokio::spawn(publish);
            }
        }

        if let Some(client_id) = &held_locally {
            self.session_manager.release_locally(client_id);
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
            let response = time::timeout_at(self.connect_deadline, self.connection.read_packet())
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
    // Boxed, as are the other refusals: they are rare, and inline they would
    // take room in the state of every connection
    fn redirect<'a>(
        &'a mut self,
        connect_packet: &'a ConnectPacket,
        redirect: &'a Redirect,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
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
                        server_reference: Some(ServerReference::new(
                            redirect.server_reference.clone(),
                        )),
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
        })
    }

    /// Why a will the broker can't honour is refused [MQTT-3.2.2-12]
    /// [MQTT-3.2.2-13]
    fn refused_will(&self, connect_packet: &ConnectPacket) -> Option<ReasonCode> {
        let flags = &connect_packet.flags;
        if !flags.will_flag {
            return None;
        }
        if flags.will_qos as u8 > self.limits.maximum_qos as u8 {
            return Some(ReasonCode::QoSNotSupported);
        }
        if flags.will_retain && !self.limits.retain_available {
            return Some(ReasonCode::RetainNotSupported);
        }
        // A will is published like a PUBLISH, so its topic must be one;
        // `$` topics belong to the broker
        let topic = connect_packet
            .payload
            .will_topic
            .as_deref()
            .unwrap_or_default();
        if validate_publish_topic(topic).is_err() || topic.starts_with('$') {
            return Some(ReasonCode::TopicNameInvalid);
        }
        None
    }

    fn reject_connect<'a>(
        &'a mut self,
        connect_packet: &'a ConnectPacket,
        reason_code: ReasonCode,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
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
        })
    }

    async fn handle_connection(
        &mut self,
        session: &Session,
        keepalive: u16,
        lease: &mut ConnectionLease,
        protocol_version: ProtocolVersion,
        expiry: &mut Option<Duration>,
    ) -> Result<()> {
        let session = session.clone();

        // Keep-alive timeout is 1.5x the keepalive interval; 0 disables it
        let timeout_duration =
            (keepalive > 0).then(|| Duration::from_millis(keepalive as u64 * 1500));

        let deadline = time::sleep(timeout_duration.unwrap_or_default());
        tokio::pin!(deadline);

        let client_id = session.get_client_id().await;
        while !self.shutdown.is_shutdown() {
            // Re-evaluated after every packet, so acknowledgements reopen it
            let window_open = session.can_send_more().await;

            // Only waiting happens here, and the work an event brings runs in
            // a boxed future while it lasts: an idle connection holds just
            // what waiting takes, not the state of its heaviest handling
            let step = tokio::select! {
                // So are the packets most clients send, PUBLISH and its
                // acknowledgements; the rest are boxed
                packet = self.connection.read_packet() => match HotPacket::try_from(packet?) {
                    Ok(packet) => Step::Done(self.handle_hot(&session, &client_id, packet).await?),
                    Err(packet) => Step::Wake(Wake::Packet(packet)),
                },
                // The common case, a message to send, isn't boxed: it would
                // cost an allocation per message
                message = session.next_outgoing(window_open) => {
                    Step::Done(self.send_outgoing(&session, &client_id, message).await?)
                }
                _ = &mut deadline, if timeout_duration.is_some() => Step::Wake(Wake::KeepAliveExpired),
                Ok(()) = lease.takeover.changed() => Step::Wake(Wake::TakenOver),
                _ = self.shutdown.recv() => {
                    return Err(ReasonCode::ServerShuttingDown.into());
                }
            };
            let flow = match step {
                Step::Done(flow) => flow,
                Step::Wake(wake) => {
                    Box::pin(self.handle_wake(
                        wake,
                        &session,
                        timeout_duration,
                        protocol_version,
                        expiry,
                    ))
                    .await?
                }
            };
            match flow {
                Flow::Continue => {}
                Flow::PacketHandled => {
                    // Reset keep-alive deadline after receiving any packet
                    if let Some(timeout) = timeout_duration {
                        deadline.as_mut().reset(Instant::now() + timeout);
                    }
                }
                Flow::Close => return Ok(()),
            }
        }

        Ok(())
    }

    /// Handles the packets clients send most, without boxing, as
    /// `handle_wake` does the rest.
    async fn handle_hot(
        &mut self,
        session: &Session,
        client_id: &str,
        packet: HotPacket,
    ) -> Result<Flow> {
        let mut session = session.clone();
        // Packet identifiers are never 0 [MQTT-2.2.1-3]
        if packet.packet_id() == Some(0) {
            return Err(ReasonCode::ProtocolError.into());
        }
        let response = match packet {
            HotPacket::Publish(publish) => {
                Stats::add(&self.stats.messages_received);
                let authorizer = self.auth_config.authorizer.as_deref();
                session
                    .handle_publish(publish, &self.broker, authorizer)
                    .await?
            }
            HotPacket::PubAck(ack) => {
                let packet_id = ack.packet_id;
                let response = session.handle_puback(ack).await?;
                self.session_manager
                    .remove_inflight(client_id, packet_id)
                    .await;
                response
            }
            HotPacket::PubRec(rec) => {
                let (packet_id, failed) = (rec.packet_id, rec.reason.get_code() >= 0x80);
                let response = session.handle_pubrec(rec).await?;
                if failed {
                    self.session_manager
                        .remove_inflight(client_id, packet_id)
                        .await;
                }
                // After PUBREC only PUBREL remains to be sent for a QoS 2 message
                if let Some(ControlPacket::PubRel(pubrel)) = &response {
                    if pubrel.reason == ReasonCode::Success {
                        self.session_manager
                            .release_inflight(client_id, pubrel.packet_id)
                            .await;
                    }
                }
                response
            }
            HotPacket::PubRel(rel) => session.handle_pubrel(rel).await?,
            HotPacket::PubComp(comp) => {
                let packet_id = comp.packet_id;
                let response = session.handle_pubcomp(comp).await?;
                self.session_manager
                    .remove_inflight(client_id, packet_id)
                    .await;
                response
            }
            HotPacket::PingReq => Some(ControlPacket::PingResp(PingRespPacket {})),
        };
        if let Some(response) = response {
            debug!("Sending {} to client {:?}", describe(&response), client_id);
            self.connection.write_packet(response).await?;
        }
        Ok(Flow::PacketHandled)
    }

    /// Sends a message the session queued for the client.
    async fn send_outgoing(
        &mut self,
        session: &Session,
        client_id: &str,
        message: Message,
    ) -> Result<Flow> {
        let mut session = session.clone();
        // Checked per message: a permitted subscription can still
        // match topics the client may not read
        if let Some(authorizer) = &self.auth_config.authorizer {
            // Re-read each time: re-authentication can change the user
            let identity = session.identity().await;
            if !authorizer.authorize(&identity, &message.topic, Access::Read) {
                return Ok(Flow::Continue);
            }
        }
        // Encoded within this block, so only its bytes wait while the
        // in-flight record is stored, not the packet
        let (write, inflight) = {
            let Some(Outgoing { packet, inflight }) = session.prepare_publish(message).await else {
                return Ok(Flow::Continue);
            };
            debug!("Sending {} to client {:?}", describe(&packet), client_id);
            (self.connection.write_packet(packet), inflight)
        };
        if let Some(inflight) = inflight {
            self.session_manager
                .store_inflight(client_id, &inflight)
                .await;
        }
        write.await?;
        Stats::add(&self.stats.messages_sent);
        Ok(Flow::Continue)
    }

    /// Does what `wake` calls for; see `handle_connection`.
    async fn handle_wake(
        &mut self,
        wake: Wake,
        session: &Session,
        timeout_duration: Option<Duration>,
        protocol_version: ProtocolVersion,
        expiry: &mut Option<Duration>,
    ) -> Result<Flow> {
        let mut session = session.clone();
        match wake {
            Wake::Packet(maybe_packet) => {
                let packet = match maybe_packet {
                    None => {
                        // Connection closed without DISCONNECT - abnormal
                        return Ok(Flow::Close);
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
                            self.session_manager.delete_will(&client_id);
                        }
                        return Ok(Flow::Close);
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
                    )
                    .await?;

                // Persist subscriptions after subscribe/unsubscribe operations
                if is_subscription_change {
                    let client_id = session.get_client_id().await;
                    let subscriptions = session.get_subscriptions().await;
                    self.session_manager
                        .save_subscriptions(&client_id, subscriptions)
                        .await;
                }

                // Remove inflight message after acknowledgment
                if let Some(packet_id) = inflight_completed {
                    let client_id = session.get_client_id().await;
                    self.session_manager
                        .remove_inflight(&client_id, packet_id)
                        .await;
                }

                // After PUBREC only PUBREL remains to be sent for a QoS 2 message
                if let Some(ControlPacket::PubRel(ref pubrel)) = maybe_res {
                    if pubrel.reason == ReasonCode::Success {
                        let client_id = session.get_client_id().await;
                        self.session_manager
                            .release_inflight(&client_id, pubrel.packet_id)
                            .await;
                    }
                }

                if let Some(res) = maybe_res {
                    debug!(
                        "Sending {} to client {:?}",
                        describe(&res),
                        session.get_client_id().await
                    );
                    self.connection.write_packet(res).await?;
                }

                Ok(Flow::PacketHandled)
            }
            Wake::KeepAliveExpired => {
                let client_id = session.get_client_id().await;
                warn!(
                    "Keep-alive timeout for client `{}` ({:?} with no packets)",
                    client_id,
                    timeout_duration.unwrap_or_default()
                );
                // An abnormal disconnect: the will is published
                Err(ReasonCode::KeepAliveTimeout.into())
            }
            Wake::TakenOver => {
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
                Ok(Flow::Close)
            }
        }
    }
}

/// Connections each client address holds.
#[derive(Default)]
struct AddressSlots(std::sync::Mutex<HashMap<IpAddr, usize>>);

impl AddressSlots {
    /// A slot for `ip`, unless it holds `max` already.
    fn acquire(self: &Arc<Self>, ip: IpAddr, max: usize) -> Option<AddressSlot> {
        let mut counts = self.0.lock().unwrap();
        let count = counts.entry(ip).or_default();
        if *count >= max {
            return None;
        }
        *count += 1;
        Some(AddressSlot {
            slots: Arc::clone(self),
            ip,
        })
    }
}

/// One connection's place in its address's count, freed when dropped.
struct AddressSlot {
    slots: Arc<AddressSlots>,
    ip: IpAddr,
}

impl Drop for AddressSlot {
    fn drop(&mut self) {
        let mut counts = self.slots.0.lock().unwrap();
        if let Some(count) = counts.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

/// An address, or a block of them in CIDR form, such as `10.0.0.0/8`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressRange {
    network: IpAddr,
    prefix: u8,
}

impl AddressRange {
    pub fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 client may reach an IPv6 socket as a mapped address
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.network, ip) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(network) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(network) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

impl std::str::FromStr for AddressRange {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, String> {
        let invalid = || format!("`{text}` is not an address or a CIDR block");
        let (address, prefix) = text.split_once('/').unwrap_or((text, ""));
        let network: IpAddr = address.parse().map_err(|_| invalid())?;
        let bits = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            "" => bits,
            prefix => prefix
                .parse()
                .ok()
                .filter(|p| *p <= bits)
                .ok_or_else(invalid)?,
        };
        Ok(AddressRange { network, prefix })
    }
}

/// A connection after its handshakes, with TLS or without.
enum Secured<S> {
    Plain(S),
    Tls(Box<tokio_rustls::server::TlsStream<S>>),
}

/// What woke a connection up, beyond a message to send. Only exists
/// between waking and the boxed handling, so its size costs nothing while
/// idle, and boxing the packet would cost an allocation per packet.
#[allow(clippy::large_enum_variant)]
enum Wake {
    Packet(Option<ControlPacket>),
    KeepAliveExpired,
    TakenOver,
}

/// The packets clients send most, handled without boxing.
enum HotPacket {
    Publish(PublishPacket),
    PubAck(PubAckPacket),
    PubRec(PubRecPacket),
    PubRel(PubRelPacket),
    PubComp(PubCompPacket),
    PingReq,
}

impl HotPacket {
    fn packet_id(&self) -> Option<u16> {
        match self {
            HotPacket::Publish(p) => p.packet_id,
            HotPacket::PubAck(p) => Some(p.packet_id),
            HotPacket::PubRec(p) => Some(p.packet_id),
            HotPacket::PubRel(p) => Some(p.packet_id),
            HotPacket::PubComp(p) => Some(p.packet_id),
            HotPacket::PingReq => None,
        }
    }

    /// The packet back, as read, when it isn't one of these: not an error,
    /// just the other way, so its size doesn't matter.
    #[allow(clippy::result_large_err)]
    fn try_from(packet: Option<ControlPacket>) -> std::result::Result<Self, Option<ControlPacket>> {
        Ok(match packet {
            Some(ControlPacket::Publish(p)) => HotPacket::Publish(p),
            Some(ControlPacket::PubAck(p)) => HotPacket::PubAck(p),
            Some(ControlPacket::PubRec(p)) => HotPacket::PubRec(p),
            Some(ControlPacket::PubRel(p)) => HotPacket::PubRel(p),
            Some(ControlPacket::PubComp(p)) => HotPacket::PubComp(p),
            Some(ControlPacket::PingReq(_)) => HotPacket::PingReq,
            other => return Err(other),
        })
    }
}

/// A wake-up either handled already or to be handled.
#[allow(clippy::large_enum_variant)]
enum Step {
    Done(Flow),
    Wake(Wake),
}

/// What a connection does once it handled what woke it.
enum Flow {
    Continue,
    /// A packet came in, so the keep-alive deadline starts over
    PacketHandled,
    Close,
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
        origin: None,
    };

    if let Err(e) = broker.publish(&topic, message).await {
        error!("Failed to publish will message: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_address_ranges() {
        let range: AddressRange = "10.0.0.0/8".parse().unwrap();
        assert!(range.contains("10.200.0.1".parse().unwrap()));
        assert!(!range.contains("11.0.0.1".parse().unwrap()));
        // An IPv4 client on an IPv6 socket
        assert!(range.contains("::ffff:10.0.0.1".parse().unwrap()));

        let single: AddressRange = "192.0.2.7".parse().unwrap();
        assert!(single.contains("192.0.2.7".parse().unwrap()));
        assert!(!single.contains("192.0.2.8".parse().unwrap()));

        let v6: AddressRange = "2001:db8::/32".parse().unwrap();
        assert!(v6.contains("2001:db8:1::1".parse().unwrap()));
        assert!(!v6.contains("2001:db9::1".parse().unwrap()));

        let everything: AddressRange = "0.0.0.0/0".parse().unwrap();
        assert!(everything.contains("203.0.113.1".parse().unwrap()));

        for invalid in ["10.0.0.0/33", "example.com", "10.0.0.0/x", ""] {
            assert!(invalid.parse::<AddressRange>().is_err(), "{invalid}");
        }
    }
}
