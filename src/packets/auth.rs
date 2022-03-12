use bytes::{Buf, BufMut, BytesMut};

use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::properties::AuthenticationData;
use crate::properties::AuthenticationMethod;
use crate::properties::UserProperty;
use crate::properties::{Property, ReasonString};
use crate::reason::ReasonCode;
use crate::result::Result;

use super::control_packet_type::ControlPacketType;

#[derive(Default, PartialEq, Debug)]
pub struct AuthProperties {
    auth_method: Option<AuthenticationMethod>,
    auth_data: Option<AuthenticationData>,
    reason_string: Option<ReasonString>,
    user_property: Option<Vec<UserProperty>>,
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
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Option<Self>> {
        let len = VariableByteInteger::decode(buffer, None)?.unwrap();
        if len.0 == 0 {
            return Ok(None);
        } else if (buffer.remaining() as u32) < len.0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let mut encoded_properties = buffer.take(len.0 as usize);
        let mut properties = AuthProperties::default();

        loop {
            let p = Property::decode(&mut encoded_properties, None)?.unwrap();

            match p {
                Property::AuthenticationMethod => {
                    properties.auth_method =
                        AuthenticationMethod::decode(&mut encoded_properties, None)?
                }

                Property::AuthenticationData => {
                    properties.auth_data =
                        AuthenticationData::decode(&mut encoded_properties, None)?
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

                _ => return Err(ReasonCode::MalformedPacket.into()),
            }

            if !encoded_properties.has_remaining() {
                break;
            }
        }

        Ok(Some(properties))
    }
}

#[derive(PartialEq, Debug)]
pub struct AuthPacket {
    reason: ReasonCode,
    properties: Option<AuthProperties>,
}

impl ControlPacketType for AuthPacket {
    const PACKET_TYPE: u8 = 0x0e;
}

impl Encoder for AuthPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        let mut remaining_len = 0;

        buffer.put_u8(0x0d << 4);
        remaining_len += self.reason.encoded_size();
        remaining_len += self.properties.encoded_size();
        VariableByteInteger(remaining_len as u32).encode(buffer);

        self.reason.encode(buffer);
        self.properties.encode(buffer);
    }
}

impl Decoder for AuthPacket {
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Option<Self>> {
        let reserved = buffer.get_u8() & 0xF;

        if reserved != 0 {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let reason = ReasonCode::decode(buffer, None)?.unwrap();
        let properties = AuthProperties::decode(buffer, None)?;

        let _ = VariableByteInteger::decode(buffer, None); //Remaining length

        Ok(Some(AuthPacket { reason, properties }))
    }
}
