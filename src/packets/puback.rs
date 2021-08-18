use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct PubAckPacket {
}

impl ControlPacket for PubAckPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PubAck;
}
