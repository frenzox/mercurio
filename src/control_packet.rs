use std::io::{Cursor, Seek, SeekFrom};

use bytes::Buf;

use crate::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    packets::{
        auth::AuthPacket, connack::ConnAckPacket, connect::ConnectPacket,
        control_packet_type::ControlPacketType, disconnect::DisconnectPacket,
        pingreq::PingReqPacket, pingresp::PingRespPacket, puback::PubAckPacket,
        pubcomp::PubCompPacket, publish::PublishPacket, pubrec::PubRecPacket, pubrel::PubRelPacket,
        suback::SubAckPacket, subscribe::SubscribePacket, unsubscribe::UnsubscribePacket,
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
    PingReq(PingReqPacket),
    PingResp(PingRespPacket),
    Disconnect(DisconnectPacket),
    Auth(AuthPacket),
}

impl ControlPacket {
    pub fn check(src: &mut Cursor<&[u8]>) -> Result<()>
    where
        Self: Sized,
    {
        let remaining_len_pos = 1;

        let len = match src.seek(SeekFrom::End(0)) {
            Ok(n) => n,
            Err(err) => return Err(err.into()),
        };

        src.set_position(remaining_len_pos);

        let remaining_len = VariableByteInteger::decode(src, None)?;
        if (len as usize - remaining_len.encoded_size() - 1) >= remaining_len.0 as usize {
            return Ok(());
        }

        Err(Error::PacketIncomplete)
    }

    pub fn parse(src: &mut Cursor<&[u8]>) -> Result<ControlPacket>
    where
        Self: Sized,
    {
        let packet_type: u8 = src.get_u8() >> 4;

        let packet = match packet_type {
            ConnectPacket::PACKET_TYPE => ControlPacket::Connect(ConnectPacket::decode(src, None)?),
            ConnAckPacket::PACKET_TYPE => ControlPacket::ConnAck(ConnAckPacket::decode(src, None)?),
            PublishPacket::PACKET_TYPE => ControlPacket::Publish(PublishPacket::decode(src, None)?),
            PubAckPacket::PACKET_TYPE => ControlPacket::PubAck(PubAckPacket::decode(src, None)?),
            PubRecPacket::PACKET_TYPE => ControlPacket::PubRec(PubRecPacket::decode(src, None)?),
            PubRelPacket::PACKET_TYPE => ControlPacket::PubRel(PubRelPacket::decode(src, None)?),
            PubCompPacket::PACKET_TYPE => ControlPacket::PubComp(PubCompPacket::decode(src, None)?),
            SubscribePacket::PACKET_TYPE => {
                ControlPacket::Subscribe(SubscribePacket::decode(src, None)?)
            }
            SubAckPacket::PACKET_TYPE => ControlPacket::SubAck(SubAckPacket::decode(src, None)?),
            UnsubscribePacket::PACKET_TYPE => {
                ControlPacket::Unsubscribe(UnsubscribePacket::decode(src, None)?)
            }
            PingReqPacket::PACKET_TYPE => ControlPacket::PingReq(PingReqPacket::decode(src, None)?),
            PingRespPacket::PACKET_TYPE => {
                ControlPacket::PingResp(PingRespPacket::decode(src, None)?)
            }
            DisconnectPacket::PACKET_TYPE => {
                ControlPacket::Disconnect(DisconnectPacket::decode(src, None)?)
            }
            AuthPacket::PACKET_TYPE => ControlPacket::Auth(AuthPacket::decode(src, None)?),
            _ => return Err(ReasonCode::MalformedPacket.into()),
        };

        Ok(packet)
    }
}
