use std::{collections::HashMap, pin::Pin, sync::Arc};

use bytes::Bytes;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::{Stream, StreamExt, StreamMap};
use tracing::info;
use uuid::Uuid;

type Messages = Pin<Box<dyn Stream<Item = Message> + Send>>;

use mercurio_core::{
    message::Message, properties::AssignedClientIdentifier, qos::QoS, reason::ReasonCode, Result,
};
use mercurio_packets::{
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

use crate::{broker::Broker, connection::Connection};

/// Will message to be published on abnormal client disconnect.
#[derive(Debug, Clone)]
pub struct WillMessage {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
}

impl WillMessage {
    /// Extract will message from a ConnectPacket if will_flag is set.
    fn from_connect_packet(packet: &ConnectPacket) -> Option<Self> {
        if !packet.flags.will_flag {
            return None;
        }

        let topic = packet.payload.will_topic.as_ref()?;
        let payload = packet.payload.will_payload.clone().unwrap_or_default();

        Some(WillMessage {
            topic: topic.clone(),
            payload,
            qos: packet.flags.will_qos,
            retain: packet.flags.will_retain,
        })
    }
}

pub struct SessionDropGuard {
    session: Session,
}

#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
}

struct Shared {
    state: Mutex<State>,
}

struct State {
    pub connect_packet: ConnectPacket,
    /// Cached client ID to avoid lock acquisition for logging
    client_id: String,
    subscriptions: StreamMap<String, Messages>,
    /// Maps packet_id -> PublishPacket for O(1) lookup/removal
    unacknowledged_messages: HashMap<u16, PublishPacket>,
    /// Maps packet_id -> PubRecPacket for O(1) lookup/removal
    pubrecs: HashMap<u16, PubRecPacket>,
    /// Will message to be published on abnormal disconnect
    will_message: Option<WillMessage>,
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
                    client_id,
                    subscriptions: StreamMap::new(),
                    // Pre-allocate with reasonable capacity for typical usage
                    unacknowledged_messages: HashMap::with_capacity(16),
                    pubrecs: HashMap::with_capacity(16),
                    will_message,
                }),
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

    /// Returns a clone of the client ID. For logging, prefer using the cached version.
    pub(crate) async fn get_client_id(&self) -> String {
        let session = self.shared.state.lock().await;
        session.client_id.clone()
    }

    pub async fn begin(&mut self, connection: &mut Connection, resume: bool) -> Result<()> {
        let mut ack = ConnAckPacket::default();
        ack.flags.session_present = resume;

        let client_id = {
            let mut session = self.shared.state.lock().await;

            if session.connect_packet.payload.client_id.is_empty() {
                let uuid = Uuid::new_v4();
                let generated_id = uuid.hyphenated().to_string();
                session.connect_packet.payload.client_id = generated_id.clone();
                session.client_id = generated_id.clone();
                ack.properties = Some(ConnAckProperties {
                    assigned_client_id: Some(AssignedClientIdentifier::new(generated_id)),
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

    async fn handle_publish(
        &mut self,
        packet: PublishPacket,
        broker: &Broker,
    ) -> Result<Option<ControlPacket>> {
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
                    // Store for QoS 2 flow - use packet_id as key for O(1) lookup
                    let mut session = self.shared.state.lock().await;
                    session
                        .unacknowledged_messages
                        .insert(packet_id, packet.clone());

                    Ok(Some(ControlPacket::PubRec(PubRecPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    })))
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

        broker.publish(&topic, message)?;

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
        // O(1) removal
        session.pubrecs.remove(&packet.packet_id);

        Ok(Some(ControlPacket::PubComp(PubCompPacket {
            packet_id: packet.packet_id,
            reason: ReasonCode::Success,
            properties: None,
        })))
    }

    async fn handle_subscribe(
        &mut self,
        packet: mercurio_packets::subscribe::SubscribePacket,
        broker: &Broker,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        // Pre-allocate payload vec based on subscription count
        let mut ack = SubAckPacket {
            packet_id: packet.packet_id,
            properties: None,
            payload: Vec::with_capacity(packet.payload.len()),
        };

        for sub in &packet.payload {
            let (mut rx, retained_messages) = broker.subscribe(&sub.topic_filter);
            ack.payload.push(SubAckPayload {
                reason_code: ReasonCode::GrantedQoS0,
            });

            // Create a stream that first yields retained messages, then live messages
            let stream = Box::pin(async_stream::stream! {
                // First deliver any retained messages
                for msg in retained_messages {
                    yield msg;
                }

                // Then continue with live messages
                loop {
                    match rx.recv().await {
                        Ok(msg) => yield msg,
                        // If we lagged in consuming messages, just resume.
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => break,
                    }
                }
            });

            session
                .subscriptions
                .insert(sub.topic_filter.to_string(), stream);
        }

        Ok(Some(ControlPacket::SubAck(ack)))
    }

    async fn handle_unsubscribe(
        &mut self,
        packet: UnsubscribePacket,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let mut ack = UnsubAckPacket {
            packet_id: packet.packet_id,
            properties: None,
            payload: Vec::with_capacity(packet.payload.len()),
        };

        for unsub in &packet.payload {
            // Remove the subscription from the StreamMap
            // Returns Some if the subscription existed, None otherwise
            let reason_code = if session.subscriptions.remove(&unsub.topic_filter).is_some() {
                ReasonCode::Success
            } else {
                ReasonCode::NoSubscriptionExisted
            };

            ack.payload.push(UnsubAckPayload { reason_code });
        }

        Ok(Some(ControlPacket::UnsubAck(ack)))
    }

    pub(crate) async fn process_incoming(
        &mut self,
        packet: ControlPacket,
        broker: &Broker,
    ) -> Result<Option<ControlPacket>> {
        match packet {
            ControlPacket::Publish(packet) => self.handle_publish(packet, broker).await,
            ControlPacket::PubAck(packet) => self.handle_puback(packet).await,
            ControlPacket::PubRec(packet) => self.handle_pubrec(packet).await,
            ControlPacket::PubRel(packet) => self.handle_pubrel(packet).await,
            ControlPacket::PubComp(packet) => self.handle_pubcomp(packet).await,
            ControlPacket::Subscribe(packet) => self.handle_subscribe(packet, broker).await,
            ControlPacket::Unsubscribe(packet) => self.handle_unsubscribe(packet).await,
            ControlPacket::PingReq(_) => Ok(Some(ControlPacket::PingResp(PingRespPacket {}))),
            ControlPacket::Disconnect(packet) => Ok(Some(ControlPacket::Disconnect(packet))),
            ControlPacket::Auth(_) => todo!(),

            // Some packets are not supposed to be received by the server.
            // Namely: ConnAck, UnsubAck, PingResp
            // The Connect packet is handled before the session is created.
            _ => Err(ReasonCode::ProtocolError.into()),
        }
    }

    pub(crate) async fn process_outgoing(&mut self) -> Option<ControlPacket> {
        let mut session = self.shared.state.lock().await;

        match session.subscriptions.next().await {
            Some((topic, message)) => {
                let publish = PublishPacket {
                    dup: message.dup,
                    qos_level: message.qos,
                    retain: false,
                    topic_name: topic,
                    packet_id: message.packet_id,
                    properties: None,
                    payload: message.payload,
                };

                // Track for acknowledgment if QoS > 0
                if let Some(packet_id) = publish.packet_id {
                    match message.qos {
                        QoS::AtMostOnce => {}
                        QoS::AtLeastOnce | QoS::ExactlyOnce => {
                            // O(1) insertion
                            session
                                .unacknowledged_messages
                                .insert(packet_id, publish.clone());
                        }
                        QoS::Invalid => unreachable!(),
                    }
                }

                Some(ControlPacket::Publish(publish))
            }
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mercurio_packets::{
        connect::{ConnectFlags, ConnectPayload},
        subscribe::{SubscribePayload, SubscriptionOptions},
        unsubscribe::UnsubscribePayload,
    };

    fn create_test_session() -> Session {
        let connect_packet = ConnectPacket {
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

        let packet = UnsubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![UnsubscribePayload {
                topic_filter: "nonexistent/topic".to_string(),
            }],
        };

        let result = session.handle_unsubscribe(packet).await.unwrap();

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
        let broker = crate::broker::Broker::new();

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

        let result = session.handle_unsubscribe(unsub_packet).await.unwrap();

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
        let broker = crate::broker::Broker::new();

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

        let result = session.handle_unsubscribe(unsub_packet).await.unwrap();

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
