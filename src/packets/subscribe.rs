use crate::control_packet::*;

pub struct SubscribePacket {}

impl ControlPacket for SubscribePacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Subscribe;
}
