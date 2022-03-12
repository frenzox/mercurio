use bytes::{Buf, BufMut, BytesMut};

use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::result::Result;

use super::control_packet_type::ControlPacketType;

#[derive(PartialEq, Debug)]
pub struct PingRespPacket {}

impl ControlPacketType for PingRespPacket {
    const PACKET_TYPE: u8 = 0x0c;
}

impl Encoder for PingRespPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(Self::PACKET_TYPE << 4);

        let remaining_len = 0;
        VariableByteInteger(remaining_len).encode(buffer);
    }
}

impl Decoder for PingRespPacket {
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Option<Self>> {
        buffer.advance(1);
        Ok(Some(Self {}))
    }
}
