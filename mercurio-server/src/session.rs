use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};

use bytes::Bytes;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::Mutex,
    time::Duration,
};
use tracing::{info, warn};

use mercurio_core::{
    message::Message,
    properties::{AssignedClientIdentifier, MaximumPacketSize},
    qos::QoS,
    reason::ReasonCode,
    topic::{validate_publish_topic, validate_subscribe_filter},
    Result,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connack::{ConnAckPacket, ConnAckProperties},
    connect::ConnectPacket,
    pingresp::PingRespPacket,
    puback::PubAckPacket,
    pubcomp::PubCompPacket,
    publish::PublishPacket,
    pubrec::PubRecPacket,
    pubrel::PubRelPacket,
    suback::{SubAckPacket, SubAckPayload},
    unsuback::{UnsubAckPacket, UnsubAckPayload},
    unsubscribe::UnsubscribePacket,
    ControlPacket,
};

use crate::{
    auth::{AuthManager, AuthResult},
    broker::Broker,
    connection::{Connection, MAX_PACKET_SIZE},
    queue::{MessageQueue, Subscriber},
    topic_tree::TopicTree,
};
use mercurio_storage::{RetainedMessageStore, StoredSubscription, StoredWillMessage};

/// Will message to be published on abnormal client disconnect.
#[derive(Debug, Clone)]
pub struct WillMessage {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
    /// How long to wait after the connection closes before publishing
    pub delay: Duration,
}

impl WillMessage {
    /// Extract will message from a ConnectPacket if will_flag is set.
    fn from_connect_packet(packet: &ConnectPacket) -> Option<Self> {
        if !packet.flags.will_flag {
            return None;
        }

        let topic = packet.payload.will_topic.as_ref()?;
        let payload = packet.payload.will_payload.clone().unwrap_or_default();
        let delay = packet
            .payload
            .will_properties
            .as_ref()
            .and_then(|p| p.will_delay_interval.as_ref())
            .map_or(0, |d| d.value);

        Some(WillMessage {
            topic: topic.clone(),
            payload,
            qos: packet.flags.will_qos,
            retain: packet.flags.will_retain,
            delay: Duration::from_secs(delay.into()),
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
            // ponytail: storage doesn't keep the delay, so wills restored after a
            // restart publish immediately; add a column if that matters
            delay: Duration::ZERO,
        }
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
    granted_qos: HashMap<String, QoS>,
    /// Where the subscriptions are registered, once there are any
    topics: Option<TopicTree<Subscriber>>,
    /// Outgoing QoS 1/2 messages awaiting PUBACK/PUBREC, by packet_id
    unacknowledged_messages: HashMap<u16, PublishPacket>,
    /// Outgoing QoS 2 messages awaiting PUBCOMP, by packet_id
    pubrecs: HashMap<u16, PubRecPacket>,
    /// Incoming QoS 2 packet ids already delivered and awaiting PUBREL
    incoming_qos2: HashSet<u16>,
    /// Last packet identifier assigned to an outgoing QoS > 0 PUBLISH
    last_packet_id: u16,
    /// Will message to be published on abnormal disconnect
    will_message: Option<WillMessage>,
    /// Current authentication method name for ongoing re-authentication
    auth_method: Option<String>,
}

impl SessionDropGuard {
    pub fn new(connect_packet: ConnectPacket) -> Self {
        SessionDropGuard {
            session: Session::new(connect_packet),
        }
    }

    pub(crate) fn session(&self) -> Session {
        self.session.clone()
    }
}

impl Session {
    pub fn new(connect_packet: ConnectPacket) -> Self {
        let client_id = connect_packet.payload.client_id.clone();
        let will_message = WillMessage::from_connect_packet(&connect_packet);
        Session {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    connect_packet,
                    generation: 0,
                    client_id,
                    granted_qos: HashMap::new(),
                    topics: None,
                    // Pre-allocate with reasonable capacity for typical usage
                    unacknowledged_messages: HashMap::with_capacity(16),
                    pubrecs: HashMap::with_capacity(16),
                    incoming_qos2: HashSet::new(),
                    last_packet_id: 0,
                    will_message,
                    auth_method: None,
                }),
                queue: Arc::default(),
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

    /// Set the will message. Called when restoring from storage.
    pub(crate) async fn set_will(&self, will: WillMessage) {
        let mut session = self.shared.state.lock().await;
        session.will_message = Some(will);
    }

    /// Returns a clone of the client ID. For logging, prefer using the cached version.
    pub(crate) async fn get_client_id(&self) -> String {
        let session = self.shared.state.lock().await;
        session.client_id.clone()
    }

    /// Returns the subscriptions of this session with their granted QoS.
    pub(crate) async fn get_subscriptions(&self) -> Vec<StoredSubscription> {
        let session = self.shared.state.lock().await;
        session
            .granted_qos
            .iter()
            .map(|(topic_filter, qos)| StoredSubscription {
                topic_filter: topic_filter.clone(),
                qos: *qos,
            })
            .collect()
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
        for StoredSubscription { topic_filter, qos } in subscriptions {
            // Retained messages are only sent for a new SUBSCRIBE, not when an
            // existing subscription is resumed [MQTT-3.3.1-9]
            broker
                .subscribe(
                    &topic_filter,
                    self.shared.subscriber_id,
                    self.subscriber(qos),
                )
                .await?;
            session.granted_qos.insert(topic_filter, qos);
        }

        Ok(())
    }

    pub async fn begin<S>(
        &mut self,
        connection: &mut Connection<S>,
        resume: bool,
        assigned_client_id: bool,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        let mut ack = ConnAckPacket::default();
        ack.flags.session_present = resume;

        let client_id = {
            let session = self.shared.state.lock().await;

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
                    maximum_packet_size: Some(MaximumPacketSize::new(MAX_PACKET_SIZE as u32)),
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
        packet: PublishPacket,
        broker: &Broker<S>,
    ) -> Result<Option<ControlPacket>> {
        // Validate topic name (no wildcards, no null chars, etc.)
        if validate_publish_topic(&packet.topic_name).is_err() {
            return Err(ReasonCode::TopicNameInvalid.into());
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
                    let first_delivery = self
                        .shared
                        .state
                        .lock()
                        .await
                        .incoming_qos2
                        .insert(packet_id);

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
        let message = Message {
            packet_id: packet.packet_id,
            topic: Arc::clone(&topic),
            dup: packet.dup,
            retain: packet.retain,
            qos: packet.qos_level,
            payload: packet.payload,
        };

        broker.publish(&topic, message).await?;

        response
    }

    async fn handle_puback(&mut self, packet: PubAckPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        // O(1) removal instead of O(n) search + remove
        session.unacknowledged_messages.remove(&packet.packet_id);
        Ok(None)
    }

    async fn handle_pubrec(&mut self, packet: PubRecPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        // O(1) removal
        session.unacknowledged_messages.remove(&packet.packet_id);

        let packet_id = packet.packet_id;
        // O(1) insertion
        session.pubrecs.insert(packet_id, packet);

        Ok(Some(ControlPacket::PubRel(PubRelPacket {
            packet_id,
            reason: ReasonCode::Success,
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
        session.incoming_qos2.remove(&packet.packet_id);

        Ok(Some(ControlPacket::PubComp(PubCompPacket {
            packet_id: packet.packet_id,
            reason: ReasonCode::Success,
            properties: None,
        })))
    }

    async fn handle_subscribe<S: RetainedMessageStore>(
        &mut self,
        packet: mercurio_packets::subscribe::SubscribePacket,
        broker: &Broker<S>,
    ) -> Result<Option<ControlPacket>> {
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

            let granted_qos = sub.subs_opt.qos;
            let subscriber = self.subscriber(granted_qos);
            let retained_messages = broker
                .subscribe(
                    &sub.topic_filter,
                    self.shared.subscriber_id,
                    subscriber.clone(),
                )
                .await?;
            for message in retained_messages {
                subscriber.deliver(message);
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
                .granted_qos
                .insert(sub.topic_filter.to_string(), granted_qos);
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
            broker.unsubscribe(&unsub.topic_filter, self.shared.subscriber_id);
            let reason_code = if session.granted_qos.remove(&unsub.topic_filter).is_some() {
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

        // Get auth method from packet properties
        let method_name = packet
            .properties
            .auth_method
            .as_ref()
            .map(|m| m.value.as_str());

        // Get auth data from packet properties
        let auth_data = packet.properties.auth_data.as_ref().map(|d| &d.value);

        match packet.reason {
            ReasonCode::ReAuthenticate => {
                // Client wants to re-authenticate
                let Some(method_name) = method_name else {
                    return Err(ReasonCode::ProtocolError.into());
                };

                if !auth_manager.supports_method(method_name) {
                    return Err(ReasonCode::BadAuthenticationMethod.into());
                }

                // Start re-authentication
                session.auth_method = Some(method_name.to_string());

                match auth_manager.start_auth(method_name, auth_data) {
                    AuthResult::Success => {
                        session.auth_method = None;
                        Ok(Some(ControlPacket::Auth(AuthPacket {
                            reason: ReasonCode::Success,
                            properties: AuthProperties::default(),
                        })))
                    }
                    AuthResult::Continue(challenge) => {
                        use mercurio_core::properties::{AuthenticationData, AuthenticationMethod};
                        Ok(Some(ControlPacket::Auth(AuthPacket {
                            reason: ReasonCode::ContinueAuthentication,
                            properties: AuthProperties {
                                auth_method: Some(AuthenticationMethod {
                                    value: method_name.to_string(),
                                }),
                                auth_data: Some(AuthenticationData { value: challenge }),
                                reason_string: None,
                                user_property: None,
                            },
                        })))
                    }
                    AuthResult::Failed(reason) => Err(reason.into()),
                }
            }
            ReasonCode::ContinueAuthentication => {
                // Client is continuing an ongoing authentication
                let Some(ref current_method) = session.auth_method else {
                    // No authentication in progress
                    return Err(ReasonCode::ProtocolError.into());
                };

                let Some(auth_data) = auth_data else {
                    return Err(ReasonCode::ProtocolError.into());
                };

                match auth_manager.continue_auth(current_method, auth_data) {
                    AuthResult::Success => {
                        session.auth_method = None;
                        Ok(Some(ControlPacket::Auth(AuthPacket {
                            reason: ReasonCode::Success,
                            properties: AuthProperties::default(),
                        })))
                    }
                    AuthResult::Continue(challenge) => {
                        use mercurio_core::properties::{AuthenticationData, AuthenticationMethod};
                        Ok(Some(ControlPacket::Auth(AuthPacket {
                            reason: ReasonCode::ContinueAuthentication,
                            properties: AuthProperties {
                                auth_method: Some(AuthenticationMethod {
                                    value: current_method.clone(),
                                }),
                                auth_data: Some(AuthenticationData { value: challenge }),
                                reason_string: None,
                                user_property: None,
                            },
                        })))
                    }
                    AuthResult::Failed(reason) => {
                        session.auth_method = None;
                        Err(reason.into())
                    }
                }
            }
            _ => {
                // Unexpected reason code
                Err(ReasonCode::ProtocolError.into())
            }
        }
    }

    pub(crate) async fn process_incoming<S: RetainedMessageStore>(
        &mut self,
        packet: ControlPacket,
        broker: &Broker<S>,
        auth_manager: Option<&AuthManager>,
    ) -> Result<Option<ControlPacket>> {
        match packet {
            ControlPacket::Publish(packet) => self.handle_publish(packet, broker).await,
            ControlPacket::PubAck(packet) => self.handle_puback(packet).await,
            ControlPacket::PubRec(packet) => self.handle_pubrec(packet).await,
            ControlPacket::PubRel(packet) => self.handle_pubrel(packet).await,
            ControlPacket::PubComp(packet) => self.handle_pubcomp(packet).await,
            ControlPacket::Subscribe(packet) => self.handle_subscribe(packet, broker).await,
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

    fn subscriber(&self, granted_qos: QoS) -> Subscriber {
        Subscriber {
            queue: Arc::clone(&self.shared.queue),
            granted_qos,
        }
    }

    /// Removes every subscription of this session from the broker. Called
    /// when the session ends, so nothing is queued for it anymore.
    pub(crate) async fn unsubscribe_all(&self) {
        let mut session = self.shared.state.lock().await;
        if let Some(topics) = session.topics.take() {
            for topic_filter in session.granted_qos.keys() {
                topics.unsubscribe(topic_filter, self.shared.subscriber_id);
            }
        }
        session.granted_qos.clear();
    }

    /// Waits for the next message to send to the client. Cancel safe, and
    /// doesn't hold the session state while waiting.
    pub(crate) async fn next_outgoing(&self) -> Message {
        self.shared.queue.pop().await
    }

    /// Turns a queued message into the PUBLISH to send, tracking it until
    /// acknowledged. Returns None if the message had to be dropped.
    pub(crate) async fn prepare_publish(&mut self, message: Message) -> Option<ControlPacket> {
        let mut session = self.shared.state.lock().await;

        {
            let packet_id = match message.qos {
                QoS::AtMostOnce => None,
                _ => match session.next_packet_id() {
                    Some(id) => Some(id),
                    None => {
                        // ponytail: drops on exhaustion; enforce Receive Maximum to
                        // apply backpressure instead
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
                properties: None,
                payload: message.payload,
            };

            if let Some(packet_id) = packet_id {
                session
                    .unacknowledged_messages
                    .insert(packet_id, publish.clone());
            }

            Some(ControlPacket::Publish(publish))
        }
    }
}

impl State {
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
        subscribe::{SubscribePayload, SubscriptionOptions},
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
            .handle_subscribe(subscribe_packet, &broker)
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
            .handle_subscribe(subscribe_packet, &broker)
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
        }
    }

    async fn next_publish(session: &mut Session) -> PublishPacket {
        let message = session.next_outgoing().await;
        match session.prepare_publish(message).await {
            Some(ControlPacket::Publish(p)) => p,
            other => panic!("Expected Publish packet, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_outgoing_publish_uses_published_topic_name() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        session
            .handle_subscribe(subscribe_packet("a/+", QoS::AtMostOnce), &broker)
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
            .handle_subscribe(subscribe_packet("q/0", QoS::AtMostOnce), &broker)
            .await
            .unwrap();
        assert!(matches!(
            ack,
            Some(ControlPacket::SubAck(ref a)) if a.payload[0].reason_code == ReasonCode::GrantedQoS0
        ));

        let ack = session
            .handle_subscribe(subscribe_packet("q/1", QoS::AtLeastOnce), &broker)
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
            .handle_subscribe(subscribe_packet("p", QoS::ExactlyOnce), &broker)
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
            .insert(u16::MAX, PublishPacket::default());
        state
            .unacknowledged_messages
            .insert(1, PublishPacket::default());

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
            .handle_subscribe(subscribe_packet("t", QoS::ExactlyOnce), &broker)
            .await
            .unwrap();

        for dup in [false, true] {
            let response = publisher
                .process_incoming(qos2_publish("t", 5, dup), &broker, None)
                .await
                .unwrap();
            assert!(matches!(response, Some(ControlPacket::PubRec(ref r)) if r.packet_id == 5));
        }

        next_publish(&mut subscriber).await;
        assert!(
            timeout(Duration::from_millis(50), subscriber.next_outgoing())
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
            )
            .await
            .unwrap();
        publisher
            .process_incoming(qos2_publish("t", 5, false), &broker, None)
            .await
            .unwrap();
        next_publish(&mut subscriber).await;
    }

    #[tokio::test]
    async fn test_incoming_pubrel_leaves_outgoing_qos2_state_alone() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        session
            .process_incoming(
                ControlPacket::PubRec(PubRecPacket {
                    packet_id: 9,
                    reason: ReasonCode::Success,
                    properties: None,
                }),
                &broker,
                None,
            )
            .await
            .unwrap();
        session
            .process_incoming(
                ControlPacket::PubRel(PubRelPacket {
                    packet_id: 9,
                    reason: ReasonCode::Success,
                    properties: None,
                }),
                &broker,
                None,
            )
            .await
            .unwrap();

        assert!(session.shared.state.lock().await.pubrecs.contains_key(&9));
    }

    #[tokio::test]
    async fn test_subscriptions_record_granted_qos() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        session
            .handle_subscribe(subscribe_packet("s", QoS::AtLeastOnce), &broker)
            .await
            .unwrap();

        assert_eq!(
            session.get_subscriptions().await,
            vec![StoredSubscription {
                topic_filter: "s".to_string(),
                qos: QoS::AtLeastOnce,
            }]
        );
    }

    #[tokio::test]
    async fn test_restored_subscription_keeps_granted_qos() {
        let mut session = create_test_session();
        let broker = crate::broker::Broker::new(Arc::new(MemoryStore::new()));

        session
            .restore_subscriptions(
                vec![StoredSubscription {
                    topic_filter: "r".to_string(),
                    qos: QoS::AtMostOnce,
                }],
                &broker,
            )
            .await
            .unwrap();
        broker
            .publish("r", message("r", QoS::ExactlyOnce, Some(3), false))
            .await
            .unwrap();

        assert_eq!(next_publish(&mut session).await.qos_level, QoS::AtMostOnce);
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
            .handle_subscribe(subscribe_packet("r/1", QoS::AtMostOnce), &broker)
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
