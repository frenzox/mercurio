use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct SubAckPacket {
}

impl ControlPacket for SubAckPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::SubAck;
}
