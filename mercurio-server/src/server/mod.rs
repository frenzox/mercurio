//! Running the broker: [`Server`] and its [`ServerBuilder`], its
//! configuration and limits, and in-process connections.

use std::{
    future::{poll_fn, Future},
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::Poll,
};

use crate::error::{ConfigError, Error, Result, ServerError};
use crate::CONNECTIONS;
use futures_util::FutureExt;

#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::{
    io::{AsyncRead, AsyncWrite, DuplexStream},
    net::TcpStream,
    sync::{broadcast, mpsc, OwnedSemaphorePermit, Semaphore},
    time::{self, Duration, Instant},
};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use mercurio_core::{
    message::Message,
    properties::{AuthenticationData, AuthenticationMethod, ReasonString, ServerReference},
    protocol::ProtocolVersion,
    reason::ReasonCode,
    topic::validate_publish_topic,
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
use mercurio_storage::{memory::MemoryStore, MqttStore, SessionKey, SessionOwner, Write};

use crate::{
    acl::Access,
    admin::{Admin, AdminExtension, AdminParts, ListenerInfo},
    bans::{Bans, Candidate, FailedLogins},
    broker::Broker,
    connection::Connection,
    http,
    plugin::{
        guarded, AuthResult, Chain, ClientInfo, ConnectionInfo, Credentials, Decision, Event,
        Plugin,
    },
    proxy,
    session::{
        cap_expiry_interval, cap_session_expiry, effective_keepalive, Handled, Outgoing, Session,
        WillMessage,
    },
    session_manager::{session_expiry, Close, ConnectionLease, SessionManager, StartError},
    shutdown::{Shutdown, Tasks},
    sys::{self, ConnectedGuard, DropCause, Stats},
    tls::{certificate_name, Tls},
    websocket,
};

mod address;
mod config;
mod pacing;

pub(crate) use address::address_key;
pub use address::AddressRange;
use address::{AddressSlot, AddressSlots};
use pacing::{Bucket, Pacing};

#[cfg(unix)]
pub use config::PeerFilter;
pub use config::{
    AuthConfig, ConnectPolicy, Limits, ListenerConfig, LocalConnector, LocalListener,
    LoginFailures, Overflow, QueueLimits, Rate, Redirect, ServerConfig, ShareRedelivery,
    ShareStrategy, Socket, Transport, DEFAULT_MAX_CONNECT_PACKET_SIZE, DEFAULT_MAX_INFLIGHT,
    DEFAULT_MAX_PACKET_SIZE, DEFAULT_MAX_SUBSCRIPTIONS, DEFAULT_MAX_TOPIC_FILTER_LENGTH,
};

/// A connection waiting to be served, with the index of its endpoint.
enum Accepted {
    Tcp(TcpStream, usize),
    #[cfg(unix)]
    Unix(UnixStream, usize),
    Local(DuplexStream),
}

/// A listener ready to accept connections.
struct Endpoint {
    listener: Socket,
    settings: EndpointSettings,
    /// One permit per allowed concurrent connection, when limited
    connection_slots: Option<Arc<Semaphore>>,
    state: Arc<ListenerState>,
    /// Its connections get the trusted chain, set by `ServerBuilder::build`
    trusted: bool,
    /// The broker's limits with the listener's own settings, set by
    /// `ServerBuilder::build`
    limits: Limits,
    /// The settings of the listener that replace the broker's
    overrides: LimitOverrides,
    /// New connections allowed, when limited
    connection_rate: Option<Bucket>,
    #[cfg(unix)]
    peer_filter: Option<PeerFilter>,
}

/// A listener as the admin API reports it, with its open connections.
pub(crate) struct ListenerState {
    /// How its clients' listener is reported: its address or path
    pub name: Arc<str>,
    pub info: ListenerInfo,
    pub connections: AtomicUsize,
}

/// One connection in its listener's count, until dropped.
struct ListenerSlot(Arc<ListenerState>);

impl ListenerSlot {
    fn new(state: &Arc<ListenerState>) -> Self {
        state.connections.fetch_add(1, Ordering::Relaxed);
        ListenerSlot(Arc::clone(state))
    }
}

impl Drop for ListenerSlot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::Relaxed);
    }
}

/// How the connections of a listener are served.
#[derive(Clone)]
struct EndpointSettings {
    transport: Transport,
    tls: Option<Tls>,
    proxy_protocol: bool,
    proxy_from: Arc<[AddressRange]>,
    mount_point: Option<String>,
    auth: Option<Arc<AuthConfig>>,
    chain: Option<Chain>,
    http_site: Option<Arc<http::Site>>,
    allowed_origins: Option<Arc<[String]>>,
    use_username_as_clientid: bool,
}

/// A listener's own limits, replacing the broker's.
#[derive(Clone, Copy)]
struct LimitOverrides {
    max_packet_size: Option<usize>,
    max_connect_packet_size: Option<usize>,
    max_keepalive: Option<u16>,
    connect_timeout: Option<Duration>,
}

impl LimitOverrides {
    fn apply(&self, mut limits: Limits) -> Limits {
        if let Some(max) = self.max_packet_size {
            limits.max_packet_size = max;
        }
        if let Some(max) = self.max_connect_packet_size {
            limits.max_connect_packet_size = max;
        }
        if let Some(max) = self.max_keepalive {
            limits.max_keepalive = Some(max);
        }
        if let Some(timeout) = self.connect_timeout {
            limits.connect_timeout = timeout;
        }
        limits
    }
}

impl Endpoint {
    /// The listener `config`, the `index`th of the broker, once its
    /// settings are checked.
    fn new(
        index: usize,
        config: ListenerConfig,
        timeout: Duration,
    ) -> std::result::Result<Self, ConfigError> {
        let (address, path) = match &config.listener {
            Socket::Tcp(listener) => (listener.local_addr().ok().map(|a| a.to_string()), None),
            #[cfg(unix)]
            Socket::Unix(listener) => (
                None,
                listener
                    .local_addr()
                    .ok()
                    .and_then(|a| a.as_pathname().map(|p| p.display().to_string())),
            ),
        };
        let name = address.clone().or_else(|| path.clone()).unwrap_or_default();
        if let Some(kind) = config.problem() {
            return Err(ConfigError {
                listener: index,
                name,
                kind,
            });
        }
        let state = Arc::new(ListenerState {
            name: name.into(),
            info: ListenerInfo {
                address,
                path,
                protocol: match config.transport {
                    Transport::Mqtt => "mqtt",
                    Transport::WebSocket => "websocket",
                }
                .to_string(),
                tls: config.tls.is_some(),
                mount_point: config.mount_point.clone(),
                connections: 0,
            },
            connections: AtomicUsize::new(0),
        });
        Ok(Self {
            state,
            trusted: config.trusted,
            limits: Limits::default(),
            connection_rate: config.max_connection_rate.map(Bucket::new),
            overrides: LimitOverrides {
                max_packet_size: config.max_packet_size,
                max_connect_packet_size: config.max_connect_packet_size,
                max_keepalive: config.max_keepalive,
                connect_timeout: config.connect_timeout,
            },
            #[cfg(unix)]
            peer_filter: config.peer_filter,
            listener: config.listener,
            connection_slots: config.max_connections.map(connection_slots),
            settings: EndpointSettings {
                transport: config.transport,
                tls: config.tls,
                proxy_protocol: config.proxy_protocol,
                proxy_from: config.proxy_from.into(),
                mount_point: config.mount_point,
                // Trusted clients log in as in-process ones do
                auth: match config.trusted {
                    true => Some(Arc::new(AuthConfig::default())),
                    false => config.auth.map(Arc::new),
                },
                chain: config.plugins.map(|p| Chain::new(p, timeout)),
                http_site: (config.http_dir.is_some() || config.health || config.metrics).then(
                    || {
                        Arc::new(http::Site::new(
                            config.http_dir,
                            config.health,
                            config.metrics,
                        ))
                    },
                ),
                allowed_origins: config.allowed_origins.map(Into::into),
                use_username_as_clientid: config.use_username_as_clientid,
            },
        })
    }
}

impl Endpoint {
    /// Whether a client of this Unix socket may be served: always without a
    /// peer filter, otherwise only when its credentials can be read and the
    /// filter accepts them. A filter that panics refuses.
    #[cfg(unix)]
    fn admits(&self, socket: &UnixStream) -> bool {
        let Some(filter) = &self.peer_filter else {
            return true;
        };
        let peer = match socket.peer_cred() {
            Ok(peer) => peer,
            Err(e) => {
                warn!(
                    "Closing a client of {}: its credentials can't be read: {}",
                    self.state.name, e
                );
                return false;
            }
        };
        let admitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| filter(&peer)));
        if !matches!(admitted, Ok(true)) {
            warn!(
                "Closing a client of {}: user {} (process {:?}) may not connect",
                self.state.name,
                peer.uid(),
                peer.pid()
            );
            return false;
        }
        true
    }
}

/// Everything a connection handler needs besides its stream.
struct HandlerParts<St: MqttStore + 'static> {
    broker: Broker<St>,
    session_manager: SessionManager<St>,
    shutdown: Shutdown,
    shutdown_complete: mpsc::Sender<()>,
    auth_config: Arc<AuthConfig>,
    chain: Chain,
    policy: Arc<ConnectPolicy>,
    limits: Limits,
    stats: Arc<Stats>,
    /// The client certificate's Common Name, when it stands in for the
    /// user name
    cert_identity: Option<String>,
    /// Whether the connection is TLS
    tls: bool,
    mount_point: Option<String>,
    http_site: Option<Arc<http::Site>>,
    allowed_origins: Option<Arc<[String]>>,
    /// An in-process connection, from the local listener
    local: bool,
    /// When the PROXY header, the TLS and WebSocket handshakes and CONNECT
    /// must all be done by; one deadline, so a client trickling bytes can't
    /// take the time allowed once per step
    connect_deadline: Instant,
    address_slots: Arc<AddressSlots>,
    /// Reported as the client's listener: its address or path
    listener: Arc<str>,
    bans: Arc<Bans>,
    use_username_as_clientid: bool,
    /// Where its failed logins are counted; None when they aren't
    failed_logins: Option<Arc<FailedLogins>>,
    /// Where tasks outliving the connection run, such as a delayed will
    tasks: Tasks,
}

impl<St: MqttStore + 'static> HandlerParts<St> {
    /// Serves a connection accepted on a listener, after the PROXY header
    /// and the TLS handshake if the listener has them. `_slots` and
    /// `_listener` are held for as long as it runs, and `address_slot` too;
    /// on a PROXY listener that slot is taken here instead, once the header
    /// gave the client's own address.
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
        _listener: ListenerSlot,
        address_slot: Option<AddressSlot>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let transport = settings.transport;
        let proxied = settings.proxy_protocol;
        let Some((parts, stream, peer_addr)) =
            Box::pin(self.handshake(socket, settings, peer_addr)).await
        else {
            return;
        };
        let _address_slot = match (proxied, parts.limits.max_connections_per_ip, peer_addr) {
            (true, Some(max), Some(peer)) => match parts.address_slots.acquire(peer.ip(), max) {
                Some(slot) => Some(slot),
                None => {
                    return warn!("{} holds the most connections allowed, closing", peer.ip());
                }
            },
            _ => address_slot,
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
                warn!(target: CONNECTIONS, "{:?} may not send a PROXY header, closing", peer_addr);
                return None;
            }
            let header = proxy::read_header(&mut socket);
            match connect_step(&mut self.shutdown, self.connect_deadline, header).await? {
                Ok(Ok(source)) => {
                    debug!(target: CONNECTIONS, "{:?} is proxying {:?}", peer_addr, source);
                    peer_addr = source.or(peer_addr);
                    if let Some(peer) = peer_addr.filter(|p| self.bans.refuses_address(p.ip())) {
                        warn!("{} is banned, closing", peer.ip());
                        return None;
                    }
                }
                Ok(Err(e)) => {
                    warn!(target: CONNECTIONS, "Bad PROXY header from {:?}: {}", peer_addr, e);
                    return None;
                }
                Err(_) => {
                    debug!(target: CONNECTIONS, "No PROXY header from {:?} in time", peer_addr);
                    return None;
                }
            }
        }
        self.mount_point = settings.mount_point;
        self.use_username_as_clientid = settings.use_username_as_clientid;
        self.http_site = settings.http_site;
        self.allowed_origins = settings.allowed_origins;
        if let Some(auth) = settings.auth {
            self.auth_config = auth;
        }
        if let Some(chain) = settings.chain {
            self.chain = chain;
        }
        let Some(tls) = settings.tls else {
            return Some((*self, Secured::Plain(socket), peer_addr));
        };
        let accept = tls.acceptor().accept(socket);
        match connect_step(&mut self.shutdown, self.connect_deadline, accept).await? {
            Ok(Ok(tls_stream)) => {
                debug!(target: CONNECTIONS, "TLS handshake completed for {:?}", peer_addr);
                // Clients without a certificate, where it is optional, log
                // in as on a listener that asks for none
                let certificate = tls_stream.get_ref().1.peer_certificates();
                if let (Some(part), Some(cert)) = (
                    tls.username_from_certificate(),
                    certificate.and_then(|c| c.first()),
                ) {
                    self.cert_identity = certificate_name(cert, part);
                    if self.cert_identity.is_none() {
                        warn!(
                            target: CONNECTIONS,
                            "No Common Name in the certificate of {:?}", peer_addr
                        );
                        return None;
                    }
                }
                self.tls = true;
                Some((*self, Secured::Tls(Box::new(tls_stream)), peer_addr))
            }
            Ok(Err(e)) => {
                debug!(target: CONNECTIONS, "TLS handshake failed for {:?}: {}", peer_addr, e);
                None
            }
            Err(_) => {
                debug!(target: CONNECTIONS, "TLS handshake timed out for {:?}", peer_addr);
                None
            }
        }
    }

    /// Reads the HTTP request on a WebSocket listener with a site to serve.
    /// A WebSocket upgrade is returned to complete; any other request is
    /// answered from `site`, under its own deadline as sending a file may
    /// take longer than connecting does, and None returned.
    async fn serve_http<S>(
        &mut self,
        stream: S,
        site: &http::Site,
        peer_addr: Option<SocketAddr>,
    ) -> Option<http::Rewind<S>>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let request = Box::pin(http::read_request(stream));
        let (stream, head) =
            match connect_step(&mut self.shutdown, self.connect_deadline, request).await? {
                Ok(Ok(http::Request::Upgrade(upgrade))) => return Some(upgrade),
                Ok(Ok(http::Request::Plain(stream, head))) => (stream, head),
                Ok(Err(e)) => {
                    debug!(target: CONNECTIONS, "HTTP request from {:?} failed: {}", peer_addr, e);
                    return None;
                }
                Err(_) => {
                    debug!(target: CONNECTIONS, "HTTP request from {:?} timed out", peer_addr);
                    return None;
                }
            };
        let deadline = Instant::now() + http::RESPONSE_TIMEOUT;
        let stopping = self.shutdown.is_signalled();
        let (broker, session_manager, stats) = (&self.broker, &self.session_manager, &self.stats);
        let page = |endpoint| async move {
            match endpoint {
                http::Endpoint::Health if stopping => http::Page {
                    status: "503 Service Unavailable",
                    content_type: "text/plain; charset=utf-8",
                    body: "stopping\n".into(),
                },
                http::Endpoint::Health => http::Page {
                    status: "200 OK",
                    content_type: "text/plain; charset=utf-8",
                    body: "ok\n".into(),
                },
                http::Endpoint::Metrics => {
                    let counters = sys::current(broker, session_manager, stats).await;
                    http::Page {
                        status: "200 OK",
                        content_type: "text/plain; version=0.0.4; charset=utf-8",
                        body: sys::prometheus(&counters, stats),
                    }
                }
            }
        };
        let response = Box::pin(http::respond(stream, &head, site, page));
        // Once stopping, the answer must not be cut short by it
        let answered = match stopping {
            true => Some(time::timeout_at(deadline, response).await),
            false => connect_step(&mut self.shutdown, deadline, response).await,
        };
        match answered? {
            Ok(Err(e)) => debug!(
                target: CONNECTIONS,
                "HTTP response to {:?} failed: {}", peer_addr, e
            ),
            Err(_) => debug!(target: CONNECTIONS, "HTTP response to {:?} timed out", peer_addr),
            Ok(Ok(())) => {}
        }
        None
    }

    /// Serves one client on `stream`, after the WebSocket handshake if it
    /// came in on the WebSocket listener.
    async fn serve<S>(mut self, stream: S, transport: Transport, peer_addr: Option<SocketAddr>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        match transport {
            Transport::Mqtt => Box::pin(self.handle(Connection::new(stream), peer_addr)).await,
            Transport::WebSocket => {
                let stream = match self.http_site.clone() {
                    Some(site) => match Box::pin(self.serve_http(stream, &site, peer_addr)).await {
                        Some(upgrade) => upgrade,
                        None => return,
                    },
                    None => http::Rewind::new(stream, Vec::new()),
                };
                // An MQTT packet may span messages, but no honest client
                // needs one larger than the largest packet
                let handshake = Box::pin(websocket::accept(
                    stream,
                    self.limits.max_packet_size,
                    self.allowed_origins.as_deref(),
                ));
                let handshake = connect_step(&mut self.shutdown, self.connect_deadline, handshake);
                let Some(handshake) = handshake.await else {
                    return;
                };
                match handshake {
                    Ok(Ok(websocket)) => {
                        let budget = websocket.read_budget();
                        let mut connection = Connection::new(websocket);
                        connection.set_read_budget(budget);
                        Box::pin(self.handle(connection, peer_addr)).await
                    }
                    Ok(Err(e)) => debug!(
                        target: CONNECTIONS,
                        "WebSocket handshake failed for {:?}: {}", peer_addr, e
                    ),
                    Err(_) => debug!(
                        target: CONNECTIONS,
                        "WebSocket handshake timed out for {:?}", peer_addr
                    ),
                }
            }
        }
    }

    async fn handle<S>(self, mut connection: Connection<S>, peer: Option<SocketAddr>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        connection.set_mount_point(self.mount_point.clone());
        connection.set_stats(Arc::clone(&self.stats));
        connection.set_write_timeout(self.limits.write_timeout);
        let mut handler = Handler {
            broker: self.broker,
            session_manager: self.session_manager,
            connection,
            shutdown: self.shutdown,
            auth_config: self.auth_config,
            chain: self.chain,
            peer,
            policy: self.policy,
            limits: self.limits,
            stats: self.stats,
            cert_identity: self.cert_identity,
            tls: self.tls,
            mount_point: self.mount_point,
            connection_info: None,
            local: self.local,
            connect_deadline: self.connect_deadline,
            listener: self.listener,
            bans: self.bans,
            use_username_as_clientid: self.use_username_as_clientid,
            failed_logins: self.failed_logins,
            tasks: self.tasks,
            _shutdown_complete: self.shutdown_complete,
        };
        Listener::<St>::handle_connection(&mut handler).await;
    }
}

struct Listener<St: MqttStore + 'static> {
    endpoints: Vec<Endpoint>,
    local_listener: LocalListener,
    /// Given to local connections instead of `chain`
    trusted_chain: Chain,
    /// Where the next accept starts looking, so a busy listener does not
    /// starve the others
    next_endpoint: usize,
    broker: Broker<St>,
    session_manager: SessionManager<St>,
    notify_shutdown: broadcast::Sender<()>,
    /// Cloned into every connection handler; see `Handler::_shutdown_complete`
    shutdown_complete: mpsc::Sender<()>,
    auth_config: Arc<AuthConfig>,
    chain: Chain,
    policy: Arc<ConnectPolicy>,
    stats: Arc<Stats>,
    limits: Limits,
    /// One permit per allowed concurrent connection, when limited
    /// Connections per client address, for `max_connections_per_ip`
    address_slots: Arc<AddressSlots>,
    connection_slots: Option<Arc<Semaphore>>,
    bans: Arc<Bans>,
    failed_logins: Option<Arc<FailedLogins>>,
    /// Where connections and everything else the server starts run
    tasks: Tasks,
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
    chain: Chain,
    /// The client's address, from the PROXY header when there is one
    peer: Option<SocketAddr>,
    policy: Arc<ConnectPolicy>,
    stats: Arc<Stats>,
    limits: Limits,
    cert_identity: Option<String>,
    tls: bool,
    mount_point: Option<String>,
    /// What plugins are told about the connection, once CONNECT is read
    connection_info: Option<Arc<ConnectionInfo>>,
    /// An in-process connection, from the local listener
    local: bool,
    connect_deadline: Instant,
    /// Reported as the client's listener: its address or path
    listener: Arc<str>,
    bans: Arc<Bans>,
    use_username_as_clientid: bool,
    failed_logins: Option<Arc<FailedLogins>>,
    tasks: Tasks,
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

/// Checks credentials, at most one login per core at a time, so a flood of
/// logins queues instead of starving the broker of CPU for password hashes.
/// Run as a `connect_step`, so a login still queued when the client's time
/// to connect is up gives up its place without a check.
async fn validate_credentials(
    chain: &Chain,
    client: &ClientInfo,
    credentials: &Credentials<'_>,
) -> Decision {
    static CHECKS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let checks = CHECKS.get_or_init(|| {
        let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
        Arc::new(tokio::sync::Semaphore::new(cores))
    });
    let Ok(_permit) = checks.acquire().await else {
        return Decision::Deny;
    };
    chain.authenticate(client, credentials).await
}

/// One permit per connection allowed. A semaphore holds fewer than
/// `usize::MAX` permits; a limit beyond that many is out of reach anyway.
fn connection_slots(max: usize) -> Arc<Semaphore> {
    Arc::new(Semaphore::new(max.min(Semaphore::MAX_PERMITS)))
}

/// Runs one step of connecting, such as a handshake or reading CONNECT,
/// within the connect deadline. None if the server starts shutting down
/// first: a client that isn't connected has nothing to be told, so it is
/// just closed.
async fn connect_step<F: Future>(
    shutdown: &mut Shutdown,
    deadline: Instant,
    step: F,
) -> Option<std::result::Result<F::Output, time::error::Elapsed>> {
    tokio::select! {
        result = time::timeout_at(deadline, step) => Some(result),
        _ = shutdown.recv() => None,
    }
}

/// Every plugin of the broker and of its listeners, each once.
fn distinct_plugins(
    main: &[Arc<dyn Plugin>],
    listeners: &[Option<Vec<Arc<dyn Plugin>>>],
) -> Vec<Arc<dyn Plugin>> {
    let mut all: Vec<Arc<dyn Plugin>> = Vec::new();
    for plugin in main.iter().chain(listeners.iter().flatten().flatten()) {
        if !all.iter().any(|seen| Arc::ptr_eq(seen, plugin)) {
            all.push(Arc::clone(plugin));
        }
    }
    all
}

/// Sets a broker up: its listeners, storage and settings. Start from
/// [`Server::builder`]; [`ServerBuilder::build`] checks the configuration
/// and restores what storage holds, and [`ServerBuilder::run`] serves at
/// once.
pub struct ServerBuilder<St: MqttStore + 'static = MemoryStore> {
    config: ServerConfig,
    listeners: Vec<ListenerConfig>,
    local_listener: Option<LocalListener>,
    storage: Arc<St>,
}

impl Server<MemoryStore> {
    /// A broker without listeners, keeping its state in memory, with the
    /// default settings.
    pub fn builder() -> ServerBuilder<MemoryStore> {
        ServerBuilder {
            config: ServerConfig::default(),
            listeners: Vec::new(),
            local_listener: None,
            storage: Arc::new(MemoryStore::new()),
        }
    }
}

impl<St: MqttStore + 'static> ServerBuilder<St> {
    /// Keeps sessions, retained messages, wills and in-flight messages in
    /// `storage` instead of memory.
    pub fn storage<S: MqttStore + 'static>(self, storage: Arc<S>) -> ServerBuilder<S> {
        ServerBuilder {
            config: self.config,
            listeners: self.listeners,
            local_listener: self.local_listener,
            storage,
        }
    }

    /// Serves `listener` too. Listeners are numbered in the order they are
    /// added, from 0, in [`ConfigError`]s.
    #[must_use]
    pub fn listener(mut self, listener: ListenerConfig) -> Self {
        self.listeners.push(listener);
        self
    }

    /// Serves each of `listeners` too.
    #[must_use]
    pub fn listeners(mut self, listeners: impl IntoIterator<Item = ListenerConfig>) -> Self {
        self.listeners.extend(listeners);
        self
    }

    /// Accepts in-process connections from the [`LocalConnector`] paired
    /// with `listener`, for connectors needed before the broker is built.
    #[must_use]
    pub fn local_listener(mut self, listener: LocalListener) -> Self {
        self.local_listener = Some(listener);
        self
    }

    /// Replaces every broker-wide setting with `config`'s.
    #[must_use]
    pub fn config(mut self, config: ServerConfig) -> Self {
        self.config = config;
        self
    }

    /// Sets [`ServerConfig::auth`].
    #[must_use]
    pub fn with_auth(mut self, auth: AuthConfig) -> Self {
        self.config.auth = auth;
        self
    }

    /// Sets [`ServerConfig::limits`].
    #[must_use]
    pub fn with_limits(mut self, limits: Limits) -> Self {
        self.config.limits = limits;
        self
    }

    /// Sets [`ServerConfig::policy`].
    #[must_use]
    pub fn with_policy(mut self, policy: ConnectPolicy) -> Self {
        self.config.policy = policy;
        self
    }

    /// Sets [`ServerConfig::sys_interval`].
    #[must_use]
    pub fn with_sys_interval(mut self, interval: Option<Duration>) -> Self {
        self.config.sys_interval = interval;
        self
    }

    /// Sets [`ServerConfig::plugins`].
    #[must_use]
    pub fn with_plugins(mut self, plugins: Vec<Arc<dyn Plugin>>) -> Self {
        self.config.plugins = plugins;
        self
    }

    /// Sets [`ServerConfig::plugin_timeout`].
    #[must_use]
    pub fn with_plugin_timeout(mut self, timeout: Duration) -> Self {
        self.config.plugin_timeout = timeout;
        self
    }

    /// Sets [`ServerConfig::plugin_tick_interval`].
    #[must_use]
    pub fn with_plugin_tick_interval(mut self, interval: Duration) -> Self {
        self.config.plugin_tick_interval = interval;
        self
    }

    /// Sets [`ServerConfig::admin_extension`].
    #[must_use]
    pub fn with_admin_extension(mut self, extension: Arc<dyn AdminExtension>) -> Self {
        self.config.admin_extension = Some(extension);
        self
    }

    /// Sets [`ServerConfig::share_strategy`].
    #[must_use]
    pub fn with_share_strategy(mut self, strategy: ShareStrategy) -> Self {
        self.config.share_strategy = strategy;
        self
    }

    /// Sets [`ServerConfig::share_redelivery`].
    #[must_use]
    pub fn with_share_redelivery(mut self, redelivery: ShareRedelivery) -> Self {
        self.config.share_redelivery = redelivery;
        self
    }

    /// Builds the broker and serves it until `shutdown` completes; see
    /// [`ServerBuilder::build`] and [`Server::run`].
    pub async fn run(self, shutdown: impl Future) -> std::result::Result<(), ServerError> {
        self.build().await?.run(shutdown).await
    }

    /// Checks the configuration, binding nothing more, and restores the
    /// sessions, wills and bans storage holds. Fails before anyone is
    /// served if a listener's settings can't be served together, with a
    /// [`ServerError::Config`] naming it, or if storage can't be read.
    pub async fn build(self) -> std::result::Result<Server<St>, ServerError> {
        let ServerBuilder {
            config,
            listeners,
            local_listener,
            storage,
        } = self;
        let ServerConfig {
            auth: auth_config,
            limits,
            policy,
            sys_interval,
            plugins,
            plugin_timeout,
            plugin_tick_interval,
            admin_extension,
            share_strategy,
            share_redelivery,
        } = config;
        let stats = Arc::new(Stats::default());
        let policy = Arc::new(policy);

        let (notify_shutdown, _) = broadcast::channel(1);
        let (shutdown_complete, shutdown_complete_rx) = mpsc::channel(1);

        let listener_plugins: Vec<_> = listeners.iter().map(|l| l.plugins.clone()).collect();
        let mut endpoints = listeners
            .into_iter()
            .enumerate()
            .map(|(i, l)| Endpoint::new(i, l, plugin_timeout))
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let auth_config = Arc::new(auth_config);
        let all_plugins = distinct_plugins(&plugins, &listener_plugins);
        let tasks = Tasks::default();
        let broker = Broker::new(Arc::clone(&storage))
            .counted_in(Arc::clone(&stats))
            .with_share_strategy(share_strategy)
            .with_share_redelivery(share_redelivery);
        let session_manager = SessionManager::for_broker(
            Arc::clone(&storage),
            limits.queue,
            limits.write_timeout,
            limits.max_sessions,
            Arc::clone(&stats),
            tasks.clone(),
        );
        session_manager.set_share_groups(Arc::new(broker.clone()));
        // A PostgreSQL standby gets here once it took the database over, so
        // it serves with the bans in force then
        let bans = Arc::new(Bans::load(Arc::clone(&storage).ban_store()).await?);
        broker.forget_stored_sys().await;

        let admin = Admin::new(AdminParts {
            directory: Arc::new(session_manager.clone()),
            retained: broker.retained_store(),
            topics: broker.topics(),
            stats: Arc::clone(&stats),
            listeners: endpoints.iter().map(|e| Arc::clone(&e.state)).collect(),
            storage: storage.backend_name(),
            extension: admin_extension,
            plugins: all_plugins.clone(),
            bans: Arc::clone(&bans),
            tasks: tasks.clone(),
        });
        let chain = Chain::new(plugins, plugin_timeout)
            .with_admin(admin.clone())
            .with_tasks(tasks.clone());
        for endpoint in &mut endpoints {
            endpoint.limits = endpoint.overrides.apply(limits);
            endpoint.settings.chain = match endpoint.trusted {
                true => Some(chain.trusted("admin socket")),
                false => endpoint
                    .settings
                    .chain
                    .take()
                    .map(|chain| chain.with_admin(admin.clone()).with_tasks(tasks.clone())),
            };
        }

        let server = Listener {
            endpoints,
            local_listener: local_listener.unwrap_or_else(|| LocalConnector::new().1),
            trusted_chain: chain.trusted("in-process"),
            next_endpoint: 0,
            broker,
            session_manager: session_manager.clone(),
            address_slots: Arc::default(),
            connection_slots: limits.max_connections.map(connection_slots),
            notify_shutdown,
            shutdown_complete,
            auth_config,
            chain,
            policy,
            limits,
            stats,
            bans,
            failed_logins: limits
                .login_failures
                .map(|l| Arc::new(FailedLogins::new(l))),
            tasks: tasks.clone(),
        };

        session_manager.set_expiry_chain(
            Chain::new(all_plugins.clone(), plugin_timeout).with_tasks(tasks.clone()),
        );
        let restored_wills = session_manager.restore_from_storage(&server.broker).await?;

        Ok(Server {
            listener: server,
            admin,
            shutdown_complete_rx,
            restored_wills,
            plugins: all_plugins,
            plugin_timeout,
            plugin_tick_interval,
            sys_interval,
        })
    }
}

/// A broker set up but not serving yet, from [`ServerBuilder::build`]:
/// its configuration is checked and the sessions an earlier run stored
/// are restored. [`Server::admin`] administers it from then on,
/// [`Server::local_connector`] connects to it from the same program, and
/// [`Server::run`] serves it.
pub struct Server<St: MqttStore + 'static> {
    listener: Listener<St>,
    admin: Admin,
    shutdown_complete_rx: mpsc::Receiver<()>,
    restored_wills: Vec<(SessionKey, WillMessage, Duration)>,
    plugins: Vec<Arc<dyn Plugin>>,
    plugin_timeout: Duration,
    plugin_tick_interval: Duration,
    sys_interval: Option<Duration>,
}

/// How often expired retained messages are removed from storage.
const RETAINED_SWEEP: Duration = Duration::from_secs(60);

impl<St: MqttStore + 'static> Server<St> {
    /// Administers this broker, before and while it runs.
    pub fn admin(&self) -> Admin {
        self.admin.clone()
    }

    /// Opens in-process connections to this broker once it runs.
    pub fn local_connector(&self) -> LocalConnector {
        self.listener.local_listener.connector.clone()
    }

    /// Serves clients until `shutdown` completes, then lets connections
    /// close and storage catch up, for a few seconds at most. Every task
    /// the server started has ended by the time this returns, those still
    /// running aborted, and the server holds `storage` no more: it may be
    /// opened again at once. [`Admin`] handles kept from
    /// [`Server::admin`] still hold it until dropped.
    pub async fn run(self, shutdown: impl Future) -> std::result::Result<(), ServerError> {
        let Server {
            listener: mut server,
            admin,
            mut shutdown_complete_rx,
            restored_wills,
            plugins,
            plugin_timeout,
            plugin_tick_interval,
            sys_interval,
        } = self;

        let session_manager = server.session_manager.clone();
        let tasks = server.tasks.clone();
        if !plugins.is_empty() {
            let mut ticks = time::interval(plugin_tick_interval);
            ticks.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
            let mut stop = server.notify_shutdown.subscribe();
            tasks.spawn(async move {
                loop {
                    tokio::select! {
                        _ = ticks.tick() => {}
                        _ = stop.recv() => break,
                    }
                    for plugin in &plugins {
                        guarded(plugin.name(), "on_tick", plugin_timeout, || {
                            plugin.on_tick()
                        })
                        .await;
                    }
                }
            });
        }
        if let Some(interval) = sys_interval {
            sys::spawn_publisher(
                &tasks,
                server.broker.clone(),
                session_manager.clone(),
                Arc::clone(&server.stats),
                admin.started(),
                interval,
            );
        }
        // Lookups leave expired retained messages out; this removes them
        let broker = server.broker.clone();
        let mut stop = server.notify_shutdown.subscribe();
        tasks.spawn(async move {
            let mut ticks = time::interval(RETAINED_SWEEP);
            ticks.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = ticks.tick() => {}
                    _ = stop.recv() => break,
                }
                broker.remove_expired_retained().await;
            }
        });
        // Its own timer: the $SYS publisher may be off, or slower
        let bans = Arc::clone(&server.bans);
        let mut stop = server.notify_shutdown.subscribe();
        tasks.spawn(async move {
            let mut ticks = time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = ticks.tick() => {}
                    _ = stop.recv() => break,
                }
                bans.sweep().await;
            }
        });
        for (key, will, due) in restored_wills {
            let broker = server.broker.clone();
            let session_manager = session_manager.clone();
            tasks.spawn(async move {
                time::sleep(due).await;
                if session_manager.take_restored_will(&key).await {
                    publish_will(&broker, will, &key).await;
                    session_manager.restored_will_published(&key).await;
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
        // What is left, such as expiry timers, delayed wills and idle
        // writers, ends here: a restarted broker takes it up from storage
        tasks.stop().await;
        Ok(())
    }
}

impl<St: MqttStore + 'static> Listener<St> {
    async fn run(&mut self) {
        loop {
            let accepted = self.accept().await;
            #[cfg(unix)]
            if let Accepted::Unix(socket, endpoint) = &accepted {
                if !self.endpoints[*endpoint].admits(socket) {
                    continue;
                }
            }
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

            // Before any handshake, file or password check: a banned
            // address gets nothing from the broker. Behind a proxy the
            // client's address comes with the PROXY header.
            let proxied = endpoint.is_some_and(|i| self.endpoints[i].settings.proxy_protocol);
            if let Some(peer) = peer_addr.filter(|_| !proxied) {
                if self.bans.refuses_address(peer.ip()) {
                    warn!("{} is banned, closing", peer.ip());
                    continue;
                }
            }

            // In-process connections, which have no endpoint, are trusted
            let trusted = endpoint.is_none_or(|i| self.endpoints[i].trusted);

            if let Some(i) = endpoint.filter(|_| !trusted) {
                let endpoint = &mut self.endpoints[i];
                if endpoint
                    .connection_rate
                    .as_mut()
                    .is_some_and(|b| !b.try_take())
                {
                    warn!(
                        "Connection rate of {} exceeded, refusing {:?}",
                        endpoint.state.name, peer_addr
                    );
                    continue;
                }
            }

            // Held by the connection's task for as long as it runs. Trusted
            // clients take none, so an operator or an in-process bridge can
            // reach a full broker.
            let slot = match &self.connection_slots {
                Some(slots) if !trusted => match Arc::clone(slots).try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        warn!("Connection limit reached, refusing {:?}", peer_addr);
                        continue;
                    }
                },
                _ => None,
            };
            let listener_slots = endpoint.and_then(|i| self.endpoints[i].connection_slots.clone());
            let listener_slot = match listener_slots {
                Some(slots) if !trusted => match slots.try_acquire_owned() {
                    Ok(permit) => Some(permit),
                    Err(_) => {
                        warn!(
                            "Listener connection limit reached, refusing {:?}",
                            peer_addr
                        );
                        continue;
                    }
                },
                _ => None,
            };
            let slots = [slot, listener_slot];

            // Taken before any handshake, so an address can't hold more
            // half-open TLS or WebSocket connections than served ones.
            // Behind a proxy it waits for the PROXY header's address.
            let address_slot = match (self.limits.max_connections_per_ip, peer_addr) {
                (Some(max), Some(peer)) if !proxied => {
                    match self.address_slots.acquire(peer.ip(), max) {
                        Some(slot) => Some(slot),
                        None => {
                            warn!("{} holds the most connections allowed, closing", peer.ip());
                            continue;
                        }
                    }
                }
                _ => None,
            };

            info!(target: CONNECTIONS, "Got a connection: {:?}", peer_addr);
            let limits = endpoint.map_or(self.limits, |i| self.endpoints[i].limits);

            let parts = HandlerParts {
                broker: self.broker.clone(),
                session_manager: self.session_manager.clone(),
                shutdown: Shutdown::new(self.notify_shutdown.subscribe()),
                shutdown_complete: self.shutdown_complete.clone(),
                auth_config: Arc::clone(&self.auth_config),
                chain: self.chain.clone(),
                policy: Arc::clone(&self.policy),
                limits,
                stats: Arc::clone(&self.stats),
                cert_identity: None,
                tls: false,
                mount_point: None,
                http_site: None,
                allowed_origins: None,
                local: false,
                connect_deadline: Instant::now() + limits.connect_timeout,
                address_slots: Arc::clone(&self.address_slots),
                listener: Arc::from("local"),
                bans: Arc::clone(&self.bans),
                use_username_as_clientid: false,
                // Trusted clients skip authentication altogether
                failed_logins: self.failed_logins.clone().filter(|_| !trusted),
                tasks: self.tasks.clone(),
            };
            match accepted {
                Accepted::Tcp(socket, endpoint) => {
                    crate::connection::configure_tcp(&socket);
                    let parts = HandlerParts {
                        listener: Arc::clone(&self.endpoints[endpoint].state.name),
                        ..parts
                    };
                    let settings = self.endpoints[endpoint].settings.clone();
                    let counted = ListenerSlot::new(&self.endpoints[endpoint].state);
                    self.tasks.spawn(Box::new(parts).serve_accepted(
                        socket,
                        settings,
                        peer_addr,
                        slots,
                        counted,
                        address_slot,
                    ));
                }
                #[cfg(unix)]
                Accepted::Unix(socket, endpoint) => {
                    let policy = match self.endpoints[endpoint].trusted {
                        true => Arc::new(ConnectPolicy {
                            redirect: None,
                            client_id_prefixes: Vec::new(),
                            ..(*parts.policy).clone()
                        }),
                        false => Arc::clone(&parts.policy),
                    };
                    let parts = HandlerParts {
                        listener: Arc::clone(&self.endpoints[endpoint].state.name),
                        policy,
                        ..parts
                    };
                    let settings = self.endpoints[endpoint].settings.clone();
                    let counted = ListenerSlot::new(&self.endpoints[endpoint].state);
                    self.tasks.spawn(Box::new(parts).serve_accepted(
                        socket,
                        settings,
                        peer_addr,
                        slots,
                        counted,
                        address_slot,
                    ));
                }
                Accepted::Local(stream) => {
                    let parts = HandlerParts {
                        auth_config: Arc::new(AuthConfig::default()),
                        chain: self.trusted_chain.clone(),
                        local: true,
                        ..parts
                    };
                    self.tasks.spawn(async move {
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
        // Until the client is authenticated; see `Handler::run`
        let limits = &handler.limits;
        handler
            .connection
            .set_max_packet_size(limits.max_connect_packet_size.min(limits.max_packet_size));

        let first_packet = connect_step(
            &mut handler.shutdown,
            handler.connect_deadline,
            handler.connection.read_packet(),
        );
        let Some(first_packet) = first_packet.await else {
            return debug!("Connection closed for shutdown before CONNECT");
        };

        // Boxed, as it is kept until the connection ends
        let first_packet = Box::new(first_packet.map(|read| read.map_err(Error::from)));
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
                    Err(Error::Reason(ReasonCode::ServerShuttingDown)) => {
                        debug!(target: CONNECTIONS, "Connection closed for shutdown")
                    }
                    // Only the broker's own storage failing is its error;
                    // anything else a client can cause at will
                    Err(Error::Storage(e)) => {
                        error!("Storage failed serving {:?}: {}", handler.peer, e)
                    }
                    Err(Error::Io(e)) => {
                        debug!(target: CONNECTIONS, "Connection of {:?} failed: {}", handler.peer, e)
                    }
                    Err(err) => warn!(
                        target: CONNECTIONS,
                        "Closing the connection of {:?}: {}", handler.peer, err
                    ),
                    Ok(()) => {}
                }
            }
            Err(_) => debug!(
                target: CONNECTIONS,
                "No CONNECT from {:?} in time, closing", handler.peer
            ),
            Ok(Ok(None)) => debug!(
                target: CONNECTIONS,
                "{:?} closed the connection before CONNECT", handler.peer
            ),
            Ok(Ok(Some(packet))) => warn!(
                target: CONNECTIONS,
                "{:?} sent {} before CONNECT, closing",
                handler.peer,
                describe(&packet)
            ),
            Ok(Err(Error::Reason(ReasonCode::UnsupportedProtocolVersion))) => {
                debug!(
                    target: CONNECTIONS,
                    "{:?} asked for an unsupported protocol version", handler.peer
                );
                // Answered in the MQTT 3.1.1 format, which every version can
                // read [MQTT-3.1.2-2]
                handler
                    .connection
                    .set_protocol_version(ProtocolVersion::V3_1_1);
                let connack = ConnAckPacket {
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
            Ok(Err(Error::Reason(reason)))
                if handler.connection.connect_version() == Some(ProtocolVersion::V5) =>
            {
                warn!(
                    target: CONNECTIONS,
                    "Refusing invalid CONNECT from {:?}: {:?}", handler.peer, reason
                );
                let connack = ConnAckPacket {
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
            Ok(Err(Error::Io(e))) => debug!(
                target: CONNECTIONS,
                "Connection of {:?} failed before CONNECT: {}", handler.peer, e
            ),
            Ok(Err(e)) => warn!(
                target: CONNECTIONS,
                "Refusing invalid CONNECT from {:?}: {}", handler.peer, e
            ),
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
                if let Poll::Ready(Some(stream)) = self.local_listener.connections.poll_recv(cx) {
                    return Poll::Ready(Ok(Accepted::Local(stream)));
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
        let mut connection_info =
            ConnectionInfo::new(Arc::clone(&self.listener), connect_packet.protocol_version);
        connection_info.tls = self.tls;
        connection_info.certificate = self.cert_identity.clone();
        connection_info.mount_point = self.mount_point.clone();
        self.connection_info = Some(Arc::new(connection_info));

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
        // What enhanced authentication sends the client with its CONNACK
        let mut auth_data = None;

        // MQTT 5 enhanced authentication replaces the user name and password
        // check when the client asks for it
        if let Some(identity) = self.cert_identity.clone() {
            // Verified by the TLS handshake, in place of any credentials
            verified_user = Some(identity);
        } else if let Some(method) = authentication_method {
            // Boxed, like the rest of the connect phase, so its state isn't
            // kept for the whole connection
            match Box::pin(self.authenticate_enhanced(&connect_packet, &method)).await? {
                Some((username, data)) => {
                    verified_user = username;
                    auth_data = data;
                }
                None => return Ok(()),
            }
        } else {
            let require_auth = self.auth_config.require_auth;
            // Credentials that are sent are checked even when authentication
            // isn't required, so a plugin that refuses them is never skipped;
            // only those no plugin decides on are then let in, unverified
            let authenticated = match (
                &connect_packet.payload.user_name,
                &connect_packet.payload.password,
            ) {
                (Some(username), Some(password)) => {
                    let client =
                        self.client_info(&connect_packet.payload.client_id, Some(username.clone()));
                    let credentials = Credentials::new(username, password);
                    let checked = validate_credentials(&self.chain, &client, &credentials);
                    let checked = connect_step(&mut self.shutdown, self.connect_deadline, checked);
                    let decision = checked.await.ok_or(ReasonCode::ServerShuttingDown)?;
                    let Ok(decision) = decision else {
                        warn!("Login of user `{}` not checked in time, closing", username);
                        return Ok(());
                    };
                    match decision {
                        Decision::Allow => {
                            verified_user = Some(username.clone());
                            true
                        }
                        Decision::Defer if !require_auth => true,
                        _ => {
                            warn!("Authentication failed for user `{}`", username);
                            false
                        }
                    }
                }
                _ if !require_auth => true,
                (None, None) if self.auth_config.allow_anonymous => true,
                _ => {
                    warn!("Authentication required but no credentials provided");
                    false
                }
            };

            if !authenticated {
                self.login_failed().await;
                return self
                    .reject_connect(&connect_packet, ReasonCode::BadUserNameOrPassword)
                    .await;
            }
        }
        self.connection
            .set_max_packet_size(self.limits.max_packet_size);

        if let Some(reason) = self.refused_will(&connect_packet) {
            return self.reject_connect(&connect_packet, reason).await;
        }

        let mut renamed = false;
        if self.use_username_as_clientid {
            let Some(username) = &verified_user else {
                warn!(
                    "Client `{}` has no verified user name to use as its client id, refusing",
                    connect_packet.payload.client_id
                );
                return self
                    .reject_connect(&connect_packet, ReasonCode::NotAuthorized)
                    .await;
            };
            renamed = connect_packet.payload.client_id != *username;
            connect_packet.payload.client_id = username.clone();
        }

        // MQTT 3.x servers must reject a zero-length client id unless the
        // session is clean, as there would be no way to resume it [MQTT-3.1.3-8]
        if connect_packet.payload.client_id.is_empty()
            && !connect_packet.clean_start
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
        // After authentication, since user name bans match the verified
        // user, and once the client id is final
        if self.banned(&connect_packet.payload.client_id, verified_user.as_deref()) {
            warn!(
                "Client `{}` is banned, refusing",
                connect_packet.payload.client_id
            );
            return self
                .reject_connect(&connect_packet, ReasonCode::Banned)
                .await;
        }
        // A will is published on the client's behalf, so it needs the same
        // permission as a PUBLISH, checked once the client id is final
        if let Some(will) = &connect_packet.payload.will {
            let identity =
                self.client_info(&connect_packet.payload.client_id, verified_user.clone());
            if !self.chain.may(&identity, &will.topic, Access::Write).await {
                return self
                    .reject_connect(&connect_packet, ReasonCode::NotAuthorized)
                    .await;
            }
        }

        // In-process clients don't reconnect, so a network client taking
        // over their id would end them for good. They have no mount point,
        // so only clients of listeners without one can collide with them.
        let mount_point = self.connection.mount_point().map(str::to_string);
        let key = SessionKey::new(
            mount_point.clone(),
            connect_packet.payload.client_id.clone(),
        );
        if !self.local && self.session_manager.is_held_locally(&key) {
            warn!("Client id {} belongs to an in-process client", key);
            return self
                .reject_connect(&connect_packet, ReasonCode::NotAuthorized)
                .await;
        }

        let protocol_version = connect_packet.protocol_version;
        let keepalive =
            effective_keepalive(connect_packet.keepalive, protocol_version, &self.limits);
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
            mount_point: key.mount_point.clone(),
        };
        let client_id = connect_packet.payload.client_id.clone();
        let mut start_result = match Box::pin(self.session_manager.start_session(
            *connect_packet,
            owner,
            expiry,
        ))
        .await
        {
            Ok(start_result) => start_result,
            Err(StartError::OwnedByAnother(connect_packet)) => {
                return self
                    .reject_connect(&connect_packet, ReasonCode::NotAuthorized)
                    .await;
            }
            Err(StartError::Busy) => {
                warn!(
                    "The connection using client id `{}` didn't let go of it in time",
                    client_id
                );
                return self
                    .refuse(
                        protocol_version,
                        client_maximum_packet_size,
                        ReasonCode::ServerBusy,
                    )
                    .await;
            }
            Err(StartError::TooManySessions) => {
                return self
                    .refuse(
                        protocol_version,
                        client_maximum_packet_size,
                        ReasonCode::QuotaExceeded,
                    )
                    .await;
            }
            // Logged where the admissions are counted
            Err(StartError::Crowded) => {
                return self
                    .refuse(
                        protocol_version,
                        client_maximum_packet_size,
                        ReasonCode::ServerBusy,
                    )
                    .await;
            }
            Err(StartError::SetAside) => {
                return self
                    .refuse(
                        protocol_version,
                        client_maximum_packet_size,
                        ReasonCode::ServerUnavailable,
                    )
                    .await;
            }
            Err(StartError::Storage(e)) => return Err(e),
        };
        // A client whose id was replaced learns the one it has
        start_result.assigned_client_id |= assigned_client_id || renamed;
        let mut session = start_result.session;
        let mut lease = start_result.lease;
        let key = lease.key().clone();
        if self.local {
            self.session_manager.hold_locally(&key);
        }

        session.set_verified_user(verified_user.clone()).await;
        session
            .set_peer(self.peer, self.connection_info.clone())
            .await;
        // A ban added after the check above, while this client had yet to
        // claim its id, also missed it in addBan's walk of the connected
        // clients. Checked again now that the id is held and the session
        // shows who the client is, either this check or that walk sees the
        // other; the connection then closes with Banned once it is up.
        {
            let client_id = session.get_client_id().await;
            if self.banned(&client_id, verified_user.as_deref()) {
                warn!(
                    "Client `{}` was banned while connecting, closing",
                    client_id
                );
                let close = Close::Admin {
                    reason: ReasonCode::AdministrativeAction,
                    send_will: false,
                };
                self.session_manager.close_own(&lease, close).await;
            }
        }
        let counters = session
            .attach(Arc::clone(&self.listener), keepalive, expiry)
            .await;
        self.connection.set_counters(counters);

        let mut announced = false;
        let result = async {
            Box::pin(session.begin(
                &mut self.connection,
                start_result.resume,
                start_result.assigned_client_id,
                auth_data,
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
            let resend = session.pending_retransmissions().await;
            self.session_manager
                .save_changes(&key, resend.dropped.into_iter().map(Write::RemoveInflight))
                .await;
            for packet in resend.packets {
                Box::pin(self.connection.write_packet(packet)).await?;
            }

            let _connected = ConnectedGuard::new(Arc::clone(&self.stats));
            if !self.chain.is_empty() {
                self.chain.emit(Event::Connected {
                    client: session.identity().await,
                });
                announced = true;
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

        let reason = match &result {
            Err(Error::Reason(reason)) => Some(*reason),
            _ => None,
        };
        // Plugins never saw a connection that failed before it was accepted
        if announced {
            self.chain.emit(Event::Disconnected {
                client: session.identity().await,
                reason,
            });
        }

        // Tell MQTT 5 clients why the server is closing the connection
        if let Err(Error::Reason(reason)) = &result {
            if protocol_version.supports_reason_codes() && reason.code() >= 0x80 {
                let text = session.take_auth_failure().await;
                let disconnect = ControlPacket::Disconnect(DisconnectPacket {
                    reason: *reason,
                    properties: Some(DisconnectProperties {
                        reason_string: explained(*reason, text, client_maximum_packet_size),
                        ..Default::default()
                    }),
                });
                let _ = Box::pin(self.connection.write_packet(disconnect)).await;
            }
        }

        session.detach(lease.generation, expiry).await;

        // A member of a share group that only lost its connection gives
        // its messages to the members connected, when asked to; a session
        // ending with its connection gives them as it ends, and one taken
        // over keeps them for its client
        if self.broker.share_redelivery() == ShareRedelivery::OnDisconnect
            && expiry != Some(Duration::ZERO)
            && session.generation() == lease.generation
        {
            self.session_manager
                .hand_back_on_disconnect(&key, &session)
                .await;
        }

        // A close that drops the will can lose the race against a read error,
        // an EOF or a DISCONNECT with a will
        if matches!(
            *lease.close.borrow(),
            Some(Close::Admin {
                send_will: false,
                ..
            })
        ) {
            session.clear_will().await;
            self.session_manager.delete_will(&key);
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
            let will_session = session.clone();
            let will_key = key.clone();
            let pending = will.clone();

            let publish = async move {
                if !delay.is_zero() {
                    time::sleep(delay).await;
                    if !will_session.claim_pending_will(generation).await {
                        return;
                    }
                }
                publish_will(&broker, will, &will_key).await;
                session_manager.delete_will(&will_key);
            };

            if delay.is_zero() {
                Box::pin(publish).await;
            } else if let Some(task) = self.tasks.spawn(publish) {
                // Aborted, freeing the will, as soon as a new connection
                // takes the session over or the session is deleted. Once the
                // server has stopped, the stored will waits for a restart.
                session.set_pending_will(generation, task, pending).await;
            }
        }

        if self.local {
            self.session_manager.release_locally(&key);
        }
        self.session_manager.end_connection(lease, expiry).await;

        result
    }

    /// The client of this connection as plugins see it, before it has a
    /// session.
    fn client_info(&self, client_id: &str, username: Option<String>) -> ClientInfo {
        let mut client = ClientInfo::new(client_id, username);
        client.peer = self.peer;
        client.connection = self.connection_info.clone();
        client
    }

    /// Runs the AUTH exchange of MQTT 5 enhanced authentication during
    /// CONNECT. Returns the user name the method verified, if any, and the
    /// data for the CONNACK, or None having refused the connection.
    async fn authenticate_enhanced(
        &mut self,
        connect_packet: &ConnectPacket,
        method: &str,
    ) -> Result<Option<(Option<String>, Option<bytes::Bytes>)>> {
        let client = self.client_info(
            &connect_packet.payload.client_id,
            connect_packet.payload.user_name.clone(),
        );
        let exchange = self.chain.begin_auth(&client, method);
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
        let mut result = self.chain.step(&mut exchange, initial_data).await;

        loop {
            let challenge = match result {
                AuthResult::Success { username, data } => return Ok(Some((username, data))),
                AuthResult::Failed {
                    reason,
                    reason_string: text,
                } => {
                    warn!("Enhanced authentication with `{}` failed", method);
                    self.login_failed().await;
                    let properties = connect_packet.properties.as_ref();
                    let text = text.filter(|_| {
                        properties
                            .and_then(|p| p.request_problem_information.as_ref())
                            .is_none_or(|r| r.value != 0)
                    });
                    let client_maximum = properties
                        .and_then(|p| p.maximum_packet_size.as_ref())
                        .map(|m| m.value);
                    let connack = ConnAckPacket {
                        flags: ConnAckFlags {
                            session_present: false,
                        },
                        reason_code: reason,
                        properties: Some(ConnAckProperties {
                            reason_string: explained(reason, text, client_maximum),
                            ..Default::default()
                        }),
                    };
                    let _ = self
                        .connection
                        .write_packet(ControlPacket::ConnAck(connack))
                        .await;
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
            let response = connect_step(
                &mut self.shutdown,
                self.connect_deadline,
                self.connection.read_packet(),
            )
            .await
            .ok_or(ReasonCode::ServerShuttingDown)?
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
            result = self.chain.step(&mut exchange, data.as_ref()).await;
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

    /// Counts a failed login against the client's address, which may get
    /// it banned.
    async fn login_failed(&self) {
        if let (Some(failed_logins), Some(peer)) = (&self.failed_logins, self.peer) {
            failed_logins.failed(peer.ip(), &self.bans).await;
        }
    }

    /// Whether a ban refuses this client, connecting with `client_id` as
    /// the verified `username`.
    fn banned(&self, client_id: &str, username: Option<&str>) -> bool {
        self.bans.refuses(&Candidate {
            client_id,
            username,
            ip: self.peer.map(|peer| peer.ip()),
        })
    }

    /// Why a will the broker can't honour is refused [MQTT-3.2.2-12]
    /// [MQTT-3.2.2-13]
    fn refused_will(&self, connect_packet: &ConnectPacket) -> Option<ReasonCode> {
        let will = connect_packet.payload.will.as_ref()?;
        if will.qos as u8 > self.limits.maximum_qos as u8 {
            return Some(ReasonCode::QoSNotSupported);
        }
        if will.retain && !self.limits.retain_available {
            return Some(ReasonCode::RetainNotSupported);
        }
        // A will is published like a PUBLISH, so its topic must be one;
        // `$` topics belong to the broker
        let topic = will.topic.as_str();
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
        let client_maximum = connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.maximum_packet_size.as_ref())
            .map(|m| m.value);
        self.refuse(connect_packet.protocol_version, client_maximum, reason_code)
    }

    /// Answers a CONNECT of `protocol_version` with `reason_code`, its
    /// reason string fitting in `client_maximum`.
    fn refuse(
        &mut self,
        protocol_version: ProtocolVersion,
        client_maximum: Option<u32>,
        reason_code: ReasonCode,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + '_>> {
        Box::pin(async move {
            let connack = ConnAckPacket {
                flags: ConnAckFlags {
                    session_present: false,
                },
                reason_code,
                properties: protocol_version
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

        // Clients over their publish rate aren't read from until it allows
        // another message
        let mut pacing = match self.local || self.chain.is_trusted() {
            true => Pacing::new(&Limits::default()),
            false => Pacing::new(&self.limits),
        };
        let paused = time::sleep(Duration::ZERO);
        tokio::pin!(paused);
        let mut reading = true;

        let key = lease.key().clone();
        while !self.shutdown.is_shutdown() {
            // Every turn of the loop follows a message sent or a packet
            // read, so retained messages are queued as the queue drains
            if session.owes_retained() {
                Box::pin(session.feed_retained(&self.broker, &self.chain)).await;
            }
            // Re-evaluated after every packet, so acknowledgements reopen it
            let window_open = session.can_send_more().await;
            // What a resumed session held back goes before anything newer
            if window_open {
                if let Some(packet) = session.next_retransmission().await {
                    Box::pin(self.connection.write_packet(packet)).await?;
                    continue;
                }
            }

            // Only waiting happens here, and the work an event brings runs in
            // a boxed future while it lasts: an idle connection holds just
            // what waiting takes, not the state of its heaviest handling
            let step = tokio::select! {
                packet = self.connection.read_packet(), if reading => Step::Read(packet?),
                // The client is sending, so it is alive: its keep-alive
                // starts over once it may send again
                _ = &mut paused, if !reading => {
                    reading = true;
                    // A client with nothing waiting by now has slowed down
                    // to its rate; one with more is still over it
                    match self.connection.read_packet().now_or_never() {
                        Some(packet) => Step::Read(packet?),
                        None => {
                            pacing.caught_up();
                            Step::Done(Flow::PacketHandled)
                        }
                    }
                }
                // The common case, a message to send, isn't boxed: it would
                // cost an allocation per message
                (message, journaled) = session.next_outgoing(window_open) => {
                    Step::Done(self.send_outgoing(&session, &key, message, journaled).await?)
                }
                // A task that runs late, starved by others, can find the
                // deadline passed and a packet waiting that came in time;
                // the select picks either at random, so the packet is
                // looked for before giving up on the client. Biasing the
                // select towards reading would do it too, but a client
                // sending without pause would then hold off its outgoing
                // messages, takeovers and shutdown for as long as it sends.
                _ = &mut deadline, if timeout_duration.is_some() && reading => {
                    match self.connection.read_packet().now_or_never() {
                        Some(packet) => Step::Read(packet?),
                        None => Step::Wake(Wake::KeepAliveExpired),
                    }
                }
                Ok(()) = lease.close.changed() => {
                    let close = (*lease.close.borrow_and_update()).unwrap_or(Close::TakenOver);
                    Step::Wake(Wake::Closed(close))
                }
                _ = self.shutdown.recv() => {
                    return Err(ReasonCode::ServerShuttingDown.into());
                }
            };
            let handled = match step {
                Step::Done(flow) => Ok(flow),
                Step::Wake(wake) => Err(wake),
                // So are the packets most clients send, PUBLISH and its
                // acknowledgements; the rest are boxed
                Step::Read(packet) => match HotPacket::try_from(packet) {
                    Ok(packet) => {
                        let published = match &packet {
                            HotPacket::Publish(p) => {
                                Some(p.payload.as_ref().map_or(0, |p| p.len()))
                            }
                            _ => None,
                        };
                        let flow = self.handle_hot(&session, &key, packet).await?;
                        if let Some(wait) = published.and_then(|bytes| pacing.publish(bytes)) {
                            if pacing.past_grace() {
                                warn!("Client {} stayed over its publish rate, disconnecting", key);
                                return Err(ReasonCode::MessageRateTooHigh.into());
                            }
                            reading = false;
                            paused
                                .as_mut()
                                .reset(Instant::now().checked_add(wait).unwrap_or_else(far_future));
                        }
                        Ok(flow)
                    }
                    Err(packet) => Err(Wake::Packet(packet)),
                },
            };
            let flow = match handled {
                Ok(flow) => flow,
                Err(wake) => {
                    Box::pin(self.handle_wake(
                        wake,
                        &session,
                        &key,
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
        key: &SessionKey,
        packet: HotPacket,
    ) -> Result<Flow> {
        let mut session = session.clone();
        // Packet identifiers are never 0 [MQTT-2.2.1-3]
        if packet.packet_id() == Some(0) {
            return Err(ReasonCode::ProtocolError.into());
        }
        let Handled { response, write } = match packet {
            HotPacket::Publish(publish) => {
                Stats::add(&self.stats.messages_received);
                session
                    .handle_publish(publish, &self.broker, &self.chain)
                    .await?
            }
            HotPacket::PubAck(ack) => session.handle_puback(ack).await?,
            HotPacket::PubRec(rec) => session.handle_pubrec(rec).await?,
            HotPacket::PubRel(rel) => session.handle_pubrel(rel).await?,
            HotPacket::PubComp(comp) => session.handle_pubcomp(comp).await?,
            HotPacket::PingReq => Handled {
                response: Some(ControlPacket::PingResp(PingRespPacket {})),
                write: None,
            },
        };
        if let Some(write) = write {
            self.session_manager.save_changes(key, [write]).await;
        }
        if let Some(response) = response {
            debug!("Sending {} to client {}", describe(&response), key);
            self.connection.write_packet(response).await?;
        }
        Ok(Flow::PacketHandled)
    }

    /// Sends a message the session queued for the client, at `journaled`
    /// in the queue's journal if it is stored there.
    async fn send_outgoing(
        &mut self,
        session: &Session,
        key: &SessionKey,
        message: Message,
        journaled: Option<u64>,
    ) -> Result<Flow> {
        let mut session = session.clone();
        // Checked per message: a permitted subscription can still
        // match topics the client may not read
        if !self.chain.is_empty() && !self.chain.is_trusted() {
            // Re-read each time: re-authentication can change the user
            let identity = session.identity().await;
            if !self
                .chain
                .may(&identity, &message.topic, Access::Read)
                .await
            {
                session.count_drop(DropCause::NotAuthorized);
                if let Some(sequence) = journaled {
                    self.session_manager.dequeue(key, sequence).await;
                }
                return Ok(Flow::Continue);
            }
        }
        // Encoded within this block, so only its bytes wait while the
        // in-flight record is stored, not the packet
        let (write, inflight) = {
            let Some(Outgoing { packet, inflight }) = session.prepare_publish(message).await else {
                if let Some(sequence) = journaled {
                    self.session_manager.dequeue(key, sequence).await;
                }
                return Ok(Flow::Continue);
            };
            debug!("Sending {} to client {}", describe(&packet), key);
            (self.connection.write_packet(packet), inflight)
        };
        match inflight {
            Some(inflight) => {
                self.session_manager
                    .store_inflight(key, &inflight, journaled)
                    .await
            }
            None => {
                if let Some(sequence) = journaled {
                    self.session_manager.dequeue(key, sequence).await;
                }
            }
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
        key: &SessionKey,
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
                            let interval = cap_expiry_interval(interval, &self.limits);
                            *expiry = (interval != u32::MAX)
                                .then(|| Duration::from_secs(interval.into()));
                        }

                        // Only a normal disconnection discards the will
                        // [MQTT-3.14.4-3]: 0x04 asks for it, and an error
                        // code means the client didn't end as it meant to.
                        // MQTT 3.x has no reason and reads as normal.
                        if disconnect.reason == ReasonCode::NormalDisconnection {
                            session.clear_will().await;
                            self.session_manager.delete_will(key);
                        }
                        return Ok(Flow::Close);
                    }
                    Some(packet) => packet,
                };

                let maybe_res = session
                    .process_incoming(packet, &self.broker, &self.chain)
                    .await?;

                let changes = session.take_changes().await;
                self.session_manager.save_changes(key, changes).await;

                // A re-authentication may land on a banned user name; the
                // ban is in force before addBan walks the connected clients,
                // so checking after the user is set covers both orders
                if matches!(&maybe_res, Some(ControlPacket::Auth(a)) if a.reason == ReasonCode::Success)
                {
                    let identity = session.identity().await;
                    if self.banned(&identity.client_id, identity.username.as_deref()) {
                        warn!(
                            "Client `{}` re-authenticated as a banned user, closing",
                            identity.client_id
                        );
                        let close = Close::Admin {
                            reason: ReasonCode::AdministrativeAction,
                            send_will: false,
                        };
                        return self.close_for(&session, key, protocol_version, close).await;
                    }
                }

                if let Some(res) = maybe_res {
                    debug!("Sending {} to client {}", describe(&res), key);
                    self.connection.write_packet(res).await?;
                }

                Ok(Flow::PacketHandled)
            }
            Wake::KeepAliveExpired => {
                warn!(
                    "Keep-alive timeout for client {} ({:?} with no packets)",
                    key,
                    timeout_duration.unwrap_or_default()
                );
                // An abnormal disconnect: the will is published
                Err(ReasonCode::KeepAliveTimeout.into())
            }
            Wake::Closed(close) => self.close_for(&session, key, protocol_version, close).await,
        }
    }

    /// Tells the client why its connection closes, dropping its will if
    /// the close says so.
    async fn close_for(
        &mut self,
        session: &Session,
        key: &SessionKey,
        protocol_version: ProtocolVersion,
        close: Close,
    ) -> Result<Flow> {
        let reason = match close {
            Close::TakenOver => {
                info!("Client {} was taken over by a new connection", key);
                ReasonCode::SessionTakenOver
            }
            Close::Admin { reason, send_will } => {
                info!("Closing the connection of client {}: {}", key, reason);
                if !send_will {
                    session.clear_will().await;
                    self.session_manager.delete_will(key);
                }
                reason
            }
        };
        if protocol_version.supports_reason_codes() {
            let _ = self
                .connection
                .write_packet(ControlPacket::Disconnect(DisconnectPacket {
                    reason,
                    properties: Some(DisconnectProperties {
                        reason_string: reason_string(
                            reason,
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
    /// A packet other than the `HotPacket`s, or None once the client closed
    Packet(Option<ControlPacket>),
    KeepAliveExpired,
    Closed(Close),
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

/// A wake-up either handled already or to be handled, or a packet read.
#[allow(clippy::large_enum_variant)]
enum Step {
    Done(Flow),
    Wake(Wake),
    Read(Option<ControlPacket>),
}

/// What a connection does once it handled what woke it.
enum Flow {
    Continue,
    /// A packet came in, so the keep-alive deadline starts over
    PacketHandled,
    Close,
}

/// An instant no timer reaches, for waits too long to add to now.
fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86400 * 365 * 30)
}

/// Explains `reason` to the client, unless the text would make the packet
/// larger than the client accepts [MQTT-3.1.2-25].
fn reason_string(
    reason: ReasonCode,
    client_maximum_packet_size: Option<u32>,
) -> Option<ReasonString> {
    fitting(reason.to_string(), client_maximum_packet_size)
}

/// Explains `reason` to the client in `text`, as `reason_string` does
/// without it or when it would make the packet too large.
fn explained(
    reason: ReasonCode,
    text: Option<String>,
    client_maximum_packet_size: Option<u32>,
) -> Option<ReasonString> {
    text.and_then(|text| fitting(text, client_maximum_packet_size))
        .or_else(|| reason_string(reason, client_maximum_packet_size))
}

/// `text` as a reason string, unless it would make the packet larger than
/// the client accepts.
fn fitting(text: String, client_maximum_packet_size: Option<u32>) -> Option<ReasonString> {
    // Room for the rest of a CONNACK or DISCONNECT around the string
    let fits = client_maximum_packet_size.is_none_or(|max| text.len() + 16 <= max as usize);
    fits.then(|| ReasonString::new(text))
}

async fn publish_will<St: MqttStore>(broker: &Broker<St>, will: WillMessage, key: &SessionKey) {
    info!(
        "Publishing will message for client {} on topic `{}`",
        key, will.topic
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
        share_group: None,
    };

    if let Err(e) = broker.publish(&topic, message).await {
        error!("Failed to publish will message: {}", e);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reason_strings_fit_the_client_maximum_packet_size() {
        let text = || Some("the token expired an hour ago".to_string());
        let explain = |max| explained(ReasonCode::NotAuthorized, text(), max).map(|r| r.value);
        assert_eq!(
            explain(None).as_deref(),
            Some("the token expired an hour ago")
        );
        // The method's text is longer than the reason code's own
        assert_eq!(explain(Some(40)).as_deref(), Some("Not authorized"));
        assert_eq!(explain(Some(20)), None);
    }
}
