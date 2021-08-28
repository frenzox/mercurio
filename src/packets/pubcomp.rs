use crate::control_packet::*;

pub struct PubCompPacket {}

impl ControlPacket for PubCompPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PubComp;
}
