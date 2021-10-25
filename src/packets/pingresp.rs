use bytes::{Buf, BufMut, BytesMut};

use crate::control_packet::*;
use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::reason::ReasonCode;

pub struct PingRespPacket {}

impl ControlPacket for PingRespPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PingResp;
}

impl Encoder for PingRespPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8((Self::PACKET_TYPE as u8) << 4);

        let remaining_len = 0;
        VariableByteInteger(remaining_len).encode(buffer);
    }
}

impl Decoder for PingRespPacket {
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        buffer.advance(1);
        Ok(Some(Self {}))
    }
}
