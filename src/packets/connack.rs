use crate::control_packet::*;
use crate::endec::*;
use crate::properties::*;
use crate::reason::ReasonCode;
use bytes::Buf;
use bytes::BufMut;
use std::mem;

#[derive(Default, Debug, PartialEq)]
pub struct ConnAckFlags {
    session_present: bool,
}

impl Encoder for ConnAckFlags {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let flags = 0b0000_0001 & (self.session_present as u8);
        buffer.put_u8(flags);
    }

    fn get_encoded_size(&self) -> usize {
        mem::size_of::<u8>()
    }
}

impl Decoder for ConnAckFlags {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        let encoded = buffer.get_u8();

        if (0b1111_1110 & encoded) != 0 {
            return Err(ReasonCode::MalformedPacket);
        }

        Ok(Some(ConnAckFlags {
            session_present: (0b0000_0001 & encoded) != 0,
        }))
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct ConnAckProperties {
    pub session_expiry_interval: Option<SessionExpiryInterval>,
    pub receive_maximum: Option<ReceiveMaximum>,
    pub maximum_qos: Option<MaximumQoS>,
    pub retain_available: Option<RetainAvailable>,
    pub maximum_packet_size: Option<MaximumPacketSize>,
    pub assigned_client_id: Option<AssignedClientIdentifier>,
    pub topic_alias_max: Option<TopicAliasMaximum>,
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
    pub wildcard_subscription_available: Option<WildcardSubscriptionAvailable>,
    pub subscription_identifier_available: Option<SubscriptionIdentifierAvailable>,
    pub shared_subscription_available: Option<SharedSubscriptionAvailable>,
    pub server_keepalive: Option<ServerKeepAlive>,
    pub response_information: Option<ResponseInformation>,
    pub server_reference: Option<ServerReference>,
    pub authentication_method: Option<AuthenticationMethod>,
    pub authentication_data: Option<AuthenticationData>,
}

impl Encoder for ConnAckProperties {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.session_expiry_interval.encode(buffer);
        self.receive_maximum.encode(buffer);
        self.maximum_qos.encode(buffer);
        self.retain_available.encode(buffer);
        self.maximum_packet_size.encode(buffer);
        self.assigned_client_id.encode(buffer);
        self.topic_alias_max.encode(buffer);
        self.reason_string.encode(buffer);

        if let Some(props) = &self.user_property {
            for property in props {
                property.encode(buffer);
            }
        }

        self.wildcard_subscription_available.encode(buffer);
        self.subscription_identifier_available.encode(buffer);
        self.shared_subscription_available.encode(buffer);
        self.server_keepalive.encode(buffer);
        self.response_information.encode(buffer);
        self.server_reference.encode(buffer);
        self.authentication_method.encode(buffer);
        self.authentication_data.encode(buffer);
    }

    fn get_encoded_size(&self) -> usize {
        let mut len: usize = 0;

        len += self.session_expiry_interval.get_encoded_size();
        len += self.receive_maximum.get_encoded_size();
        len += self.maximum_qos.get_encoded_size();
        len += self.retain_available.get_encoded_size();
        len += self.maximum_packet_size.get_encoded_size();
        len += self.assigned_client_id.get_encoded_size();
        len += self.topic_alias_max.get_encoded_size();
        len += self.reason_string.get_encoded_size();

        if let Some(props) = &self.user_property {
            for property in props {
                len += property.get_encoded_size();
            }
        }

        len += self.wildcard_subscription_available.get_encoded_size();
        len += self.subscription_identifier_available.get_encoded_size();
        len += self.shared_subscription_available.get_encoded_size();
        len += self.server_keepalive.get_encoded_size();
        len += self.response_information.get_encoded_size();
        len += self.server_reference.get_encoded_size();
        len += self.authentication_method.get_encoded_size();
        len += self.authentication_data.get_encoded_size();

        len
    }
}

impl Decoder for ConnAckProperties {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        let len = VariableByteInteger::decode(buffer)?.unwrap();
        if len.0 == 0 {
            return Ok(None);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);
        let mut connack_properties = ConnAckProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties)?.unwrap();

            match p {
                Property::SessionExpiryInterval => {
                    connack_properties.session_expiry_interval =
                        SessionExpiryInterval::decode(&mut encoded_properties)?
                }

                Property::ReceiveMaximum => {
                    connack_properties.receive_maximum =
                        ReceiveMaximum::decode(&mut encoded_properties)?
                }

                Property::MaximumQoS => {
                    connack_properties.maximum_qos = MaximumQoS::decode(&mut encoded_properties)?
                }

                Property::RetainAvailable => {
                    connack_properties.retain_available =
                        RetainAvailable::decode(&mut encoded_properties)?
                }

                Property::MaximumPacketSize => {
                    connack_properties.maximum_packet_size =
                        MaximumPacketSize::decode(&mut encoded_properties)?
                }

                Property::AssignedClientIdentifier => {
                    connack_properties.assigned_client_id =
                        AssignedClientIdentifier::decode(&mut encoded_properties)?
                }

                Property::TopicAliasMaximum => {
                    connack_properties.topic_alias_max =
                        TopicAliasMaximum::decode(&mut encoded_properties)?
                }

                Property::ReasonString => {
                    connack_properties.reason_string =
                        ReasonString::decode(&mut encoded_properties)?
                }

                Property::UserProperty => {
                    let user_property = UserProperty::decode(&mut encoded_properties)?.unwrap();

                    if let Some(v) = &mut connack_properties.user_property {
                        v.push(user_property);
                    } else {
                        let v = vec![user_property];
                        connack_properties.user_property = Some(v);
                    }
                }

                Property::WildcardSubscriptionAvailable => {
                    connack_properties.wildcard_subscription_available =
                        WildcardSubscriptionAvailable::decode(&mut encoded_properties)?
                }

                Property::SubscriptionIdentifierAvailable => {
                    connack_properties.subscription_identifier_available =
                        SubscriptionIdentifierAvailable::decode(&mut encoded_properties)?
                }

                Property::SharedSubscriptionAvailable => {
                    connack_properties.shared_subscription_available =
                        SharedSubscriptionAvailable::decode(&mut encoded_properties)?
                }

                Property::ServerKeepAlive => {
                    connack_properties.server_keepalive =
                        ServerKeepAlive::decode(&mut encoded_properties)?
                }

                Property::ResponseInformation => {
                    connack_properties.response_information =
                        ResponseInformation::decode(&mut encoded_properties)?
                }

                Property::ServerReference => {
                    connack_properties.server_reference =
                        ServerReference::decode(&mut encoded_properties)?
                }

                Property::AuthenticationMethod => {
                    connack_properties.authentication_method =
                        AuthenticationMethod::decode(&mut encoded_properties)?
                }

                Property::AuthenticationData => {
                    connack_properties.authentication_data =
                        AuthenticationData::decode(&mut encoded_properties)?
                }

                _ => return Err(ReasonCode::MalformedPacket),
            }

            if !encoded_properties.has_remaining() {
                break;
            }
        }

        Ok(Some(connack_properties))
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct ConnAckPacket {
    flags: ConnAckFlags,
    reason_code: ReasonCode,
    properties: Option<ConnAckProperties>,
}

impl Encoder for ConnAckPacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        buffer.put_u8((Self::PACKET_TYPE as u8) << 4);
        remaining_len += self.flags.get_encoded_size();
        remaining_len += self.reason_code.get_encoded_size();
        remaining_len +=
            VariableByteInteger(self.properties.get_encoded_size() as u32).get_encoded_size();
        remaining_len += self.properties.get_encoded_size();
        VariableByteInteger(remaining_len as u32).encode(buffer);

        // Variable header
        self.flags.encode(buffer);
        self.reason_code.encode(buffer);

        // Properties
        VariableByteInteger(self.properties.get_encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);
    }
}

impl Decoder for ConnAckPacket {
    fn decode<T: Buf>(buffer: &mut T) -> Result<Option<Self>, ReasonCode> {
        buffer.advance(1); // Packet type
        let _ = VariableByteInteger::decode(buffer); //Remaining length

        let flags = ConnAckFlags::decode(buffer)?.unwrap();
        let reason_code = ReasonCode::decode(buffer, &ControlPacketType::ConnAck)?.unwrap();
        let properties = ConnAckProperties::decode(buffer)?;

        Ok(Some(ConnAckPacket {
            flags,
            reason_code,
            properties,
        }))
    }
}

impl ControlPacket for ConnAckPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::ConnAck;
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::packets::connack::*;

    #[test]
    fn test_connack_packet_encode_decode() {
        let expected = vec![
            0x20, 0x45, 0x0, 0x0, 0x42, 0x25, 0x1, 0x27, 0x0, 0x10, 0x0, 0x0, 0x12, 0x0, 0x2f,
            0x4d, 0x7a, 0x41, 0x77, 0x4e, 0x7a, 0x45, 0x7a, 0x4e, 0x54, 0x55, 0x32, 0x4d, 0x6a,
            0x4d, 0x7a, 0x4d, 0x6a, 0x4d, 0x34, 0x4f, 0x44, 0x45, 0x32, 0x4d, 0x7a, 0x41, 0x77,
            0x4f, 0x54, 0x63, 0x7a, 0x4e, 0x54, 0x49, 0x34, 0x4e, 0x54, 0x41, 0x35, 0x4f, 0x54,
            0x63, 0x79, 0x4e, 0x44, 0x49, 0x22, 0xff, 0xff, 0x28, 0x1, 0x29, 0x1, 0x2a, 0x1,
        ];

        let flags = ConnAckFlags {
            session_present: false,
        };

        let reason_code = ReasonCode::Success;
        let properties = ConnAckProperties {
            assigned_client_id: AssignedClientIdentifier::new(String::from(
                "MzAwNzEzNTU2MjMzMjM4ODE2MzAwOTczNTI4NTA5OTcyNDI",
            ))
            .into(),
            maximum_packet_size: MaximumPacketSize::new(1048576).into(),
            retain_available: RetainAvailable::new(true).into(),
            shared_subscription_available: SharedSubscriptionAvailable::new(true).into(),
            subscription_identifier_available: SubscriptionIdentifierAvailable::new(true).into(),
            topic_alias_max: TopicAliasMaximum::new(65535).into(),
            wildcard_subscription_available: WildcardSubscriptionAvailable::new(true).into(),
            ..Default::default()
        };

        let packet = ConnAckPacket {
            flags,
            reason_code,
            properties: properties.into(),
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = ConnAckPacket::decode(&mut bytes)
            .expect("Unexpected error")
            .unwrap();

        assert_eq!(packet, new_packet);
    }
}
