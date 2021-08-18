use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct PubCompPacket {
}

impl ControlPacket for PubCompPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PubComp;
}
