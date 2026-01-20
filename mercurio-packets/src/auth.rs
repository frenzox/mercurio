#[cfg(not(feature = "std"))]
use alloc::{vec, vec::Vec};

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    properties::*,
    reason::ReasonCode,
};

#[derive(Default, PartialEq, Eq, Debug, Clone)]
pub struct AuthProperties {
    pub auth_method: Option<AuthenticationMethod>,
    pub auth_data: Option<AuthenticationData>,
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for AuthProperties {
    fn encode(&self, buffer: &mut BytesMut) {
        self.auth_method.encode(buffer);
        self.auth_data.encode(buffer);
        self.reason_string.encode(buffer);
        self.user_property.encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        let mut len = 0;

        len += self.auth_method.encoded_size();
        len += self.auth_data.encoded_size();
        len += self.reason_string.encoded_size();
        len += self.user_property.encoded_size();

        len
    }
}

impl Decoder for AuthProperties {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        use Property::*;

        let len = VariableByteInteger::decode(buffer)?;
        let mut properties = AuthProperties::default();

        if len.0 == 0 {
            return Ok(properties);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(Error::PacketIncomplete);
        }

        let mut encoded_properties = buffer.take(len.0 as usize);

        while encoded_properties.has_remaining() {
            match Property::decode(&mut encoded_properties)? {
                AuthenticationMethod(v) => properties.auth_method = Some(v),
                AuthenticationData(v) => properties.auth_data = Some(v),
                ReasonString(v) => properties.reason_string = Some(v),
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

#[derive(Eq, PartialEq, Debug, Clone)]
pub struct AuthPacket {
    pub reason: ReasonCode,
    pub properties: AuthProperties,
}

const PACKET_TYPE: u8 = 0x0f;

impl Encoder for AuthPacket {
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

impl Decoder for AuthPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        let reserved = buffer.get_u8() & 0xF;

        if reserved != 0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let _ = VariableByteInteger::decode(buffer)?; //Remaining length
        let reason = ReasonCode::decode(buffer)?;
        let properties = AuthProperties::decode(buffer)?;

        Ok(AuthPacket { reason, properties })
    }
}
