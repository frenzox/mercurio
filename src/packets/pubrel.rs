use crate::control_packet::*;

pub struct PubRelPacket {}

impl ControlPacket for PubRelPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PubRel;
}
