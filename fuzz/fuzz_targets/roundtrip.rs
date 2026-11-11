//! Every packet the encoder can write for valid field values must decode
//! back to the same packet, in each protocol version.
//!
//! Generation stays inside what the encoder documents as valid and in the
//! canonical form the decoder returns: a property block that sets nothing
//! is `None`, reason code 0x00 is named as the packet names it, and fields
//! a version cannot carry are left at their defaults. Values the encoder
//! writes but the decoder refuses (packet identifier 0, a reason code the
//! packet does not list, an empty SUBSCRIBE, a NUL in a string, ...) are
//! left out on purpose: the encoder can't refuse them yet.
#![no_main]

use arbitrary::Arbitrary;
use bytes::{Bytes, BytesMut};
use libfuzzer_sys::fuzz_target;
use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    protocol::{PacketType, ProtocolVersion},
    qos::QoS,
    reason::ReasonCode,
};
use mercurio_packets::{
    auth::{AuthPacket, AuthProperties},
    connack::{ConnAckFlags, ConnAckPacket, ConnAckProperties},
    connect::{ConnectFlags, ConnectPacket, ConnectPayload, ConnectProperties, WillProperties},
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
    user: Vec<(String, String)>,
}

#[derive(Arbitrary, Debug)]
struct Auth {
    method: String,
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
        request_problem_information: Option<bool>,
        auth: Option<Auth>,
        user: Vec<(String, String)>,
    },
    ConnAck {
        session_present: bool,
        reason: u8,
        maximum_qos: Option<bool>,
        assigned_client_id: Option<String>,
        server_keepalive: Option<u16>,
        auth: Option<Auth>,
        common: Common,
    },
    Publish {
        dup: bool,
        qos: u8,
        retain: bool,
        topic: String,
        packet_id: u16,
        payload: Vec<u8>,
        payload_format: Option<bool>,
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
        first: (String, u8),
        rest: Vec<(String, u8)>,
        subscription_id: Option<u32>,
        user: Vec<(String, String)>,
    },
    SubAck {
        packet_id: u16,
        first: u8,
        rest: Vec<u8>,
        common: Common,
    },
    Unsubscribe {
        packet_id: u16,
        first: String,
        rest: Vec<String>,
        user: Vec<(String, String)>,
    },
    UnsubAck {
        packet_id: u16,
        first: u8,
        rest: Vec<u8>,
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
        auth: Option<Auth>,
        common: Common,
    },
}

/// MQTT strings never hold U+0000 [MQTT-1.5.4-2].
fn text(s: String) -> String {
    s.replace('\0', "")
}

/// A topic name: no wildcards, never empty.
fn topic(s: String) -> String {
    let s = s.replace(['\0', '+', '#'], "");
    if s.is_empty() {
        "t".into()
    } else {
        s
    }
}

fn packet_id(id: u16) -> u16 {
    id.max(1)
}

fn qos(n: u8) -> QoS {
    [QoS::AtMostOnce, QoS::AtLeastOnce, QoS::ExactlyOnce][usize::from(n % 3)]
}

/// One of the reason codes `packet_type` lists, picked by `n`.
fn reason(n: u8, packet_type: PacketType) -> ReasonCode {
    let valid: Vec<ReasonCode> = (0..=u8::MAX)
        .filter_map(|byte| ReasonCode::decode(&mut &[byte][..]).ok())
        .filter(|code| code.is_valid_for(packet_type))
        .collect();
    valid[usize::from(n) % valid.len()]
}

/// A property block that sets nothing is the same as none.
fn present<P: Default + PartialEq>(properties: P) -> Option<P> {
    (properties != P::default()).then_some(properties)
}

fn user_properties(user: Vec<(String, String)>) -> Option<Vec<UserProperty>> {
    let user: Vec<_> = user
        .into_iter()
        .map(|(key, value)| UserProperty::new(text(key), text(value)))
        .collect();
    (!user.is_empty()).then_some(user)
}

fn auth_properties(
    auth: Option<Auth>,
) -> (Option<AuthenticationMethod>, Option<AuthenticationData>) {
    match auth {
        Some(auth) => (
            Some(AuthenticationMethod::new(text(auth.method))),
            auth.data
                .map(|data| AuthenticationData::new(Bytes::from(data))),
        ),
        None => (None, None),
    }
}

/// Reason String and User Properties, the properties of the acknowledgements.
fn common(common: Common) -> (Option<ReasonString>, Option<Vec<UserProperty>>) {
    (
        common.reason_string.map(|s| ReasonString::new(text(s))),
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
            auth,
            user,
        } => {
            let (authentication_method, authentication_data) = auth_properties(auth);
            let properties = v5
                .then(|| {
                    present(ConnectProperties {
                        session_expiry_interval: session_expiry.map(SessionExpiryInterval::new),
                        receive_maximum: receive_maximum.map(|n| ReceiveMaximum::new(n.max(1))),
                        maximum_packet_size: maximum_packet_size
                            .map(|n| MaximumPacketSize::new(n.max(1))),
                        request_problem_information: request_problem_information
                            .map(|b| RequestProblemInformation::new(b as u8)),
                        user_property: user_properties(user),
                        authentication_method,
                        authentication_data,
                        ..Default::default()
                    })
                })
                .flatten();

            // MQTT 3.x has no password without a user name [MQTT-3.1.2-22]
            let password = password.filter(|_| v5 || user_name.is_some());
            let mut flags = ConnectFlags {
                user_name: user_name.is_some(),
                password: password.is_some(),
                clean_start,
                ..Default::default()
            };
            let mut payload = ConnectPayload {
                client_id: text(client_id),
                user_name: user_name.map(text),
                password: password.map(Bytes::from),
                ..Default::default()
            };
            if let Some(will) = will {
                flags.will_flag = true;
                flags.will_retain = will.retain;
                flags.will_qos = qos(will.qos);
                payload.will_topic = Some(topic(will.topic));
                payload.will_payload = Some(Bytes::from(will.payload));
                if v5 {
                    payload.will_properties = present(WillProperties {
                        will_delay_interval: will.delay.map(WillDelayInterval::new),
                        content_type: will.content_type.map(|s| ContentType::new(text(s))),
                        user_property: user_properties(will.user),
                        ..Default::default()
                    });
                }
            }

            ControlPacket::Connect(ConnectPacket {
                protocol_version: version,
                flags,
                keepalive,
                properties,
                payload,
            })
        }
        Packet::ConnAck {
            session_present,
            reason: code,
            maximum_qos,
            assigned_client_id,
            server_keepalive,
            auth,
            common: c,
        } => {
            let reason_code = if v5 {
                reason(code, PacketType::ConnAck)
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
                        maximum_qos: maximum_qos.map(|b| MaximumQoS::new(b as u8)),
                        assigned_client_id: assigned_client_id
                            .map(|s| AssignedClientIdentifier::new(text(s))),
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
                protocol_version: version,
                flags: ConnAckFlags { session_present },
                reason_code,
                properties,
            })
        }
        Packet::Publish {
            dup,
            qos: q,
            retain,
            topic: name,
            packet_id: id,
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
            let qos_level = qos(q);
            let at_most_once = qos_level == QoS::AtMostOnce;
            // The MQTT 5 decoder always returns the property block
            let properties = v5.then(|| PublishProperties {
                payload_format_indicator: payload_format
                    .map(|b| PayloadFormatIndicator::new(b as u8)),
                message_expiry_interval: message_expiry.map(MessageExpiryInterval::new),
                topic_alias: topic_alias.map(|n| TopicAlias::new(n.max(1))),
                response_topic: response_topic.map(|s| ResponseTopic::new(topic(s))),
                correlation_data: correlation_data.map(|d| CorrelationData::new(Bytes::from(d))),
                user_property: user_properties(user),
                subscription_identifiers: subscription_identifiers
                    .into_iter()
                    .map(|n| {
                        SubscriptionIdentifier::new(VariableByteInteger((n % 268_435_455) + 1))
                    })
                    .collect(),
                content_type: content_type.map(|s| ContentType::new(text(s))),
            });

            ControlPacket::Publish(PublishPacket {
                dup: dup && !at_most_once,
                qos_level,
                retain,
                topic_name: topic(name),
                packet_id: (!at_most_once).then(|| packet_id(id)),
                properties,
                payload: Some(Bytes::from(payload)),
            })
        }
        Packet::Ack {
            kind,
            packet_id: id,
            reason: code,
            common: c,
        } => {
            let packet_id = packet_id(id);
            let packet_type = [
                PacketType::PubAck,
                PacketType::PubRec,
                PacketType::PubRel,
                PacketType::PubComp,
            ][usize::from(kind) % 4];
            // MQTT 3.x acknowledgements carry only the packet id
            let (reason, (reason_string, user_property)) = if v5 {
                (reason(code, packet_type), common(c))
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
            packet_id: id,
            first,
            rest,
            subscription_id,
            user,
        } => {
            let payload = std::iter::once(first)
                .chain(rest)
                .map(|(filter, options)| SubscribePayload {
                    topic_filter: text(filter),
                    subs_opt: if v5 {
                        SubscriptionOptions {
                            qos: qos(options),
                            no_local: options & 0b0000_0100 != 0,
                            retain_as_pub: options & 0b0000_1000 != 0,
                            retain_handling: [
                                RetainHandling::SendRetained,
                                RetainHandling::SendRetainedIfNonExisting,
                                RetainHandling::DoNotSendRetained,
                            ][usize::from((options >> 4) % 3)],
                        }
                    } else {
                        // MQTT 3.x has only the QoS
                        SubscriptionOptions {
                            qos: qos(options),
                            ..Default::default()
                        }
                    },
                })
                .collect();
            let properties = v5
                .then(|| {
                    present(SubscribeProperties {
                        subscription_id: subscription_id.map(|n| {
                            SubscriptionIdentifier::new(VariableByteInteger((n % 268_435_455) + 1))
                        }),
                        user_property: user_properties(user),
                    })
                })
                .flatten();

            ControlPacket::Subscribe(SubscribePacket {
                packet_id: packet_id(id),
                properties,
                payload,
            })
        }
        Packet::SubAck {
            packet_id: id,
            first,
            rest,
            common: c,
        } => {
            let payload = std::iter::once(first)
                .chain(rest)
                .map(|code| {
                    let reason_code = if v5 {
                        match reason(code, PacketType::SubAck) {
                            // 0x00 is Granted QoS 0 in SUBACK
                            ReasonCode::Success => ReasonCode::GrantedQoS0,
                            code => code,
                        }
                    } else {
                        // MQTT 3.x has one failure code, read back as this
                        [
                            ReasonCode::GrantedQoS0,
                            ReasonCode::GrantedQoS1,
                            ReasonCode::GrantedQoS2,
                            ReasonCode::UnspecifiedError,
                        ][usize::from(code) % 4]
                    };
                    SubAckPayload { reason_code }
                })
                .collect();
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
                protocol_version: version,
                packet_id: packet_id(id),
                properties,
                payload,
            })
        }
        Packet::Unsubscribe {
            packet_id: id,
            first,
            rest,
            user,
        } => {
            let payload = std::iter::once(first)
                .chain(rest)
                .map(|filter| UnsubscribePayload {
                    topic_filter: text(filter),
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
                packet_id: packet_id(id),
                properties,
                payload,
            })
        }
        Packet::UnsubAck {
            packet_id: id,
            first,
            rest,
            common: c,
        } => {
            // A 3.x UNSUBACK is only the packet id
            let payload = if v5 {
                std::iter::once(first)
                    .chain(rest)
                    .map(|code| UnsubAckPayload {
                        reason_code: reason(code, PacketType::UnsubAck),
                    })
                    .collect()
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
                protocol_version: version,
                packet_id: packet_id(id),
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
            let reason = match reason(code, PacketType::Disconnect) {
                // 0x00 is Normal disconnection in DISCONNECT
                ReasonCode::Success => ReasonCode::NormalDisconnection,
                code => code,
            };
            let (reason_string, user_property) = common(c);
            ControlPacket::Disconnect(DisconnectPacket {
                reason,
                properties: present(DisconnectProperties {
                    session_expiry_interval: session_expiry.map(SessionExpiryInterval::new),
                    reason_string,
                    user_property,
                    server_reference: server_reference.map(|s| ServerReference::new(text(s))),
                }),
            })
        }
        Packet::Auth {
            reason: code,
            auth,
            common: c,
        } => {
            // AUTH only exists in MQTT 5
            if !v5 {
                return None;
            }
            let (auth_method, auth_data) = auth_properties(auth);
            let (reason_string, user_property) = common(c);
            ControlPacket::Auth(AuthPacket {
                reason: reason(code, PacketType::Auth),
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
    packet.encode_with_version(&mut encoded, version);
    if version == ProtocolVersion::V5 {
        assert_eq!(encoded.len(), packet.encoded_size(), "{packet:?}");
    }

    let decoded = ControlPacket::parse_with_version(&mut encoded, version)
        .unwrap_or_else(|e| panic!("{packet:?} encodes to a packet that fails with {e:?}"));
    assert_eq!(decoded, packet);
    assert!(encoded.is_empty(), "encoding wrote more than one packet");
});
