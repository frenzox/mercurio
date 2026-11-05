use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::codec::{Decoder, Encoder, VariableByteInteger};

#[derive(PartialEq, Eq, Debug)]
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
        VariableByteInteger::decode(buffer)?;
        Ok(Self {})
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_consumes_whole_packet() {
        let mut buffer = BytesMut::from(&[0xd0, 0x00, 0xd0, 0x00][..]);
        PingRespPacket::decode(&mut buffer).unwrap();
        assert_eq!(buffer.len(), 2);
    }
}
