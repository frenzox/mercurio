use std::io::{Cursor, Seek, SeekFrom};

use bytes::{Buf, BytesMut};

use crate::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    packets::{
        auth::AuthPacket, connack::ConnAckPacket, connect::ConnectPacket,
        control_packet_type::ControlPacketType, disconnect::DisconnectPacket,
        pingreq::PingReqPacket, pingresp::PingRespPacket, puback::PubAckPacket,
        pubcomp::PubCompPacket, publish::PublishPacket, pubrec::PubRecPacket, pubrel::PubRelPacket,
        suback::SubAckPacket, subscribe::SubscribePacket, unsuback::UnsubAckPacket,
        unsubscribe::UnsubscribePacket,
    },
    reason::ReasonCode,
    result::Result,
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

impl ControlPacket {
    pub fn check(src: &mut BytesMut) -> Result<()>
    where
        Self: Sized,
    {
        let mut peeker = Cursor::new(&src[..]);
        let remaining_len_pos = 1;

        let len = match peeker.seek(SeekFrom::End(0)) {
            Ok(n) => n,
            Err(err) => return Err(err.into()),
        };

        peeker.set_position(remaining_len_pos);

        let remaining_len = VariableByteInteger::decode(&mut peeker, None)?;
        if (len as usize - remaining_len.encoded_size() - 1) >= remaining_len.0 as usize {
            return Ok(());
        }

        Err(Error::PacketIncomplete)
    }

    pub fn parse(src: &mut BytesMut) -> Result<ControlPacket>
    where
        Self: Sized,
    {
        use ControlPacket::*;
        let mut peeker = Cursor::new(&src[..]);
        let packet_type: u8 = peeker.get_u8() >> 4;

        let packet = match packet_type {
            ConnectPacket::PACKET_TYPE => Connect(ConnectPacket::decode(src, None)?),
            ConnAckPacket::PACKET_TYPE => ConnAck(ConnAckPacket::decode(src, None)?),
            PublishPacket::PACKET_TYPE => Publish(PublishPacket::decode(src, None)?),
            PubAckPacket::PACKET_TYPE => PubAck(PubAckPacket::decode(src, None)?),
            PubRecPacket::PACKET_TYPE => PubRec(PubRecPacket::decode(src, None)?),
            PubRelPacket::PACKET_TYPE => PubRel(PubRelPacket::decode(src, None)?),
            PubCompPacket::PACKET_TYPE => PubComp(PubCompPacket::decode(src, None)?),
            SubscribePacket::PACKET_TYPE => Subscribe(SubscribePacket::decode(src, None)?),
            SubAckPacket::PACKET_TYPE => SubAck(SubAckPacket::decode(src, None)?),
            UnsubscribePacket::PACKET_TYPE => Unsubscribe(UnsubscribePacket::decode(src, None)?),
            PingReqPacket::PACKET_TYPE => PingReq(PingReqPacket::decode(src, None)?),
            PingRespPacket::PACKET_TYPE => PingResp(PingRespPacket::decode(src, None)?),
            DisconnectPacket::PACKET_TYPE => Disconnect(DisconnectPacket::decode(src, None)?),
            AuthPacket::PACKET_TYPE => Auth(AuthPacket::decode(src, None)?),
            _ => return Err(ReasonCode::MalformedPacket.into()),
        };

        Ok(packet)
    }
}
