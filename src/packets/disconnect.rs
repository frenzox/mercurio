use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct DisconnectPacket {
}

impl ControlPacket for DisconnectPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Disconnect;
}
