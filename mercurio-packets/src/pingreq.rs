//! PINGREQ, sent by a client to keep an idle connection alive (section 3.12).

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::codec::{Decoder, Encoder, VariableByteInteger};

use crate::PacketType;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PingReqPacket {}

const PACKET_TYPE: u8 = 0x0c;

impl Encoder for PingReqPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(PACKET_TYPE << 4);

        let remaining_len = 0;
        VariableByteInteger(remaining_len).encode(buffer);
    }

    fn encoded_size(&self) -> usize {
        2
    }
}

impl Decoder for PingReqPacket {
    fn decode<T: Buf>(buffer: &mut T) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::PingReq, |_, _| Ok(Self {}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_consumes_whole_packet() {
        let mut buffer = BytesMut::from(&[0xc0, 0x00, 0xc0, 0x00][..]);
        PingReqPacket::decode(&mut buffer).unwrap();
        assert_eq!(buffer.len(), 2);
    }
}
