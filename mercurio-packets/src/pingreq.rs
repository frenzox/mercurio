use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::codec::{Decoder, Encoder, VariableByteInteger};

#[derive(PartialEq, Eq, Debug)]
pub struct PingReqPacket {}

const PACKET_TYPE: u8 = 0x0c;

impl Encoder for PingReqPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        buffer.put_u8(PACKET_TYPE << 4);

        let remaining_len = 0;
        VariableByteInteger(remaining_len).encode(buffer);
    }
}

impl Decoder for PingReqPacket {
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
        let mut buffer = BytesMut::from(&[0xc0, 0x00, 0xc0, 0x00][..]);
        PingReqPacket::decode(&mut buffer).unwrap();
        assert_eq!(buffer.len(), 2);
    }
}
