use bytes::{Bytes, BytesMut};

#[repr(u8)]
pub enum ControlPacketType {
    Connect = 0x01,
    ConnAck,
    Publish,
    PubAck,
    PubRec,
    PubRel,
    PubComp,
    Subscribe,
    SubAck,
    Unsubscribe,
    PingReq,
    PingResp,
    Disconnect,
    Auth,
}

pub trait ControlPacket {
    const PACKET_TYPE: ControlPacketType;
}
