use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct UnsubscribePacket {
}

impl ControlPacket for UnsubscribePacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Unsubscribe;
}
