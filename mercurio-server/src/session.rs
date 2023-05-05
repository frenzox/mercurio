use std::{pin::Pin, sync::Arc};

use rand::Rng;
use tokio::sync::{broadcast, Mutex};
use tokio_stream::{Stream, StreamExt, StreamMap};
use tracing::info;

type Messages = Pin<Box<dyn Stream<Item = Message> + Send>>;

use mercurio_core::{
    message::Message, properties::AssignedClientIdentifier, qos::QoS, reason::ReasonCode, Result,
};
use mercurio_packets::{
    connack::{ConnAckPacket, ConnAckProperties},
    connect::ConnectPacket,
    pingresp::PingRespPacket,
    puback::PubAckPacket,
    publish::PublishPacket,
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
    connect_packet: ConnectPacket,
    subscriptions: StreamMap<String, Messages>,
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
                }),
            }),
        }
    }

    pub(crate) async fn set_connect_packet(&mut self, connect_packet: ConnectPacket) {
        let mut session = self.shared.state.lock().await;
        session.connect_packet = connect_packet;
    }

    pub async fn begin(&mut self, connection: &mut Connection, resume: bool) -> Result<()> {
        let mut ack = ConnAckPacket::default();
        ack.flags.session_present = resume;

        {
            let mut session = self.shared.state.lock().await;

            if session.connect_packet.payload.client_id.is_empty() {
                let random_bytes: [u8; 16] = rand::thread_rng().gen();
                session.connect_packet.payload.client_id = uuid::Builder::from_bytes(random_bytes)
                    .set_variant(uuid::Variant::RFC4122)
                    .set_version(uuid::Version::Random)
                    .build()
                    .to_hyphenated()
                    .to_string();

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

    pub(crate) async fn process_incoming(
        &mut self,
        packet: ControlPacket,
        broker: &Broker,
    ) -> Result<Option<ControlPacket>> {
        match packet {
            ControlPacket::Connect(_) => Err(ReasonCode::ProtocolError.into()),
            ControlPacket::ConnAck(_) => Err(ReasonCode::ProtocolError.into()),
            ControlPacket::Publish(p) => {
                let topic = p.topic_name.clone();

                let message = Message {
                    topic: p.topic_name,
                    dup: p.dup,
                    retain: p.retain,
                    qos: p.qos_level,
                    payload: p.payload,
                };

                broker.publish(&topic, message)?;

                if p.qos_level == QoS::AtMostOnce {
                    return Ok(None);
                }

                Ok(ControlPacket::PubAck(PubAckPacket::default()).into())
            }
            ControlPacket::PubAck(_) => todo!(),
            ControlPacket::PubRec(_) => todo!(),
            ControlPacket::PubRel(_) => todo!(),
            ControlPacket::PubComp(_) => todo!(),
            ControlPacket::Subscribe(p) => {
                let mut session = self.shared.state.lock().await;
                let mut ack = SubAckPacket {
                    packet_id: p.packet_id,
                    properties: None,
                    payload: Vec::new(),
                };

                for sub in &p.payload {
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
            ControlPacket::SubAck(_) => todo!(),
            ControlPacket::Unsubscribe(_) => todo!(),
            ControlPacket::UnsubAck(_) => todo!(),
            ControlPacket::PingReq(_) => Ok(ControlPacket::PingResp(PingRespPacket {}).into()),
            ControlPacket::PingResp(_) => todo!(),
            ControlPacket::Disconnect(p) => Ok(ControlPacket::Disconnect(p).into()),
            ControlPacket::Auth(_) => todo!(),
        }
    }

    pub(crate) async fn process_outgoing(&mut self) -> Option<ControlPacket> {
        let mut session = self.shared.state.lock().await;

        match session.subscriptions.next().await {
            Some((topic, message)) => ControlPacket::Publish(PublishPacket {
                dup: message.dup,
                qos_level: message.qos,
                retain: false,
                topic_name: topic,
                packet_id: 0.into(),
                properties: None,
                payload: message.payload,
            })
            .into(),
            None => None,
        }
    }
}
