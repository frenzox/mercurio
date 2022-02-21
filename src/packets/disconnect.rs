use bytes::{Buf, BufMut, BytesMut};

use crate::control_packet::{ControlPacket, ControlPacketType};
use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::properties::ServerReference;
use crate::properties::SessionExpiryInterval;
use crate::properties::UserProperty;
use crate::properties::{Property, ReasonString};
use crate::reason::ReasonCode;

#[derive(Default)]
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
        let mut properties = DisconnectProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties, None)?.unwrap();

            match p {
                Property::SessionExpiryInterval => {
                    properties.session_expiry_interval =
                        SessionExpiryInterval::decode(&mut encoded_properties, None)?
                }

                Property::ReasonString => {
                    properties.reason_string = ReasonString::decode(&mut encoded_properties, None)?
                }

                Property::UserProperty => {
                    let user_property =
                        UserProperty::decode(&mut encoded_properties, None)?.unwrap();

                    if let Some(v) = &mut properties.user_property {
                        v.push(user_property);
                    } else {
                        let v = vec![user_property];
                        properties.user_property = Some(v);
                    }
                }

                Property::ServerReference => {
                    properties.server_reference =
                        ServerReference::decode(&mut encoded_properties, None)?
                }

                _ => return Err(ReasonCode::MalformedPacket),
            }

            if !encoded_properties.has_remaining() {
                break;
            }
        }

        Ok(Some(properties))
    }
}

pub struct DisconnectPacket {
    reason: ReasonCode,
    properties: Option<DisconnectProperties>,
}

impl ControlPacket for DisconnectPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Disconnect;
}

impl Encoder for DisconnectPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        buffer.put_u8((Self::PACKET_TYPE as u8) << 4);
        remaining_len += self.reason.encoded_size();
        remaining_len += self.properties.encoded_size();
        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.reason.encode(buffer);
        self.properties.encode(buffer);
    }
}

impl Decoder for DisconnectPacket {
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        let reserved = buffer.get_u8() & 0xF;

        if reserved != 0 {
            return Err(ReasonCode::MalformedPacket);
        }

        let reason = ReasonCode::decode(buffer, None)?.unwrap();
        let properties = DisconnectProperties::decode(buffer, None)?;

        let _ = VariableByteInteger::decode(buffer, None); //Remaining length

        Ok(Some(DisconnectPacket { reason, properties }))
    }
}
