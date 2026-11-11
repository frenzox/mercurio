//! PINGRESP, the server's answer to PINGREQ (section 3.13).

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::protocol::ProtocolVersion;

use crate::{Packet, PacketType};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PingRespPacket {}

impl Packet for PingRespPacket {
    fn encode(&self, buffer: &mut BytesMut, _: ProtocolVersion) {
        buffer.put_slice(&[0xd0, 0x00]);
    }

    fn encoded_size(&self, _: ProtocolVersion) -> usize {
        2
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::PingResp, version, |_, _| Ok(Self {}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_consumes_whole_packet() {
        let mut buffer = BytesMut::from(&[0xd0, 0x00, 0xd0, 0x00][..]);
        PingRespPacket::decode(&mut buffer, ProtocolVersion::V5).unwrap();
        assert_eq!(buffer.len(), 2);
    }
}
