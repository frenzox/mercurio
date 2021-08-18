use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct PingReqPacket {
}

impl ControlPacket for PingReqPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PingReq;
}
