use crate::control_packet::*;

pub struct PublishPacket {}

impl ControlPacket for PublishPacket {
    const PACKET_TYPE: ControlPacketType = ControlPacketType::Publish;
}
