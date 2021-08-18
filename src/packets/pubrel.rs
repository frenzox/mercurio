use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct PubRelPacket {
}

impl ControlPacket for PubRelPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PubRel;
}
