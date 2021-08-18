use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct SubscribePacket {
}

impl ControlPacket for SubscribePacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Subscribe;
}
