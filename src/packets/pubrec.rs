use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct PubRecPacket {
}

impl ControlPacket for PubRecPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::PubRec;
}
