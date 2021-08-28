use crate::control_packet::*;

pub struct PubAckPacket {}

impl ControlPacket for PubAckPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PubAck;
}
