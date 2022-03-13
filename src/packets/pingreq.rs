use bytes::{Buf, BufMut, BytesMut};

use crate::codec::{Decoder, Encoder, VariableByteInteger};
use crate::result::Result;

use super::control_packet_type::ControlPacketType;

#[derive(PartialEq, Debug)]
pub struct PingReqPacket {}

impl ControlPacketType for PingReqPacket {
    const PACKET_TYPE: u8 = 0x0b;
}

impl Encoder for PingReqPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(Self::PACKET_TYPE << 4);

        let remaining_len = 0;
        VariableByteInteger(remaining_len).encode(buffer);
    }
}

impl Decoder for PingReqPacket {
    type Context = ();

    fn decode<T: Buf>(buffer: &mut T, _context: Option<&Self::Context>) -> Result<Self> {
        buffer.advance(1);
        Ok(Self {})
    }
}
