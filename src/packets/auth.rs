use crate::control_packet::*;

pub struct AuthPacket {}

impl ControlPacket for AuthPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Auth;
}
