#[cfg(not(feature = "std"))]
use alloc::{vec, vec::Vec};

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    reason::ReasonCode,
};

#[derive(Default, PartialEq, Eq, Debug)]
pub struct DisconnectProperties {
    session_expiry_interval: Option<SessionExpiryInterval>,
    reason_string: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
    server_reference: Option<ServerReference>,
}

impl Encoder for DisconnectProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.session_expiry_interval.encode(buffer);
        self.reason_string.encode(buffer);
        self.user_property.encode(buffer);
        self.server_reference.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.session_expiry_interval.encoded_size();
        len += self.reason_string.encoded_size();
        len += self.user_property.encoded_size();
        len += self.server_reference.encoded_size();

        len
    }
}

impl Decoder for DisconnectProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = DisconnectProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
                SessionExpiryInterval(v) => properties.session_expiry_interval = Some(v),
                ReasonString(v) => properties.reason_string = Some(v),
                UserProperty(v) => {
                    if let Some(vec) = &mut properties.user_property {
                        vec.push(v);
                    } else {
                        let vec = vec![v];
                        properties.user_property = Some(vec);
                    }
                }
                ServerReference(v) => properties.server_reference = Some(v),
                _ => return Err(ReasonCode::MalformedPacket.into()),
            }
        }

        Ok(properties)
    }
}

#[derive(PartialEq, Eq, Debug)]
pub struct DisconnectPacket {
    pub reason: ReasonCode,
    pub properties: Option<DisconnectProperties>,
}

const PACKET_TYPE: u8 = 0x0e;

impl Encoder for DisconnectPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        buffer.put_u8(PACKET_TYPE << 4);
        remaining_len += self.reason.encoded_size();
        remaining_len += self.properties.encoded_size();
        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.reason.encode(buffer);
        self.properties.encode(buffer);
    }
}

impl Decoder for DisconnectPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let reserved = buffer.get_u8() & 0xF;

        if reserved != 0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let min_len_reason = 1;
        let min_len_properties = 2;

        let remaining_len = VariableByteInteger::decode(buffer)?; //Remaining length
        let reason = match remaining_len.0.cmp(&min_len_reason) {
            core::cmp::Ordering::Less => ReasonCode::NormalDisconnection,
            _ => ReasonCode::decode(buffer)?,
        };

        let properties = match remaining_len.0.cmp(&min_len_properties) {
            core::cmp::Ordering::Less => None,
            _ => Some(DisconnectProperties::decode(buffer)?),
        };

        Ok(DisconnectPacket { reason, properties })
    }
}
