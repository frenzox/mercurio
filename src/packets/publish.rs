use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct PublishPacket {
}

impl ControlPacket for PublishPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Publish;
}
