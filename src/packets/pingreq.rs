use bytes::{Buf, BufMut, BytesMut};

use crate::control_packet::*;
use crate::endec::{Decoder, Encoder, VariableByteInteger};
use crate::reason::ReasonCode;

pub struct PingReqPacket {}

impl ControlPacket for PingReqPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PingReq;
}

impl Encoder for PingReqPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8((Self::PACKET_TYPE as u8) << 4);

        let remaining_len = 0;
        VariableByteInteger(remaining_len).encode(buffer);
    }
}

impl Decoder for PingReqPacket {
    type Context = ();

    fn decode<T: Buf>(
        buffer: &mut T,
        _context: Option<&Self::Context>,
    ) -> Result<Option<Self>, ReasonCode> {
        buffer.advance(1);
        Ok(Some(Self {}))
    }
}
