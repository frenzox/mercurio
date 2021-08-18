use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct ConnAckPacket {
}

impl ControlPacket for ConnAckPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::ConnAck;
}
