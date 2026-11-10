//! SUBSCRIBE, a client's request for the messages of some topic filters,
//! with their options (section 3.8).

use core::mem;

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use bytes::{Buf, BufMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    qos::QoS,
    reason::ReasonCode,
};

use crate::PacketType;

#[derive(Default, Debug, PartialEq, Eq)]
/// MQTT 5 only.
pub struct SubscribeProperties {
    /// Number the server adds to each PUBLISH it sends for these
    /// subscriptions (section 3.8.2.1.2).
    pub subscription_id: Option<SubscriptionIdentifier>,
    pub user_property: Option<Vec<UserProperty>>,
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

        let mut properties = SubscribeProperties::default();

        decode_property_list(buffer, |property| match property {
            SubscriptionIdentifier(v) => set_once(&mut properties.subscription_id, v),
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        Ok(properties)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
#[derive(Default)]
/// Whether the server sends retained messages when the subscription is
/// made (section 3.8.3.1). MQTT 5 only. MQTT 3.x always sends them.
pub enum RetainHandling {
    #[default]
    SendRetained = 0x00,
    /// Send them only if the subscription didn't exist already.
    SendRetainedIfNonExisting = 0x01,
    DoNotSendRetained = 0x02,
    /// Any value above 2. Decoding rejects it as a protocol error.
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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
/// The Subscription Options byte (section 3.8.3.1). MQTT 3.x has only
/// `qos`: decoding rejects the other fields, and encoding leaves them out.
pub struct SubscriptionOptions {
    /// Highest QoS the server uses to send messages for this subscription.
    pub qos: QoS,
    /// Don't send this client the messages it published itself. A protocol
    /// error on a shared subscription.
    pub no_local: bool,
    /// Keep the RETAIN flag of forwarded messages as the publisher set it.
    /// When false the server clears it, except on retained messages sent
    /// because of a new subscription.
    pub retain_as_pub: bool,
    pub retain_handling: RetainHandling,
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
        mem::size_of::<u8>()
    }
}

impl Decoder for SubscriptionOptions {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let opt = u8::decode(buffer)?;

        // Bits 6 and 7 are reserved [MQTT-3.8.3-5]
        if opt & 0b1100_0000 != 0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

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
    /// At least one entry. An empty list decodes as a protocol error.
    pub payload: Vec<SubscribePayload>,
}

const PACKET_TYPE: u8 = 0x08;

impl Encoder for SubscribePacket {
    fn encode(&self, buffer: &mut bytes::BytesMut) {
        self.encode_inner(buffer, true);
    }

    fn encoded_size(&self) -> usize {
        crate::packet_size(self.remaining_len(true))
    }
}

impl Decoder for SubscribePacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        Self::decode_inner(buffer, true)
    }
}

impl SubscribePacket {
    /// Encodes the packet in MQTT 3.x format (no properties).
    pub fn encode_v3(&self, buffer: &mut bytes::BytesMut) {
        self.encode_inner(buffer, false);
    }

    /// Decodes the packet in MQTT 3.x format (no properties).
    pub fn decode_v3<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        Self::decode_inner(buffer, false)
    }

    fn remaining_len(&self, with_properties: bool) -> usize {
        // MQTT 3.x writes the same one byte of options per filter
        let mut len = self.packet_id.encoded_size() + self.payload.encoded_size();
        if with_properties {
            len += crate::property_list_size(&self.properties);
        }
        len
    }

    fn encode_inner(&self, buffer: &mut bytes::BytesMut, with_properties: bool) {
        // Fixed header
        let mut fixed_header: u8 = PACKET_TYPE << 4;
        fixed_header |= 0b0000_0010;
        fixed_header.encode(buffer);

        VariableByteInteger(self.remaining_len(with_properties) as u32).encode(buffer);

        self.packet_id.encode(buffer);
        if with_properties {
            VariableByteInteger(self.properties.encoded_size() as u32).encode(buffer);
            self.properties.encode(buffer);
            self.payload.encode(buffer);
        } else {
            // MQTT 3.x only has the QoS; the other option bits are reserved
            // and must be zero [MQTT-3.8.3-4]
            for sub in &self.payload {
                sub.topic_filter.encode(buffer);
                (sub.subs_opt.qos as u8).encode(buffer);
            }
        }
    }

    fn decode_inner<T: Buf>(buffer: &mut T, with_properties: bool) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::Subscribe, |_, body| {
            Self::decode_body(body, with_properties)
        })
    }

    fn decode_body<T: Buf>(buffer: &mut T, with_properties: bool) -> crate::Result<Self> {
        let packet_id = u16::decode(buffer)?;
        let properties = if with_properties {
            crate::present(SubscribeProperties::decode(buffer)?)
        } else {
            None
        };

        if !buffer.has_remaining() {
            return Err(ReasonCode::ProtocolError.into());
        }

        let mut payload = Vec::new();

        while buffer.has_remaining() {
            let sub = SubscribePayload::decode(buffer)?;

            // MQTT 3.x only defines the QoS bits; the rest are reserved [MQTT-3.8.3-4]
            if !with_properties
                && sub.subs_opt
                    != (SubscriptionOptions {
                        qos: sub.subs_opt.qos,
                        ..Default::default()
                    })
            {
                return Err(ReasonCode::MalformedPacket.into());
            }

            payload.push(sub);
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
    use std::string::ToString;

    use bytes::{Bytes, BytesMut};

    use crate::subscribe::*;

    #[test]
    fn test_v3_encoding_keeps_only_the_qos() {
        let packet = SubscribePacket {
            packet_id: 1,
            properties: None,
            payload: vec![SubscribePayload {
                topic_filter: "t".to_string(),
                subs_opt: SubscriptionOptions {
                    qos: QoS::AtLeastOnce,
                    no_local: true,
                    retain_as_pub: true,
                    retain_handling: RetainHandling::DoNotSendRetained,
                },
            }],
        };
        let mut encoded = BytesMut::new();
        packet.encode_v3(&mut encoded);
        assert_eq!(
            &encoded[..],
            [0x82, 0x06, 0x00, 0x01, 0x00, 0x01, b't', 0x01]
        );
    }

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
