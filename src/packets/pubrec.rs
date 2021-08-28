use crate::control_packet::*;

pub struct PubRecPacket {}

impl ControlPacket for PubRecPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PubRec;
}
