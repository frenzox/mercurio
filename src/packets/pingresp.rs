use bytes::{Buf, BufMut, BytesMut};

use crate::codec::{Decoder, Encoder, VariableByteInteger};

#[derive(PartialEq, Debug)]
pub struct PingRespPacket {}

const PACKET_TYPE: u8 = 0x0d;

impl Encoder for PingRespPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(PACKET_TYPE << 4);

        let remaining_len = 0;
        VariableByteInteger(remaining_len).encode(buffer);
    }
}

impl Decoder for PingRespPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        buffer.advance(1);
        Ok(Self {})
    }
}
