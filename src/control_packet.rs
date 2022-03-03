use core::panic;

#[repr(u8)]
#[derive(PartialEq, Debug)]
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

impl From<u8> for ControlPacketType {
    fn from(n: u8) -> Self {
        match n {
            0x01 => Self::Connect,
            0x02 => Self::ConnAck,
            0x03 => Self::Publish,
            0x04 => Self::PubRec,
            0x05 => Self::PubRel,
            0x06 => Self::PubComp,
            0x07 => Self::Subscribe,
            0x08 => Self::SubAck,
            0x09 => Self::Unsubscribe,
            0x0a => Self::PingReq,
            0x0b => Self::PingResp,
            0x0c => Self::Disconnect,
            0x0d => Self::Auth,
            _ => panic!(),
        }
    }
}

pub trait ControlPacket {
    fn packet_type(&self) -> ControlPacketType;
}
