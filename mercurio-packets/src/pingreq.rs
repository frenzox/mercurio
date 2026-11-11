//! PINGREQ, sent by a client to keep an idle connection alive (section 3.12).

use bytes::{Buf, BufMut, BytesMut};

use mercurio_core::{error::EncodeError, protocol::ProtocolVersion};

use crate::{Packet, PacketType};

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PingReqPacket {}

impl Packet for PingReqPacket {
    fn encode(&self, buffer: &mut BytesMut, _: ProtocolVersion) -> Result<(), EncodeError> {
        crate::write_packet(buffer, |buffer| {
            buffer.put_slice(&[0xc0, 0x00]);
            Ok(())
        })
    }

    fn encoded_size(&self, _: ProtocolVersion) -> usize {
        2
    }

    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> crate::Result<Self> {
        crate::decode_packet(buffer, PacketType::PingReq, version, |_, _| Ok(Self {}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_consumes_whole_packet() {
        let mut buffer = BytesMut::from(&[0xc0, 0x00, 0xc0, 0x00][..]);
        PingReqPacket::decode(&mut buffer, ProtocolVersion::V5).unwrap();
        assert_eq!(buffer.len(), 2);
    }
}
