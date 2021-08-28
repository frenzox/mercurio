use crate::control_packet::*;

pub struct PingReqPacket {}

impl ControlPacket for PingReqPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PingReq;
}
