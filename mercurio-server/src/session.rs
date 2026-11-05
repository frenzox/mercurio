use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use bytes::{Bytes, BytesMut};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc::UnboundedSender, Mutex},
    time::Duration,
};
use tracing::{info, warn};

use mercurio_core::{
    codec::{Encoder, VariableByteInteger},
    message::{unix_millis, Message, MessageProperties},
    properties::{
        AssignedClientIdentifier, AuthenticationData, AuthenticationMethod, MaximumPacketSize,
        MessageExpiryInterval, ReceiveMaximum, ResponseInformation, RetainAvailable,
        ServerKeepAlive, SharedSubscriptionAvailable, SubscriptionIdentifier,
        SubscriptionIdentifierAvailable, TopicAlias, TopicAliasMaximum,
        WildcardSubscriptionAvailable,
    },
    qos::QoS,
    reason::ReasonCode,
    topic::{parse_shared_subscription, validate_publish_topic, validate_subscribe_filter},
    Result,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connack::{ConnAckPacket, ConnAckProperties},
    connect::ConnectPacket,
    pingresp::PingRespPacket,
    puback::PubAckPacket,
    pubcomp::PubCompPacket,
    publish::{PublishPacket, PublishProperties},
    pubrec::PubRecPacket,
    pubrel::PubRelPacket,
    suback::{SubAckPacket, SubAckPayload},
    subscribe::RetainHandling,
    unsuback::{UnsubAckPacket, UnsubAckPayload},
    unsubscribe::UnsubscribePacket,
    ControlPacket,
};

use crate::{
    acl::{Access, Authorizer, ClientIdentity},
    auth::{AuthExchange, AuthManager, AuthResult},
    broker::Broker,
    connection::Connection,
    queue::{JournalEvent, MessageQueue, QueueLimits, Subscriber},
    server::{ConnectPolicy, Limits},
    topic_tree::TopicTree,
};
use mercurio_storage::{
    InflightMessage, RetainedMessageStore, StoredSubscription, StoredWillMessage,
};

/// Will message to be published on abnormal client disconnect.
#[derive(Debug, Clone)]
pub struct WillMessage {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
    /// How long to wait after the connection closes before publishing
    pub delay: Duration,
    pub properties: MessageProperties,
}

impl WillMessage {
    /// Extract will message from a ConnectPacket if will_flag is set.
    fn from_connect_packet(packet: &ConnectPacket) -> Option<Self> {
        if !packet.flags.will_flag {
            return None;
        }

        let topic = packet.payload.will_topic.as_ref()?;
        let payload = packet.payload.will_payload.clone().unwrap_or_default();
        let will_properties = packet.payload.will_properties.as_ref();
        let delay = will_properties
            .and_then(|p| p.will_delay_interval.as_ref())
            .map_or(0, |d| d.value);
        let properties = will_properties
            .map(|p| MessageProperties {
                payload_format_indicator: p.payload_format_indicator.clone(),
                message_expiry_interval: p.message_expiry_interval.clone(),
                content_type: p.content_type.clone(),
                response_topic: p.response_topic.clone(),
                correlation_data: p.correlation_data.clone(),
                user_property: p.user_property.clone(),
            })
            .unwrap_or_default();

        Some(WillMessage {
            topic: topic.clone(),
            payload,
            qos: packet.flags.will_qos,
            retain: packet.flags.will_retain,
            delay: Duration::from_secs(delay.into()),
            properties,
        })
    }
}

impl From<WillMessage> for StoredWillMessage {
    fn from(will: WillMessage) -> Self {
        StoredWillMessage {
            topic: will.topic,
            payload: will.payload,
            qos: will.qos,
            retain: will.retain,
            delay_secs: will.delay.as_secs().try_into().unwrap_or(u32::MAX),
            properties: will.properties,
        }
    }
}

impl From<StoredWillMessage> for WillMessage {
    fn from(stored: StoredWillMessage) -> Self {
        WillMessage {
            topic: stored.topic,
            payload: stored.payload,
            qos: stored.qos,
            retain: stored.retain,
            delay: Duration::from_secs(stored.delay_secs.into()),
            properties: stored.properties,
        }
    }
}

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

/// The PUBLISH properties forwarding a message's application properties,
/// along with the identifier of the subscription it was delivered through.
fn publish_properties(
    properties: &MessageProperties,
    subscription_identifier: Option<u32>,
) -> Option<PublishProperties> {
    Some(PublishProperties {
        subscription_identifier: subscription_identifier
            .map(|id| SubscriptionIdentifier::new(VariableByteInteger(id))),
        payload_format_indicator: properties.payload_format_indicator.clone(),
        message_expiry_interval: properties.message_expiry_interval.clone(),
        content_type: properties.content_type.clone(),
        response_topic: properties.response_topic.clone(),
        correlation_data: properties.correlation_data.clone(),
        user_property: properties.user_property.clone(),
        ..Default::default()
    })
}

/// The keep-alive the connection runs with: the client's own, unless it
/// exceeds the server's maximum or disables keep-alive, in which case MQTT 5
/// clients are told to use the maximum. MQTT 3.x has no way to tell them.
pub(crate) fn effective_keepalive(connect_packet: &ConnectPacket, limits: &Limits) -> u16 {
    let requested = connect_packet.keepalive;
    match limits.max_keepalive {
        Some(max)
            if connect_packet.protocol_version.supports_properties()
                && (requested == 0 || requested > max) =>
        {
            max
        }
        _ => requested,
    }
}

/// Source of unique subscriber ids, one per session.
static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);

pub struct SessionDropGuard {
    session: Session,
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
}

struct State {
    pub connect_packet: ConnectPacket,
    /// Generation of the latest connection to use this session
    generation: u64,
    /// Cached client ID to avoid lock acquisition for logging
    client_id: String,
    /// Topic filters subscribed to, with their granted QoS
    subscriptions: HashMap<String, StoredSubscription>,
    /// Where the subscriptions are registered, once there are any
    topics: Option<TopicTree<Subscriber>>,
    /// Outgoing QoS 1/2 messages awaiting PUBACK/PUBREC, by packet_id, with
    /// the sequence they were sent in
    unacknowledged_messages: HashMap<u16, (u64, PublishPacket)>,
    /// Outgoing QoS 2 messages awaiting PUBCOMP, by packet_id, with the
    /// sequence they were sent in
    pubrecs: HashMap<u16, u64>,
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
    /// QoS 2 messages the client may have awaiting PUBREL, as advertised
    receive_maximum: u16,
    /// Will message to be published on abnormal disconnect
    will_message: Option<WillMessage>,
    /// Re-authentication in progress, if any
    auth_exchange: Option<Box<dyn AuthExchange>>,
    /// User name verified by authentication, the one access control trusts
    verified_user: Option<String>,
}

impl From<Session> for SessionDropGuard {
    fn from(session: Session) -> Self {
        SessionDropGuard { session }
    }
}

impl SessionDropGuard {
    pub fn new(connect_packet: ConnectPacket, queue_limits: QueueLimits) -> Self {
        SessionDropGuard {
            session: Session::with_queue_limits(connect_packet, queue_limits),
        }
    }

    pub(crate) fn session(&self) -> Session {
        self.session.clone()
    }
}

impl Session {
    #[cfg(test)]
    pub fn new(connect_packet: ConnectPacket) -> Self {
        Self::with_queue_limits(connect_packet, QueueLimits::default())
    }

    pub fn with_queue_limits(connect_packet: ConnectPacket, queue_limits: QueueLimits) -> Self {
        let client_id = connect_packet.payload.client_id.clone();
        let will_message = WillMessage::from_connect_packet(&connect_packet);
        Session {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    connect_packet,
                    generation: 0,
                    client_id,
                    subscriptions: HashMap::new(),
                    topics: None,
                    // Pre-allocate with reasonable capacity for typical usage
                    unacknowledged_messages: HashMap::with_capacity(16),
                    pubrecs: HashMap::with_capacity(16),
                    next_sequence: 0,
                    incoming_qos2: HashSet::new(),
                    last_packet_id: 0,
                    topic_aliases: HashMap::new(),
                    topic_alias_maximum: 0,
                    outgoing_aliases: HashMap::new(),
                    send_topic_aliases: false,
                    receive_maximum: u16::MAX,
                    will_message,
                    auth_exchange: None,
                    verified_user: None,
                }),
                queue: Arc::new(MessageQueue::new(queue_limits)),
                subscriber_id: NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed),
            }),
        }
    }

    pub(crate) async fn set_connect_packet(&mut self, connect_packet: ConnectPacket) {
        let mut session = self.shared.state.lock().await;
        session.client_id = connect_packet.payload.client_id.clone();
        session.will_message = WillMessage::from_connect_packet(&connect_packet);
        session.connect_packet = connect_packet;
    }

    /// Clear the will message. Called on normal DISCONNECT.
    pub(crate) async fn clear_will(&self) {
        let mut session = self.shared.state.lock().await;
        session.will_message = None;
    }

    /// Take the will message for publishing. Called on abnormal disconnect.
    /// Returns None if no will was set or it was already cleared.
    pub(crate) async fn take_will(&self) -> Option<WillMessage> {
        let mut session = self.shared.state.lock().await;
        session.will_message.take()
    }

    pub(crate) async fn generation(&self) -> u64 {
        self.shared.state.lock().await.generation
    }

    pub(crate) async fn set_generation(&self, generation: u64) {
        self.shared.state.lock().await.generation = generation;
    }

    /// Get a clone of the will message without removing it.
    pub(crate) async fn get_will(&self) -> Option<WillMessage> {
        let session = self.shared.state.lock().await;
        session.will_message.clone()
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
            // Retained messages are only sent for a new SUBSCRIBE, not when an
            // existing subscription is resumed [MQTT-3.3.1-9]
            broker
                .subscribe(
                    &subscription.topic_filter,
                    self.shared.subscriber_id,
                    self.subscriber(&subscription),
                )
                .await?;
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
            session.topic_alias_maximum = limits.topic_alias_maximum;
            session.receive_maximum = limits.receive_maximum;

            // Set protocol version from the CONNECT packet
            ack.protocol_version = session.connect_packet.protocol_version;

            // CONNACK properties only exist in MQTT 5.0
            if session
                .connect_packet
                .protocol_version
                .supports_properties()
            {
                ack.properties = Some(ConnAckProperties {
                    assigned_client_id: assigned_client_id
                        .then(|| AssignedClientIdentifier::new(session.client_id.clone())),
                    maximum_packet_size: Some(MaximumPacketSize::new(
                        limits.max_packet_size.try_into().unwrap_or(u32::MAX),
                    )),
                    topic_alias_max: Some(TopicAliasMaximum::new(limits.topic_alias_maximum)),
                    // Echoes the method a successful enhanced authentication used
                    authentication_method: session
                        .connect_packet
                        .properties
                        .as_ref()
                        .and_then(|p| p.authentication_method.clone()),
                    receive_maximum: Some(ReceiveMaximum::new(limits.receive_maximum)),
                    response_information: policy
                        .response_information
                        .as_ref()
                        .filter(|_| {
                            session
                                .connect_packet
                                .properties
                                .as_ref()
                                .and_then(|p| p.request_response_information.as_ref())
                                .is_some_and(|r| r.value == 1)
                        })
                        .map(|template| {
                            ResponseInformation::new(template.replace("%c", &session.client_id))
                        }),
                    server_keepalive: {
                        let keepalive = effective_keepalive(&session.connect_packet, limits);
                        (keepalive != session.connect_packet.keepalive)
                            .then(|| ServerKeepAlive::new(keepalive))
                    },
                    // Spelled out even where absence means the same, for
                    // clients and tools that inspect them
                    retain_available: Some(RetainAvailable::new(true)),
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
            "Client with id `{}` {} a session",
            client_id,
            if resume { "resumed" } else { "started" }
        );

        connection.write_packet(ControlPacket::ConnAck(ack)).await?;

        Ok(())
    }

    async fn handle_publish<S: RetainedMessageStore>(
        &mut self,
        mut packet: PublishPacket,
        broker: &Broker<S>,
        authorizer: Option<&dyn Authorizer>,
    ) -> Result<Option<ControlPacket>> {
        let topic_alias = packet
            .properties
            .as_ref()
            .and_then(|p| p.topic_alias.as_ref())
            .map(|alias| alias.value);
        if let Some(alias) = topic_alias {
            let mut session = self.shared.state.lock().await;
            if alias == 0 || alias > session.topic_alias_maximum {
                return Err(ReasonCode::TopicAliasInvalid.into());
            }
            // An empty topic name refers to the alias, a non-empty one sets it
            if packet.topic_name.is_empty() {
                match session.topic_aliases.get(&alias) {
                    Some(topic) => packet.topic_name = topic.clone(),
                    None => return Err(ReasonCode::ProtocolError.into()),
                }
            } else {
                session
                    .topic_aliases
                    .insert(alias, packet.topic_name.clone());
            }
        }

        // Validate topic name (no wildcards, no null chars, etc.)
        if validate_publish_topic(&packet.topic_name).is_err() {
            return Err(ReasonCode::TopicNameInvalid.into());
        }

        // Only the server sets subscription identifiers [MQTT-3.3.4-6]
        if packet
            .properties
            .as_ref()
            .is_some_and(|p| p.subscription_identifier.is_some())
        {
            return Err(ReasonCode::ProtocolError.into());
        }

        if let Some(authorizer) = authorizer {
            let identity = self.identity().await;
            if !authorizer.authorize(&identity, &packet.topic_name, Access::Write) {
                warn!(
                    "Client `{}` may not publish to `{}`",
                    identity.client_id, packet.topic_name
                );
                return self.refuse_publish(&packet).await;
            }
        }

        let response = match packet.qos_level {
            QoS::AtMostOnce => Ok(None),
            QoS::AtLeastOnce => {
                if let Some(packet_id) = packet.packet_id {
                    Ok(Some(ControlPacket::PubAck(PubAckPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    })))
                } else {
                    Err(ReasonCode::ProtocolError.into())
                }
            }
            QoS::ExactlyOnce => {
                if let Some(packet_id) = packet.packet_id {
                    let first_delivery = {
                        let mut session = self.shared.state.lock().await;
                        // The client may only have as many QoS 2 messages
                        // awaiting PUBREL as advertised [MQTT-3.3.4-8]
                        if !session.incoming_qos2.contains(&packet_id)
                            && session.incoming_qos2.len() >= usize::from(session.receive_maximum)
                        {
                            return Err(ReasonCode::ReceiveMaximumExceeded.into());
                        }
                        session.incoming_qos2.insert(packet_id)
                    };

                    let pubrec = ControlPacket::PubRec(PubRecPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    });

                    // A retransmission before PUBREL was already delivered
                    // once; forwarding it again would break exactly-once.
                    if !first_delivery {
                        return Ok(Some(pubrec));
                    }

                    Ok(Some(pubrec))
                } else {
                    Err(ReasonCode::ProtocolError.into())
                }
            }
            QoS::Invalid => Err(ReasonCode::ProtocolError.into()),
        };

        // Publish to broker - convert topic to Arc<str> for cheap cloning to subscribers
        let topic: Arc<str> = packet.topic_name.into();
        let properties = message_properties(packet.properties.as_ref());
        let message = Message {
            packet_id: packet.packet_id,
            topic: Arc::clone(&topic),
            dup: packet.dup,
            retain: packet.retain,
            qos: packet.qos_level,
            payload: packet.payload,
            expires_at: Message::expiry_deadline(&properties),
            properties,
            subscription_identifier: None,
        };

        broker
            .publish_from(&topic, message, Some(self.shared.subscriber_id))
            .await?;

        response
    }

    /// Acknowledges a PUBLISH the client was not allowed to make without
    /// forwarding it. MQTT 3.x has no way to report the refusal, so those
    /// clients get a normal acknowledgement.
    async fn refuse_publish(&self, packet: &PublishPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let reports_reasons = session
            .connect_packet
            .protocol_version
            .supports_reason_codes();
        let reason = if reports_reasons {
            ReasonCode::NotAuthorized
        } else {
            ReasonCode::Success
        };

        match (packet.qos_level, packet.packet_id) {
            (QoS::AtMostOnce, _) => Ok(None),
            (QoS::AtLeastOnce, Some(packet_id)) => Ok(Some(ControlPacket::PubAck(PubAckPacket {
                packet_id,
                reason,
                properties: None,
            }))),
            (QoS::ExactlyOnce, Some(packet_id)) => {
                // A successful PUBREC still expects the PUBREL exchange
                if !reports_reasons {
                    session.incoming_qos2.insert(packet_id);
                }
                Ok(Some(ControlPacket::PubRec(PubRecPacket {
                    packet_id,
                    reason,
                    properties: None,
                })))
            }
            _ => Err(ReasonCode::ProtocolError.into()),
        }
    }

    /// Who the client of this session is, for access control.
    pub(crate) async fn identity(&self) -> ClientIdentity {
        let session = self.shared.state.lock().await;
        ClientIdentity {
            client_id: session.client_id.clone(),
            username: session.verified_user.clone(),
        }
    }

    /// Starts keeping the queue on disk while the client is away.
    pub(crate) fn start_queue_journal(&self, journal: UnboundedSender<JournalEvent>) {
        self.shared.queue.start_journal(journal);
    }

    pub(crate) fn stop_queue_journal(&self) {
        self.shared.queue.stop_journal();
    }

    /// Puts back messages queued for the client before a restart.
    pub(crate) fn restore_queue(&self, messages: Vec<(u64, Message)>) {
        self.shared.queue.restore(messages);
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

    async fn handle_puback(&mut self, packet: PubAckPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        // O(1) removal instead of O(n) search + remove
        session.unacknowledged_messages.remove(&packet.packet_id);
        Ok(None)
    }

    async fn handle_pubrec(&mut self, packet: PubRecPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let packet_id = packet.packet_id;
        let sequence = match session.unacknowledged_messages.remove(&packet_id) {
            Some((sequence, _)) => Some(sequence),
            None => session.pubrecs.get(&packet_id).copied(),
        };

        // A failure reason ends the exchange: the client won't take the message
        if packet.reason.get_code() >= 0x80 {
            session.pubrecs.remove(&packet_id);
            return Ok(None);
        }

        let reason = if let Some(sequence) = sequence {
            session.pubrecs.insert(packet_id, sequence);
            ReasonCode::Success
        } else {
            session.unknown_packet_id_reason()
        };

        Ok(Some(ControlPacket::PubRel(PubRelPacket {
            packet_id,
            reason,
            properties: None,
        })))
    }

    async fn handle_pubcomp(&mut self, packet: PubCompPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        // O(1) removal
        session.pubrecs.remove(&packet.packet_id);
        Ok(None)
    }

    async fn handle_pubrel(&mut self, packet: PubRelPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let reason = if session.incoming_qos2.remove(&packet.packet_id) {
            ReasonCode::Success
        } else {
            session.unknown_packet_id_reason()
        };

        Ok(Some(ControlPacket::PubComp(PubCompPacket {
            packet_id: packet.packet_id,
            reason,
            properties: None,
        })))
    }

    async fn handle_subscribe<S: RetainedMessageStore>(
        &mut self,
        packet: mercurio_packets::subscribe::SubscribePacket,
        broker: &Broker<S>,
        authorizer: Option<&dyn Authorizer>,
    ) -> Result<Option<ControlPacket>> {
        let subscription_identifier = packet
            .properties
            .as_ref()
            .and_then(|p| p.subscription_id.as_ref())
            .map(|id| id.value.0);
        // An identifier of 0 is a protocol error
        if subscription_identifier == Some(0) {
            return Err(ReasonCode::ProtocolError.into());
        }

        // A client can't opt out of its own messages on a shared subscription
        // [MQTT-3.8.3-4]
        if packet.payload.iter().any(|sub| {
            sub.subs_opt.no_local && parse_shared_subscription(&sub.topic_filter).is_some()
        }) {
            return Err(ReasonCode::ProtocolError.into());
        }

        let mut session = self.shared.state.lock().await;
        // Pre-allocate payload vec based on subscription count
        let mut ack = SubAckPacket {
            protocol_version: session.connect_packet.protocol_version,
            packet_id: packet.packet_id,
            properties: None,
            payload: Vec::with_capacity(packet.payload.len()),
        };

        for sub in &packet.payload {
            // Validate topic filter before subscribing
            if validate_subscribe_filter(&sub.topic_filter).is_err() {
                ack.payload.push(SubAckPayload {
                    reason_code: ReasonCode::TopicFilterInvalid,
                });
                continue;
            }

            if let Some(authorizer) = authorizer {
                let identity = ClientIdentity {
                    client_id: session.client_id.clone(),
                    username: session.verified_user.clone(),
                };
                if !authorizer.authorize(&identity, &sub.topic_filter, Access::Read) {
                    warn!(
                        "Client `{}` may not subscribe to `{}`",
                        identity.client_id, sub.topic_filter
                    );
                    ack.payload.push(SubAckPayload {
                        reason_code: ReasonCode::NotAuthorized,
                    });
                    continue;
                }
            }

            let granted_qos = sub.subs_opt.qos;
            let subscription = StoredSubscription {
                topic_filter: sub.topic_filter.clone(),
                qos: granted_qos,
                no_local: sub.subs_opt.no_local,
                retain_as_published: sub.subs_opt.retain_as_pub,
                subscription_identifier,
            };
            let subscriber = self.subscriber(&subscription);
            let existed = session.subscriptions.contains_key(&sub.topic_filter);
            let retained_messages = broker
                .subscribe(
                    &sub.topic_filter,
                    self.shared.subscriber_id,
                    subscriber.clone(),
                )
                .await?;

            // Shared subscriptions are never sent retained messages
            let shared = parse_shared_subscription(&sub.topic_filter).is_some();
            let send_retained = !shared
                && match sub.subs_opt.retain_handling {
                    RetainHandling::SendRetained => true,
                    RetainHandling::SendRetainedIfNonExisting => !existed,
                    _ => false,
                };
            if send_retained {
                for message in retained_messages {
                    subscriber.deliver_retained(message);
                }
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
                .subscriptions
                .insert(sub.topic_filter.clone(), subscription);
        }

        Ok(Some(ControlPacket::SubAck(ack)))
    }

    async fn handle_unsubscribe<S: RetainedMessageStore>(
        &mut self,
        packet: UnsubscribePacket,
        broker: &Broker<S>,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let mut ack = UnsubAckPacket {
            protocol_version: session.connect_packet.protocol_version,
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
            let reason_code = if session.subscriptions.remove(&unsub.topic_filter).is_some() {
                ReasonCode::Success
            } else {
                ReasonCode::NoSubscriptionExisted
            };

            ack.payload.push(UnsubAckPayload { reason_code });
        }

        Ok(Some(ControlPacket::UnsubAck(ack)))
    }

    async fn handle_auth(
        &mut self,
        packet: AuthPacket,
        auth_manager: Option<&AuthManager>,
    ) -> Result<Option<ControlPacket>> {
        let Some(auth_manager) = auth_manager else {
            // No auth manager configured - enhanced auth not supported
            return Err(ReasonCode::BadAuthenticationMethod.into());
        };

        let mut session = self.shared.state.lock().await;

        // Every AUTH must name the method the connection was authenticated
        // with [MQTT-4.12.1-1]
        let connect_method = session
            .connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.authentication_method.as_ref())
            .map(|m| m.value.clone());
        let method_name = packet.properties.auth_method.as_ref().map(|m| &m.value);
        let Some(method) = connect_method.filter(|m| Some(m) == method_name) else {
            return Err(ReasonCode::ProtocolError.into());
        };
        let data = packet.properties.auth_data.map(|d| d.value);

        let result = match packet.reason {
            ReasonCode::ReAuthenticate => {
                let Some(mut exchange) = auth_manager.begin(&method) else {
                    return Err(ReasonCode::BadAuthenticationMethod.into());
                };
                let result = exchange.step(data.as_ref());
                session.auth_exchange = Some(exchange);
                result
            }
            ReasonCode::ContinueAuthentication => {
                // Only valid while a re-authentication is in progress
                let Some(exchange) = session.auth_exchange.as_mut() else {
                    return Err(ReasonCode::ProtocolError.into());
                };
                exchange.step(data.as_ref())
            }
            _ => return Err(ReasonCode::ProtocolError.into()),
        };

        match result {
            AuthResult::Success { username } => {
                session.auth_exchange = None;
                session.verified_user = username;
                Ok(Some(ControlPacket::Auth(AuthPacket {
                    reason: ReasonCode::Success,
                    properties: AuthProperties::default(),
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
            AuthResult::Failed(reason) => {
                session.auth_exchange = None;
                Err(reason.into())
            }
        }
    }

    pub(crate) async fn process_incoming<S: RetainedMessageStore>(
        &mut self,
        packet: ControlPacket,
        broker: &Broker<S>,
        auth_manager: Option<&AuthManager>,
        authorizer: Option<&dyn Authorizer>,
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
            ControlPacket::Publish(packet) => self.handle_publish(packet, broker, authorizer).await,
            ControlPacket::PubAck(packet) => self.handle_puback(packet).await,
            ControlPacket::PubRec(packet) => self.handle_pubrec(packet).await,
            ControlPacket::PubRel(packet) => self.handle_pubrel(packet).await,
            ControlPacket::PubComp(packet) => self.handle_pubcomp(packet).await,
            ControlPacket::Subscribe(packet) => {
                self.handle_subscribe(packet, broker, authorizer).await
            }
            ControlPacket::Unsubscribe(packet) => self.handle_unsubscribe(packet, broker).await,
            ControlPacket::PingReq(_) => Ok(Some(ControlPacket::PingResp(PingRespPacket {}))),
            ControlPacket::Disconnect(packet) => Ok(Some(ControlPacket::Disconnect(packet))),
            ControlPacket::Auth(packet) => self.handle_auth(packet, auth_manager).await,

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
        }
    }

    /// Removes every subscription of this session from the broker. Called
    /// when the session ends, so nothing is queued for it anymore.
    pub(crate) async fn unsubscribe_all(&self) {
        let mut session = self.shared.state.lock().await;
        if let Some(topics) = session.topics.take() {
            for topic_filter in session.subscriptions.keys() {
                topics.unsubscribe(topic_filter, self.shared.subscriber_id);
            }
        }
        session.subscriptions.clear();
    }

    /// Whether another QoS 1 or 2 message may be sent without exceeding the
    /// client's Receive Maximum [MQTT-3.3.4-9].
    pub(crate) async fn can_send_more(&self) -> bool {
        let session = self.shared.state.lock().await;
        let receive_maximum = session
            .connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.receive_maximum.as_ref())
            .map_or(u16::MAX, |r| r.value);
        session.unacknowledged_messages.len() + session.pubrecs.len() < usize::from(receive_maximum)
    }

    /// Waits for the next message to send to the client. Cancel safe, and
    /// doesn't hold the session state while waiting.
    pub(crate) async fn next_outgoing(&self, window_open: bool) -> Message {
        self.shared.queue.pop(!window_open).await
    }

    /// Turns a queued message into the PUBLISH to send, tracking it until
    /// acknowledged. Returns None if the message had to be dropped.
    pub(crate) async fn prepare_publish(&mut self, mut message: Message) -> Option<Outgoing> {
        // Expired messages are never delivered, and the rest carry only the
        // time they have left [MQTT-3.3.2-5, MQTT-3.3.2-6]
        if let Some(deadline) = message.expires_at {
            let now = unix_millis();
            if now >= deadline {
                return None;
            }
            let remaining_secs = (deadline - now).div_ceil(1000);
            message.properties.message_expiry_interval = Some(MessageExpiryInterval::new(
                remaining_secs.try_into().unwrap_or(u32::MAX),
            ));
        }

        let mut session = self.shared.state.lock().await;

        {
            let packet_id = match message.qos {
                QoS::AtMostOnce => None,
                _ => match session.next_packet_id() {
                    Some(id) => Some(id),
                    None => {
                        // Only reachable when the client's Receive Maximum allows
                        // every identifier to be in flight at once
                        warn!(
                            "No free packet identifier for client `{}`, dropping message",
                            session.client_id
                        );
                        return None;
                    }
                },
            };

            let publish = PublishPacket {
                dup: false,
                qos_level: message.qos,
                retain: message.retain,
                topic_name: message.topic.to_string(),
                packet_id,
                properties: publish_properties(
                    &message.properties,
                    message.subscription_identifier,
                ),
                payload: message.payload,
            };

            // Packets the client can't take are discarded as if delivered
            // [MQTT-3.1.2-25]
            if let Some(maximum) = session.client_maximum_packet_size() {
                let mut encoded = BytesMut::new();
                publish.encode(&mut encoded);
                if encoded.len() > maximum {
                    warn!(
                        "Message on `{}` exceeds the maximum packet size of client `{}`, dropping it",
                        publish.topic_name, session.client_id
                    );
                    return None;
                }
            }

            let inflight = packet_id.map(|packet_id| {
                let sequence = session.next_sequence;
                session.next_sequence += 1;
                session
                    .unacknowledged_messages
                    .insert(packet_id, (sequence, publish.clone()));
                InflightMessage {
                    packet_id,
                    topic: publish.topic_name.clone(),
                    payload: publish.payload.clone(),
                    qos: publish.qos_level,
                    dup: false,
                    sequence,
                    released: false,
                    properties: message_properties(publish.properties.as_ref()),
                    subscription_identifier: message.subscription_identifier,
                }
            });

            Some(Outgoing {
                packet: ControlPacket::Publish(session.apply_topic_alias(publish)),
                inflight,
            })
        }
    }

    /// Adds in-flight messages loaded from storage to a restored session.
    pub(crate) async fn restore_inflight(&self, inflight: Vec<InflightMessage>) {
        let mut session = self.shared.state.lock().await;

        for message in inflight {
            session.next_sequence = session.next_sequence.max(message.sequence + 1);
            if message.released {
                session.pubrecs.insert(message.packet_id, message.sequence);
            } else {
                let publish = PublishPacket {
                    dup: false,
                    qos_level: message.qos,
                    retain: false,
                    topic_name: message.topic,
                    packet_id: Some(message.packet_id),
                    properties: publish_properties(
                        &message.properties,
                        message.subscription_identifier,
                    ),
                    payload: message.payload,
                };
                session
                    .unacknowledged_messages
                    .insert(message.packet_id, (message.sequence, publish));
            }
        }
    }

    /// Packets to resend when a session resumes: unacknowledged PUBLISH
    /// packets flagged as duplicates and pending PUBRELs, in the order they
    /// were first sent [MQTT-4.4.0-1].
    pub(crate) async fn pending_retransmissions(&self) -> Vec<ControlPacket> {
        let session = self.shared.state.lock().await;

        let mut pending: Vec<(u64, ControlPacket)> = session
            .unacknowledged_messages
            .values()
            .map(|(sequence, publish)| {
                let mut publish = publish.clone();
                publish.dup = true;
                (*sequence, ControlPacket::Publish(publish))
            })
            .chain(session.pubrecs.iter().map(|(packet_id, sequence)| {
                let pubrel = PubRelPacket {
                    packet_id: *packet_id,
                    reason: ReasonCode::Success,
                    properties: None,
                };
                (*sequence, ControlPacket::PubRel(pubrel))
            }))
            .collect();

        pending.sort_by_key(|(sequence, _)| *sequence);
        pending.into_iter().map(|(_, packet)| packet).collect()
    }
}

/// A PUBLISH ready to send.
pub(crate) struct Outgoing {
    pub packet: ControlPacket,
    /// For QoS 1 and 2, the state to persist until the client acknowledges it
    pub inflight: Option<InflightMessage>,
}

impl State {
    /// Replaces the topic of an outgoing PUBLISH with an alias where the
    /// client accepts them, assigning new aliases until its maximum is
    /// reached. Only the copy sent is aliased: aliases end with the
    /// connection, so a retransmission must carry the full topic.
    fn apply_topic_alias(&mut self, mut publish: PublishPacket) -> PublishPacket {
        let maximum = self
            .connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.topic_alias_maximum.as_ref())
            .filter(|_| self.send_topic_aliases)
            .map_or(0, |m| m.value);

        let alias = match self.outgoing_aliases.get(&publish.topic_name) {
            // Already known to the client: the alias alone stands for it
            Some(&alias) => {
                publish.topic_name.clear();
                alias
            }
            None if self.outgoing_aliases.len() < usize::from(maximum) => {
                let alias = self.outgoing_aliases.len() as u16 + 1;
                self.outgoing_aliases
                    .insert(publish.topic_name.clone(), alias);
                alias
            }
            None => return publish,
        };

        publish
            .properties
            .get_or_insert_with(Default::default)
            .topic_alias = Some(TopicAlias::new(alias));
        publish
    }

    /// The largest packet the client accepts, if it set a limit.
    fn client_maximum_packet_size(&self) -> Option<usize> {
        self.connect_packet
            .properties
            .as_ref()
            .and_then(|p| p.maximum_packet_size.as_ref())
            .map(|m| m.value as usize)
    }

    /// Reason for acknowledging a packet id with no exchange in progress.
    /// MQTT 3.x acknowledgements carry no reason, so it's always success.
    fn unknown_packet_id_reason(&self) -> ReasonCode {
        if self.connect_packet.protocol_version.supports_reason_codes() {
            ReasonCode::PacketIdentifierNotFound
        } else {
            ReasonCode::Success
        }
    }

    /// Allocates the next packet identifier not currently in flight.
    /// Identifiers are per session, so they can't be copied from the publisher.
    fn next_packet_id(&mut self) -> Option<u16> {
        for _ in 0..u16::MAX {
            self.last_packet_id = self.last_packet_id.checked_add(1).unwrap_or(1);
            let id = self.last_packet_id;
            if !self.unacknowledged_messages.contains_key(&id) && !self.pubrecs.contains_key(&id) {
                return Some(id);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mercurio_core::protocol::ProtocolVersion;
    use mercurio_packets::{
        connect::{ConnectFlags, ConnectPayload},
        subscribe::{RetainHandling, SubscribePayload, SubscriptionOptions},
        unsubscribe::UnsubscribePayload,
    };
    use mercurio_storage::memory::MemoryStore;
    use tokio::time::{timeout, Duration};

    fn create_test_session() -> Session {
        let connect_packet = ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            flags: ConnectFlags::default(),
            keepalive: 60,
            properties: None,
            payload: ConnectPayload {
                client_id: "test-client".to_string(),
                will_properties: None,
                will_topic: None,
                will_payload: None,
                user_name: None,
                password: None,
            },
        };
        Session::new(connect_packet)
    }

    fn default_subscription_options() -> SubscriptionOptions {
        SubscriptionOptions::default()
    }

    fn create_connect_packet_with_will() -> ConnectPacket {
        ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            flags: ConnectFlags {
                will_flag: true,
                will_qos: QoS::AtLeastOnce,
                will_retain: true,
                clean_start: true,
                ..Default::default()
            },
            keepalive: 60,
            properties: None,
            payload: ConnectPayload {
                client_id: "test-client".to_string(),
                will_topic: Some("will/topic".to_string()),
                will_payload: Some(Bytes::from("goodbye")),
                ..Default::default()
            },
        }
    }

    fn create_connect_packet_without_will() -> ConnectPacket {
        ConnectPacket {
            protocol_version: ProtocolVersion::V5,
            flags: ConnectFlags {
                will_flag: false,
                clean_start: true,
                ..Default::default()
            },
            keepalive: 60,
            properties: None,
            payload: ConnectPayload {
                client_id: "test-client".to_string(),
                ..Default::default()
            },
        }
    }

    #[tokio::test]
    async fn test_unsubscribe_nonexistent_topic() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let packet = UnsubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![UnsubscribePayload {
                topic_filter: "nonexistent/topic".to_string(),
            }],
        };

        let result = session.handle_unsubscribe(packet, &broker).await.unwrap();

        match result {
            Some(ControlPacket::UnsubAck(ack)) => {
                assert_eq!(ack.packet_id, 1);
                assert_eq!(ack.payload.len(), 1);
                assert_eq!(
                    ack.payload[0].reason_code,
                    ReasonCode::NoSubscriptionExisted
                );
            }
            _ => panic!("Expected UnsubAck packet"),
        }
    }

    #[tokio::test]
    async fn test_unsubscribe_existing_topic() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        // First subscribe to a topic
        let subscribe_packet = mercurio_packets::subscribe::SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubscribePayload {
                topic_filter: "test/topic".to_string(),
                subs_opt: default_subscription_options(),
            }],
        };
        session
            .handle_subscribe(subscribe_packet, &broker, None)
            .await
            .unwrap();

        // Now unsubscribe
        let unsub_packet = UnsubscribePacket {
            packet_id: 2,
            properties: None,
            payload: vec![UnsubscribePayload {
                topic_filter: "test/topic".to_string(),
            }],
        };

        let result = session
            .handle_unsubscribe(unsub_packet, &broker)
            .await
            .unwrap();

        match result {
            Some(ControlPacket::UnsubAck(ack)) => {
                assert_eq!(ack.packet_id, 2);
                assert_eq!(ack.payload.len(), 1);
                assert_eq!(ack.payload[0].reason_code, ReasonCode::Success);
            }
            _ => panic!("Expected UnsubAck packet"),
        }
    }

    #[tokio::test]
    async fn test_unsubscribe_multiple_topics() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        // Subscribe to two topics
        let subscribe_packet = mercurio_packets::subscribe::SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![
                SubscribePayload {
                    topic_filter: "topic/one".to_string(),
                    subs_opt: default_subscription_options(),
                },
                SubscribePayload {
                    topic_filter: "topic/two".to_string(),
                    subs_opt: default_subscription_options(),
                },
            ],
        };
        session
            .handle_subscribe(subscribe_packet, &broker, None)
            .await
            .unwrap();

        // Unsubscribe from one existing, one non-existing
        let unsub_packet = UnsubscribePacket {
            packet_id: 3,
            properties: None,
            payload: vec![
                UnsubscribePayload {
                    topic_filter: "topic/one".to_string(),
                },
                UnsubscribePayload {
                    topic_filter: "topic/nonexistent".to_string(),
                },
                UnsubscribePayload {
                    topic_filter: "topic/two".to_string(),
                },
            ],
        };

        let result = session
            .handle_unsubscribe(unsub_packet, &broker)
            .await
            .unwrap();

        match result {
            Some(ControlPacket::UnsubAck(ack)) => {
                assert_eq!(ack.packet_id, 3);
                assert_eq!(ack.payload.len(), 3);
                assert_eq!(ack.payload[0].reason_code, ReasonCode::Success);
                assert_eq!(
                    ack.payload[1].reason_code,
                    ReasonCode::NoSubscriptionExisted
                );
                assert_eq!(ack.payload[2].reason_code, ReasonCode::Success);
            }
            _ => panic!("Expected UnsubAck packet"),
        }
    }

    fn subscribe_packet(
        topic_filter: &str,
        qos: QoS,
    ) -> mercurio_packets::subscribe::SubscribePacket {
        mercurio_packets::subscribe::SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubscribePayload {
                topic_filter: topic_filter.to_string(),
                subs_opt: SubscriptionOptions {
                    qos,
                    ..Default::default()
                },
            }],
        }
    }

    fn message(topic: &str, qos: QoS, packet_id: Option<u16>, retain: bool) -> Message {
        Message {
            packet_id,
            topic: Arc::from(topic),
            dup: false,
            qos,
            retain,
            payload: Some(Bytes::from("payload")),
            properties: Default::default(),
            expires_at: None,
            subscription_identifier: None,
        }
    }

    async fn next_publish(session: &mut Session) -> PublishPacket {
        let message = session.next_outgoing(true).await;
        match session.prepare_publish(message).await.map(|o| o.packet) {
            Some(ControlPacket::Publish(p)) => p,
            other => panic!("Expected Publish packet, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_outgoing_publish_uses_published_topic_name() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        session
            .handle_subscribe(subscribe_packet("a/+", QoS::AtMostOnce), &broker, None)
            .await
            .unwrap();
        broker
            .publish("a/b", message("a/b", QoS::AtMostOnce, None, false))
            .await
            .unwrap();

        assert_eq!(next_publish(&mut session).await.topic_name, "a/b");
    }

    #[tokio::test]
    async fn test_subscription_grants_requested_qos_and_downgrades_delivery() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let ack = session
            .handle_subscribe(subscribe_packet("q/0", QoS::AtMostOnce), &broker, None)
            .await
            .unwrap();
        assert!(matches!(
            ack,
            Some(ControlPacket::SubAck(ref a)) if a.payload[0].reason_code == ReasonCode::GrantedQoS0
        ));

        let ack = session
            .handle_subscribe(subscribe_packet("q/1", QoS::AtLeastOnce), &broker, None)
            .await
            .unwrap();
        assert!(matches!(
            ack,
            Some(ControlPacket::SubAck(ref a)) if a.payload[0].reason_code == ReasonCode::GrantedQoS1
        ));

        broker
            .publish("q/0", message("q/0", QoS::ExactlyOnce, Some(7), false))
            .await
            .unwrap();
        let publish = next_publish(&mut session).await;
        assert_eq!(publish.qos_level, QoS::AtMostOnce);
        assert_eq!(publish.packet_id, None);

        broker
            .publish("q/1", message("q/1", QoS::ExactlyOnce, Some(8), false))
            .await
            .unwrap();
        assert_eq!(next_publish(&mut session).await.qos_level, QoS::AtLeastOnce);

        broker
            .publish("q/1", message("q/1", QoS::AtMostOnce, None, false))
            .await
            .unwrap();
        assert_eq!(next_publish(&mut session).await.qos_level, QoS::AtMostOnce);
    }

    #[tokio::test]
    async fn test_outgoing_packet_ids_are_allocated_per_session() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        session
            .handle_subscribe(subscribe_packet("p", QoS::ExactlyOnce), &broker, None)
            .await
            .unwrap();

        // Two publishers reusing the same packet identifier
        for _ in 0..2 {
            broker
                .publish("p", message("p", QoS::AtLeastOnce, Some(42), false))
                .await
                .unwrap();
        }

        let first = next_publish(&mut session).await.packet_id.unwrap();
        let second = next_publish(&mut session).await.packet_id.unwrap();
        assert_ne!(first, second);
        assert_ne!(first, 0);
        assert_ne!(second, 0);
    }

    #[test]
    fn test_next_packet_id_skips_in_flight_and_wraps() {
        let mut session = create_test_session();
        let state = Arc::get_mut(&mut session.shared).unwrap().state.get_mut();

        state.last_packet_id = u16::MAX - 1;
        state
            .unacknowledged_messages
            .insert(u16::MAX, (0, PublishPacket::default()));
        state
            .unacknowledged_messages
            .insert(1, (1, PublishPacket::default()));

        assert_eq!(state.next_packet_id(), Some(2));
    }

    fn qos2_publish(topic: &str, packet_id: u16, dup: bool) -> ControlPacket {
        ControlPacket::Publish(PublishPacket {
            dup,
            qos_level: QoS::ExactlyOnce,
            retain: false,
            topic_name: topic.to_string(),
            packet_id: Some(packet_id),
            properties: None,
            payload: Some(Bytes::from("payload")),
        })
    }

    #[tokio::test]
    async fn test_incoming_qos2_retransmission_is_delivered_once() {
        let mut publisher = create_test_session();
        let mut subscriber = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        subscriber
            .handle_subscribe(subscribe_packet("t", QoS::ExactlyOnce), &broker, None)
            .await
            .unwrap();

        for dup in [false, true] {
            let response = publisher
                .process_incoming(qos2_publish("t", 5, dup), &broker, None, None)
                .await
                .unwrap();
            assert!(matches!(response, Some(ControlPacket::PubRec(ref r)) if r.packet_id == 5));
        }

        next_publish(&mut subscriber).await;
        assert!(
            timeout(Duration::from_millis(50), subscriber.next_outgoing(true))
                .await
                .is_err()
        );

        // After PUBREL the packet id is free again for a new message
        publisher
            .process_incoming(
                ControlPacket::PubRel(PubRelPacket {
                    packet_id: 5,
                    reason: ReasonCode::Success,
                    properties: None,
                }),
                &broker,
                None,
                None,
            )
            .await
            .unwrap();
        publisher
            .process_incoming(qos2_publish("t", 5, false), &broker, None, None)
            .await
            .unwrap();
        next_publish(&mut subscriber).await;
    }

    #[tokio::test]
    async fn test_incoming_pubrel_leaves_outgoing_qos2_state_alone() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
        let packet_id = send_qos2_to(&mut session, &broker).await;

        let pubrel = session
            .process_incoming(pubrec(packet_id, ReasonCode::Success), &broker, None, None)
            .await
            .unwrap();
        assert!(
            matches!(pubrel, Some(ControlPacket::PubRel(ref p)) if p.reason == ReasonCode::Success)
        );
        session
            .process_incoming(pubrel_packet(packet_id), &broker, None, None)
            .await
            .unwrap();

        assert!(session
            .shared
            .state
            .lock()
            .await
            .pubrecs
            .contains_key(&packet_id));
    }

    /// Delivers one QoS 2 message to `session`, returning its packet id.
    async fn send_qos2_to(
        session: &mut Session,
        broker: &crate::broker::Broker<MemoryStore>,
    ) -> u16 {
        session
            .handle_subscribe(subscribe_packet("q2", QoS::ExactlyOnce), broker, None)
            .await
            .unwrap();
        broker
            .publish("q2", message("q2", QoS::ExactlyOnce, Some(1), false))
            .await
            .unwrap();
        next_publish(session).await.packet_id.unwrap()
    }

    fn pubrec(packet_id: u16, reason: ReasonCode) -> ControlPacket {
        ControlPacket::PubRec(PubRecPacket {
            packet_id,
            reason,
            properties: None,
        })
    }

    fn pubrel_packet(packet_id: u16) -> ControlPacket {
        ControlPacket::PubRel(PubRelPacket {
            packet_id,
            reason: ReasonCode::Success,
            properties: None,
        })
    }

    #[tokio::test]
    async fn test_unknown_packet_ids_are_reported() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let pubcomp = session
            .process_incoming(pubrel_packet(77), &broker, None, None)
            .await
            .unwrap();
        assert!(matches!(
            pubcomp,
            Some(ControlPacket::PubComp(ref p)) if p.reason == ReasonCode::PacketIdentifierNotFound
        ));

        let pubrel = session
            .process_incoming(pubrec(78, ReasonCode::Success), &broker, None, None)
            .await
            .unwrap();
        assert!(matches!(
            pubrel,
            Some(ControlPacket::PubRel(ref p)) if p.reason == ReasonCode::PacketIdentifierNotFound
        ));
        assert!(session.shared.state.lock().await.pubrecs.is_empty());
    }

    #[tokio::test]
    async fn test_failed_pubrec_ends_the_exchange() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
        let packet_id = send_qos2_to(&mut session, &broker).await;

        let response = session
            .process_incoming(
                pubrec(packet_id, ReasonCode::UnspecifiedError),
                &broker,
                None,
                None,
            )
            .await
            .unwrap();

        assert!(response.is_none());
        let state = session.shared.state.lock().await;
        assert!(state.pubrecs.is_empty());
        assert!(state.unacknowledged_messages.is_empty());
    }

    #[tokio::test]
    async fn test_subscriptions_record_granted_qos() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        session
            .handle_subscribe(subscribe_packet("s", QoS::AtLeastOnce), &broker, None)
            .await
            .unwrap();

        assert_eq!(
            session.get_subscriptions().await,
            vec![StoredSubscription::new("s", QoS::AtLeastOnce)]
        );
    }

    #[tokio::test]
    async fn test_restored_subscription_keeps_granted_qos() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        session
            .restore_subscriptions(vec![StoredSubscription::new("r", QoS::AtMostOnce)], &broker)
            .await
            .unwrap();
        broker
            .publish("r", message("r", QoS::ExactlyOnce, Some(3), false))
            .await
            .unwrap();

        assert_eq!(next_publish(&mut session).await.qos_level, QoS::AtMostOnce);
    }

    #[tokio::test]
    async fn test_expired_messages_are_dropped_and_others_carry_remaining_time() {
        let mut session = create_test_session();

        let mut expired = message("t", QoS::AtMostOnce, None, false);
        expired.expires_at = Some(unix_millis() - 1);
        assert!(session.prepare_publish(expired).await.is_none());

        let mut fresh = message("t", QoS::AtMostOnce, None, false);
        fresh.expires_at = Some(unix_millis() + 5000);
        let Some(ControlPacket::Publish(publish)) =
            session.prepare_publish(fresh).await.map(|o| o.packet)
        else {
            panic!("Expected Publish");
        };
        let interval = publish.properties.unwrap().message_expiry_interval.unwrap();
        assert_eq!(interval.value, 5);
    }

    fn subscribe_with(
        topic_filter: &str,
        subs_opt: SubscriptionOptions,
    ) -> mercurio_packets::subscribe::SubscribePacket {
        mercurio_packets::subscribe::SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubscribePayload {
                topic_filter: topic_filter.to_string(),
                subs_opt,
            }],
        }
    }

    async fn nothing_queued(session: &Session) -> bool {
        timeout(Duration::from_millis(50), session.next_outgoing(true))
            .await
            .is_err()
    }

    #[tokio::test]
    async fn test_no_local_skips_own_messages() {
        let mut session = create_test_session();
        let mut other = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let options = SubscriptionOptions {
            no_local: true,
            ..Default::default()
        };
        session
            .handle_subscribe(subscribe_with("n", options), &broker, None)
            .await
            .unwrap();

        let publish = ControlPacket::Publish(PublishPacket {
            topic_name: "n".into(),
            payload: Some(Bytes::from("x")),
            ..Default::default()
        });
        session
            .process_incoming(publish, &broker, None, None)
            .await
            .unwrap();
        assert!(nothing_queued(&session).await);

        let publish = ControlPacket::Publish(PublishPacket {
            topic_name: "n".into(),
            payload: Some(Bytes::from("x")),
            ..Default::default()
        });
        other
            .process_incoming(publish, &broker, None, None)
            .await
            .unwrap();
        next_publish(&mut session).await;
    }

    #[tokio::test]
    async fn test_retain_as_published_keeps_retain_flag() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let options = SubscriptionOptions {
            retain_as_pub: true,
            ..Default::default()
        };
        session
            .handle_subscribe(subscribe_with("r", options), &broker, None)
            .await
            .unwrap();
        broker
            .publish("r", message("r", QoS::AtMostOnce, None, true))
            .await
            .unwrap();

        assert!(next_publish(&mut session).await.retain);
    }

    #[tokio::test]
    async fn test_retain_handling_options() {
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
        broker
            .publish("h", message("h", QoS::AtMostOnce, None, true))
            .await
            .unwrap();

        let if_new = SubscriptionOptions {
            retain_handling: RetainHandling::SendRetainedIfNonExisting,
            ..Default::default()
        };
        let mut session = create_test_session();
        session
            .handle_subscribe(subscribe_with("h", if_new), &broker, None)
            .await
            .unwrap();
        assert!(next_publish(&mut session).await.retain);
        let if_new = SubscriptionOptions {
            retain_handling: RetainHandling::SendRetainedIfNonExisting,
            ..Default::default()
        };
        session
            .handle_subscribe(subscribe_with("h", if_new), &broker, None)
            .await
            .unwrap();
        assert!(nothing_queued(&session).await);

        let never = SubscriptionOptions {
            retain_handling: RetainHandling::DoNotSendRetained,
            ..Default::default()
        };
        let mut session = create_test_session();
        session
            .handle_subscribe(subscribe_with("h", never), &broker, None)
            .await
            .unwrap();
        assert!(nothing_queued(&session).await);
    }

    fn subscribe_with_identifier(
        topic_filter: &str,
        identifier: u32,
    ) -> mercurio_packets::subscribe::SubscribePacket {
        let mut packet = subscribe_packet(topic_filter, QoS::AtMostOnce);
        packet.properties = Some(mercurio_packets::subscribe::SubscribeProperties {
            subscription_id: Some(SubscriptionIdentifier::new(VariableByteInteger(identifier))),
            user_property: None,
        });
        packet
    }

    #[tokio::test]
    async fn test_deliveries_carry_their_subscription_identifier() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        for (filter, identifier) in [("s/+", 7), ("s/#", 9)] {
            session
                .handle_subscribe(subscribe_with_identifier(filter, identifier), &broker, None)
                .await
                .unwrap();
        }
        broker
            .publish("s/x", message("s/x", QoS::AtMostOnce, None, false))
            .await
            .unwrap();

        let mut identifiers = Vec::new();
        for _ in 0..2 {
            let properties = next_publish(&mut session).await.properties.unwrap();
            identifiers.push(properties.subscription_identifier.unwrap().value.0);
        }
        identifiers.sort();
        assert_eq!(identifiers, [7, 9]);
    }

    #[tokio::test]
    async fn test_invalid_subscription_identifiers_are_protocol_errors() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let zero = session
            .handle_subscribe(subscribe_with_identifier("z", 0), &broker, None)
            .await;
        assert!(matches!(
            zero,
            Err(mercurio_core::error::Error::MQTTReasonCode(
                ReasonCode::ProtocolError
            ))
        ));

        let from_client = ControlPacket::Publish(PublishPacket {
            topic_name: "t".into(),
            properties: Some(PublishProperties {
                subscription_identifier: Some(SubscriptionIdentifier::new(VariableByteInteger(1))),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(matches!(
            session
                .process_incoming(from_client, &broker, None, None)
                .await,
            Err(mercurio_core::error::Error::MQTTReasonCode(
                ReasonCode::ProtocolError
            ))
        ));
    }

    fn aliased_publish(topic: &str, alias: u16) -> ControlPacket {
        ControlPacket::Publish(PublishPacket {
            topic_name: topic.into(),
            properties: Some(PublishProperties {
                topic_alias: Some(mercurio_core::properties::TopicAlias::new(alias)),
                ..Default::default()
            }),
            payload: Some(Bytes::from("x")),
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn test_topic_aliases_resolve_to_registered_topics() {
        let mut publisher = create_test_session();
        let mut subscriber = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
        publisher.shared.state.lock().await.topic_alias_maximum = 5;
        subscriber
            .handle_subscribe(subscribe_packet("a/b", QoS::AtMostOnce), &broker, None)
            .await
            .unwrap();

        for topic in ["a/b", ""] {
            publisher
                .process_incoming(aliased_publish(topic, 1), &broker, None, None)
                .await
                .unwrap();
            let publish = next_publish(&mut subscriber).await;
            assert_eq!(publish.topic_name, "a/b");
            // The alias is not forwarded
            assert!(publish.properties.unwrap().topic_alias.is_none());
        }

        for (topic, alias, reason) in [
            ("a/b", 6, ReasonCode::TopicAliasInvalid),
            ("a/b", 0, ReasonCode::TopicAliasInvalid),
            ("", 2, ReasonCode::ProtocolError),
        ] {
            let result = publisher
                .process_incoming(aliased_publish(topic, alias), &broker, None, None)
                .await;
            assert!(
                matches!(result, Err(mercurio_core::error::Error::MQTTReasonCode(r)) if r == reason),
                "alias {alias}"
            );
        }
    }

    #[tokio::test]
    async fn test_messages_beyond_client_maximum_packet_size_are_dropped() {
        let mut connect = create_connect_packet_without_will();
        connect.properties = Some(mercurio_packets::connect::ConnectProperties {
            maximum_packet_size: Some(MaximumPacketSize::new(32)),
            ..Default::default()
        });
        let mut session = Session::new(connect);

        let small = message("t", QoS::AtLeastOnce, None, false);
        assert!(session.prepare_publish(small).await.is_some());

        let mut large = message("t", QoS::AtLeastOnce, None, false);
        large.payload = Some(Bytes::from(vec![0u8; 64]));
        assert!(session.prepare_publish(large).await.is_none());
        // Nothing is left awaiting an acknowledgement that will never come
        assert_eq!(
            session
                .shared
                .state
                .lock()
                .await
                .unacknowledged_messages
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn test_shared_subscription_splits_messages_without_retained() {
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));
        broker
            .publish("w", message("w", QoS::AtMostOnce, None, true))
            .await
            .unwrap();

        let mut members = [create_test_session(), create_test_session()];
        for member in &mut members {
            member
                .handle_subscribe(
                    subscribe_packet("$share/g/w", QoS::AtMostOnce),
                    &broker,
                    None,
                )
                .await
                .unwrap();
            assert!(nothing_queued(member).await);
        }

        for _ in 0..4 {
            broker
                .publish("w", message("w", QoS::AtMostOnce, None, false))
                .await
                .unwrap();
        }
        for member in &mut members {
            for _ in 0..2 {
                assert_eq!(next_publish(member).await.topic_name, "w");
            }
            assert!(nothing_queued(member).await);
        }
    }

    #[tokio::test]
    async fn test_no_local_on_shared_subscription_is_a_protocol_error() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let options = SubscriptionOptions {
            no_local: true,
            ..Default::default()
        };
        let result = session
            .handle_subscribe(subscribe_with("$share/g/w", options), &broker, None)
            .await;
        assert!(matches!(
            result,
            Err(mercurio_core::error::Error::MQTTReasonCode(
                ReasonCode::ProtocolError
            ))
        ));
    }

    #[tokio::test]
    async fn test_packet_identifier_zero_is_a_protocol_error() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let mut subscribe = subscribe_packet("t", QoS::AtMostOnce);
        subscribe.packet_id = 0;
        let packets = [
            qos2_publish("t", 0, false),
            ControlPacket::Subscribe(subscribe),
            ControlPacket::Unsubscribe(UnsubscribePacket {
                packet_id: 0,
                properties: None,
                payload: vec![UnsubscribePayload {
                    topic_filter: "t".into(),
                }],
            }),
            pubrel_packet(0),
        ];

        for packet in packets {
            let result = session.process_incoming(packet, &broker, None, None).await;
            assert!(matches!(
                result,
                Err(mercurio_core::error::Error::MQTTReasonCode(
                    ReasonCode::ProtocolError
                ))
            ));
        }
    }

    #[tokio::test]
    async fn test_unsubscribe_reports_invalid_filters() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        let packet = UnsubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![UnsubscribePayload {
                topic_filter: "a/#/b".into(),
            }],
        };
        let Some(ControlPacket::UnsubAck(ack)) =
            session.handle_unsubscribe(packet, &broker).await.unwrap()
        else {
            panic!("Expected UNSUBACK");
        };
        assert_eq!(ack.payload[0].reason_code, ReasonCode::TopicFilterInvalid);
    }

    #[tokio::test]
    async fn test_outgoing_topics_are_aliased_up_to_the_client_maximum() {
        let mut connect = create_connect_packet_without_will();
        connect.properties = Some(mercurio_packets::connect::ConnectProperties {
            topic_alias_maximum: Some(TopicAliasMaximum::new(1)),
            ..Default::default()
        });
        let mut session = Session::new(connect);

        // Off unless the operator enables it
        let outgoing = session
            .prepare_publish(message("a", QoS::AtMostOnce, None, false))
            .await
            .unwrap();
        let ControlPacket::Publish(publish) = outgoing.packet else {
            panic!("Expected Publish");
        };
        assert!(publish.properties.unwrap().topic_alias.is_none());

        session.shared.state.lock().await.send_topic_aliases = true;

        let mut sent = Vec::new();
        for topic in ["a", "a", "b"] {
            let outgoing = session
                .prepare_publish(message(topic, QoS::AtLeastOnce, None, false))
                .await
                .unwrap();
            let ControlPacket::Publish(publish) = outgoing.packet else {
                panic!("Expected Publish");
            };
            let alias = publish
                .properties
                .unwrap()
                .topic_alias
                .map(|alias| alias.value);
            sent.push((publish.topic_name, alias));
        }
        assert_eq!(
            sent,
            [
                ("a".to_string(), Some(1)),
                (String::new(), Some(1)),
                ("b".to_string(), None)
            ]
        );

        // What would be resent after a reconnect keeps the full topic
        let state = session.shared.state.lock().await;
        assert!(state
            .unacknowledged_messages
            .values()
            .all(|(_, p)| !p.topic_name.is_empty()));
    }

    #[tokio::test]
    async fn test_retained_delivery_sets_retain_flag_only_for_retained() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        broker
            .publish("r/1", message("r/1", QoS::AtMostOnce, None, true))
            .await
            .unwrap();
        session
            .handle_subscribe(subscribe_packet("r/1", QoS::AtMostOnce), &broker, None)
            .await
            .unwrap();
        assert!(next_publish(&mut session).await.retain);

        broker
            .publish("r/1", message("r/1", QoS::AtMostOnce, None, true))
            .await
            .unwrap();
        assert!(!next_publish(&mut session).await.retain);
    }

    #[test]
    fn test_will_message_from_connect_packet() {
        let packet = create_connect_packet_with_will();
        let will = WillMessage::from_connect_packet(&packet);

        assert!(will.is_some());
        let will = will.unwrap();
        assert_eq!(will.topic, "will/topic");
        assert_eq!(will.payload.as_ref(), b"goodbye");
        assert_eq!(will.qos, QoS::AtLeastOnce);
        assert!(will.retain);
    }

    #[test]
    fn test_will_message_keeps_will_properties() {
        let mut packet = create_connect_packet_with_will();
        packet.payload.will_properties = Some(mercurio_packets::connect::WillProperties {
            content_type: Some(mercurio_core::properties::ContentType::new("text".into())),
            ..Default::default()
        });

        let will = WillMessage::from_connect_packet(&packet).unwrap();
        assert_eq!(will.properties.content_type.unwrap().value, "text");
    }

    #[test]
    fn test_will_message_from_connect_packet_no_will() {
        let packet = create_connect_packet_without_will();
        let will = WillMessage::from_connect_packet(&packet);

        assert!(will.is_none());
    }

    #[tokio::test]
    async fn test_session_stores_will() {
        let packet = create_connect_packet_with_will();
        let session = Session::new(packet);

        let will = session.take_will().await;
        assert!(will.is_some());
        let will = will.unwrap();
        assert_eq!(will.topic, "will/topic");
    }

    #[tokio::test]
    async fn test_session_no_will_when_not_set() {
        let packet = create_connect_packet_without_will();
        let session = Session::new(packet);

        let will = session.take_will().await;
        assert!(will.is_none());
    }

    #[tokio::test]
    async fn test_clear_will() {
        let packet = create_connect_packet_with_will();
        let session = Session::new(packet);

        // Will should exist initially
        {
            let state = session.shared.state.lock().await;
            assert!(state.will_message.is_some());
        }

        // Clear the will
        session.clear_will().await;

        // Will should be None after clearing
        let will = session.take_will().await;
        assert!(will.is_none());
    }

    #[tokio::test]
    async fn test_take_will_removes_will() {
        let packet = create_connect_packet_with_will();
        let session = Session::new(packet);

        // First take should return the will
        let will1 = session.take_will().await;
        assert!(will1.is_some());

        // Second take should return None
        let will2 = session.take_will().await;
        assert!(will2.is_none());
    }

    #[tokio::test]
    async fn test_set_connect_packet_updates_will() {
        let packet_no_will = create_connect_packet_without_will();
        let mut session = Session::new(packet_no_will);

        // No will initially
        assert!(session.take_will().await.is_none());

        // Update with packet that has will
        let packet_with_will = create_connect_packet_with_will();
        session.set_connect_packet(packet_with_will).await;

        // Now will should exist
        let will = session.take_will().await;
        assert!(will.is_some());
        assert_eq!(will.unwrap().topic, "will/topic");
    }
}
