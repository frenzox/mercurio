use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct PingRespPacket {
}

impl ControlPacket for PingRespPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PingResp;
}
