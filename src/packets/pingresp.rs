use crate::control_packet::*;

pub struct PingRespPacket {}

impl ControlPacket for PingRespPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PingResp;
}
