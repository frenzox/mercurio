use std::io::{Cursor, Seek, SeekFrom};

use bytes::Buf;

use crate::{
    endec::{Decoder, Encoder, VariableByteInteger},
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

        if let Some(remaining_len) = VariableByteInteger::decode(src, None)? {
            if (len as usize - remaining_len.encoded_size() - 1) >= remaining_len.0 as usize {
                return Ok(());
            }
        }

        Err(Error::PacketIncomplete)
    }

    pub fn parse(src: &mut Cursor<&[u8]>) -> Result<ControlPacket>
    where
        Self: Sized,
    {
        let packet_type: u8 = src.get_u8() >> 4;

        let packet = match packet_type {
            ConnectPacket::PACKET_TYPE => {
                ControlPacket::Connect(ConnectPacket::decode(src, None)?.unwrap())
            }
            ConnAckPacket::PACKET_TYPE => {
                ControlPacket::ConnAck(ConnAckPacket::decode(src, None)?.unwrap())
            }
            PublishPacket::PACKET_TYPE => {
                ControlPacket::Publish(PublishPacket::decode(src, None)?.unwrap())
            }
            PubAckPacket::PACKET_TYPE => {
                ControlPacket::PubAck(PubAckPacket::decode(src, None)?.unwrap())
            }
            PubRecPacket::PACKET_TYPE => {
                ControlPacket::PubRec(PubRecPacket::decode(src, None)?.unwrap())
            }
            PubRelPacket::PACKET_TYPE => {
                ControlPacket::PubRel(PubRelPacket::decode(src, None)?.unwrap())
            }
            PubCompPacket::PACKET_TYPE => {
                ControlPacket::PubComp(PubCompPacket::decode(src, None)?.unwrap())
            }
            SubscribePacket::PACKET_TYPE => {
                ControlPacket::Subscribe(SubscribePacket::decode(src, None)?.unwrap())
            }
            SubAckPacket::PACKET_TYPE => {
                ControlPacket::SubAck(SubAckPacket::decode(src, None)?.unwrap())
            }
            UnsubscribePacket::PACKET_TYPE => {
                ControlPacket::Unsubscribe(UnsubscribePacket::decode(src, None)?.unwrap())
            }
            PingReqPacket::PACKET_TYPE => {
                ControlPacket::PingReq(PingReqPacket::decode(src, None)?.unwrap())
            }
            PingRespPacket::PACKET_TYPE => {
                ControlPacket::PingResp(PingRespPacket::decode(src, None)?.unwrap())
            }
            DisconnectPacket::PACKET_TYPE => {
                ControlPacket::Disconnect(DisconnectPacket::decode(src, None)?.unwrap())
            }
            AuthPacket::PACKET_TYPE => ControlPacket::Auth(AuthPacket::decode(src, None)?.unwrap()),
            _ => return Err(ReasonCode::MalformedPacket.into()),
        };

        Ok(packet)
    }
}
