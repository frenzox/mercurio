#![doc = include_str!("../README.md")]
#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(not(feature = "std"))]
extern crate alloc;

// Tests use std even when the library is built without it
#[cfg(all(test, not(feature = "std")))]
#[macro_use]
extern crate std;

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

use bytes::{buf::Take, Buf, BytesMut};

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

pub use mercurio_core::protocol::PacketType;

#[repr(u8)]
#[derive(Clone, PartialEq, Eq, Debug)]
/// Any MQTT control packet, as decoded from or encoded to the wire.
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
    /// The type of this packet.
    pub fn packet_type(&self) -> PacketType {
        use ControlPacket::*;

        match self {
            Connect(_) => PacketType::Connect,
            ConnAck(_) => PacketType::ConnAck,
            Publish(_) => PacketType::Publish,
            PubAck(_) => PacketType::PubAck,
            PubRec(_) => PacketType::PubRec,
            PubRel(_) => PacketType::PubRel,
            PubComp(_) => PacketType::PubComp,
            Subscribe(_) => PacketType::Subscribe,
            SubAck(_) => PacketType::SubAck,
            Unsubscribe(_) => PacketType::Unsubscribe,
            UnsubAck(_) => PacketType::UnsubAck,
            PingReq(_) => PacketType::PingReq,
            PingResp(_) => PacketType::PingResp,
            Disconnect(_) => PacketType::Disconnect,
            Auth(_) => PacketType::Auth,
        }
    }

    /// Check if the buffer contains a complete MQTT packet.
    ///
    /// Returns `Ok(())` if the packet is complete, or `Err(Error::PacketIncomplete)`
    /// if more data is needed.
    pub fn check(src: &[u8]) -> crate::Result<()> {
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

    /// Removes one packet from the front of `src` and decodes it in the
    /// wire format of `version`.
    ///
    /// Returns `Error::PacketIncomplete`, with `src` untouched, until the
    /// whole packet is there. A packet that is malformed, or that doesn't use
    /// all its own bytes, is still removed from `src` and gives a reason code
    /// error, usually `MalformedPacket`. A CONNECT is read in the version it
    /// names, whatever `version` is.
    pub fn parse(src: &mut BytesMut, version: ProtocolVersion) -> crate::Result<ControlPacket> {
        use ControlPacket::*;

        let frame_len = Self::frame_len(src)?;
        if src.len() < frame_len {
            return Err(Error::PacketIncomplete);
        }

        // Decode from exactly this packet's bytes, so a malformed packet can't
        // read into the next one
        let mut frame = src.split_to(frame_len).freeze();
        let frame = &mut frame;
        let packet = match (frame[0] >> 4).try_into()? {
            PacketType::Connect => Connect(ConnectPacket::decode(frame, version)?),
            PacketType::ConnAck => ConnAck(ConnAckPacket::decode(frame, version)?),
            PacketType::Publish => Publish(PublishPacket::decode(frame, version)?),
            PacketType::PubAck => PubAck(PubAckPacket::decode(frame, version)?),
            PacketType::PubRec => PubRec(PubRecPacket::decode(frame, version)?),
            PacketType::PubRel => PubRel(PubRelPacket::decode(frame, version)?),
            PacketType::PubComp => PubComp(PubCompPacket::decode(frame, version)?),
            PacketType::Subscribe => Subscribe(SubscribePacket::decode(frame, version)?),
            PacketType::SubAck => SubAck(SubAckPacket::decode(frame, version)?),
            PacketType::Unsubscribe => Unsubscribe(UnsubscribePacket::decode(frame, version)?),
            PacketType::UnsubAck => UnsubAck(UnsubAckPacket::decode(frame, version)?),
            PacketType::PingReq => PingReq(PingReqPacket::decode(frame, version)?),
            PacketType::PingResp => PingResp(PingRespPacket::decode(frame, version)?),
            PacketType::Disconnect => Disconnect(DisconnectPacket::decode(frame, version)?),
            PacketType::Auth => Auth(AuthPacket::decode(frame, version)?),
        };

        Ok(packet)
    }

    /// Appends the packet in the wire format of `version`.
    ///
    /// MQTT 3.x has no reason codes or properties: PUBACK, PUBREC, PUBREL,
    /// PUBCOMP and DISCONNECT leave theirs out, CONNACK maps its reason code
    /// to a 3.x return code, and SUBACK writes every failure as 0x80.
    pub fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion) {
        use ControlPacket::*;

        match self {
            Connect(p) => p.encode(buffer, version),
            ConnAck(p) => p.encode(buffer, version),
            Publish(p) => p.encode(buffer, version),
            PubAck(p) => p.encode(buffer, version),
            PubRec(p) => p.encode(buffer, version),
            PubRel(p) => p.encode(buffer, version),
            PubComp(p) => p.encode(buffer, version),
            Subscribe(p) => p.encode(buffer, version),
            SubAck(p) => p.encode(buffer, version),
            Unsubscribe(p) => p.encode(buffer, version),
            UnsubAck(p) => p.encode(buffer, version),
            PingReq(p) => p.encode(buffer, version),
            PingResp(p) => p.encode(buffer, version),
            Disconnect(p) => p.encode(buffer, version),
            Auth(p) => p.encode(buffer, version),
        }
    }

    /// Number of bytes [`encode`](Self::encode) writes for `version`.
    pub fn encoded_size(&self, version: ProtocolVersion) -> usize {
        use ControlPacket::*;

        match self {
            Connect(p) => p.encoded_size(version),
            ConnAck(p) => p.encoded_size(version),
            Publish(p) => p.encoded_size(version),
            PubAck(p) => p.encoded_size(version),
            PubRec(p) => p.encoded_size(version),
            PubRel(p) => p.encoded_size(version),
            PubComp(p) => p.encoded_size(version),
            Subscribe(p) => p.encoded_size(version),
            SubAck(p) => p.encoded_size(version),
            Unsubscribe(p) => p.encoded_size(version),
            UnsubAck(p) => p.encoded_size(version),
            PingReq(p) => p.encoded_size(version),
            PingResp(p) => p.encoded_size(version),
            Disconnect(p) => p.encoded_size(version),
            Auth(p) => p.encoded_size(version),
        }
    }
}

/// One kind of control packet, written and read in the wire format of a
/// protocol version. Each packet type implements it; [`ControlPacket`] does
/// the same for any packet.
pub trait Packet: Sized {
    /// Appends the packet in the wire format of `version`.
    fn encode(&self, buffer: &mut BytesMut, version: ProtocolVersion);

    /// Number of bytes [`encode`](Packet::encode) writes for `version`.
    fn encoded_size(&self, version: ProtocolVersion) -> usize;

    /// Reads one packet in the wire format of `version` from the front of
    /// `buffer`, and advances past it.
    ///
    /// Fails with [`Error::PacketIncomplete`] when `buffer` ends before the
    /// packet does, and with a reason code when the packet is of another
    /// type or malformed. On failure `buffer` may be partly consumed.
    fn decode<T: Buf>(buffer: &mut T, version: ProtocolVersion) -> Result<Self>;
}

/// Decodes one packet of `packet_type` from the front of `buffer`: reads
/// the fixed header, then hands `decode_body` the flags and a reader over
/// exactly the Remaining Length bytes.
///
/// Fails with [`Error::PacketIncomplete`] when `buffer` ends before the
/// packet does, and with [`ReasonCode::MalformedPacket`] on another packet
/// type, on reserved flags [MQTT-2.1.3-1], when the body runs short, or when
/// `decode_body` leaves bytes of the packet unread.
pub(crate) fn decode_packet<T, P, F>(
    buffer: &mut T,
    packet_type: PacketType,
    version: ProtocolVersion,
    decode_body: F,
) -> Result<P>
where
    T: Buf,
    F: FnOnce(u8, &mut Take<&mut T>) -> Result<P>,
{
    let first_byte = u8::decode(buffer)?;
    if first_byte >> 4 != packet_type as u8 {
        return Err(ReasonCode::MalformedPacket.into());
    }

    // Flags are free on PUBLISH, 0b0010 on PUBREL, SUBSCRIBE and
    // UNSUBSCRIBE, and zero elsewhere. MQTT 3.1 sets DUP when it resends one
    // of those three (3.1 section 2.1); 3.1.1 reserved that bit.
    let flags = first_byte & 0x0f;
    let expected_flags = match packet_type {
        PacketType::Publish => flags,
        PacketType::PubRel | PacketType::Subscribe | PacketType::Unsubscribe
            if version == ProtocolVersion::V3_1 && flags == 0b1010 =>
        {
            flags
        }
        PacketType::PubRel | PacketType::Subscribe | PacketType::Unsubscribe => 0b0010,
        _ => 0,
    };
    if flags != expected_flags {
        return Err(ReasonCode::MalformedPacket.into());
    }

    let remaining_len = VariableByteInteger::decode(buffer)?.0 as usize;
    if buffer.remaining() < remaining_len {
        return Err(Error::PacketIncomplete);
    }

    // The whole packet is there, so running short inside it is malformed
    let mut body = buffer.take(remaining_len);
    let packet = decode_body(flags, &mut body).map_err(|e| match e {
        Error::PacketIncomplete => ReasonCode::MalformedPacket.into(),
        e => e,
    })?;
    if body.has_remaining() {
        return Err(ReasonCode::MalformedPacket.into());
    }

    Ok(packet)
}

/// Reads an MQTT 5 reason code. A code that `packet_type` doesn't list is
/// malformed, like a code no packet lists.
pub(crate) fn decode_reason<T: Buf>(buffer: &mut T, packet_type: PacketType) -> Result<ReasonCode> {
    let reason = ReasonCode::decode(buffer)?;
    if !reason.is_valid_for(packet_type) {
        return Err(ReasonCode::MalformedPacket.into());
    }
    Ok(reason)
}

/// Reads a Packet Identifier, which is never 0 [MQTT-2.2.1-3].
pub(crate) fn decode_packet_id<T: Buf>(buffer: &mut T) -> Result<u16> {
    match u16::decode(buffer)? {
        0 => Err(ReasonCode::ProtocolError.into()),
        packet_id => Ok(packet_id),
    }
}

/// Size of a whole packet whose body is `remaining_len` bytes: the first
/// byte, the Remaining Length and the body.
pub(crate) fn packet_size(remaining_len: usize) -> usize {
    1 + VariableByteInteger(remaining_len as u32).encoded_size() + remaining_len
}

/// Size of a property list with its length prefix.
pub(crate) fn property_list_size<P: Encoder>(properties: &P) -> usize {
    let len = properties.encoded_size();
    VariableByteInteger(len as u32).encoded_size() + len
}

/// A property block that sets nothing reads as no block at all: the two
/// mean the same, and senders use either.
pub(crate) fn present<P: Default + PartialEq>(properties: P) -> Option<P> {
    (properties != P::default()).then_some(properties)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use mercurio_core::qos::QoS;

    use super::*;

    fn parse_v3(bytes: &[u8]) -> ControlPacket {
        let mut buffer = BytesMut::from(bytes);
        let packet = ControlPacket::parse(&mut buffer, ProtocolVersion::V3_1_1).unwrap();
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
        packet.encode(&mut encoded, ProtocolVersion::V3_1_1);
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

        assert!(ControlPacket::parse(&mut buffer, ProtocolVersion::V3_1_1).is_err());

        // The same option byte is valid in MQTT 5, which adds a properties length
        let wire = [
            0x82, 0x09, 0x00, 0x01, 0x00, 0x00, 0x03, b'a', b'/', b'#', 0x05,
        ];
        let mut buffer = BytesMut::from(&wire[..]);
        assert!(ControlPacket::parse(&mut buffer, ProtocolVersion::V5).is_ok());
    }

    #[test]
    fn test_v3_acks_carry_only_the_packet_id() {
        let packet = ControlPacket::PubRel(pubrel::PubRelPacket {
            packet_id: 7,
            reason: ReasonCode::PacketIdentifierNotFound,
            properties: None,
        });
        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, ProtocolVersion::V3_1_1);
        assert_eq!(&encoded[..], [0x62, 0x02, 0x00, 0x07]);
    }

    #[test]
    fn test_dup_on_pubrel_subscribe_unsubscribe_only_in_mqtt_3_1() {
        // PUBREL, SUBSCRIBE "a" at QoS 0 and UNSUBSCRIBE "a", DUP set
        let resent: [&[u8]; 3] = [
            &[0x6a, 0x02, 0x00, 0x01],
            &[0x8a, 0x06, 0x00, 0x01, 0x00, 0x01, b'a', 0x00],
            &[0xaa, 0x05, 0x00, 0x01, 0x00, 0x01, b'a'],
        ];
        for bytes in resent {
            let parse = |version| ControlPacket::parse(&mut BytesMut::from(bytes), version);
            assert!(parse(ProtocolVersion::V3_1).is_ok(), "{bytes:02x?}");
            assert!(
                matches!(
                    parse(ProtocolVersion::V3_1_1),
                    Err(Error::Reason(ReasonCode::MalformedPacket))
                ),
                "{bytes:02x?}"
            );
        }
    }

    #[test]
    fn test_encoding_follows_the_version_passed_in() {
        let suback = ControlPacket::SubAck(suback::SubAckPacket {
            packet_id: 1,
            properties: None,
            payload: vec![suback::SubAckPayload {
                reason_code: ReasonCode::NotAuthorized,
            }],
        });
        let encode = |version| {
            let mut buffer = BytesMut::new();
            suback.encode(&mut buffer, version);
            buffer
        };
        assert_eq!(
            &encode(ProtocolVersion::V3_1)[..],
            [0x90, 0x03, 0x00, 0x01, 0x80]
        );
        assert_eq!(
            &encode(ProtocolVersion::V5)[..],
            [0x90, 0x04, 0x00, 0x01, 0x00, 0x87]
        );
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
    use std::{
        panic::{catch_unwind, AssertUnwindSafe},
        vec::Vec,
    };

    use bytes::Bytes;

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
                while ControlPacket::check(&buffer).is_ok() {
                    if ControlPacket::parse(&mut buffer, version).is_err() {
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

    /// The samples with the protocol version each is valid in, and a few
    /// more packets that leave out what the spec lets them.
    fn versioned_samples() -> Vec<(Vec<u8>, ProtocolVersion)> {
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
        packets.extend([
            (vec![0x40, 0x03, 0x00, 0x01, 0x10], ProtocolVersion::V5),
            (vec![0x62, 0x03, 0x00, 0x01, 0x92], ProtocolVersion::V5),
            (vec![0xe0, 0x00], ProtocolVersion::V5),
            (vec![0xf0, 0x00], ProtocolVersion::V5),
            (vec![0x20, 0x02, 0x01, 0x00], ProtocolVersion::V3_1_1),
            (vec![0x90, 0x03, 0x00, 0x01, 0x01], ProtocolVersion::V3_1_1),
            (vec![0xb0, 0x02, 0x00, 0x01], ProtocolVersion::V3_1_1),
        ]);
        packets
    }

    #[test]
    fn test_packet_type_is_the_first_nibble() {
        for (bytes, version) in versioned_samples() {
            let packet = ControlPacket::parse(&mut BytesMut::from(&bytes[..]), version).unwrap();
            assert_eq!(packet.packet_type() as u8, bytes[0] >> 4, "{packet:?}");
            assert_eq!(packet.clone(), packet);
        }
    }

    #[test]
    fn test_encoded_size_is_what_encode_writes() {
        for (bytes, version) in versioned_samples() {
            let packet = ControlPacket::parse(&mut BytesMut::from(&bytes[..]), version).unwrap();
            let mut encoded = BytesMut::new();
            packet.encode(&mut encoded, version);
            assert_eq!(packet.encoded_size(version), encoded.len(), "{packet:?}");
        }
    }

    #[test]
    fn test_valid_packets_parse_completely() {
        for (bytes, version) in versioned_samples() {
            let mut buffer = BytesMut::from(&bytes[..]);
            // Followed by another packet that must stay untouched
            buffer.extend_from_slice(&[0xc0, 0x00]);
            let result = ControlPacket::parse(&mut buffer, version);
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
            ControlPacket::parse(&mut buffer, ProtocolVersion::V5)
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
            ControlPacket::parse(&mut buffer, ProtocolVersion::V5),
            Err(Error::Reason(ReasonCode::ProtocolError))
        ));

        // The same with two User Properties, which may repeat
        let user_properties = [
            0x30, 0x10, 0x00, 0x01, b't', 0x0c, 0x26, 0x00, 0x01, b'k', 0x00, 0x00, 0x26, 0x00,
            0x01, b'k', 0x00, 0x00,
        ];
        let mut buffer = BytesMut::from(&user_properties[..]);
        assert!(ControlPacket::parse(&mut buffer, ProtocolVersion::V5).is_ok());
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

    fn assert_protocol_error(packets: &[(&[u8], ProtocolVersion)]) {
        for &(bytes, version) in packets {
            let result = ControlPacket::parse(&mut BytesMut::from(bytes), version);
            assert!(
                matches!(result, Err(Error::Reason(ReasonCode::ProtocolError))),
                "{bytes:02x?} ({version}): {result:?}"
            );
        }
    }

    #[test]
    fn test_reason_codes_must_belong_to_their_packet() {
        // Per packet: one code its table lists, then one it doesn't
        let cases: [(&str, &[u8], &[u8]); 9] = [
            (
                "CONNACK",
                &[0x20, 0x03, 0x00, 0x87, 0x00],
                &[0x20, 0x03, 0x00, 0x10, 0x00],
            ),
            (
                "PUBACK",
                &[0x40, 0x03, 0x00, 0x01, 0x10],
                &[0x40, 0x03, 0x00, 0x01, 0x92],
            ),
            (
                "PUBREC",
                &[0x50, 0x03, 0x00, 0x01, 0x91],
                &[0x50, 0x03, 0x00, 0x01, 0x92],
            ),
            (
                "PUBREL",
                &[0x62, 0x03, 0x00, 0x01, 0x92],
                &[0x62, 0x03, 0x00, 0x01, 0x10],
            ),
            (
                "PUBCOMP",
                &[0x70, 0x03, 0x00, 0x01, 0x92],
                &[0x70, 0x03, 0x00, 0x01, 0x80],
            ),
            (
                "SUBACK",
                &[0x90, 0x04, 0x00, 0x01, 0x00, 0x9e],
                &[0x90, 0x04, 0x00, 0x01, 0x00, 0x11],
            ),
            (
                "UNSUBACK",
                &[0xb0, 0x04, 0x00, 0x01, 0x00, 0x11],
                &[0xb0, 0x04, 0x00, 0x01, 0x00, 0x01],
            ),
            ("DISCONNECT", &[0xe0, 0x01, 0x8e], &[0xe0, 0x01, 0x92]),
            ("AUTH", &[0xf0, 0x01, 0x18], &[0xf0, 0x01, 0x87]),
        ];

        for (name, valid, invalid) in cases {
            let parse = |bytes: &[u8]| {
                ControlPacket::parse(&mut BytesMut::from(bytes), ProtocolVersion::V5)
            };
            assert!(parse(valid).is_ok(), "{name}: {:?}", parse(valid));
            assert!(
                matches!(
                    parse(invalid),
                    Err(Error::Reason(ReasonCode::MalformedPacket))
                ),
                "{name}: {:?}",
                parse(invalid)
            );
        }
    }

    #[test]
    fn test_suback_without_return_codes_is_a_protocol_error() {
        assert_protocol_error(&[
            (&[0x90, 0x02, 0x00, 0x01], ProtocolVersion::V3_1),
            (&[0x90, 0x02, 0x00, 0x01], ProtocolVersion::V3_1_1),
            (&[0x90, 0x03, 0x00, 0x01, 0x00], ProtocolVersion::V5),
        ]);
    }

    #[test]
    fn test_packet_identifier_zero_is_a_protocol_error() {
        use ProtocolVersion::{V3_1_1, V5};

        assert_protocol_error(&[
            (&[0x32, 0x06, 0x00, 0x01, b't', 0x00, 0x00, 0x00], V5),
            (&[0x32, 0x05, 0x00, 0x01, b't', 0x00, 0x00], V3_1_1),
            (&[0x40, 0x02, 0x00, 0x00], V5),
            (&[0x50, 0x02, 0x00, 0x00], V5),
            (&[0x62, 0x02, 0x00, 0x00], V5),
            (&[0x70, 0x02, 0x00, 0x00], V3_1_1),
            (&[0x82, 0x07, 0x00, 0x00, 0x00, 0x00, 0x01, b'a', 0x00], V5),
            (&[0x82, 0x06, 0x00, 0x00, 0x00, 0x01, b'a', 0x00], V3_1_1),
            (&[0x90, 0x04, 0x00, 0x00, 0x00, 0x00], V5),
            (&[0x90, 0x03, 0x00, 0x00, 0x00], V3_1_1),
            (&[0xa2, 0x06, 0x00, 0x00, 0x00, 0x00, 0x01, b'a'], V5),
            (&[0xa2, 0x05, 0x00, 0x00, 0x00, 0x01, b'a'], V3_1_1),
            (&[0xb0, 0x04, 0x00, 0x00, 0x00, 0x00], V5),
            (&[0xb0, 0x02, 0x00, 0x00], V3_1_1),
        ]);
    }

    #[test]
    fn test_reserved_flags_are_malformed() {
        let malformed: [(&[u8], ProtocolVersion); 9] = [
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
            // SUBSCRIBE with a reserved option bit set
            (
                &[0x82, 0x08, 0x00, 0x01, 0x00, 0x00, 0x02, b'a', b'#', 0x41],
                ProtocolVersion::V5,
            ),
            // MQTT 3.1.1 SUBACK with a reserved return code
            (&[0x90, 0x03, 0x00, 0x01, 0x03], ProtocolVersion::V3_1_1),
            // PUBLISH whose topic holds U+0000
            (
                &[0x30, 0x04, 0x00, 0x02, b'a', 0x00],
                ProtocolVersion::V3_1_1,
            ),
        ];

        for (bytes, version) in malformed {
            let mut buffer = BytesMut::from(bytes);
            assert!(
                matches!(
                    ControlPacket::parse(&mut buffer, version),
                    Err(Error::Reason(ReasonCode::MalformedPacket))
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

    type DirectDecoder = fn(&mut Bytes) -> Result<()>;

    /// Every per-packet decoder, in MQTT 5 and MQTT 3.1.1.
    fn direct_decoders() -> Vec<(&'static str, DirectDecoder)> {
        vec![
            ("CONNECT", run::<ConnectPacket, 5>),
            ("CONNECT v3", run::<ConnectPacket, 4>),
            ("CONNACK", run::<ConnAckPacket, 5>),
            ("CONNACK v3", run::<ConnAckPacket, 4>),
            ("PUBLISH", run::<PublishPacket, 5>),
            ("PUBLISH v3", run::<PublishPacket, 4>),
            ("PUBACK", run::<PubAckPacket, 5>),
            ("PUBACK v3", run::<PubAckPacket, 4>),
            ("PUBREC", run::<PubRecPacket, 5>),
            ("PUBREC v3", run::<PubRecPacket, 4>),
            ("PUBREL", run::<PubRelPacket, 5>),
            ("PUBREL v3", run::<PubRelPacket, 4>),
            ("PUBCOMP", run::<PubCompPacket, 5>),
            ("PUBCOMP v3", run::<PubCompPacket, 4>),
            ("SUBSCRIBE", run::<SubscribePacket, 5>),
            ("SUBSCRIBE v3", run::<SubscribePacket, 4>),
            ("SUBACK", run::<SubAckPacket, 5>),
            ("SUBACK v3", run::<SubAckPacket, 4>),
            ("UNSUBSCRIBE", run::<UnsubscribePacket, 5>),
            ("UNSUBSCRIBE v3", run::<UnsubscribePacket, 4>),
            ("UNSUBACK", run::<UnsubAckPacket, 5>),
            ("UNSUBACK v3", run::<UnsubAckPacket, 4>),
            ("PINGREQ", run::<PingReqPacket, 5>),
            ("PINGREQ v3", run::<PingReqPacket, 4>),
            ("PINGRESP", run::<PingRespPacket, 5>),
            ("PINGRESP v3", run::<PingRespPacket, 4>),
            ("DISCONNECT", run::<DisconnectPacket, 5>),
            ("DISCONNECT v3", run::<DisconnectPacket, 4>),
            ("AUTH", run::<AuthPacket, 5>),
            ("AUTH v3", run::<AuthPacket, 4>),
        ]
    }

    fn run<P: Packet, const LEVEL: u8>(bytes: &mut Bytes) -> Result<()> {
        let version = ProtocolVersion::try_from(LEVEL).expect("a protocol level");
        P::decode(bytes, version).map(|_| ())
    }

    fn decode_direct(name: &str, decode: DirectDecoder, bytes: &[u8]) -> Result<()> {
        let mut buffer = Bytes::copy_from_slice(bytes);
        catch_unwind(AssertUnwindSafe(|| decode(&mut buffer)))
            .unwrap_or_else(|_| panic!("{name} decoder panicked on {bytes:02x?}"))
    }

    #[test]
    fn test_direct_decoders_refuse_empty_and_truncated_input() {
        for (name, decode) in direct_decoders() {
            assert!(decode_direct(name, decode, &[]).is_err(), "{name}");
            for sample in samples() {
                for len in 1..sample.len() {
                    assert!(
                        decode_direct(name, decode, &sample[..len]).is_err(),
                        "{name} accepted {:02x?}",
                        &sample[..len]
                    );
                }
            }
        }
    }

    #[test]
    fn test_direct_decoders_never_panic() {
        for (name, decode) in direct_decoders() {
            for sample in samples() {
                for i in 0..sample.len() {
                    for value in [0x00, 0x01, 0x7f, 0x80, 0xff, sample[i].wrapping_add(1)] {
                        let mut mutated = sample.clone();
                        mutated[i] = value;
                        let _ = decode_direct(name, decode, &mutated);
                    }
                }
            }
        }
    }

    #[test]
    fn test_direct_decoders_stop_at_the_packet_end() {
        let pingreq = [0xc0, 0x00];
        for (name, decode) in direct_decoders() {
            for sample in samples() {
                let mut bytes = Bytes::from([&sample[..], &pingreq].concat());
                if decode(&mut bytes).is_ok() {
                    assert_eq!(&bytes[..], &pingreq, "{name} on {sample:02x?}");
                }
            }
        }
    }

    #[test]
    fn test_direct_decoders_check_the_packet_type() {
        // A PUBREC is not a PUBACK, and a PINGRESP not a PINGREQ
        let pubrec = [0x50, 0x02, 0x00, 0x01];
        assert!(
            PubAckPacket::decode(&mut Bytes::copy_from_slice(&pubrec), ProtocolVersion::V5)
                .is_err()
        );
        assert!(
            PingReqPacket::decode(&mut Bytes::from_static(&[0xd0, 0x00]), ProtocolVersion::V5)
                .is_err()
        );
    }

    #[test]
    fn test_direct_decoders_refuse_bytes_left_in_the_packet() {
        let malformed: [(&str, DirectDecoder, &[u8]); 3] = [
            ("PINGREQ", run::<PingReqPacket, 5>, &[0xc0, 0x01, 0x00]),
            (
                "CONNACK v3",
                run::<ConnAckPacket, 4>,
                &[0x20, 0x03, 0x00, 0x00, 0x00],
            ),
            (
                "UNSUBACK v3",
                run::<UnsubAckPacket, 4>,
                &[0xb0, 0x03, 0x00, 0x01, 0x00],
            ),
        ];
        for (name, decode, bytes) in malformed {
            assert!(
                matches!(
                    decode_direct(name, decode, bytes),
                    Err(Error::Reason(ReasonCode::MalformedPacket))
                ),
                "{name}"
            );
        }
    }
}

#[cfg(test)]
mod property_list_tests {
    use bytes::Bytes;

    use super::*;

    fn decode_all(bytes: &'static [u8]) -> [Result<()>; 14] {
        fn run<P: Decoder>(bytes: &'static [u8]) -> Result<()> {
            P::decode(&mut Bytes::from_static(bytes)).map(|_| ())
        }
        [
            run::<connect::ConnectProperties>(bytes),
            run::<connect::WillProperties>(bytes),
            run::<connack::ConnAckProperties>(bytes),
            run::<publish::PublishProperties>(bytes),
            run::<puback::PubAckProperties>(bytes),
            run::<pubrec::PubRecProperties>(bytes),
            run::<pubrel::PubRelProperties>(bytes),
            run::<pubcomp::PubCompProperties>(bytes),
            run::<subscribe::SubscribeProperties>(bytes),
            run::<suback::SubAckProperties>(bytes),
            run::<unsubscribe::UnsubscribeProperties>(bytes),
            run::<unsuback::UnsubAckProperties>(bytes),
            run::<disconnect::DisconnectProperties>(bytes),
            run::<auth::AuthProperties>(bytes),
        ]
    }

    #[test]
    fn test_property_list_past_the_packet_is_malformed() {
        // A length of 5 with one byte after it, and a User Property cut
        // short inside a list whose length fits
        for bytes in [&[0x05, 0x26][..], &[0x02, 0x26, 0x00]] {
            for (i, result) in decode_all(bytes).into_iter().enumerate() {
                assert!(
                    matches!(result, Err(Error::Reason(ReasonCode::MalformedPacket))),
                    "{bytes:02x?} in decoder {i}: {result:?}"
                );
            }
        }
    }

    #[test]
    fn test_authentication_data_needs_a_method() {
        fn run<P: Decoder>(bytes: &'static [u8]) -> Result<()> {
            P::decode(&mut Bytes::from_static(bytes)).map(|_| ())
        }
        type Decode = fn(&'static [u8]) -> Result<()>;
        let decoders: [Decode; 3] = [
            run::<connect::ConnectProperties>,
            run::<connack::ConnAckProperties>,
            run::<auth::AuthProperties>,
        ];

        // Authentication Data alone, then with Authentication Method "m"
        let data_only = &[0x03, 0x16, 0x00, 0x00][..];
        let with_method = &[0x07, 0x16, 0x00, 0x00, 0x15, 0x00, 0x01, b'm'][..];
        for (i, decode) in decoders.into_iter().enumerate() {
            assert!(
                matches!(
                    decode(data_only),
                    Err(Error::Reason(ReasonCode::ProtocolError))
                ),
                "decoder {i}"
            );
            assert!(decode(with_method).is_ok(), "decoder {i}");
        }
    }

    #[test]
    fn test_every_property_list_takes_user_properties() {
        // Two User Properties "k" = "" and "" = ""
        let list = &[
            0x0b, 0x26, 0x00, 0x01, b'k', 0x00, 0x00, 0x26, 0x00, 0x00, 0x00, 0x00,
        ][..];
        for (i, result) in decode_all(list).into_iter().enumerate() {
            assert!(result.is_ok(), "decoder {i}: {result:?}");
        }
    }
}
