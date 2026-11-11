use std::{
    collections::{HashMap, HashSet, VecDeque},
    net::SocketAddr,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use crate::CONNECTIONS;
use bytes::Bytes;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::Mutex,
    task::AbortHandle,
    time::Duration,
};
use tracing::{info, warn};

use mercurio_core::{
    message::{Message, MessageProperties},
    properties::{
        AssignedClientIdentifier, AuthenticationData, AuthenticationMethod, MaximumPacketSize,
        MaximumQoS, ReceiveMaximum, ResponseInformation, RetainAvailable, ServerKeepAlive,
        SessionExpiryInterval, SharedSubscriptionAvailable, SubscriptionIdentifierAvailable,
        TopicAliasMaximum, WildcardSubscriptionAvailable,
    },
    protocol::ProtocolVersion,
    qos::QoS,
    reason::ReasonCode,
    topic::{parse_shared_subscription, validate_subscribe_filter},
};

use crate::error::Result;
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connack::{ConnAckPacket, ConnAckProperties},
    connect::ConnectPacket,
    pingresp::PingRespPacket,
    publish::PublishProperties,
    suback::{SubAckPacket, SubAckPayload},
    subscribe::RetainHandling,
    unsuback::{UnsubAckPacket, UnsubAckPayload},
    unsubscribe::UnsubscribePacket,
    ControlPacket,
};

use crate::{
    acl::Access,
    broker::{Broker, Publisher},
    connection::Connection,
    plugin::{AuthResult, Chain, ClientInfo, ConnectionInfo, Event, Exchange},
    queue::{MessageQueue, QueueLimits, Subscriber},
    server::{ConnectPolicy, Limits},
    shutdown::Tasks,
    sys::{DropCause, Stats},
    topic_tree::TopicTree,
};
use mercurio_storage::{
    InflightMessage, RetainedMessageStore, SessionOwner, SessionState, StoredSubscription, Write,
};

mod inbound;
mod outbound;
mod presence;
mod retained;
mod will;

use outbound::Unacknowledged;
pub(crate) use outbound::{Held, Outgoing};
use presence::Presence;
pub(crate) use presence::SessionSnapshot;
use retained::RetainedDelivery;
pub use will::WillMessage;

/// The application properties of a received PUBLISH, which are forwarded to
/// subscribers. Per-hop properties such as the topic alias are left out.
fn message_properties(properties: Option<&PublishProperties>) -> MessageProperties {
    let Some(p) = properties else {
        return MessageProperties::default();
    };
    MessageProperties {
        payload_format_indicator: p.payload_format_indicator.clone(),
        message_expiry_interval: p.message_expiry_interval.clone(),
        content_type: p.content_type.clone(),
        response_topic: p.response_topic.clone(),
        correlation_data: p.correlation_data.clone(),
        user_property: p.user_property.clone(),
    }
}

/// Lowers the session expiry an MQTT 5 client asks for to the server's
/// maximum, no expiry included, so every later use of the CONNECT sees the
/// expiry the session will have.
pub(crate) fn cap_session_expiry(connect_packet: &mut ConnectPacket, limits: &Limits) {
    let Some(max) = limits.max_session_expiry else {
        return;
    };
    if !connect_packet.protocol_version.supports_properties() {
        return;
    }
    let max = max_expiry_interval(max);
    let requested = connect_packet
        .properties
        .as_ref()
        .and_then(|p| p.session_expiry_interval.as_ref())
        .map_or(0, |e| e.value);
    if requested > max {
        connect_packet
            .properties
            .get_or_insert_with(Default::default)
            .session_expiry_interval = Some(SessionExpiryInterval::new(max));
    }
}

/// Lowers the Session Expiry Interval a DISCONNECT asks for to the server's
/// maximum, no expiry included, as CONNECT's is.
pub(crate) fn cap_expiry_interval(interval: u32, limits: &Limits) -> u32 {
    limits
        .max_session_expiry
        .map_or(interval, |max| interval.min(max_expiry_interval(max)))
}

/// `max` as a Session Expiry Interval. u32::MAX means no expiry, so the
/// most a maximum can be is one less.
fn max_expiry_interval(max: Duration) -> u32 {
    u32::try_from(max.as_secs())
        .unwrap_or(u32::MAX - 1)
        .min(u32::MAX - 1)
}

/// The session expiry to announce in CONNACK: the server's maximum, when
/// the client's request was lowered to it.
fn announced_session_expiry(client: &ClientSettings, limits: &Limits) -> Option<u32> {
    let max = max_expiry_interval(limits.max_session_expiry?);
    (client.session_expiry_interval? == max).then_some(max)
}

/// The keep-alive the connection runs with: the client's own, unless it
/// exceeds the server's maximum or disables keep-alive, in which case MQTT 5
/// clients are told to use the maximum. MQTT 3.x has no way to tell them.
pub(crate) fn effective_keepalive(
    requested: u16,
    protocol_version: ProtocolVersion,
    limits: &Limits,
) -> u16 {
    match limits.max_keepalive {
        Some(max)
            if protocol_version.supports_properties() && (requested == 0 || requested > max) =>
        {
            max
        }
        _ => requested,
    }
}

/// What a session keeps of its client's latest CONNECT: only what is read
/// once the connection is up. The rest, credentials and user properties
/// included, goes with the packet instead of living as long as the session,
/// which may be forever.
#[derive(Debug, Clone)]
struct ClientSettings {
    protocol_version: ProtocolVersion,
    clean_start: bool,
    keepalive: u16,
    /// As requested, once capped to the server's maximum
    session_expiry_interval: Option<u32>,
    receive_maximum: Option<u16>,
    maximum_packet_size: Option<u32>,
    topic_alias_maximum: Option<u16>,
    request_response_information: bool,
    /// Whether the client takes reason strings beyond the reason code's
    /// own: true unless it sent Request Problem Information 0
    request_problem_information: bool,
    /// The enhanced authentication method, which every AUTH must name
    authentication_method: Option<String>,
}

impl From<&ConnectPacket> for ClientSettings {
    fn from(connect: &ConnectPacket) -> Self {
        let properties = connect.properties.as_ref();
        ClientSettings {
            protocol_version: connect.protocol_version,
            clean_start: connect.clean_start,
            keepalive: connect.keepalive,
            session_expiry_interval: properties
                .and_then(|p| p.session_expiry_interval.as_ref())
                .map(|e| e.value),
            receive_maximum: properties
                .and_then(|p| p.receive_maximum.as_ref())
                .map(|r| r.value),
            maximum_packet_size: properties
                .and_then(|p| p.maximum_packet_size.as_ref())
                .map(|m| m.value),
            topic_alias_maximum: properties
                .and_then(|p| p.topic_alias_maximum.as_ref())
                .map(|m| m.value),
            request_response_information: properties
                .and_then(|p| p.request_response_information.as_ref())
                .is_some_and(|r| r.value == 1),
            request_problem_information: properties
                .and_then(|p| p.request_problem_information.as_ref())
                .is_none_or(|r| r.value != 0),
            authentication_method: properties
                .and_then(|p| p.authentication_method.as_ref())
                .map(|m| m.value.clone()),
        }
    }
}

/// Source of unique subscriber ids, one per session.
static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);

pub struct SessionDropGuard {
    session: Session,
    /// Where unsubscribing runs when it has to wait
    tasks: Tasks,
}

#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
    /// Messages waiting to be sent, filled by the broker
    queue: Arc<MessageQueue>,
    /// Identifies this session's subscriptions in the topic tree
    subscriber_id: u64,
    /// A hash of the client id, which never changes for a session
    client_key: u64,
    /// Generation of the latest connection to use this session. Changed
    /// with `state` locked, so checks made under that lock see it settled;
    /// readable without the lock by the session manager.
    generation: AtomicU64,
    /// Whether `State::retained` holds anything, read without the lock on
    /// every turn of the connection loop
    owes_retained: AtomicBool,
}

struct State {
    client: ClientSettings,
    /// Cached client ID to avoid lock acquisition for logging
    client_id: String,
    /// Topic filters subscribed to, with their granted QoS
    subscriptions: HashMap<String, StoredSubscription>,
    /// Where the subscriptions are registered, once there are any
    topics: Option<TopicTree<Subscriber>>,
    /// Retained messages owed to new subscriptions, in subscription order
    retained: VecDeque<RetainedDelivery>,
    /// Outgoing QoS 1/2 messages awaiting PUBACK/PUBREC, by packet_id
    unacknowledged_messages: HashMap<u16, Unacknowledged>,
    /// Outgoing QoS 2 messages awaiting PUBCOMP, by packet_id, with the
    /// sequence they were sent in
    pubrecs: HashMap<u16, u64>,
    /// Unacknowledged messages a resumed session has yet to resend on this
    /// connection, in order, held back by the client's Receive Maximum
    unsent: VecDeque<u16>,
    /// Sequence number of the next outgoing QoS 1/2 message
    next_sequence: u64,
    /// Incoming QoS 2 packet ids already delivered and awaiting PUBREL
    incoming_qos2: HashSet<u16>,
    /// Last packet identifier assigned to an outgoing QoS > 0 PUBLISH
    last_packet_id: u16,
    /// Topic names the client registered under an alias on this connection
    topic_aliases: HashMap<u16, String>,
    /// Highest alias the client may use, as advertised in CONNACK
    topic_alias_maximum: u16,
    /// Aliases this connection assigned to topics sent to the client
    outgoing_aliases: HashMap<String, u16>,
    send_topic_aliases: bool,
    /// Highest QoS the client may use, as advertised in CONNACK
    maximum_qos: QoS,
    retain_available: bool,
    check_retain_source: bool,
    /// QoS 2 messages the client may have awaiting PUBREL, as advertised
    receive_maximum: u16,
    /// Most QoS 1 and 2 messages sent and not yet acknowledged, whatever
    /// the client's Receive Maximum
    max_inflight: u16,
    /// Will message to be published on abnormal disconnect
    will_message: Option<WillMessage>,
    /// The task holding the will of the last connection until its delay
    /// passes, with a copy of that will and the connection's generation; a
    /// connection that takes the session over aborts it
    pending_will: Option<(AbortHandle, WillMessage, u64)>,
    /// Re-authentication in progress, if any
    auth_exchange: Option<Exchange>,
    /// Why the last re-authentication failed, in the method's words, for
    /// the DISCONNECT that follows
    auth_failure: Option<String>,
    /// User name verified by authentication, the one access control trusts
    verified_user: Option<String>,
    /// The client's address, None for in-process clients
    peer: Option<SocketAddr>,
    /// The connection plugins are told about; None until a client connects
    connection_info: Option<Arc<ConnectionInfo>>,
    /// Who may resume the session
    owner: SessionOwner,
    max_subscriptions: Option<usize>,
    max_topic_filter_length: usize,
    /// What SUBSCRIBE and UNSUBSCRIBE changed since they were last stored
    subscription_changes: Vec<Write>,
    max_retained_messages: Option<usize>,
    /// What the admin API reports about the connection
    presence: Presence,
}

/// Takes the session's subscriptions out of the topic tree once the
/// session leaves the session manager, even when what removed it is
/// cancelled before it unsubscribed: nothing is queued for a session no one
/// can reach.
impl Drop for SessionDropGuard {
    fn drop(&mut self) {
        let shared = &self.session.shared;
        if let Ok(mut state) = shared.state.try_lock() {
            return state.unsubscribe_all(shared.subscriber_id);
        }
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let session = self.session.clone();
            self.tasks
                .spawn_on(async move { session.unsubscribe_all().await }, &runtime);
        }
    }
}

impl SessionDropGuard {
    pub(crate) fn new(
        connect_packet: ConnectPacket,
        queue_limits: QueueLimits,
        stats: Arc<Stats>,
        tasks: Tasks,
    ) -> Self {
        SessionDropGuard {
            session: Session::with_queue_limits(connect_packet, queue_limits, stats),
            tasks,
        }
    }

    /// Guards `session`, unsubscribing in `tasks` when that has to wait.
    pub(crate) fn guarding(session: Session, tasks: Tasks) -> Self {
        SessionDropGuard { session, tasks }
    }

    pub(crate) fn session(&self) -> Session {
        self.session.clone()
    }
}

impl Session {
    #[cfg(test)]
    pub fn new(connect_packet: ConnectPacket) -> Self {
        Self::with_queue_limits(connect_packet, QueueLimits::default(), Default::default())
    }

    /// A session whose queue holds up to `queue_limits` and counts the
    /// messages it loses in `stats`.
    pub(crate) fn with_queue_limits(
        connect_packet: ConnectPacket,
        queue_limits: QueueLimits,
        stats: Arc<Stats>,
    ) -> Self {
        let client_id = connect_packet.payload.client_id.clone();
        let client_key = {
            use std::hash::{Hash, Hasher};
            let mut hasher = std::hash::DefaultHasher::new();
            client_id.hash(&mut hasher);
            hasher.finish()
        };
        let will_message = WillMessage::from_connect_packet(&connect_packet);
        let client = ClientSettings::from(&connect_packet);
        let presence = Presence::default();
        let queue = MessageQueue::new(queue_limits).counted_in(stats);
        queue.set_counters(Arc::clone(&presence.counters));
        Session {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    client,
                    client_id,
                    subscriptions: HashMap::new(),
                    topics: None,
                    retained: VecDeque::new(),
                    unacknowledged_messages: HashMap::new(),
                    pubrecs: HashMap::new(),
                    unsent: VecDeque::new(),
                    next_sequence: 0,
                    incoming_qos2: HashSet::new(),
                    last_packet_id: 0,
                    topic_aliases: HashMap::new(),
                    topic_alias_maximum: 0,
                    outgoing_aliases: HashMap::new(),
                    send_topic_aliases: false,
                    maximum_qos: QoS::ExactlyOnce,
                    retain_available: true,
                    check_retain_source: false,
                    receive_maximum: u16::MAX,
                    max_inflight: u16::MAX,
                    will_message,
                    pending_will: None,
                    auth_exchange: None,
                    auth_failure: None,
                    verified_user: None,
                    peer: None,
                    connection_info: None,
                    owner: SessionOwner::default(),
                    max_subscriptions: None,
                    max_topic_filter_length: usize::MAX,
                    subscription_changes: Vec::new(),
                    max_retained_messages: None,
                    presence,
                }),
                queue: Arc::new(queue),
                subscriber_id: NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed),
                client_key,
                generation: AtomicU64::new(0),
                owes_retained: AtomicBool::new(false),
            }),
        }
    }

    pub(crate) async fn set_connect_packet(&mut self, connect_packet: ConnectPacket) {
        let mut session = self.shared.state.lock().await;
        session.client_id = connect_packet.payload.client_id.clone();
        session.will_message = WillMessage::from_connect_packet(&connect_packet);
        session.client = ClientSettings::from(&connect_packet);
    }

    pub(crate) fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    /// Hands the session to `generation`, dropping the will an earlier
    /// connection left waiting for its delay.
    pub(crate) async fn set_generation(&self, generation: u64) {
        let mut state = self.shared.state.lock().await;
        self.shared.generation.store(generation, Ordering::Release);
        if let Some((will, _, _)) = state.pending_will.take() {
            will.abort();
        }
    }

    /// Hands the session to `generation`, a connection that has yet to
    /// start it, keeping the will an earlier connection left waiting: if
    /// that connection is abandoned, the will is still due.
    pub(crate) async fn hand_over(&self, generation: u64) {
        let _state = self.shared.state.lock().await;
        self.shared.generation.store(generation, Ordering::Release);
    }

    /// Returns a clone of the client ID. For logging, prefer using the cached version.
    pub(crate) async fn get_client_id(&self) -> String {
        let session = self.shared.state.lock().await;
        session.client_id.clone()
    }

    /// Returns the subscriptions of this session with their granted QoS and
    /// options.
    pub(crate) async fn get_subscriptions(&self) -> Vec<StoredSubscription> {
        let session = self.shared.state.lock().await;
        session.subscriptions.values().cloned().collect()
    }

    /// What SUBSCRIBE and UNSUBSCRIBE packets changed since this was last
    /// called, as writes to store them.
    pub(crate) async fn take_subscription_changes(&self) -> Vec<Write> {
        std::mem::take(&mut self.shared.state.lock().await.subscription_changes)
    }

    /// Restore subscriptions by re-subscribing to the broker.
    /// Called when resuming a persisted session.
    pub(crate) async fn restore_subscriptions<S: RetainedMessageStore>(
        &mut self,
        subscriptions: Vec<StoredSubscription>,
        broker: &Broker<S>,
    ) -> Result<()> {
        let mut session = self.shared.state.lock().await;

        session.topics = Some(broker.topics());
        for subscription in subscriptions {
            // Stored by an older version that accepted it, such as one
            // deeper than the level limit
            if let Err(e) = validate_subscribe_filter(&subscription.topic_filter) {
                warn!(
                    "Dropping stored subscription of `{}` to an invalid filter: {}",
                    session.client_id, e
                );
                continue;
            }
            // Retained messages are only sent for a new SUBSCRIBE, not when an
            // existing subscription is resumed [MQTT-3.3.1-9]
            broker.subscribe(
                &subscription.topic_filter,
                self.shared.subscriber_id,
                self.subscriber(&subscription),
            );
            session
                .subscriptions
                .insert(subscription.topic_filter.clone(), subscription);
        }

        Ok(())
    }

    pub async fn begin<S>(
        &mut self,
        connection: &mut Connection<S>,
        resume: bool,
        assigned_client_id: bool,
        auth_data: Option<Bytes>,
        limits: &Limits,
        policy: &ConnectPolicy,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut ack = ConnAckPacket::default();
        ack.flags.session_present = resume;

        let client_id = {
            let mut session = self.shared.state.lock().await;

            // Topic aliases only live as long as the connection [MQTT-3.3.2-7]
            session.topic_aliases.clear();
            session.outgoing_aliases.clear();
            session.send_topic_aliases = limits.send_topic_aliases;
            session.maximum_qos = limits.maximum_qos;
            session.retain_available = limits.retain_available;
            session.check_retain_source = limits.check_retain_source;
            session.topic_alias_maximum = limits.topic_alias_maximum;
            session.receive_maximum = limits.receive_maximum;
            session.max_inflight = limits.max_inflight.max(1);
            session.max_subscriptions = limits.max_subscriptions;
            session.max_topic_filter_length = limits.max_topic_filter_length;
            session.max_retained_messages = limits.max_retained_messages;

            // CONNACK properties only exist in MQTT 5.0
            if session.client.protocol_version.supports_properties() {
                ack.properties = Some(ConnAckProperties {
                    assigned_client_id: assigned_client_id
                        .then(|| AssignedClientIdentifier::new(session.client_id.clone())),
                    maximum_packet_size: Some(MaximumPacketSize::new(
                        limits.max_packet_size.try_into().unwrap_or(u32::MAX),
                    )),
                    topic_alias_max: Some(TopicAliasMaximum::new(limits.topic_alias_maximum)),
                    // Echoes the method a successful enhanced authentication used
                    authentication_method: session
                        .client
                        .authentication_method
                        .clone()
                        .map(AuthenticationMethod::new),
                    authentication_data: auth_data.map(AuthenticationData::new),
                    receive_maximum: Some(ReceiveMaximum::new(limits.receive_maximum)),
                    response_information: policy
                        .response_information
                        .as_ref()
                        .filter(|_| session.client.request_response_information)
                        .map(|template| {
                            ResponseInformation::new(template.replace("%c", &session.client_id))
                        }),
                    session_expiry_interval: announced_session_expiry(&session.client, limits)
                        .map(SessionExpiryInterval::new),
                    server_keepalive: {
                        let client = &session.client;
                        let keepalive =
                            effective_keepalive(client.keepalive, client.protocol_version, limits);
                        (keepalive != client.keepalive).then(|| ServerKeepAlive::new(keepalive))
                    },
                    // Spelled out even where absence means the same, for
                    // clients and tools that inspect them
                    retain_available: Some(RetainAvailable::new(limits.retain_available)),
                    // Absent means QoS 2; only 0 and 1 may be sent
                    maximum_qos: (limits.maximum_qos != QoS::ExactlyOnce)
                        .then(|| MaximumQoS::new(limits.maximum_qos as u8)),
                    wildcard_subscription_available: Some(WildcardSubscriptionAvailable::new(true)),
                    subscription_identifier_available: Some(SubscriptionIdentifierAvailable::new(
                        true,
                    )),
                    shared_subscription_available: Some(SharedSubscriptionAvailable::new(true)),
                    ..Default::default()
                });
            }

            session.client_id.clone()
        };

        info!(
            target: CONNECTIONS,
            "Client with id `{}` {} a session",
            client_id,
            if resume { "resumed" } else { "started" }
        );

        connection.write_packet(ControlPacket::ConnAck(ack)).await?;

        Ok(())
    }

    /// Who the client of this session is, for access control.
    pub(crate) async fn identity(&self) -> ClientInfo {
        self.shared.state.lock().await.client_info()
    }

    /// Identifies this session's subscriptions in the topic tree.
    pub(crate) fn subscriber_id(&self) -> u64 {
        self.shared.subscriber_id
    }

    /// This session as the publisher of a message.
    fn publisher(&self) -> Publisher {
        Publisher {
            session: self.shared.subscriber_id,
            key: self.shared.client_key,
        }
    }

    /// Counts a message for the client lost for `cause`.
    pub(crate) fn count_drop(&self, cause: DropCause) {
        self.shared.queue.count_drop(cause, 1);
    }

    /// What storage holds for this session when it is up to date, as the
    /// writes that store it once what was stored is removed: the session
    /// with its subscriptions, its will if it has one, and for a session
    /// that outlives its connection, its messages in flight, the packet ids
    /// of incoming QoS 2 messages awaiting PUBREL and its journaled queue.
    pub(crate) async fn stored_state(&self, persistent: bool) -> Vec<Write> {
        let state = self.shared.state.lock().await;
        let mut writes = vec![Write::SaveSession(SessionState {
            client_id: state.client_id.clone(),
            subscriptions: state.subscriptions.values().cloned().collect(),
            expiry_interval: state
                .presence
                .session_expiry
                .map(|expiry| expiry.as_secs().try_into().unwrap_or(u32::MAX)),
            expires_at: state.presence.expires_at,
            owner: state.owner.clone(),
        })];
        let pending = state.pending_will.as_ref().map(|(_, will, _)| will);
        if let Some(will) = state.will_message.as_ref().or(pending) {
            writes.push(Write::StoreWill(will.clone().into()));
        }
        if !persistent {
            return writes;
        }
        for (packet_id, sent) in &state.unacknowledged_messages {
            let properties = sent.publish.properties.as_ref();
            writes.push(Write::StoreInflight(InflightMessage {
                packet_id: *packet_id,
                topic: sent.publish.topic_name.clone(),
                payload: sent.publish.payload.clone(),
                qos: sent.publish.qos_level,
                dup: false,
                retain: sent.publish.retain,
                expires_at: sent.expires_at,
                sequence: sent.sequence,
                released: false,
                properties: message_properties(properties),
                subscription_identifiers: properties
                    .map(|p| {
                        p.subscription_identifiers
                            .iter()
                            .map(|id| id.value())
                            .collect()
                    })
                    .unwrap_or_default(),
                share_group: sent.share_group.as_deref().map(str::to_string),
            }));
        }
        for (packet_id, sequence) in &state.pubrecs {
            writes.push(Write::StoreInflight(InflightMessage {
                packet_id: *packet_id,
                topic: String::new(),
                payload: None,
                qos: QoS::ExactlyOnce,
                dup: false,
                retain: false,
                expires_at: None,
                sequence: *sequence,
                released: true,
                properties: MessageProperties::default(),
                subscription_identifiers: Vec::new(),
                share_group: None,
            }));
        }
        writes.extend(
            state
                .incoming_qos2
                .iter()
                .map(|packet_id| Write::StoreIncomingQos2(*packet_id)),
        );
        writes.extend(
            self.shared
                .queue
                .journaled()
                .into_iter()
                .map(|(sequence, message)| Write::Enqueue(sequence, message)),
        );
        writes
    }

    /// Starts keeping the queue's QoS 1 and 2 messages in storage, unless
    /// it does already. With `recorded`, those queued are there already.
    pub(crate) fn start_queue_journal(&self, journal: crate::writer::Writer, recorded: bool) {
        self.shared.queue.start_journal(journal, recorded);
    }

    pub(crate) fn stop_queue_journal(&self) {
        self.shared.queue.stop_journal();
    }

    /// Puts back messages queued for the client before a restart.
    pub(crate) fn restore_queue(&self, messages: Vec<(u64, Message)>) {
        self.shared.queue.restore(messages);
    }

    /// Why the last re-authentication failed, in the method's words, if
    /// it gave some and the client takes them.
    pub(crate) async fn take_auth_failure(&self) -> Option<String> {
        self.shared.state.lock().await.auth_failure.take()
    }

    /// The largest packet the client accepts, if it set a limit.
    pub(crate) async fn client_maximum_packet_size(&self) -> Option<u32> {
        let session = self.shared.state.lock().await;
        session.client_maximum_packet_size().map(|m| m as u32)
    }

    /// Records the user name authentication verified for this connection.
    pub(crate) async fn set_verified_user(&self, username: Option<String>) {
        self.shared.state.lock().await.verified_user = username;
    }

    /// Holds the session's state until dropped, so whatever needs it waits.
    #[cfg(test)]
    pub(crate) async fn hold_state(&self) -> impl Sized + '_ {
        self.shared.state.lock().await
    }

    /// Records the address and connection of the client now connected to
    /// this session.
    pub(crate) async fn set_peer(
        &self,
        peer: Option<SocketAddr>,
        connection_info: Option<Arc<ConnectionInfo>>,
    ) {
        let mut state = self.shared.state.lock().await;
        state.peer = peer;
        state.connection_info = connection_info;
    }

    pub(crate) async fn owner(&self) -> SessionOwner {
        self.shared.state.lock().await.owner.clone()
    }

    pub(crate) async fn set_owner(&self, owner: SessionOwner) {
        self.shared.state.lock().await.owner = owner;
    }

    async fn handle_subscribe<S: RetainedMessageStore>(
        &mut self,
        packet: mercurio_packets::subscribe::SubscribePacket,
        broker: &Broker<S>,
        chain: &Chain,
    ) -> Result<Option<ControlPacket>> {
        let subscription_identifier = packet
            .properties
            .as_ref()
            .and_then(|p| p.subscription_id.as_ref())
            .map(|id| id.value());

        // A client can't opt out of its own messages on a shared subscription
        // [MQTT-3.8.3-4]
        if packet.payload.iter().any(|sub| {
            sub.subs_opt.no_local && parse_shared_subscription(&sub.topic_filter).is_some()
        }) {
            return Err(ReasonCode::ProtocolError.into());
        }

        // Access control may wait on a plugin, so it runs before the
        // session state is locked
        let mut refused = Vec::with_capacity(packet.payload.len());
        let identity = self.identity().await;
        let max_length = self.shared.state.lock().await.max_topic_filter_length;
        for sub in &packet.payload {
            refused.push(
                if sub.topic_filter.len() > max_length
                    || validate_subscribe_filter(&sub.topic_filter).is_err()
                {
                    Some(ReasonCode::TopicFilterInvalid)
                } else if !chain.is_empty() && !chain.is_trusted() {
                    // The share group decides who gets each message, not which
                    // messages flow, so only the topic filter is checked, as
                    // mosquitto does
                    let filter = parse_shared_subscription(&sub.topic_filter)
                        .map_or(sub.topic_filter.as_str(), |(_, filter)| filter);
                    if chain.may(&identity, filter, Access::Subscribe).await {
                        None
                    } else {
                        warn!(
                            "Client `{}` may not subscribe to `{}`",
                            identity.client_id, sub.topic_filter
                        );
                        Some(ReasonCode::NotAuthorized)
                    }
                } else {
                    None
                },
            );
        }

        let mut session = self.shared.state.lock().await;
        let mut ack = SubAckPacket {
            packet_id: packet.packet_id,
            properties: None,
            payload: Vec::with_capacity(packet.payload.len()),
        };

        for (sub, refused) in packet.payload.iter().zip(refused) {
            if let Some(reason_code) = refused {
                ack.payload.push(SubAckPayload { reason_code });
                continue;
            }

            let at_limit = session
                .max_subscriptions
                .is_some_and(|max| session.subscriptions.len() >= max);
            if at_limit && !session.subscriptions.contains_key(&sub.topic_filter) {
                warn!(
                    "Client `{}` holds the most subscriptions allowed",
                    session.client_id
                );
                ack.payload.push(SubAckPayload {
                    reason_code: ReasonCode::QuotaExceeded,
                });
                continue;
            }

            let granted_qos = if sub.subs_opt.qos as u8 > session.maximum_qos as u8 {
                session.maximum_qos
            } else {
                sub.subs_opt.qos
            };
            let subscription = StoredSubscription {
                topic_filter: sub.topic_filter.clone(),
                qos: granted_qos,
                no_local: sub.subs_opt.no_local,
                retain_as_published: sub.subs_opt.retain_as_pub,
                subscription_identifier,
            };
            let subscriber = self.subscriber(&subscription);
            let existed = session.subscriptions.contains_key(&sub.topic_filter);
            broker.subscribe(
                &sub.topic_filter,
                self.shared.subscriber_id,
                subscriber.clone(),
            );
            if !chain.is_empty() {
                chain.emit(Event::Subscribed {
                    client: session.client_info(),
                    filter: sub.topic_filter.clone(),
                    qos: granted_qos,
                });
            }

            // Shared subscriptions are never sent retained messages
            let shared = parse_shared_subscription(&sub.topic_filter).is_some();
            let send_retained = !shared
                && match sub.subs_opt.retain_handling {
                    RetainHandling::SendRetained => true,
                    RetainHandling::SendRetainedIfNonExisting => !existed,
                    _ => false,
                };
            // Queued a page at a time by the connection loop. A repeated
            // subscription starts over, or else sends what the earlier one
            // was still owed with the new options.
            if send_retained {
                session.retained.retain(|d| d.filter != sub.topic_filter);
                session.retained.push_back(RetainedDelivery {
                    filter: sub.topic_filter.clone(),
                    subscriber,
                    messages: VecDeque::new(),
                    after: None,
                    done: false,
                });
                self.shared.owes_retained.store(true, Ordering::Relaxed);
            } else if let Some(owed) = session
                .retained
                .iter_mut()
                .find(|d| d.filter == sub.topic_filter)
            {
                owed.subscriber = subscriber;
            }
            ack.payload.push(SubAckPayload {
                reason_code: match granted_qos {
                    QoS::AtLeastOnce => ReasonCode::GrantedQoS1,
                    QoS::ExactlyOnce => ReasonCode::GrantedQoS2,
                    _ => ReasonCode::GrantedQoS0,
                },
            });

            session.topics = Some(broker.topics());
            session
                .subscription_changes
                .push(Write::AddSubscription(subscription.clone()));
            session
                .subscriptions
                .insert(sub.topic_filter.clone(), subscription);
        }

        Ok(Some(ControlPacket::SubAck(ack)))
    }

    async fn handle_unsubscribe<S: RetainedMessageStore>(
        &mut self,
        packet: UnsubscribePacket,
        broker: &Broker<S>,
        chain: &Chain,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let mut ack = UnsubAckPacket {
            packet_id: packet.packet_id,
            properties: None,
            payload: Vec::with_capacity(packet.payload.len()),
        };

        for unsub in &packet.payload {
            if validate_subscribe_filter(&unsub.topic_filter).is_err() {
                ack.payload.push(UnsubAckPayload {
                    reason_code: ReasonCode::TopicFilterInvalid,
                });
                continue;
            }

            broker.unsubscribe(&unsub.topic_filter, self.shared.subscriber_id);
            session.retained.retain(|d| d.filter != unsub.topic_filter);
            let reason_code = if session.subscriptions.remove(&unsub.topic_filter).is_some() {
                session
                    .subscription_changes
                    .push(Write::RemoveSubscription(unsub.topic_filter.clone()));
                ReasonCode::Success
            } else {
                ReasonCode::NoSubscriptionExisted
            };
            if reason_code == ReasonCode::Success && !chain.is_empty() {
                chain.emit(Event::Unsubscribed {
                    client: session.client_info(),
                    filter: unsub.topic_filter.clone(),
                });
            }

            ack.payload.push(UnsubAckPayload { reason_code });
        }

        Ok(Some(ControlPacket::UnsubAck(ack)))
    }

    async fn handle_auth(
        &mut self,
        packet: AuthPacket,
        chain: &Chain,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;

        // Every AUTH must name the method the connection was authenticated
        // with [MQTT-4.12.1-1]
        let connect_method = session.client.authentication_method.clone();
        let method_name = packet.properties.auth_method.as_ref().map(|m| &m.value);
        let Some(method) = connect_method.filter(|m| Some(m) == method_name) else {
            return Err(ReasonCode::ProtocolError.into());
        };
        let data = packet.properties.auth_data.map(|d| d.value);

        let result = match packet.reason {
            ReasonCode::ReAuthenticate => {
                let client = session.client_info();
                let Some(mut exchange) = chain.begin_auth(&client, &method) else {
                    return Err(ReasonCode::BadAuthenticationMethod.into());
                };
                let result = chain.step(&mut exchange, data.as_ref()).await;
                session.auth_exchange = Some(exchange);
                result
            }
            ReasonCode::ContinueAuthentication => {
                // Only valid while a re-authentication is in progress
                let Some(exchange) = session.auth_exchange.as_mut() else {
                    return Err(ReasonCode::ProtocolError.into());
                };
                chain.step(exchange, data.as_ref()).await
            }
            _ => return Err(ReasonCode::ProtocolError.into()),
        };

        match result {
            AuthResult::Success { username, data } => {
                session.auth_exchange = None;
                session.verified_user = username;
                // An AUTH with properties names its method [MQTT-3.15.2-1]
                Ok(Some(ControlPacket::Auth(AuthPacket {
                    reason: ReasonCode::Success,
                    properties: AuthProperties {
                        auth_method: Some(AuthenticationMethod::new(method)),
                        auth_data: data.map(AuthenticationData::new),
                        ..Default::default()
                    },
                })))
            }
            AuthResult::Continue(challenge) => Ok(Some(ControlPacket::Auth(AuthPacket {
                reason: ReasonCode::ContinueAuthentication,
                properties: AuthProperties {
                    auth_method: Some(AuthenticationMethod::new(method)),
                    auth_data: Some(AuthenticationData::new(challenge)),
                    ..Default::default()
                },
            }))),
            AuthResult::Failed {
                reason,
                reason_string,
            } => {
                session.auth_exchange = None;
                session.auth_failure =
                    reason_string.filter(|_| session.client.request_problem_information);
                Err(reason.into())
            }
        }
    }

    pub(crate) async fn process_incoming<S: RetainedMessageStore>(
        &mut self,
        packet: ControlPacket,
        broker: &Broker<S>,
        chain: &Chain,
    ) -> Result<Option<ControlPacket>> {
        // Packet identifiers are never 0 [MQTT-2.2.1-3]
        let packet_id = match &packet {
            ControlPacket::Publish(p) => p.packet_id,
            ControlPacket::PubAck(p) => Some(p.packet_id),
            ControlPacket::PubRec(p) => Some(p.packet_id),
            ControlPacket::PubRel(p) => Some(p.packet_id),
            ControlPacket::PubComp(p) => Some(p.packet_id),
            ControlPacket::Subscribe(p) => Some(p.packet_id),
            ControlPacket::Unsubscribe(p) => Some(p.packet_id),
            _ => None,
        };
        if packet_id == Some(0) {
            return Err(ReasonCode::ProtocolError.into());
        }

        match packet {
            ControlPacket::Publish(packet) => self.handle_publish(packet, broker, chain).await,
            ControlPacket::PubAck(packet) => self.handle_puback(packet).await,
            ControlPacket::PubRec(packet) => self.handle_pubrec(packet).await,
            ControlPacket::PubRel(packet) => self.handle_pubrel(packet).await,
            ControlPacket::PubComp(packet) => self.handle_pubcomp(packet).await,
            ControlPacket::Subscribe(packet) => self.handle_subscribe(packet, broker, chain).await,
            ControlPacket::Unsubscribe(packet) => {
                self.handle_unsubscribe(packet, broker, chain).await
            }
            ControlPacket::PingReq(_) => Ok(Some(ControlPacket::PingResp(PingRespPacket {}))),
            ControlPacket::Disconnect(packet) => Ok(Some(ControlPacket::Disconnect(packet))),
            ControlPacket::Auth(packet) => self.handle_auth(packet, chain).await,

            // Some packets are not supposed to be received by the server.
            // Namely: ConnAck, UnsubAck, PingResp
            // The Connect packet is handled before the session is created.
            _ => Err(ReasonCode::ProtocolError.into()),
        }
    }

    fn subscriber(&self, subscription: &StoredSubscription) -> Subscriber {
        Subscriber {
            queue: Arc::clone(&self.shared.queue),
            session_id: self.shared.subscriber_id,
            granted_qos: subscription.qos,
            no_local: subscription.no_local,
            retain_as_published: subscription.retain_as_published,
            subscription_identifier: subscription.subscription_identifier,
            share: parse_shared_subscription(&subscription.topic_filter)
                .is_some()
                .then(|| Arc::from(subscription.topic_filter.as_str())),
        }
    }

    /// Removes every subscription of this session from the broker. Called
    /// when the session ends, so nothing is queued for it anymore.
    pub(crate) async fn unsubscribe_all(&self) {
        let mut session = self.shared.state.lock().await;
        session.unsubscribe_all(self.shared.subscriber_id);
    }
}

impl State {
    /// Who the client of this session is, for plugins.
    fn client_info(&self) -> ClientInfo {
        let mut client = ClientInfo::new(self.client_id.clone(), self.verified_user.clone());
        client.peer = self.peer;
        client.connection = self.connection_info.clone();
        client
    }

    /// Removes every subscription of the session `subscriber_id` from the
    /// topic tree.
    fn unsubscribe_all(&mut self, subscriber_id: u64) {
        if let Some(topics) = self.topics.take() {
            for topic_filter in self.subscriptions.keys() {
                topics.unsubscribe(topic_filter, subscriber_id);
            }
        }
        self.subscriptions.clear();
        self.retained.clear();
    }

    /// The largest packet the client accepts, if it set a limit.
    fn client_maximum_packet_size(&self) -> Option<usize> {
        self.client.maximum_packet_size.map(|m| m as usize)
    }

    /// Reason for acknowledging a packet id with no exchange in progress.
    /// MQTT 3.x acknowledgements carry no reason, so it's always success.
    fn unknown_packet_id_reason(&self) -> ReasonCode {
        if self.client.protocol_version.supports_reason_codes() {
            ReasonCode::PacketIdentifierNotFound
        } else {
            ReasonCode::Success
        }
    }
}

#[cfg(test)]
mod tests;
