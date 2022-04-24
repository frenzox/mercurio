use std::{
    convert::{TryFrom, TryInto},
    io::{Cursor, Seek, SeekFrom},
};

use bytes::{Buf, BytesMut};

use crate::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    packets::{
        auth::AuthPacket, connack::ConnAckPacket, connect::ConnectPacket,
        disconnect::DisconnectPacket, pingreq::PingReqPacket, pingresp::PingRespPacket,
        puback::PubAckPacket, pubcomp::PubCompPacket, publish::PublishPacket, pubrec::PubRecPacket,
        pubrel::PubRelPacket, suback::SubAckPacket, subscribe::SubscribePacket,
        unsuback::UnsubAckPacket, unsubscribe::UnsubscribePacket,
    },
    reason::ReasonCode,
};

#[repr(u8)]
#[derive(PartialEq, Debug)]
pub enum ControlPacket {
    Connect(ConnectPacket),
    ConnAck(ConnAckPacket),
    Publish(PublishPacket),
    PubAck(PubAckPacket),
    PubRec(PubRecPacket),
    PubRel(PubRelPacket),
    PubComp(PubCompPacket),
    Subscribe(SubscribePacket),
    SubAck(SubAckPacket),
    Unsubscribe(UnsubscribePacket),
    UnsubAck(UnsubAckPacket),
    PingReq(PingReqPacket),
    PingResp(PingRespPacket),
    Disconnect(DisconnectPacket),
    Auth(AuthPacket),
}

#[repr(C)]
pub enum PacketType {
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
    UnsubAck,
    PingReq,
    PingResp,
    Disconnect,
    Auth,
}

impl TryFrom<u8> for PacketType {
    type Error = ReasonCode;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        use PacketType::*;

        let res = match value {
            0x01 => Connect,
            0x02 => ConnAck,
            0x03 => Publish,
            0x04 => PubAck,
            0x05 => PubRec,
            0x06 => PubRel,
            0x07 => PubComp,
            0x08 => Subscribe,
            0x09 => SubAck,
            0x0a => Unsubscribe,
            0x0b => UnsubAck,
            0x0c => PingReq,
            0x0d => PingResp,
            0x0e => Disconnect,
            0x0f => Auth,
            _ => return Err(ReasonCode::MalformedPacket),
        };

        Ok(res)
    }
}

impl ControlPacket {
    pub fn check(src: &mut BytesMut) -> crate::Result<()> {
        let mut peeker = Cursor::new(&src[..]);
        let remaining_len_pos = 1;

        let len = match peeker.seek(SeekFrom::End(0)) {
            Ok(n) => n,
            Err(err) => return Err(err.into()),
        };

        peeker.set_position(remaining_len_pos);

        let remaining_len = VariableByteInteger::decode(&mut peeker)?;
        if (len as usize - remaining_len.encoded_size() - 1) >= remaining_len.0 as usize {
            return Ok(());
        }

        Err(Error::PacketIncomplete)
    }

    pub fn parse(src: &mut BytesMut) -> crate::Result<ControlPacket> {
        use ControlPacket::*;

        let mut peeker = Cursor::new(&src[..]);
        let packet_type: u8 = peeker.get_u8() >> 4;

        let packet = match packet_type.try_into()? {
            PacketType::Connect => Connect(ConnectPacket::decode(src)?),
            PacketType::ConnAck => ConnAck(ConnAckPacket::decode(src)?),
            PacketType::Publish => Publish(PublishPacket::decode(src)?),
            PacketType::PubAck => PubAck(PubAckPacket::decode(src)?),
            PacketType::PubRec => PubRec(PubRecPacket::decode(src)?),
            PacketType::PubRel => PubRel(PubRelPacket::decode(src)?),
            PacketType::PubComp => PubComp(PubCompPacket::decode(src)?),
            PacketType::Subscribe => Subscribe(SubscribePacket::decode(src)?),
            PacketType::SubAck => SubAck(SubAckPacket::decode(src)?),
            PacketType::Unsubscribe => Unsubscribe(UnsubscribePacket::decode(src)?),
            PacketType::PingReq => PingReq(PingReqPacket::decode(src)?),
            PacketType::PingResp => PingResp(PingRespPacket::decode(src)?),
            PacketType::Disconnect => Disconnect(DisconnectPacket::decode(src)?),
            PacketType::Auth => Auth(AuthPacket::decode(src)?),
            _ => return Err(ReasonCode::MalformedPacket.into()),
        };

        Ok(packet)
    }
}

impl Encoder for ControlPacket {
    fn encode(&self, buffer: &mut BytesMut) {
        use ControlPacket::*;

        match self {
            Connect(p) => p.encode(buffer),
            ConnAck(p) => p.encode(buffer),
            Publish(p) => p.encode(buffer),
            PubAck(p) => p.encode(buffer),
            PubRec(p) => p.encode(buffer),
            PubRel(p) => p.encode(buffer),
            PubComp(p) => p.encode(buffer),
            Subscribe(p) => p.encode(buffer),
            SubAck(p) => p.encode(buffer),
            Unsubscribe(p) => p.encode(buffer),
            UnsubAck(p) => p.encode(buffer),
            PingReq(p) => p.encode(buffer),
            PingResp(p) => p.encode(buffer),
            Disconnect(p) => p.encode(buffer),
            Auth(p) => p.encode(buffer),
        }
    }
}
