use crate::control_packet::*;

pub struct UnsubscribePacket {}

impl ControlPacket for UnsubscribePacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Unsubscribe;
}
