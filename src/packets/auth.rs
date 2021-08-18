use crate::control_packet::*;
use bytes::{Bytes, BytesMut};

pub struct AuthPacket {
}

impl ControlPacket for AuthPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Auth;
}
