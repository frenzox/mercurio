use std::mem;

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::control_packet::{ControlPacket, ControlPacketType};
use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::properties::AuthenticationData;
use crate::properties::AuthenticationMethod;
use crate::properties::ContentType;
use crate::properties::CorrelationData;
use crate::properties::MaximumPacketSize;
use crate::properties::MessageExpiryInterval;
use crate::properties::PayloadFormatIndicator;
use crate::properties::Property;
use crate::properties::ReceiveMaximum;
use crate::properties::RequestProblemInformation;
use crate::properties::RequestResponseInformation;
use crate::properties::ResponseTopic;
use crate::properties::SessionExpiryInterval;
use crate::properties::TopicAliasMaximum;
use crate::properties::UserProperty;
use crate::properties::WillDelayInterval;
use crate::qos::QoS;
use crate::reason::ReasonCode;

#[derive(Default, Debug, PartialEq)]
pub struct ConnectFlags {
    pub user_name: bool,
    pub password: bool,
    pub will_retain: bool,
    pub will_qos: QoS,
    pub will_flag: bool,
    pub clean_start: bool,
}

impl Encoder for ConnectFlags {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut flags: u8 = 0;

        if self.user_name {
            flags = 0b1000_0000;
        }

        if self.password {
            flags |= 0b0100_0000;
        }

        if self.will_retain {
            flags |= 0b0010_0000;
        }

        if self.will_flag {
            flags |= 0b0000_0100;
            flags |= (self.will_qos as u8) << 3;
        }

        if self.clean_start {
            flags |= 0b0000_0010;
        }

        buffer.put_u8(flags);
    }

    fn encoded_size(&self) -> usize {
        mem::size_of::<u8>()
    }
}

impl Decoder for ConnectFlags {
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        if !buffer.has_remaining() {
            return Ok(None);
        }

        let byte = buffer.get_u8();
        if (byte & 0b0000_0001) != 0 {
            return Err(ReasonCode::MalformedPacket);
        }

        let mut flags = ConnectFlags {
            user_name: (byte & 0b1000_0000) != 0,
            password: (byte & 0b0100_0000) != 0,
            will_retain: (byte & 0b0010_0000) != 0,
            clean_start: (byte & 0b0000_0010) != 0,
            ..Default::default()
        };

        if (byte & 0b0000_0100) != 0 {
            flags.will_flag = true;
            flags.will_qos = ((byte >> 3) & 0b0000_0011).into();
        }

        if flags.will_qos != QoS::Invalid {
            Ok(Some(flags))
        } else {
            Err(ReasonCode::MalformedPacket)
        }
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct ConnectProperties {
    pub session_expiry_interval: Option<SessionExpiryInterval>,
    pub receive_maximum: Option<ReceiveMaximum>,
    pub maximum_packet_size: Option<MaximumPacketSize>,
    pub topic_alias_maximum: Option<TopicAliasMaximum>,
    pub request_response_information: Option<RequestResponseInformation>,
    pub request_problem_information: Option<RequestProblemInformation>,
    pub user_property: Option<Vec<UserProperty>>,
    pub authentication_method: Option<AuthenticationMethod>,
    pub authentication_data: Option<AuthenticationData>,
}

impl Encoder for ConnectProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.session_expiry_interval.encode(buffer);
        self.receive_maximum.encode(buffer);
        self.maximum_packet_size.encode(buffer);
        self.topic_alias_maximum.encode(buffer);
        self.request_response_information.encode(buffer);
        self.request_problem_information.encode(buffer);
        self.user_property.encode(buffer);
        self.authentication_method.encode(buffer);
        self.authentication_data.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.session_expiry_interval.encoded_size();
        len += self.receive_maximum.encoded_size();
        len += self.maximum_packet_size.encoded_size();
        len += self.topic_alias_maximum.encoded_size();
        len += self.request_response_information.encoded_size();
        len += self.request_problem_information.encoded_size();
        len += self.user_property.encoded_size();
        len += self.authentication_method.encoded_size();
        len += self.authentication_data.encoded_size();

        len
    }
}

impl Decoder for ConnectProperties {
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        let len = VariableByteInteger::decode(buffer, None)?.unwrap();
        if len.0 == 0 {
            return Ok(None);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);
        let mut connect_properties = ConnectProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties, None)?.unwrap();

            match p {
                Property::SessionExpiryInterval => {
                    connect_properties.session_expiry_interval =
                        SessionExpiryInterval::decode(&mut encoded_properties, None)?
                }

                Property::AuthenticationMethod => {
                    connect_properties.authentication_method =
                        AuthenticationMethod::decode(&mut encoded_properties, None)?
                }

                Property::AuthenticationData => {
                    connect_properties.authentication_data =
                        AuthenticationData::decode(&mut encoded_properties, None)?
                }

                Property::RequestProblemInformation => {
                    connect_properties.request_problem_information =
                        RequestProblemInformation::decode(&mut encoded_properties, None)?
                }

                Property::ResponseInformation => {
                    connect_properties.request_response_information =
                        RequestResponseInformation::decode(&mut encoded_properties, None)?
                }

                Property::ReceiveMaximum => {
                    connect_properties.receive_maximum =
                        ReceiveMaximum::decode(&mut encoded_properties, None)?
                }

                Property::TopicAliasMaximum => {
                    connect_properties.topic_alias_maximum =
                        TopicAliasMaximum::decode(&mut encoded_properties, None)?
                }

                Property::MaximumPacketSize => {
                    connect_properties.maximum_packet_size =
                        MaximumPacketSize::decode(&mut encoded_properties, None)?
                }

                Property::UserProperty => {
                    let user_property =
                        UserProperty::decode(&mut encoded_properties, None)?.unwrap();

                    if let Some(v) = &mut connect_properties.user_property {
                        v.push(user_property);
                    } else {
                        let v = vec![user_property];
                        connect_properties.user_property = Some(v);
                    }
                }

                _ => return Err(ReasonCode::MalformedPacket),
            }

            if !encoded_properties.has_remaining() {
                break;
            }
        }

        Ok(Some(connect_properties))
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct WillProperties {
    pub will_delay_interval: Option<WillDelayInterval>,
    pub payload_format_indicator: Option<PayloadFormatIndicator>,
    pub message_expiry_interval: Option<MessageExpiryInterval>,
    pub content_type: Option<ContentType>,
    pub response_topic: Option<ResponseTopic>,
    pub correlation_data: Option<CorrelationData>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for WillProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.will_delay_interval.encode(buffer);
        self.payload_format_indicator.encode(buffer);
        self.message_expiry_interval.encode(buffer);
        self.content_type.encode(buffer);
        self.response_topic.encode(buffer);
        self.correlation_data.encode(buffer);
        self.user_property.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.will_delay_interval.encoded_size();
        len += self.payload_format_indicator.encoded_size();
        len += self.message_expiry_interval.encoded_size();
        len += self.content_type.encoded_size();
        len += self.response_topic.encoded_size();
        len += self.correlation_data.encoded_size();
        len += self.user_property.encoded_size();
        len
    }
}

impl Decoder for WillProperties {
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        let len = VariableByteInteger::decode(buffer, None)?.unwrap();
        if len.0 == 0 {
            return Ok(None);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);
        let mut will_properties = WillProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties, None)?.unwrap();

            match p {
                Property::WillDelayInterval => {
                    will_properties.will_delay_interval =
                        WillDelayInterval::decode(&mut encoded_properties, None)?
                }

                Property::PayloadFormatIndicator => {
                    will_properties.payload_format_indicator =
                        PayloadFormatIndicator::decode(&mut encoded_properties, None)?
                }

                Property::MessageExpiryInterval => {
                    will_properties.message_expiry_interval =
                        MessageExpiryInterval::decode(&mut encoded_properties, None)?
                }

                Property::ContentType => {
                    will_properties.content_type =
                        ContentType::decode(&mut encoded_properties, None)?
                }

                Property::ResponseTopic => {
                    will_properties.response_topic =
                        ResponseTopic::decode(&mut encoded_properties, None)?
                }

                Property::CorrelationData => {
                    will_properties.correlation_data =
                        CorrelationData::decode(&mut encoded_properties, None)?
                }

                Property::UserProperty => {
                    let user_property =
                        UserProperty::decode(&mut encoded_properties, None)?.unwrap();

                    if let Some(v) = &mut will_properties.user_property {
                        v.push(user_property);
                    } else {
                        let v = vec![user_property];
                        will_properties.user_property = Some(v);
                    }
                }

                _ => return Err(ReasonCode::MalformedPacket),
            }

            if !encoded_properties.has_remaining() {
                break;
            }
        }

        Ok(Some(will_properties))
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct ConnectPayload {
    pub client_id: String,
    pub will_properties: Option<WillProperties>,
    pub will_topic: Option<String>,
    pub will_payload: Option<Bytes>,
    pub user_name: Option<String>,
    pub password: Option<Bytes>,
}

impl Encoder for ConnectPayload {
    fn encode(&self, buffer: &mut BytesMut) {
        self.client_id.encode(buffer);

        if self.will_properties.encoded_size() > 0 {
            VariableByteInteger(self.will_properties.encoded_size() as u32).encode(buffer);
            self.will_properties.encode(buffer);
        }

        self.will_topic.encode(buffer);
        self.will_payload.encode(buffer);
        self.user_name.encode(buffer);
        self.password.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.client_id.encoded_size();
        if self.will_properties.encoded_size() > 0 {
            len += VariableByteInteger(self.will_properties.encoded_size() as u32).encoded_size();
            len += self.will_properties.encoded_size();
        }

        len += self.will_topic.encoded_size();
        len += self.will_payload.encoded_size();
        len += self.user_name.encoded_size();
        len += self.password.encoded_size();

        len
    }
}

impl Decoder for ConnectPayload {
    type Context = ConnectFlags;

    fn decode<T: Buf>(
        buffer: &mut T,
        context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        let mut payload = ConnectPayload::default();

        if let Some(client_id) = String::decode(buffer, None)? {
            payload.client_id = client_id;
        }

        if context.unwrap().will_flag {
            payload.will_properties = WillProperties::decode(buffer, None)?;
            payload.will_topic = String::decode(buffer, None)?;
            payload.will_payload = Bytes::decode(buffer, None)?;
        }

        if context.unwrap().user_name {
            payload.user_name = String::decode(buffer, None)?;
        }

        if context.unwrap().password {
            payload.password = Bytes::decode(buffer, None)?;
        }

        Ok(Some(payload))
    }
}

#[derive(PartialEq, Debug)]
pub struct ConnectPacket {
    flags: ConnectFlags,
    keepalive: u16,
    properties: Option<ConnectProperties>,
    payload: ConnectPayload,
}

impl ConnectPacket {
    const PROTOCOL_NAME: &'static str = "MQTT";
    const PROTOCOL_VERSION: u8 = 5;
}

impl ControlPacket for ConnectPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Connect;
}

impl Encoder for ConnectPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        buffer.put_u8((Self::PACKET_TYPE as u8) << 4);
        remaining_len += Self::PROTOCOL_NAME.encoded_size();
        remaining_len += Self::PROTOCOL_VERSION.encoded_size();
        remaining_len += self.flags.encoded_size();
        remaining_len += self.keepalive.encoded_size();
        remaining_len += VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
        remaining_len += self.properties.encoded_size();
        remaining_len += self.payload.encoded_size();
        VariableByteInteger(remaining_len as u32).encode(buffer);

        // Variable header
        Self::PROTOCOL_NAME.encode(buffer);
        Self::PROTOCOL_VERSION.encode(buffer);
        self.flags.encode(buffer);
        self.keepalive.encode(buffer);
        VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);

        // Payload
        self.payload.encode(buffer);
    }
}

impl Decoder for ConnectPacket {
    type Context = ();
    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        buffer.advance(1); // Packet type
        let _ = VariableByteInteger::decode(buffer, None); //Remaining length

        if let Some(protocol_name) = String::decode(buffer, None)? {
            if protocol_name != Self::PROTOCOL_NAME {
                return Err(ReasonCode::MalformedPacket);
            }
        }

        if let Some(protocol_version) = u8::decode(buffer, None)? {
            if protocol_version != 5 {
                return Err(ReasonCode::MalformedPacket);
            }
        }

        let flags = ConnectFlags::decode(buffer, None)?.unwrap();
        let keepalive = u16::decode(buffer, None)?.unwrap();
        let properties = ConnectProperties::decode(buffer, None)?;
        let payload = ConnectPayload::decode(buffer, Some(&flags))?.unwrap();

        Ok(Some(ConnectPacket {
            flags,
            keepalive,
            properties,
            payload,
        }))
    }
}

#[cfg(test)]
mod tests {
    use crate::packets::connect::*;

    #[test]
    fn test_connect_packet_encoding() {
        let expected = vec![
            0x10, 0x10, 0x00, 0x04, 0x4d, 0x51, //
            0x54, 0x54, 0x05, 0x02, 0x00, 0x3c, //
            0x03, 0x21, 0x00, 0x14, 0x00, 0x00,
        ];

        let flags = ConnectFlags {
            user_name: false,
            password: false,
            will_retain: false,
            will_qos: QoS::AtMostOnce,
            will_flag: false,
            clean_start: true,
        };

        let properties = ConnectProperties {
            receive_maximum: ReceiveMaximum::new(20).into(),
            ..Default::default()
        };

        let payload = ConnectPayload::default();
        let packet = ConnectPacket {
            flags,
            keepalive: 60,
            properties: properties.into(),
            payload,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = ConnectPacket::decode(&mut bytes, None)
            .expect("Unexpected error")
            .unwrap();
        assert_eq!(packet, new_packet);
    }

    #[test]
    fn test_connect_packet_encode_decode() {
        let expected = vec![
            0x10, 0x52, 0x00, 0x04, 0x4d, 0x51, 0x54, 0x54, 0x05, 0xee, 0x00, 0x3c, 0x08, 0x11,
            0x00, 0x00, 0x00, 0x1e, 0x21, 0x00, 0x14, 0x00, 0x00, 0x0d, 0x08, 0x00, 0x0a, 0x74,
            0x65, 0x73, 0x74, 0x5f, 0x74, 0x6f, 0x70, 0x69, 0x63, 0x00, 0x0a, 0x74, 0x65, 0x73,
            0x74, 0x5f, 0x74, 0x6f, 0x70, 0x69, 0x63, 0x00, 0x0c, 0x74, 0x65, 0x73, 0x74, 0x5f,
            0x70, 0x61, 0x79, 0x6c, 0x6f, 0x61, 0x64, 0x00, 0x09, 0x74, 0x65, 0x73, 0x74, 0x5f,
            0x75, 0x73, 0x65, 0x72, 0x00, 0x08, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x70, 0x77, 0x64,
        ];

        let flags = ConnectFlags {
            user_name: true,
            password: true,
            will_retain: true,
            will_qos: QoS::AtLeastOnce,
            will_flag: true,
            clean_start: true,
        };

        let properties = ConnectProperties {
            session_expiry_interval: SessionExpiryInterval::new(30).into(),
            receive_maximum: ReceiveMaximum::new(20).into(),
            ..Default::default()
        };

        let will_properties = WillProperties {
            response_topic: ResponseTopic::new(String::from("test_topic")).into(),
            ..Default::default()
        };

        let payload = ConnectPayload {
            will_properties: will_properties.into(),
            will_topic: String::from("test_topic").into(),
            will_payload: Bytes::from("test_payload").into(),
            user_name: String::from("test_user").into(),
            password: Bytes::from("test_pwd").into(),
            ..Default::default()
        };

        let packet = ConnectPacket {
            flags,
            keepalive: 60,
            properties: properties.into(),
            payload,
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded, expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = ConnectPacket::decode(&mut bytes, None)
            .expect("Unexpected error")
            .unwrap();
        assert_eq!(packet, new_packet);
    }
}
