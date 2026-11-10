//! AUTH, the steps of MQTT 5 enhanced authentication (section 3.15). MQTT 3.x
//! has no such packet.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    properties::*,
    reason::ReasonCode,
};

use crate::PacketType;

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

        let mut properties = AuthProperties::default();

        decode_property_list(buffer, |property| match property {
            AuthenticationMethod(v) => set_once(&mut properties.auth_method, v),
            AuthenticationData(v) => set_once(&mut properties.auth_data, v),
            ReasonString(v) => set_once(&mut properties.reason_string, v),
            UserProperty(v) => {
                push_user_property(&mut properties.user_property, v);
                Ok(())
            }
            _ => Err(ReasonCode::MalformedPacket.into()),
        })?;

        Ok(properties)
    }
}

#[derive(Eq, PartialEq, Debug, Clone)]
pub struct AuthPacket {
    /// `Success`, `ContinueAuthentication` or `ReAuthenticate` (section
    /// 3.15.2.1). Decodes as `Success` when the packet leaves it out.
    pub reason: ReasonCode,
    pub properties: AuthProperties,
}

const PACKET_TYPE: u8 = 0x0f;

impl Encoder for AuthPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let properties_len = VariableByteInteger(self.properties.encoded_size() as u32);
        let remaining_len = self.reason.encoded_size()
            + properties_len.encoded_size()
            + self.properties.encoded_size();

        buffer.put_u8(PACKET_TYPE << 4);
        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.reason.encode(buffer);
        properties_len.encode(buffer);
        self.properties.encode(buffer);
    }
}

impl Decoder for AuthPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::Auth, |_, buffer| {
            // Both reason and properties may be omitted [MQTT-3.15.2-1]
            let reason = if buffer.has_remaining() {
                ReasonCode::decode(buffer)?
            } else {
                ReasonCode::Success
            };
            let properties = if buffer.has_remaining() {
                AuthProperties::decode(buffer)?
            } else {
                AuthProperties::default()
            };

            Ok(AuthPacket { reason, properties })
        })
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;

    #[test]
    fn test_round_trip() {
        let packet = AuthPacket {
            reason: ReasonCode::ContinueAuthentication,
            properties: AuthProperties {
                auth_method: Some(AuthenticationMethod::new("SCRAM-SHA-256".into())),
                auth_data: Some(AuthenticationData::new(Bytes::from_static(b"nonce"))),
                ..Default::default()
            },
        };

        let mut buffer = BytesMut::new();
        packet.encode(&mut buffer);
        assert_eq!(AuthPacket::decode(&mut buffer).unwrap(), packet);
        assert!(buffer.is_empty());
    }
}
