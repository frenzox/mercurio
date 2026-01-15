use bytes::{Buf, BufMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    qos::QoS,
    reason::ReasonCode,
};

#[derive(Default, Debug, PartialEq, Eq)]
pub struct SubscribeProperties {
    subscription_id: Option<SubscriptionIdentifier>,
    user_property: Option<Vec<UserProperty>>,
}

impl Encoder for SubscribeProperties {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.subscription_id.encode(buffer);
        self.user_property.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.subscription_id.encoded_size();
        len += self.user_property.encoded_size();

        len
    }
}

impl Decoder for SubscribeProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = SubscribeProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
                SubscriptionIdentifier(v) => properties.subscription_id = Some(v),
                UserProperty(v) => {
                    if let Some(vec) = &mut properties.user_property {
                        vec.push(v);
                    } else {
                        let vec = vec![v];
                        properties.user_property = Some(vec);
                    }
                }
                _ => return Err(ReasonCode::MalformedPacket.into()),
            }
        }

        Ok(properties)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum RetainHandling {
    SendRetained = 0x00,
    SendRetainedIfNonExisting = 0x01,
    DoNotSendRetained = 0x02,
    Invalid = 0xff,
}

impl From<u8> for RetainHandling {
    fn from(n: u8) -> Self {
        match n {
            0x00 => Self::SendRetained,
            0x01 => Self::SendRetainedIfNonExisting,
            0x02 => Self::DoNotSendRetained,
            _ => Self::Invalid,
        }
    }
}

impl Default for RetainHandling {
    fn default() -> Self {
        Self::SendRetained
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SubscriptionOptions {
    qos: QoS,
    no_local: bool,
    retain_as_pub: bool,
    retain_handling: RetainHandling,
}

impl Encoder for SubscriptionOptions {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let mut encoded: u8 = self.qos as u8;

        if self.no_local {
            encoded |= 0b0000_0100;
        }

        if self.retain_as_pub {
            encoded |= 0b0000_1000;
        }

        encoded |= (self.retain_handling as u8) << 4;

        buffer.put_u8(encoded);
    }

    fn encoded_size(&self) -> usize {
        std::mem::size_of::<u8>()
    }
}

impl Decoder for SubscriptionOptions {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let opt = buffer.get_u8();

        let qos: QoS = (opt & 0b0000_0011).into();

        if qos == QoS::Invalid {
            return Err(ReasonCode::ProtocolError.into());
        }

        let no_local = (opt & 0b0000_0100) != 0;
        let retain_as_pub = (opt & 0b0000_1000) != 0;
        let retain_handling: RetainHandling = (opt >> 4).into();

        if retain_handling == RetainHandling::Invalid {
            return Err(ReasonCode::ProtocolError.into());
        }

        Ok(SubscriptionOptions {
            qos,
            no_local,
            retain_as_pub,
            retain_handling,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SubscribePayload {
    pub topic_filter: String,
    pub subs_opt: SubscriptionOptions,
}

impl Encoder for SubscribePayload {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.topic_filter.encode(buffer);
        self.subs_opt.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.topic_filter.encoded_size();
        len += self.subs_opt.encoded_size();

        len
    }
}

impl Decoder for SubscribePayload {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let topic_filter = String::decode(buffer)?;
        let subs_opt = SubscriptionOptions::decode(buffer)?;

        Ok(SubscribePayload {
            topic_filter,
            subs_opt,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct SubscribePacket {
    pub packet_id: u16,
    pub properties: Option<SubscribeProperties>,
    pub payload: Vec<SubscribePayload>,
}

const PACKET_TYPE: u8 = 0x08;

impl Encoder for SubscribePacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        let mut remaining_len = 0;

        // Fixed header
        let mut fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header |= 0b0000_0010;
        fixed_header.encode(buffer);

        remaining_len += self.packet_id.encoded_size();
        remaining_len += VariableByteInteger(self.properties.encoded_size() as u32).encoded_size();
        remaining_len += self.properties.encoded_size();
        remaining_len += self.payload.encoded_size();

        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.packet_id.encode(buffer);
        VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
        self.properties.encode(buffer);
        self.payload.encode(buffer);
    }
}

impl Decoder for SubscribePacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1); // Packet type
        let remaining_len = VariableByteInteger::decode(buffer)?.0 as usize; //Remaining length
        let buffer_len = buffer.remaining();

        let packet_id = u16::decode(buffer)?;
        let properties = Some(SubscribeProperties::decode(buffer)?);

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError.into());
        }

        let next_packet = buffer_len - remaining_len;
        let mut payload = Vec::new();

        while buffer.remaining() > next_packet {
            payload.push(SubscribePayload::decode(buffer)?);
        }

        Ok(SubscribePacket {
            packet_id,
            properties,
            payload,
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::{Bytes, BytesMut};

    use crate::subscribe::*;

    #[test]
    fn test_subscribe_packet_encode_decode() {
        let expected = vec![
            0x82, 0x1f, 0x00, 0x01, 0x0f, 0x0b, 0x20, 0x26, 0x00, 0x03, 0x6b, 0x65, 0x79, 0x00,
            0x05, 0x76, 0x61, 0x6c, 0x75, 0x65, 0x00, 0x0a, 0x74, 0x65, 0x73, 0x74, 0x5f, 0x74,
            0x6f, 0x70, 0x69, 0x63, 0x09,
        ];

        let properties = SubscribeProperties {
            user_property: vec![UserProperty::new("key".to_string(), "value".to_string())].into(),
            subscription_id: Some(SubscriptionIdentifier::new(VariableByteInteger(32))),
        };

        let subs_opt = SubscriptionOptions {
            qos: QoS::AtLeastOnce,
            no_local: false,
            retain_as_pub: true,
            retain_handling: RetainHandling::SendRetained,
        };

        let packet = SubscribePacket {
            packet_id: 1,
            properties: Some(properties),
            payload: vec![SubscribePayload {
                topic_filter: "test_topic".to_string(),
                subs_opt,
            }],
        };

        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded);

        assert_eq!(encoded.to_vec(), expected);

        let mut bytes = Bytes::from(expected);

        let new_packet = SubscribePacket::decode(&mut bytes).expect("Unexpected error");
        assert_eq!(packet, new_packet);
    }
}
