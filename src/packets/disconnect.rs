use crate::control_packet::*;

pub struct DisconnectPacket {}

impl ControlPacket for DisconnectPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Disconnect;
}
