//! AUTH, the steps of MQTT 5 enhanced authentication (section 3.15). MQTT 3.x
//! has no such packet.

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{
    codec::{Decoder, Encoder},
    error::EncodeError,
    properties::*,
    protocol::ProtocolVersion,
    reason::ReasonCode,
};

use crate::{push_user_property, set_once, Packet, PacketType};

#[derive(Default, PartialEq, Eq, Debug, Clone)]
pub struct AuthProperties {
    pub auth_method: Option<AuthenticationMethod>,
    pub auth_data: Option<AuthenticationData>,
    pub reason_string: Option<ReasonString>,
    pub user_property: Option<Vec<UserProperty>>,
}

impl Encoder for AuthProperties {
    fn encode(&self, buffer: &mut BytesMut) -> Result<(), EncodeError> {
        crate::check_authentication(&self.auth_method, &self.auth_data)?;
        self.auth_method.encode(buffer)?;
        self.auth_data.encode(buffer)?;
        self.reason_string.encode(buffer)?;
        self.user_property.encode(buffer)?;
        Ok(())
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

        crate::check_decoded_authentication(&properties.auth_method, &properties.auth_data)?;

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

impl Packet for AuthPacket {
    /// MQTT 3.x has no AUTH packet, so encoding one for it fails.
    fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion) -> Result<(), EncodeError> {
        if !version.supports_auth_packet() {
            return Err(EncodeError::UnsupportedVersion(PacketType::Auth, version));
        }
        crate::write_packet(buffer, |buffer| {
            buffer.put_u8(PACKET_TYPE << 4);
            crate::encode_length(self.remaining_len(), buffer)?;

            crate::encode_reason(self.reason, PacketType::Auth, buffer)?;
            crate::encode_length(self.properties.encoded_size(), buffer)?;
            self.properties.encode(buffer)?;
            Ok(())
        })
    }

    fn encoded_size(&self, _: ProtocolVersion) -> usize {
        crate::packet_size(self.remaining_len())
    }

    /// MQTT 3.x has no AUTH packet, so there it is malformed.
    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        if !version.supports_auth_packet() {
            return Err(ReasonCode::MalformedPacket.into());
        }
        crate::decode_packet(buffer, PacketType::Auth, version, |_, buffer| {
            // Both reason and properties may be omitted [MQTT-3.15.2-1]
            let reason = if buffer.has_remaining() {
                crate::decode_reason(buffer, PacketType::Auth)?
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

impl AuthPacket {
    fn remaining_len(&self) -> usize {
        self.reason.encoded_size() + crate::property_list_size(&self.properties)
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
        packet.encode(&mut buffer, ProtocolVersion::V5).unwrap();
        assert_eq!(
            AuthPacket::decode(&mut buffer, ProtocolVersion::V5).unwrap(),
            packet
        );
        assert!(buffer.is_empty());
    }
}
