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
        if src.len() >= Self::frame_len(src)? {
            Ok(())
        } else {
            Err(Error::PacketIncomplete)
        }
    }

    /// The protocol version the CONNECT packet at the start of `frame` asks
    /// for, read without decoding the rest, so even a malformed CONNECT can
    /// be refused in the client's own protocol.
    pub fn connect_version(frame: &[u8]) -> Option<ProtocolVersion> {
        if frame.first()? >> 4 != PacketType::Connect as u8 {
            return None;
        }
        let mut rest = frame.get(1..)?;
        VariableByteInteger::decode(&mut rest).ok()?;
        let name_len = usize::from(u16::from_be_bytes([*rest.first()?, *rest.get(1)?]));
        let name = core::str::from_utf8(rest.get(2..2 + name_len)?).ok()?;
        let level = *rest.get(2 + name_len)?;
        ProtocolVersion::from_name_and_level(name, level)
    }

    /// Total size of the packet at the start of `src`, fixed header included.
    /// Only needs the fixed header to be available.
    pub fn frame_len(src: &[u8]) -> crate::Result<usize> {
        let Some(mut remaining_bytes) = src.get(1..) else {
            return Err(Error::PacketIncomplete);
        };
        let remaining_len = VariableByteInteger::decode(&mut remaining_bytes)?;
        let header_len = src.len() - remaining_bytes.len();
        Ok(header_len + remaining_len.0 as usize)
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
        let frame_len = Self::frame_len(src)?;
        if src.len() < frame_len {
            return Err(Error::PacketIncomplete);
        }

        // Decode from exactly this packet's bytes, so a malformed packet can't
        // read into the next one. Running short within it is malformed.
        let mut frame = src.split_to(frame_len);
        let packet = Self::decode_frame(&mut frame, version).map_err(|e| match e {
            Error::PacketIncomplete => ReasonCode::MalformedPacket.into(),
            e => e,
        })?;
        if !frame.is_empty() {
            return Err(ReasonCode::MalformedPacket.into());
        }

        Ok(packet)
    }

    fn decode_frame(src: &mut BytesMut, version: ProtocolVersion) -> crate::Result<ControlPacket> {
        use ControlPacket::*;

        let packet_type: u8 = src[0] >> 4;
        let flags = src[0] & 0x0f;

        // Fixed header flags are reserved except on PUBLISH, and must be
        // 0b0010 on PUBREL, SUBSCRIBE and UNSUBSCRIBE [MQTT-2.1.3-1]
        let expected_flags = match packet_type {
            0x03 => flags,
            0x06 | 0x08 | 0x0a => 0b0010,
            _ => 0,
        };
        if flags != expected_flags {
            return Err(ReasonCode::MalformedPacket.into());
        }

        let packet = match packet_type.try_into()? {
            PacketType::Connect => Connect(ConnectPacket::decode(src)?),
            PacketType::ConnAck => {
                if version.supports_properties() {
                    ConnAck(ConnAckPacket::decode(src)?)
                } else {
                    ConnAck(ConnAckPacket::decode_v3(src)?)
                }
            }
            PacketType::Publish => {
                if version.supports_properties() {
                    Publish(PublishPacket::decode(src)?)
                } else {
                    Publish(PublishPacket::decode_v3(src)?)
                }
            }
            PacketType::PubAck => PubAck(PubAckPacket::decode(src)?),
            PacketType::PubRec => PubRec(PubRecPacket::decode(src)?),
            PacketType::PubRel => PubRel(PubRelPacket::decode(src)?),
            PacketType::PubComp => PubComp(PubCompPacket::decode(src)?),
            PacketType::Subscribe => {
                if version.supports_properties() {
                    Subscribe(SubscribePacket::decode(src)?)
                } else {
                    Subscribe(SubscribePacket::decode_v3(src)?)
                }
            }
            PacketType::SubAck => {
                if version.supports_properties() {
                    SubAck(SubAckPacket::decode(src)?)
                } else {
                    SubAck(SubAckPacket::decode_v3(src)?)
                }
            }
            PacketType::Unsubscribe => {
                if version.supports_properties() {
                    Unsubscribe(UnsubscribePacket::decode(src)?)
                } else {
                    Unsubscribe(UnsubscribePacket::decode_v3(src)?)
                }
            }
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

    /// Encode a control packet in the wire format of the given protocol version.
    pub fn encode_with_version(&self, buffer: &mut BytesMut, version: ProtocolVersion) {
        use ControlPacket::*;

        if version.supports_properties() {
            return self.encode(buffer);
        }

        match self {
            Publish(p) => p.encode_v3(buffer),
            Subscribe(p) => p.encode_v3(buffer),
            Unsubscribe(p) => p.encode_v3(buffer),
            Disconnect(p) => p.encode_v3(buffer),
            p => p.encode(buffer),
        }
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

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use mercurio_core::qos::QoS;

    use super::*;

    fn parse_v3(bytes: &[u8]) -> ControlPacket {
        let mut buffer = BytesMut::from(bytes);
        let packet =
            ControlPacket::parse_with_version(&mut buffer, ProtocolVersion::V3_1_1).unwrap();
        assert!(buffer.is_empty(), "packet not fully consumed");
        packet
    }

    #[test]
    fn test_v3_publish_round_trip() {
        // QoS 1 PUBLISH to "a/b", packet id 10, payload "hi"
        let wire = [
            0x32, 0x09, 0x00, 0x03, b'a', b'/', b'b', 0x00, 0x0a, b'h', b'i',
        ];

        let packet = parse_v3(&wire);
        let ControlPacket::Publish(ref publish) = packet else {
            panic!("Expected Publish, got {:?}", packet);
        };
        assert_eq!(publish.topic_name, "a/b");
        assert_eq!(publish.qos_level, QoS::AtLeastOnce);
        assert_eq!(publish.packet_id, Some(10));
        assert_eq!(publish.payload, Some(Bytes::from("hi")));

        let mut encoded = BytesMut::new();
        packet.encode_with_version(&mut encoded, ProtocolVersion::V3_1_1);
        assert_eq!(&encoded[..], &wire[..]);
    }

    #[test]
    fn test_v3_subscribe_decode() {
        // SUBSCRIBE packet id 1, "a/#" at QoS 1
        let wire = [0x82, 0x08, 0x00, 0x01, 0x00, 0x03, b'a', b'/', b'#', 0x01];

        let ControlPacket::Subscribe(subscribe) = parse_v3(&wire) else {
            panic!("Expected Subscribe");
        };
        assert_eq!(subscribe.packet_id, 1);
        assert_eq!(subscribe.payload.len(), 1);
        assert_eq!(subscribe.payload[0].topic_filter, "a/#");
        assert_eq!(subscribe.payload[0].subs_opt.qos, QoS::AtLeastOnce);
    }

    #[test]
    fn test_v3_subscribe_rejects_reserved_option_bits() {
        // Same SUBSCRIBE as above but with the MQTT 5 No Local bit set
        let wire = [0x82, 0x08, 0x00, 0x01, 0x00, 0x03, b'a', b'/', b'#', 0x05];
        let mut buffer = BytesMut::from(&wire[..]);

        assert!(ControlPacket::parse_with_version(&mut buffer, ProtocolVersion::V3_1_1).is_err());

        // The same option byte is valid in MQTT 5, which adds a properties length
        let wire = [
            0x82, 0x09, 0x00, 0x01, 0x00, 0x00, 0x03, b'a', b'/', b'#', 0x05,
        ];
        let mut buffer = BytesMut::from(&wire[..]);
        assert!(ControlPacket::parse_with_version(&mut buffer, ProtocolVersion::V5).is_ok());
    }

    #[test]
    fn test_v3_unsubscribe_decode() {
        // UNSUBSCRIBE packet id 2, "a/#"
        let wire = [0xa2, 0x07, 0x00, 0x02, 0x00, 0x03, b'a', b'/', b'#'];

        let ControlPacket::Unsubscribe(unsubscribe) = parse_v3(&wire) else {
            panic!("Expected Unsubscribe");
        };
        assert_eq!(unsubscribe.packet_id, 2);
        assert_eq!(unsubscribe.payload[0].topic_filter, "a/#");
    }
}

#[cfg(test)]
mod robustness_tests {
    use std::panic::{catch_unwind, AssertUnwindSafe};

    use super::*;

    /// Valid packets of every type the broker or client may receive.
    fn samples() -> Vec<Vec<u8>> {
        vec![
            // CONNECT v5 with will, username and password
            vec![
                0x10, 0x20, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0xc6, 0x00, 0x3c, 0x00, 0x00,
                0x01, b'c', 0x05, 0x18, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, b'w', 0x00, 0x01, b'x',
                0x00, 0x01, b'u', 0x00, 0x01, b'p',
            ],
            // CONNECT v3.1.1
            vec![
                0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x02, 0x00, 0x3c, 0x00, 0x01,
                b'c',
            ],
            // CONNACK v5
            vec![0x20, 0x03, 0x00, 0x00, 0x00],
            // PUBLISH QoS 1 v5 with a property
            vec![
                0x32, 0x0b, 0x00, 0x01, b't', 0x00, 0x01, 0x02, 0x01, 0x01, b'h', b'i', b'!',
            ],
            // PUBLISH QoS 0 v3
            vec![0x30, 0x05, 0x00, 0x01, b't', b'h', b'i'],
            // PUBACK, PUBREC, PUBREL, PUBCOMP with reason and properties
            vec![0x40, 0x04, 0x00, 0x01, 0x10, 0x00],
            vec![0x50, 0x04, 0x00, 0x01, 0x10, 0x00],
            vec![0x62, 0x04, 0x00, 0x01, 0x92, 0x00],
            vec![0x70, 0x04, 0x00, 0x01, 0x92, 0x00],
            // SUBSCRIBE v5 and v3
            vec![0x82, 0x08, 0x00, 0x01, 0x00, 0x00, 0x02, b'a', b'#', 0x01],
            vec![0x82, 0x07, 0x00, 0x01, 0x00, 0x02, b'a', b'#', 0x01],
            // SUBACK v5
            vec![0x90, 0x04, 0x00, 0x01, 0x00, 0x01],
            // UNSUBSCRIBE v5 and v3
            vec![0xa2, 0x06, 0x00, 0x01, 0x00, 0x00, 0x01, b'a'],
            vec![0xa2, 0x05, 0x00, 0x01, 0x00, 0x01, b'a'],
            // UNSUBACK v5
            vec![0xb0, 0x04, 0x00, 0x01, 0x00, 0x00],
            // PINGREQ, PINGRESP
            vec![0xc0, 0x00],
            vec![0xd0, 0x00],
            // DISCONNECT with session expiry
            vec![0xe0, 0x07, 0x00, 0x05, 0x11, 0x00, 0x00, 0x00, 0x00],
            // AUTH
            vec![0xf0, 0x02, 0x18, 0x00],
        ]
    }

    /// Parses `bytes` like a connection does; panics are the failure.
    fn parse_all(bytes: &[u8]) {
        for version in [ProtocolVersion::V5, ProtocolVersion::V3_1_1] {
            let mut buffer = BytesMut::from(bytes);
            let result = catch_unwind(AssertUnwindSafe(|| {
                while ControlPacket::check(&mut buffer).is_ok() {
                    if ControlPacket::parse_with_version(&mut buffer, version).is_err() {
                        break;
                    }
                }
            }));
            assert!(
                result.is_ok(),
                "parser panicked on {:02x?} ({})",
                bytes,
                version
            );
        }
    }

    #[test]
    fn test_valid_packets_parse_completely() {
        let v3 = [1, 4, 10, 13];
        let mut packets: Vec<(Vec<u8>, ProtocolVersion)> = samples()
            .into_iter()
            .enumerate()
            .map(|(i, p)| {
                let version = if v3.contains(&i) {
                    ProtocolVersion::V3_1_1
                } else {
                    ProtocolVersion::V5
                };
                (p, version)
            })
            .collect();
        // Optional fields left out, as the spec allows
        packets.extend([
            (vec![0x40, 0x03, 0x00, 0x01, 0x10], ProtocolVersion::V5),
            (vec![0x62, 0x03, 0x00, 0x01, 0x92], ProtocolVersion::V5),
            (vec![0xe0, 0x00], ProtocolVersion::V5),
            (vec![0xf0, 0x00], ProtocolVersion::V5),
        ]);

        for (bytes, version) in packets {
            let mut buffer = BytesMut::from(&bytes[..]);
            // Followed by another packet that must stay untouched
            buffer.extend_from_slice(&[0xc0, 0x00]);
            let result = ControlPacket::parse_with_version(&mut buffer, version);
            assert!(result.is_ok(), "{:02x?}: {:?}", bytes, result);
            assert_eq!(&buffer[..], &[0xc0, 0x00], "{:02x?}", bytes);
        }
    }

    #[test]
    fn test_connect_request_information_properties() {
        // CONNECT v5 with Request Response Information 1 and Request
        // Problem Information 0
        let wire = [
            0x10, 0x12, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02, 0x00, 0x3c, 0x04, 0x19,
            0x01, 0x17, 0x00, 0x00, 0x01, b'c',
        ];
        let mut buffer = BytesMut::from(&wire[..]);
        let Ok(ControlPacket::Connect(connect)) =
            ControlPacket::parse_with_version(&mut buffer, ProtocolVersion::V5)
        else {
            panic!("Expected Connect");
        };
        let properties = connect.properties.unwrap();
        assert_eq!(properties.request_response_information.unwrap().value, 1);
        assert_eq!(properties.request_problem_information.unwrap().value, 0);
    }

    #[test]
    fn test_repeated_properties_are_protocol_errors() {
        // PUBLISH QoS 0 to "t" with Content Type "a" twice
        let repeated = [
            0x30, 0x0c, 0x00, 0x01, b't', 0x08, 0x03, 0x00, 0x01, b'a', 0x03, 0x00, 0x01, b'a',
        ];
        let mut buffer = BytesMut::from(&repeated[..]);
        assert!(matches!(
            ControlPacket::parse_with_version(&mut buffer, ProtocolVersion::V5),
            Err(Error::MQTTReasonCode(ReasonCode::ProtocolError))
        ));

        // The same with two User Properties, which may repeat
        let user_properties = [
            0x30, 0x10, 0x00, 0x01, b't', 0x0c, 0x26, 0x00, 0x01, b'k', 0x00, 0x00, 0x26, 0x00,
            0x01, b'k', 0x00, 0x00,
        ];
        let mut buffer = BytesMut::from(&user_properties[..]);
        assert!(ControlPacket::parse_with_version(&mut buffer, ProtocolVersion::V5).is_ok());
    }

    #[test]
    fn test_connect_version_is_read_from_raw_bytes() {
        let v5 = [0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x05, 0x02];
        let v311 = [0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x02];
        assert_eq!(
            ControlPacket::connect_version(&v5),
            Some(ProtocolVersion::V5)
        );
        assert_eq!(
            ControlPacket::connect_version(&v311),
            Some(ProtocolVersion::V3_1_1)
        );
        assert_eq!(ControlPacket::connect_version(&[0xc0, 0x00]), None);
        assert_eq!(ControlPacket::connect_version(&v5[..6]), None);
    }

    #[test]
    fn test_reserved_flags_are_malformed() {
        let malformed: [(&[u8], ProtocolVersion); 6] = [
            // PINGREQ with a reserved flag set
            (&[0xc1, 0x00], ProtocolVersion::V5),
            // SUBSCRIBE without the mandatory 0b0010 flags
            (
                &[0x80, 0x08, 0x00, 0x01, 0x00, 0x00, 0x02, b'a', b'#', 0x01],
                ProtocolVersion::V5,
            ),
            // PUBREL without the mandatory 0b0010 flags
            (&[0x60, 0x02, 0x00, 0x01], ProtocolVersion::V5),
            // QoS 0 PUBLISH with DUP set
            (
                &[0x38, 0x05, 0x00, 0x01, b't', b'h', b'i'],
                ProtocolVersion::V3_1_1,
            ),
            // CONNECT with will QoS 1 but no will flag
            (
                &[
                    0x10, 0x0d, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x0a, 0x00, 0x3c, 0x00,
                    0x01, b'c',
                ],
                ProtocolVersion::V3_1_1,
            ),
            // MQTT 3.1.1 CONNECT with a password but no user name
            (
                &[
                    0x10, 0x10, 0x00, 0x04, b'M', b'Q', b'T', b'T', 0x04, 0x42, 0x00, 0x3c, 0x00,
                    0x01, b'c', 0x00, 0x01, b'p',
                ],
                ProtocolVersion::V3_1_1,
            ),
        ];

        for (bytes, version) in malformed {
            let mut buffer = BytesMut::from(bytes);
            assert!(
                matches!(
                    ControlPacket::parse_with_version(&mut buffer, version),
                    Err(Error::MQTTReasonCode(ReasonCode::MalformedPacket))
                ),
                "{:02x?}",
                bytes
            );
        }
    }

    #[test]
    fn test_mutated_packets_never_panic() {
        for sample in samples() {
            for len in 0..=sample.len() {
                parse_all(&sample[..len]);
            }
            for i in 0..sample.len() {
                for value in [0x00, 0x01, 0x7f, 0x80, 0xff, sample[i].wrapping_add(1)] {
                    let mut mutated = sample.clone();
                    mutated[i] = value;
                    parse_all(&mutated);
                }
            }
        }
    }

    #[test]
    fn test_random_bytes_never_panic() {
        // Small deterministic LCG so failures are reproducible without a dependency
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (state >> 33) as u8
        };

        for _ in 0..20_000 {
            let len = 2 + next() as usize % 40;
            let mut bytes: Vec<u8> = (0..len).map(|_| next()).collect();
            // Keep the remaining length plausible so packets reach the decoders
            bytes[1] = (len - 2) as u8 & 0x7f;
            parse_all(&bytes);
        }
    }
}
