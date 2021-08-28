use crate::control_packet::*;

pub struct SubAckPacket {}

impl ControlPacket for SubAckPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::SubAck;
}
