//! The broker's configuration: its listeners, limits, authentication and
//! connect policy, as `ServerBuilder` takes them.

use std::{path::PathBuf, sync::Arc};

use mercurio_core::qos::QoS;
#[cfg(unix)]
use tokio::net::{unix::UCred, UnixListener};
use tokio::{io::DuplexStream, net::TcpListener, sync::mpsc, time::Duration};

use super::AddressRange;
use crate::{admin::AdminExtension, error::ConfigErrorKind, plugin::Plugin, tls::Tls};

/// Authentication configuration for the server.
#[derive(Default)]
#[non_exhaustive]
pub struct AuthConfig {
    /// Whether authentication is required. Credentials a client sends are
    /// checked by the plugins either way, and refused on
    /// [`Decision::Deny`](crate::plugin::Decision::Deny); without it, clients whose credentials no plugin
    /// decides on, or that send none, are let in as anonymous.
    pub require_auth: bool,
    /// With authentication required, still accept clients that send no
    /// user name. Credentials that are sent are always checked.
    pub allow_anonymous: bool,
}

setters!(AuthConfig {
    with_require_auth => require_auth: bool,
    with_allow_anonymous => allow_anonymous: bool,
});

pub use crate::{
    bans::LoginFailures,
    broker::{ShareRedelivery, ShareStrategy},
    connection::{DEFAULT_MAX_CONNECT_PACKET_SIZE, DEFAULT_MAX_PACKET_SIZE},
    queue::{Overflow, QueueLimits},
};

/// Subscriptions a session may hold unless configured otherwise: far more
/// than clients need, far fewer than it takes to exhaust the broker.
pub const DEFAULT_MAX_SUBSCRIPTIONS: usize = 10_000;

/// Longest topic filter a SUBSCRIBE may name unless configured otherwise,
/// in bytes: far longer than real filters, far shorter than the 64 KiB the
/// protocol allows.
pub const DEFAULT_MAX_TOPIC_FILTER_LENGTH: usize = 1024;

/// Unacknowledged QoS 1 and 2 messages one client may have unless
/// configured otherwise.
pub const DEFAULT_MAX_INFLIGHT: u16 = 64;

/// Tunable limits of the broker.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Limits {
    /// How long a new connection may take, in all, to send its PROXY header,
    /// complete the TLS and WebSocket handshakes and send CONNECT, enhanced
    /// authentication included, before it is closed.
    pub connect_timeout: Duration,
    /// How long writing one packet to a client may take before the
    /// connection is dropped as stalled. A connection taking over a client
    /// id waits as long for the previous one to let go, then is refused
    /// with Server Busy.
    pub write_timeout: Duration,
    /// Largest packet accepted from a client, fixed header included.
    pub max_packet_size: usize,
    /// Largest CONNECT accepted, and AUTH during enhanced authentication,
    /// fixed header included. These come before the client is
    /// authenticated, so they get less room than `max_packet_size`, which
    /// still caps it. A larger one is refused as Packet Too Large. On
    /// WebSocket listeners, which buffer whole messages, the bytes a client
    /// sends before it is authenticated are held to twice this, framing
    /// included, and the connection closed past that.
    pub max_connect_packet_size: usize,
    /// Messages waiting to be sent to one session.
    pub queue: QueueLimits,
    /// Connections beyond this many are closed right after being accepted.
    /// In-process connections and those of trusted listeners don't count.
    pub max_connections: Option<usize>,
    /// Highest topic alias MQTT 5 clients may use; 0 disables aliases.
    pub topic_alias_maximum: u16,
    /// QoS 2 messages a client may have awaiting PUBREL at once.
    pub receive_maximum: u16,
    /// QoS 1 and 2 messages sent to a client and not yet acknowledged, at
    /// most, whatever Receive Maximum the client announces. Each holds its
    /// whole message in memory. 0 counts as 1.
    pub max_inflight: u16,
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
    /// Longest topic filter, in bytes, a SUBSCRIBE may name; longer ones
    /// are refused with Topic Filter Invalid. Each filter is kept in memory
    /// and storage for as long as its session.
    pub max_topic_filter_length: usize,
    /// Longest a session may outlive its connection. MQTT 5 clients asking
    /// for longer, or for no expiry, are told to use this; it also bounds
    /// `persistent_client_expiration`. Unbounded when unset.
    pub max_session_expiry: Option<Duration>,
    /// Topics that may hold a retained message at once; retaining a message
    /// on another topic is then refused with Quota Exceeded. The broker's
    /// own `$SYS` topics, kept in memory, don't count. Unbounded when unset.
    pub max_retained_messages: Option<usize>,
    /// Connections one client address may hold at once, counting the
    /// address a PROXY header gives; further ones are closed before any
    /// TLS or WebSocket handshake. IPv6 clients count by /64 block, and
    /// IPv4 clients of an IPv6 socket as their IPv4 address. Unbounded
    /// when unset, as many devices can share an address behind NAT.
    pub max_connections_per_ip: Option<usize>,
    /// PUBLISH packets per second one client may send, with a burst
    /// allowance. Beyond it the broker stops reading from the client until
    /// the rate allows another, so the client slows down to it as TCP's
    /// flow control pushes back; nothing is dropped or refused.
    /// In-process clients and those of trusted listeners are not limited.
    /// Unlimited when unset.
    pub max_publish_rate: Option<Rate>,
    /// Like `max_publish_rate`, for the payload bytes of the PUBLISH
    /// packets one client sends per second.
    pub max_publish_bytes_rate: Option<Rate>,
    /// How long a client may stay over `max_publish_rate` or
    /// `max_publish_bytes_rate` without a break: one that still has more
    /// waiting than its rate allows after this long is disconnected with
    /// Message Rate Too High, or just disconnected if it speaks MQTT 3.x.
    /// It is over its rate from the first PUBLISH the rate holds back
    /// until it has nothing waiting when the broker reads from it again.
    /// Clients are only slowed down when unset.
    pub publish_rate_grace: Option<Duration>,
    /// Failed logins that get a client address banned for a while,
    /// through the same bans as the admin API's, listed with their expiry
    /// and the reason `automatic: …`. Addresses count as for
    /// `max_connections_per_ip`; loopback addresses, in-process clients and
    /// those of trusted listeners are never banned. Off when unset.
    pub login_failures: Option<LoginFailures>,
    /// Sessions that outlive their connection the broker may hold,
    /// connected or not; a CONNECT that would start one more is refused
    /// with Quota Exceeded (Server Unavailable for MQTT 3.x). Resuming a
    /// session is never refused. Sessions that end with their connection
    /// (Clean Session in MQTT 3.x, a zero Session Expiry Interval in
    /// MQTT 5) don't count: `max_connections` bounds them. Unbounded when
    /// unset.
    pub max_sessions: Option<usize>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            write_timeout: crate::connection::DEFAULT_WRITE_TIMEOUT,
            max_packet_size: DEFAULT_MAX_PACKET_SIZE,
            max_connect_packet_size: DEFAULT_MAX_CONNECT_PACKET_SIZE,
            queue: QueueLimits::default(),
            max_connections: None,
            topic_alias_maximum: 10,
            receive_maximum: u16::MAX,
            max_inflight: DEFAULT_MAX_INFLIGHT,
            max_keepalive: None,
            send_topic_aliases: false,
            maximum_qos: QoS::ExactlyOnce,
            retain_available: true,
            persistent_client_expiration: None,
            check_retain_source: false,
            max_subscriptions: Some(DEFAULT_MAX_SUBSCRIPTIONS),
            max_topic_filter_length: DEFAULT_MAX_TOPIC_FILTER_LENGTH,
            max_session_expiry: None,
            max_retained_messages: None,
            max_connections_per_ip: None,
            max_publish_rate: None,
            max_publish_bytes_rate: None,
            publish_rate_grace: None,
            login_failures: None,
            max_sessions: None,
        }
    }
}

setters!(Limits {
    with_connect_timeout => connect_timeout: Duration,
    with_write_timeout => write_timeout: Duration,
    with_max_packet_size => max_packet_size: usize,
    with_max_connect_packet_size => max_connect_packet_size: usize,
    with_queue => queue: QueueLimits,
    with_max_connections => max_connections: Some(usize),
    with_topic_alias_maximum => topic_alias_maximum: u16,
    with_receive_maximum => receive_maximum: u16,
    with_max_inflight => max_inflight: u16,
    with_max_keepalive => max_keepalive: Some(u16),
    with_send_topic_aliases => send_topic_aliases: bool,
    with_maximum_qos => maximum_qos: QoS,
    with_retain_available => retain_available: bool,
    with_persistent_client_expiration => persistent_client_expiration: Some(Duration),
    with_check_retain_source => check_retain_source: bool,
    with_max_subscriptions => max_subscriptions: Option<usize>,
    with_max_topic_filter_length => max_topic_filter_length: usize,
    with_max_session_expiry => max_session_expiry: Some(Duration),
    with_max_retained_messages => max_retained_messages: Some(usize),
    with_max_connections_per_ip => max_connections_per_ip: Some(usize),
    with_max_publish_rate => max_publish_rate: Some(Rate),
    with_max_publish_bytes_rate => max_publish_bytes_rate: Some(Rate),
    with_publish_rate_grace => publish_rate_grace: Some(Duration),
    with_login_failures => login_failures: Some(LoginFailures),
    with_max_sessions => max_sessions: Some(usize),
});

/// A rate of so many a second, with a burst allowance: up to `burst` may
/// come at once after a quiet spell, and `per_second` on average.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Rate {
    pub per_second: u32,
    /// At least 1
    pub burst: u32,
}

impl Rate {
    pub fn new(per_second: u32, burst: u32) -> Self {
        Rate { per_second, burst }
    }
}

/// The broker-wide settings of a [`ServerBuilder`](super::ServerBuilder). Start from
/// `ServerConfig::default()` and set what differs; fields may be added in
/// minor releases.
#[non_exhaustive]
pub struct ServerConfig {
    /// For listeners without their own
    pub auth: AuthConfig,
    pub limits: Limits,
    pub policy: ConnectPolicy,
    /// How often to publish statistics under `$SYS/broker/`; never if unset.
    pub sys_interval: Option<Duration>,
    /// Run in this order for listeners without their own list; see
    /// [`crate::plugin`].
    pub plugins: Vec<Arc<dyn Plugin>>,
    /// Longest a plugin hook may take; one that takes longer denies, or
    /// drops the message.
    pub plugin_timeout: Duration,
    /// How often plugins' `on_tick` runs.
    pub plugin_tick_interval: Duration,
    /// Serves the `$CONTROL/broker/v1` commands the broker doesn't know.
    pub admin_extension: Option<Arc<dyn AdminExtension>>,
    /// How shared subscriptions pick the member a message goes to.
    pub share_strategy: ShareStrategy,
    /// When a member's messages go to the rest of its share group.
    pub share_redelivery: ShareRedelivery,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            auth: AuthConfig::default(),
            limits: Limits::default(),
            policy: ConnectPolicy::default(),
            sys_interval: Some(Duration::from_secs(10)),
            plugins: Vec::new(),
            plugin_timeout: Duration::from_secs(5),
            plugin_tick_interval: Duration::from_secs(1),
            admin_extension: None,
            share_strategy: ShareStrategy::default(),
            share_redelivery: ShareRedelivery::default(),
        }
    }
}

setters!(ServerConfig {
    with_auth => auth: AuthConfig,
    with_limits => limits: Limits,
    with_policy => policy: ConnectPolicy,
    with_sys_interval => sys_interval: Option<Duration>,
    with_plugins => plugins: Vec<Arc<dyn Plugin>>,
    with_plugin_timeout => plugin_timeout: Duration,
    with_plugin_tick_interval => plugin_tick_interval: Duration,
    with_admin_extension => admin_extension: Some(Arc<dyn AdminExtension>),
    with_share_strategy => share_strategy: ShareStrategy,
    with_share_redelivery => share_redelivery: ShareRedelivery,
});

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

/// Decides from a Unix socket client's credentials, as the kernel reports
/// them, whether it may connect. It runs on the accept loop, so it must not
/// block.
#[cfg(unix)]
pub type PeerFilter = Arc<dyn Fn(&UCred) -> bool + Send + Sync>;

/// A listener with its own transport and TLS settings.
#[non_exhaustive]
pub struct ListenerConfig {
    pub listener: Socket,
    pub transport: Transport,
    /// Serve TLS instead of plain TCP when set.
    pub tls: Option<Tls>,
    /// Expect every connection to start with a PROXY protocol header, as
    /// sent by load balancers, and take the client's address from it.
    pub proxy_protocol: bool,
    /// With `proxy_protocol`, the proxies allowed to send the header; other
    /// connections are closed, as their header could claim any address.
    /// Required on TCP listeners with `proxy_protocol`: `0.0.0.0/0` and
    /// `::/0` let any peer send one. Unix socket listeners take none, as
    /// their clients have no address.
    pub proxy_from: Vec<AddressRange>,
    /// Prefixed to every topic the listener's clients use, so they only see
    /// their own part of the topic tree.
    pub mount_point: Option<String>,
    /// Replaces the broker's `require_auth` and `allow_anonymous` for the
    /// listener's clients. The checks themselves come from
    /// [`ListenerConfig::plugins`].
    pub auth: Option<AuthConfig>,
    /// Replaces the broker's plugins for the listener's clients.
    pub plugins: Option<Vec<Arc<dyn Plugin>>>,
    /// Connections beyond this many on this listener are closed right after
    /// being accepted, on top of the broker-wide limit.
    pub max_connections: Option<usize>,
    /// On WebSocket listeners, answer plain HTTP requests with the files of
    /// this directory. Each response may take a minute, apart from the
    /// connect timeout, and a listener sends 32 at once at most; requests
    /// beyond that get Service Unavailable.
    pub http_dir: Option<PathBuf>,
    /// On WebSocket listeners, answer `GET /healthz` with 200 OK while the
    /// broker serves and 503 Service Unavailable once it is stopping.
    pub health: bool,
    /// On WebSocket listeners, answer `GET /metrics` with the `$SYS`
    /// counters in the Prometheus text format. They tell anyone who reaches
    /// the listener how loaded the broker is, so enable it on a listener
    /// only monitoring can reach, such as one on a loopback address.
    pub metrics: bool,
    /// On WebSocket listeners, the origins of the web pages that may
    /// connect, such as `https://app.example.com`. Browsers say which page
    /// opens a WebSocket in its `Origin` header; an upgrade from any other
    /// is refused with 403 Forbidden, so a page from another site can't
    /// reach the broker through its visitors' browsers. Clients other than
    /// browsers send no `Origin` and are not affected. Any origin may
    /// connect when unset.
    pub allowed_origins: Option<Vec<String>>,
    /// Its clients skip authentication and access control, `$CONTROL` and
    /// every admin command included, as in-process connections do. They also
    /// skip the connection limits, the redirect and the client id prefixes,
    /// so an operator can reach a full or redirecting broker; bans still
    /// apply. Only a Unix socket listener without `auth`, `plugins`,
    /// `proxy_protocol` or the WebSocket transport can be trusted, as a proxy
    /// in front of it would make every remote client trusted; give it a `peer_filter`, or make sure the socket's
    /// permissions keep everyone else out.
    pub trusted: bool,
    /// On a Unix socket listener, decides from each client's credentials
    /// whether it may connect, before anything is read from it. Clients it
    /// refuses, or whose credentials can't be read, are closed at once.
    #[cfg(unix)]
    pub peer_filter: Option<PeerFilter>,
    /// New connections per second the listener accepts, with a burst
    /// allowance; further ones are closed right after being accepted, as
    /// those beyond `max_connections` are. Unlimited when unset.
    pub max_connection_rate: Option<Rate>,
    /// Replaces [`Limits::max_packet_size`] for the listener's clients.
    pub max_packet_size: Option<usize>,
    /// Replaces [`Limits::max_connect_packet_size`] for the listener's
    /// clients.
    pub max_connect_packet_size: Option<usize>,
    /// Replaces [`Limits::max_keepalive`] for the listener's clients.
    pub max_keepalive: Option<u16>,
    /// Replaces [`Limits::connect_timeout`] for the listener's clients.
    pub connect_timeout: Option<Duration>,
    /// The client id of each session is the user name its client
    /// authenticated as, whatever the client sent, as mosquitto's
    /// `use_username_as_clientid`; MQTT 5 clients are told it as their
    /// Assigned Client Identifier. Clients without a verified user name are
    /// refused with Not Authorized. One user then has one session, and no
    /// client can take over another user's id.
    pub use_username_as_clientid: bool,
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
            plugins: None,
            max_connections: None,
            http_dir: None,
            health: false,
            metrics: false,
            allowed_origins: None,
            trusted: false,
            #[cfg(unix)]
            peer_filter: None,
            max_connection_rate: None,
            max_packet_size: None,
            max_connect_packet_size: None,
            max_keepalive: None,
            connect_timeout: None,
            use_username_as_clientid: false,
        }
    }

    /// What makes these settings unservable, if anything.
    pub(super) fn problem(&self) -> Option<ConfigErrorKind> {
        let unix = !matches!(self.listener, Socket::Tcp(_));
        if self.trusted && !unix {
            return Some(ConfigErrorKind::TrustedTcp);
        }
        if self.trusted && (self.auth.is_some() || self.plugins.is_some()) {
            return Some(ConfigErrorKind::TrustedWithAuth);
        }
        if self.trusted && (self.proxy_protocol || self.transport == Transport::WebSocket) {
            return Some(ConfigErrorKind::TrustedBehindProxy);
        }
        if let Some(mount_point) = self.mount_point.as_deref() {
            if mount_point.contains(['+', '#']) {
                return Some(ConfigErrorKind::MountPointWildcard(mount_point.into()));
            }
        }
        if self.transport != Transport::WebSocket {
            let websocket_only = [
                ("http_dir", self.http_dir.is_some()),
                ("allowed_origins", self.allowed_origins.is_some()),
                ("health", self.health),
                ("metrics", self.metrics),
            ];
            if let Some((setting, _)) = websocket_only.into_iter().find(|(_, set)| *set) {
                return Some(ConfigErrorKind::WebSocketOnly(setting));
            }
        }
        // A Unix socket's file permissions decide who may send the header
        if self.proxy_protocol && self.proxy_from.is_empty() && !unix {
            return Some(ConfigErrorKind::ProxyFromRequired);
        }
        if self.use_username_as_clientid && self.trusted {
            return Some(ConfigErrorKind::TrustedUsernameAsClientId);
        }
        if !self.proxy_from.is_empty() && unix {
            return Some(ConfigErrorKind::ProxyFromOnUnix);
        }
        #[cfg(unix)]
        if self.peer_filter.is_some() && !unix {
            return Some(ConfigErrorKind::PeerFilterOnTcp);
        }
        None
    }
}

setters!(ListenerConfig {
    with_tls => tls: Some(Tls),
    with_proxy_protocol => proxy_protocol: bool,
    with_proxy_from => proxy_from: Vec<AddressRange>,
    with_mount_point => mount_point: Some(String),
    with_auth => auth: Some(AuthConfig),
    with_plugins => plugins: Some(Vec<Arc<dyn Plugin>>),
    with_max_connections => max_connections: Some(usize),
    with_http_dir => http_dir: Some(PathBuf),
    with_health => health: bool,
    with_metrics => metrics: bool,
    with_allowed_origins => allowed_origins: Some(Vec<String>),
    with_trusted => trusted: bool,
    #[cfg(unix)]
    with_peer_filter => peer_filter: Some(PeerFilter),
    with_max_connection_rate => max_connection_rate: Some(Rate),
    with_max_packet_size => max_packet_size: Some(usize),
    with_max_connect_packet_size => max_connect_packet_size: Some(usize),
    with_max_keepalive => max_keepalive: Some(u16),
    with_connect_timeout => connect_timeout: Some(Duration),
    with_use_username_as_clientid => use_username_as_clientid: bool,
});

/// Opens in-process connections to a broker, for parts of the same program
/// such as bridges. These connections skip authentication, access control
/// and the connection limit.
#[derive(Clone)]
pub struct LocalConnector(mpsc::UnboundedSender<DuplexStream>);

/// The broker's end of a [`LocalConnector`], given to
/// [`ServerBuilder::local_listener`](super::ServerBuilder::local_listener).
pub struct LocalListener {
    pub(super) connections: mpsc::UnboundedReceiver<DuplexStream>,
    /// Handed out by [`Server::local_connector`](super::Server::local_connector)
    pub(super) connector: LocalConnector,
}

impl LocalConnector {
    /// A connector, and the listener for [`ServerBuilder::local_listener`](super::ServerBuilder::local_listener)
    /// it reaches the broker through. Needed only to connect before the
    /// broker is built; [`Server::local_connector`](super::Server::local_connector) gives one otherwise.
    pub fn new() -> (LocalConnector, LocalListener) {
        let (tx, rx) = mpsc::unbounded_channel();
        let connector = LocalConnector(tx);
        let listener = LocalListener {
            connections: rx,
            connector: connector.clone(),
        };
        (connector, listener)
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

/// How MQTT is carried on a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Transport {
    /// MQTT packets directly on the stream
    Mqtt,
    /// MQTT inside WebSocket binary frames, with the `mqtt` subprotocol;
    /// plain HTTP requests can be answered from `http_dir`
    WebSocket,
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

setters!(ConnectPolicy {
    with_response_information => response_information: Some(String),
    with_redirect => redirect: Some(Redirect),
    with_require_client_id => require_client_id: bool,
    with_auto_id_prefix => auto_id_prefix: String,
    with_client_id_prefixes => client_id_prefixes: Vec<String>,
});

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
