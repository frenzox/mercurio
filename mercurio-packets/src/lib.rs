//! MQTT packet encoding and decoding for all MQTT versions.
//!
//! This crate provides packet types for MQTT 3.1, 3.1.1, and 5.0 and is
//! `no_std` compatible when the `std` feature is disabled.

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

pub mod auth;
pub mod connack;
pub mod connect;
pub mod disconnect;
pub mod pingreq;
pub mod pingresp;
pub mod puback;
pub mod pubcomp;
pub mod publish;
pub mod pubrec;
pub mod pubrel;
pub mod suback;
pub mod subscribe;
pub mod unsuback;
pub mod unsubscribe;

use core::convert::{TryFrom, TryInto};

use bytes::BytesMut;

use mercurio_core::{
    codec::{Decoder, Encoder, VariableByteInteger},
    error::Error,
    protocol::ProtocolVersion,
    reason::ReasonCode,
    Result,
};

use crate::{
    auth::AuthPacket, connack::ConnAckPacket, connect::ConnectPacket, disconnect::DisconnectPacket,
    pingreq::PingReqPacket, pingresp::PingRespPacket, puback::PubAckPacket, pubcomp::PubCompPacket,
    publish::PublishPacket, pubrec::PubRecPacket, pubrel::PubRelPacket, suback::SubAckPacket,
    subscribe::SubscribePacket, unsuback::UnsubAckPacket, unsubscribe::UnsubscribePacket,
};

#[repr(u8)]
#[derive(PartialEq, Eq, Debug)]
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

    fn try_from(value: u8) -> core::result::Result<Self, Self::Error> {
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
    /// Check if the buffer contains a complete MQTT packet.
    ///
    /// Returns `Ok(())` if the packet is complete, or `Err(Error::PacketIncomplete)`
    /// if more data is needed.
    pub fn check(src: &mut BytesMut) -> crate::Result<()> {
        let len = src.len();

        // Need at least 2 bytes (1 byte fixed header + 1 byte remaining length)
        if len < 2 {
            return Err(Error::PacketIncomplete);
        }

        // Read remaining length from position 1 (skip fixed header byte)
        let mut remaining_bytes = &src[1..];
        let remaining_len = VariableByteInteger::decode(&mut remaining_bytes)?;

        // Check if we have enough data: fixed header (1) + remaining length size + payload
        if (len - remaining_len.encoded_size() - 1) >= remaining_len.0 as usize {
            return Ok(());
        }

        Err(Error::PacketIncomplete)
    }

    pub fn parse(src: &mut BytesMut) -> crate::Result<ControlPacket> {
        // Default to MQTT 5.0 parsing
        Self::parse_with_version(src, ProtocolVersion::V5)
    }

    /// Parse a control packet with version-specific decoding.
    /// Use this when the protocol version is known (e.g., after CONNECT).
    pub fn parse_with_version(
        src: &mut BytesMut,
        version: ProtocolVersion,
    ) -> crate::Result<ControlPacket> {
        use ControlPacket::*;

        // Peek at the first byte to get packet type (don't consume)
        if src.is_empty() {
            return Err(Error::PacketIncomplete);
        }
        let packet_type: u8 = src[0] >> 4;

        let packet = match packet_type.try_into()? {
            PacketType::Connect => Connect(ConnectPacket::decode(src)?),
            PacketType::ConnAck => {
                if version.supports_properties() {
                    ConnAck(ConnAckPacket::decode(src)?)
                } else {
                    ConnAck(ConnAckPacket::decode_v3(src)?)
                }
            }
            PacketType::Publish => Publish(PublishPacket::decode(src)?),
            PacketType::PubAck => PubAck(PubAckPacket::decode(src)?),
            PacketType::PubRec => PubRec(PubRecPacket::decode(src)?),
            PacketType::PubRel => PubRel(PubRelPacket::decode(src)?),
            PacketType::PubComp => PubComp(PubCompPacket::decode(src)?),
            PacketType::Subscribe => Subscribe(SubscribePacket::decode(src)?),
            PacketType::SubAck => {
                if version.supports_properties() {
                    SubAck(SubAckPacket::decode(src)?)
                } else {
                    SubAck(SubAckPacket::decode_v3(src)?)
                }
            }
            PacketType::Unsubscribe => Unsubscribe(UnsubscribePacket::decode(src)?),
            PacketType::UnsubAck => {
                if version.supports_properties() {
                    UnsubAck(UnsubAckPacket::decode(src)?)
                } else {
                    UnsubAck(UnsubAckPacket::decode_v3(src)?)
                }
            }
            PacketType::PingReq => PingReq(PingReqPacket::decode(src)?),
            PacketType::PingResp => PingResp(PingRespPacket::decode(src)?),
            PacketType::Disconnect => Disconnect(DisconnectPacket::decode(src)?),
            PacketType::Auth => Auth(AuthPacket::decode(src)?),
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
