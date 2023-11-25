use std::{pin::Pin, sync::Arc};

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
    ControlPacket,
};

use crate::{broker::Broker, connection::Connection};

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
    subscriptions: StreamMap<String, Messages>,
    unacknowledged_messages: Vec<PublishPacket>,
    pubrecs: Vec<PubRecPacket>,
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
        Session {
            shared: Arc::new(Shared {
                state: Mutex::new(State {
                    connect_packet,
                    subscriptions: StreamMap::new(),
                    unacknowledged_messages: Vec::new(),
                    pubrecs: Vec::new(),
                }),
            }),
        }
    }

    pub(crate) async fn set_connect_packet(&mut self, connect_packet: ConnectPacket) {
        let mut session = self.shared.state.lock().await;
        session.connect_packet = connect_packet;
    }

    pub(crate) async fn get_client_id(&self) -> String {
        let session = self.shared.state.lock().await;
        session.connect_packet.payload.client_id.clone()
    }

    pub async fn begin(&mut self, connection: &mut Connection, resume: bool) -> Result<()> {
        let mut ack = ConnAckPacket::default();
        ack.flags.session_present = resume;

        {
            let mut session = self.shared.state.lock().await;

            if session.connect_packet.payload.client_id.is_empty() {
                let uuid = Uuid::new_v4();
                session.connect_packet.payload.client_id = uuid.hyphenated().to_string();
                ack.properties = Some(ConnAckProperties {
                    assigned_client_id: Some(AssignedClientIdentifier::new(
                        session.connect_packet.payload.client_id.clone(),
                    )),
                    ..Default::default()
                });
            }

            info!(
                "Client with id `{}` {} a session",
                session.connect_packet.payload.client_id,
                match resume {
                    true => "resumed",
                    false => "started",
                }
            );
        }

        connection.write_packet(ControlPacket::ConnAck(ack)).await?;

        Ok(())
    }

    async fn handle_publish(
        &mut self,
        packet: PublishPacket,
        broker: &Broker,
    ) -> Result<Option<ControlPacket>> {
        match packet.qos_level {
            QoS::AtMostOnce => Ok(None),
            QoS::AtLeastOnce => {
                if let Some(packet_id) = packet.packet_id {
                    Ok(ControlPacket::PubAck(PubAckPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    })
                    .into())
                } else {
                    Err(ReasonCode::ProtocolError.into())
                }
            }
            QoS::ExactlyOnce => {
                if let Some(packet_id) = packet.packet_id {
                    let mut session = self.shared.state.lock().await;
                    session.unacknowledged_messages.push(packet.clone());
                    Ok(ControlPacket::PubRec(PubRecPacket {
                        packet_id,
                        reason: ReasonCode::Success,
                        properties: None,
                    })
                    .into())
                } else {
                    Err(ReasonCode::ProtocolError.into())
                }
            }
            QoS::Invalid => Err(ReasonCode::ProtocolError.into()),
        }
        .and_then(|res| {
            let topic = packet.topic_name.clone();
            let message = Message {
                packet_id: packet.packet_id,
                topic: packet.topic_name,
                dup: packet.dup,
                retain: packet.retain,
                qos: packet.qos_level,
                payload: packet.payload,
            };

            broker.publish(&topic, message)?;

            Ok(res)
        })
    }

    async fn handle_puback(&mut self, packet: PubAckPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        if let Some(index) = session
            .unacknowledged_messages
            .iter()
            .position(|p| p.packet_id == Some(packet.packet_id))
        {
            session.unacknowledged_messages.remove(index);
        }

        Ok(None)
    }

    async fn handle_pubrec(&mut self, packet: PubRecPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        if let Some(index) = session
            .unacknowledged_messages
            .iter()
            .position(|p| p.packet_id == Some(packet.packet_id))
        {
            session.unacknowledged_messages.remove(index);
        }

        let packet_id = packet.packet_id;
        session.pubrecs.push(packet);

        Ok(ControlPacket::PubRel(PubRelPacket {
            packet_id,
            reason: ReasonCode::Success,
            properties: None,
        })
        .into())
    }

    async fn handle_pubcomp(&mut self, packet: PubCompPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        if let Some(index) = session
            .pubrecs
            .iter()
            .position(|p| p.packet_id == packet.packet_id)
        {
            session.pubrecs.remove(index);
        }

        Ok(None)
    }

    async fn handle_pubrel(&mut self, packet: PubRelPacket) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        if let Some(index) = session
            .pubrecs
            .iter()
            .position(|p| p.packet_id == packet.packet_id)
        {
            session.pubrecs.remove(index);
        }

        Ok(ControlPacket::PubComp(PubCompPacket {
            packet_id: packet.packet_id,
            reason: ReasonCode::Success,
            properties: None,
        })
        .into())
    }

    async fn handle_subscribe(
        &mut self,
        packet: mercurio_packets::subscribe::SubscribePacket,
        broker: &Broker,
    ) -> Result<Option<ControlPacket>> {
        let mut session = self.shared.state.lock().await;
        let mut ack = SubAckPacket {
            packet_id: packet.packet_id,
            properties: None,
            payload: Vec::new(),
        };

        for sub in &packet.payload {
            let mut rx = broker.subscribe(sub.topic_filter.to_string());
            ack.payload.push(SubAckPayload {
                reason_code: ReasonCode::GrantedQoS0,
            });

            let rx = Box::pin(async_stream::stream! {
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
                .insert(sub.topic_filter.to_string(), rx);
        }

        Ok(ControlPacket::SubAck(ack).into())
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
            ControlPacket::Unsubscribe(_) => todo!(),
            ControlPacket::PingReq(_) => Ok(ControlPacket::PingResp(PingRespPacket {}).into()),
            ControlPacket::Disconnect(packet) => Ok(ControlPacket::Disconnect(packet).into()),
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

                match message.qos {
                    mercurio_core::qos::QoS::AtMostOnce => {}
                    mercurio_core::qos::QoS::AtLeastOnce | mercurio_core::qos::QoS::ExactlyOnce => {
                        session.unacknowledged_messages.push(publish.clone());
                    }
                    mercurio_core::qos::QoS::Invalid => unreachable!(),
                };

                Some(ControlPacket::Publish(publish))
            }
            None => None,
        }
    }
}
