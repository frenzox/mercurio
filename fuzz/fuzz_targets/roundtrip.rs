//! Encoding any packet either fails, leaving the buffer as it was, or
//! writes bytes that decode back to the same packet, in each protocol
//! version.
//!
//! Field values are left as generated, including those the receiver must
//! refuse (packet identifier 0, a reason code the packet does not list, an
//! empty SUBSCRIBE, a NUL in a string, ...): the encoder has to refuse
//! them itself. Generation only keeps to the canonical form the decoder
//! returns, where an encoding has two: a property block that sets nothing
//! is `None`. Fields a version cannot carry (the reason codes and
//! properties of MQTT 3.x) are left at their defaults, as encoding for that
//! version drops them by design.
#![no_main]

use arbitrary::Arbitrary;
use bytes::{Bytes, BytesMut};
use libfuzzer_sys::fuzz_target;
use mercurio_core::{
    properties::*,
    protocol::{PacketType, ProtocolVersion},
    qos::QoS,
    reason::ReasonCode,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connack::{ConnAckFlags, ConnAckPacket, ConnAckProperties},
    connect::{ConnectPacket, ConnectPayload, ConnectProperties, WillProperties},
    disconnect::{DisconnectPacket, DisconnectProperties},
    pingreq::PingReqPacket,
    pingresp::PingRespPacket,
    puback::{PubAckPacket, PubAckProperties},
    pubcomp::{PubCompPacket, PubCompProperties},
    publish::{PublishPacket, PublishProperties},
    pubrec::{PubRecPacket, PubRecProperties},
    pubrel::{PubRelPacket, PubRelProperties},
    suback::{SubAckPacket, SubAckPayload, SubAckProperties},
    subscribe::{
        RetainHandling, SubscribePacket, SubscribePayload, SubscribeProperties, SubscriptionOptions,
    },
    unsuback::{UnsubAckPacket, UnsubAckPayload, UnsubAckProperties},
    unsubscribe::{UnsubscribePacket, UnsubscribePayload, UnsubscribeProperties},
    ControlPacket,
};

#[derive(Arbitrary, Debug)]
enum Version {
    V3_1,
    V3_1_1,
    V5,
}

#[derive(Arbitrary, Debug)]
struct Common {
    reason_string: Option<String>,
    user: Vec<(String, String)>,
}

#[derive(Arbitrary, Debug)]
struct Will {
    retain: bool,
    qos: u8,
    topic: String,
    payload: Vec<u8>,
    delay: Option<u32>,
    content_type: Option<String>,
    response_topic: Option<String>,
    user: Vec<(String, String)>,
}

#[derive(Arbitrary, Debug)]
struct Auth {
    method: Option<String>,
    data: Option<Vec<u8>>,
}

#[derive(Arbitrary, Debug)]
enum Packet {
    Connect {
        clean_start: bool,
        keepalive: u16,
        client_id: String,
        will: Option<Will>,
        user_name: Option<String>,
        password: Option<Vec<u8>>,
        session_expiry: Option<u32>,
        receive_maximum: Option<u16>,
        maximum_packet_size: Option<u32>,
        request_problem_information: Option<u8>,
        request_response_information: Option<u8>,
        auth: Auth,
        user: Vec<(String, String)>,
    },
    ConnAck {
        session_present: bool,
        reason: u8,
        maximum_qos: Option<u8>,
        receive_maximum: Option<u16>,
        assigned_client_id: Option<String>,
        server_keepalive: Option<u16>,
        auth: Auth,
        common: Common,
    },
    Publish {
        dup: bool,
        qos: u8,
        retain: bool,
        topic: String,
        packet_id: Option<u16>,
        payload: Vec<u8>,
        payload_format: Option<u8>,
        message_expiry: Option<u32>,
        topic_alias: Option<u16>,
        response_topic: Option<String>,
        correlation_data: Option<Vec<u8>>,
        subscription_identifiers: Vec<u32>,
        content_type: Option<String>,
        user: Vec<(String, String)>,
    },
    Ack {
        kind: u8,
        packet_id: u16,
        reason: u8,
        common: Common,
    },
    Subscribe {
        packet_id: u16,
        filters: Vec<(String, u8)>,
        subscription_id: Option<u32>,
        user: Vec<(String, String)>,
    },
    SubAck {
        packet_id: u16,
        codes: Vec<u8>,
        common: Common,
    },
    Unsubscribe {
        packet_id: u16,
        filters: Vec<String>,
        user: Vec<(String, String)>,
    },
    UnsubAck {
        packet_id: u16,
        codes: Vec<u8>,
        common: Common,
    },
    PingReq,
    PingResp,
    Disconnect {
        reason: u8,
        session_expiry: Option<u32>,
        server_reference: Option<String>,
        common: Common,
    },
    Auth {
        reason: u8,
        auth: Auth,
        common: Common,
    },
}

/// Any QoS. The reserved value 3 can't be built, so it wraps.
fn qos(n: u8) -> QoS {
    [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce][usize::from(n % 3)]
}

/// The reason code of a byte, or `None` for a byte that encodes none, as
/// no `ReasonCode` can hold it.
fn reason(n: u8) -> Option<ReasonCode> {
    ReasonCode::try_from(n).ok()
}

/// A property block that sets nothing is the same as none.
fn present<P: Default + PartialEq>(properties: P) -> Option<P> {
    (properties != P::default()).then_some(properties)
}

fn user_properties(user: Vec<(String, String)>) -> Option<Vec<UserProperty>> {
    let user: Vec<_> = user
        .into_iter()
        .map(|(key, value)| UserProperty::new(key, value))
        .collect();
    (!user.is_empty()).then_some(user)
}

fn auth_properties(auth: Auth) -> (Option<AuthenticationMethod>, Option<AuthenticationData>) {
    (
        auth.method.map(AuthenticationMethod::new),
        auth.data
            .map(|data| AuthenticationData::new(Bytes::from(data))),
    )
}

/// Reason String and User Properties, the properties of the acknowledgements.
fn common(common: Common) -> (Option<ReasonString>, Option<Vec<UserProperty>>) {
    (
        common.reason_string.map(ReasonString::new),
        user_properties(common.user),
    )
}

fn build(packet: Packet, version: ProtocolVersion) -> Option<ControlPacket> {
    let v5 = version == ProtocolVersion::V5;

    let packet = match packet {
        Packet::Connect {
            clean_start,
            keepalive,
            client_id,
            will,
            user_name,
            password,
            session_expiry,
            receive_maximum,
            maximum_packet_size,
            request_problem_information,
            request_response_information,
            auth,
            user,
        } => {
            let (authentication_method, authentication_data) = auth_properties(auth);
            let properties = v5
                .then(|| {
                    present(ConnectProperties {
                        session_expiry_interval: session_expiry.map(SessionExpiryInterval::new),
                        receive_maximum: receive_maximum.map(ReceiveMaximum::new),
                        maximum_packet_size: maximum_packet_size.map(MaximumPacketSize::new),
                        request_problem_information: request_problem_information
                            .map(RequestProblemInformation::new),
                        request_response_information: request_response_information
                            .map(RequestResponseInformation::new),
                        user_property: user_properties(user),
                        authentication_method,
                        authentication_data,
                        ..Default::default()
                    })
                })
                .flatten();

            let payload = ConnectPayload {
                client_id,
                will: will.map(|will| mercurio_packets::connect::Will {
                    qos: qos(will.qos),
                    retain: will.retain,
                    properties: v5
                        .then(|| {
                            present(WillProperties {
                                will_delay_interval: will.delay.map(WillDelayInterval::new),
                                content_type: will.content_type.map(ContentType::new),
                                response_topic: will.response_topic.map(ResponseTopic::new),
                                user_property: user_properties(will.user),
                                ..Default::default()
                            })
                        })
                        .flatten(),
                    topic: will.topic,
                    payload: Bytes::from(will.payload),
                }),
                user_name,
                password: password.map(Bytes::from),
            };

            ControlPacket::Connect(ConnectPacket {
                protocol_version: version,
                clean_start,
                keepalive,
                properties,
                payload,
            })
        }
        Packet::ConnAck {
            session_present,
            reason: code,
            maximum_qos,
            receive_maximum,
            assigned_client_id,
            server_keepalive,
            auth,
            common: c,
        } => {
            let reason_code = if v5 {
                reason(code)?
            } else {
                // The codes MQTT 3.x CONNACK can carry (section 3.2.2.3)
                [
                    ReasonCode::Success,
                    ReasonCode::UnsupportedProtocolVersion,
                    ReasonCode::ClientIdentifierNotValid,
                    ReasonCode::ServerUnavailable,
                    ReasonCode::BadUserNameOrPassword,
                    ReasonCode::NotAuthorized,
                ][usize::from(code) % 6]
            };
            let (authentication_method, authentication_data) = auth_properties(auth);
            let (reason_string, user_property) = common(c);
            let properties = v5
                .then(|| {
                    present(ConnAckProperties {
                        maximum_qos: maximum_qos.map(MaximumQoS::new),
                        receive_maximum: receive_maximum.map(ReceiveMaximum::new),
                        assigned_client_id: assigned_client_id.map(AssignedClientIdentifier::new),
                        server_keepalive: server_keepalive.map(ServerKeepAlive::new),
                        authentication_method,
                        authentication_data,
                        reason_string,
                        user_property,
                        ..Default::default()
                    })
                })
                .flatten();

            ControlPacket::ConnAck(ConnAckPacket {
                flags: ConnAckFlags { session_present },
                reason_code,
                properties,
            })
        }
        Packet::Publish {
            dup,
            qos: q,
            retain,
            topic,
            packet_id,
            payload,
            payload_format,
            message_expiry,
            topic_alias,
            response_topic,
            correlation_data,
            subscription_identifiers,
            content_type,
            user,
        } => {
            let properties = v5
                .then(|| {
                    present(PublishProperties {
                        payload_format_indicator: payload_format.map(PayloadFormatIndicator::new),
                        message_expiry_interval: message_expiry.map(MessageExpiryInterval::new),
                        topic_alias: topic_alias.map(TopicAlias::new),
                        response_topic: response_topic.map(ResponseTopic::new),
                        correlation_data: correlation_data
                            .map(|d| CorrelationData::new(Bytes::from(d))),
                        user_property: user_properties(user),
                        // An identifier out of range can't be built
                        subscription_identifiers: subscription_identifiers
                            .into_iter()
                            .map(SubscriptionIdentifier::new)
                            .collect::<Option<_>>()?,
                        content_type: content_type.map(ContentType::new),
                    })
                })
                .flatten();

            ControlPacket::Publish(PublishPacket {
                dup,
                qos_level: qos(q),
                retain,
                topic_name: topic,
                packet_id,
                properties,
                // The decoder reads a missing payload as an empty one
                payload: Some(Bytes::from(payload)),
            })
        }
        Packet::Ack {
            kind,
            packet_id,
            reason: code,
            common: c,
        } => {
            let packet_type = [
                PacketType::PubAck,
                PacketType::PubRec,
                PacketType::PubRel,
                PacketType::PubComp,
            ][usize::from(kind) % 4];
            // MQTT 3.x acknowledgements carry only the packet id
            let (reason, (reason_string, user_property)) = if v5 {
                (reason(code)?, common(c))
            } else {
                (ReasonCode::Success, (None, None))
            };
            match packet_type {
                PacketType::PubAck => ControlPacket::PubAck(PubAckPacket {
                    packet_id,
                    reason,
                    properties: present(PubAckProperties {
                        reason_string,
                        user_property,
                    }),
                }),
                PacketType::PubRec => ControlPacket::PubRec(PubRecPacket {
                    packet_id,
                    reason,
                    properties: present(PubRecProperties {
                        reason_string,
                        user_property,
                    }),
                }),
                PacketType::PubRel => ControlPacket::PubRel(PubRelPacket {
                    packet_id,
                    reason,
                    properties: present(PubRelProperties {
                        reason_string,
                        user_property,
                    }),
                }),
                _ => ControlPacket::PubComp(PubCompPacket {
                    packet_id,
                    reason,
                    properties: present(PubCompProperties {
                        reason_string,
                        user_property,
                    }),
                }),
            }
        }
        Packet::Subscribe {
            packet_id,
            filters,
            subscription_id,
            user,
        } => {
            let payload = filters
                .into_iter()
                .map(|(filter, options)| {
                    let retain_handling = [
                        RetainHandling::SendRetained,
                        RetainHandling::SendRetainedIfNonExisting,
                        RetainHandling::DoNotSendRetained,
                    ][usize::from((options >> 4) % 3)];
                    SubscribePayload {
                        topic_filter: filter,
                        subs_opt: if v5 {
                            SubscriptionOptions {
                                qos: qos(options),
                                no_local: options & 0b0000_0100 != 0,
                                retain_as_pub: options & 0b0000_1000 != 0,
                                retain_handling,
                            }
                        } else {
                            // MQTT 3.x has only the QoS
                            SubscriptionOptions {
                                qos: qos(options),
                                ..Default::default()
                            }
                        },
                    }
                })
                .collect();
            let subscription_id = match subscription_id {
                Some(n) => Some(SubscriptionIdentifier::new(n)?),
                None => None,
            };
            let properties = v5
                .then(|| {
                    present(SubscribeProperties {
                        subscription_id,
                        user_property: user_properties(user),
                    })
                })
                .flatten();

            ControlPacket::Subscribe(SubscribePacket {
                packet_id,
                properties,
                payload,
            })
        }
        Packet::SubAck {
            packet_id,
            codes,
            common: c,
        } => {
            let payload = codes
                .into_iter()
                .map(|code| {
                    let reason_code = if v5 {
                        reason(code)?
                    } else {
                        // MQTT 3.x has one failure code, read back as this
                        [
                            ReasonCode::GrantedQoS0,
                            ReasonCode::GrantedQoS1,
                            ReasonCode::GrantedQoS2,
                            ReasonCode::UnspecifiedError,
                        ][usize::from(code) % 4]
                    };
                    Some(SubAckPayload { reason_code })
                })
                .collect::<Option<_>>()?;
            let (reason_string, user_property) = common(c);
            let properties = v5
                .then(|| {
                    present(SubAckProperties {
                        reason_string,
                        user_property,
                    })
                })
                .flatten();

            ControlPacket::SubAck(SubAckPacket {
                packet_id,
                properties,
                payload,
            })
        }
        Packet::Unsubscribe {
            packet_id,
            filters,
            user,
        } => {
            let payload = filters
                .into_iter()
                .map(|filter| UnsubscribePayload {
                    topic_filter: filter,
                })
                .collect();
            let properties = v5
                .then(|| {
                    present(UnsubscribeProperties {
                        user_property: user_properties(user),
                    })
                })
                .flatten();

            ControlPacket::Unsubscribe(UnsubscribePacket {
                packet_id,
                properties,
                payload,
            })
        }
        Packet::UnsubAck {
            packet_id,
            codes,
            common: c,
        } => {
            // A 3.x UNSUBACK is only the packet id
            let payload = if v5 {
                codes
                    .into_iter()
                    .map(|code| {
                        Some(UnsubAckPayload {
                            reason_code: reason(code)?,
                        })
                    })
                    .collect::<Option<_>>()?
            } else {
                Vec::new()
            };
            let (reason_string, user_property) = common(c);
            let properties = v5
                .then(|| {
                    present(UnsubAckProperties {
                        reason_string,
                        user_property,
                    })
                })
                .flatten();

            ControlPacket::UnsubAck(UnsubAckPacket {
                packet_id,
                properties,
                payload,
            })
        }
        Packet::PingReq => ControlPacket::PingReq(PingReqPacket {}),
        Packet::PingResp => ControlPacket::PingResp(PingRespPacket {}),
        Packet::Disconnect {
            reason: code,
            session_expiry,
            server_reference,
            common: c,
        } => {
            // A 3.x DISCONNECT has no variable header
            if !v5 {
                return Some(ControlPacket::Disconnect(DisconnectPacket {
                    reason: ReasonCode::NormalDisconnection,
                    properties: None,
                }));
            }
            let (reason_string, user_property) = common(c);
            ControlPacket::Disconnect(DisconnectPacket {
                reason: reason(code)?,
                properties: present(DisconnectProperties {
                    session_expiry_interval: session_expiry.map(SessionExpiryInterval::new),
                    reason_string,
                    user_property,
                    server_reference: server_reference.map(ServerReference::new),
                }),
            })
        }
        Packet::Auth {
            reason: code,
            auth,
            common: c,
        } => {
            let (auth_method, auth_data) = auth_properties(auth);
            let (reason_string, user_property) = common(c);
            ControlPacket::Auth(AuthPacket {
                reason: reason(code)?,
                properties: AuthProperties {
                    auth_method,
                    auth_data,
                    reason_string,
                    user_property,
                },
            })
        }
    };

    Some(packet)
}

fuzz_target!(|input: (Version, Packet)| {
    let (version, packet) = input;
    let version = match version {
        Version::V3_1 => ProtocolVersion::V3_1,
        Version::V3_1_1 => ProtocolVersion::V3_1_1,
        Version::V5 => ProtocolVersion::V5,
    };
    let Some(packet) = build(packet, version) else {
        return;
    };

    let mut encoded = BytesMut::new();
    if packet.encode(&mut encoded, version).is_err() {
        assert!(encoded.is_empty(), "a refused {packet:?} left bytes behind");
        return;
    }
    assert_eq!(encoded.len(), packet.encoded_size(version), "{packet:?}");

    let decoded = ControlPacket::parse(&mut encoded, version)
        .unwrap_or_else(|e| panic!("{packet:?} encodes to a packet that fails with {e:?}"));
    assert_eq!(decoded, packet);
    assert!(encoded.is_empty(), "encoding wrote more than one packet");
});
