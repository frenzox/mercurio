//! The per-packet decoders are public, so they must refuse any input
//! without panicking, not only the exact frames `ControlPacket` hands them.
//! The first byte picks the decoder and the protocol version; the rest
//! is its input.
#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_packets::{
    auth::AuthPacket, connack::ConnAckPacket, connect::ConnectPacket, disconnect::DisconnectPacket,
    pingreq::PingReqPacket, pingresp::PingRespPacket, puback::PubAckPacket, pubcomp::PubCompPacket,
    publish::PublishPacket, pubrec::PubRecPacket, pubrel::PubRelPacket, suback::SubAckPacket,
    subscribe::SubscribePacket, unsuback::UnsubAckPacket, unsubscribe::UnsubscribePacket,
    ControlPacket, Packet,
};

type Decode = fn(&mut Bytes, ProtocolVersion) -> bool;

fn run<P: Packet>(bytes: &mut Bytes, version: ProtocolVersion) -> bool {
    P::decode(bytes, version).is_ok()
}

const DECODERS: [Decode; 15] = [
    run::<ConnectPacket>,
    run::<ConnAckPacket>,
    run::<PublishPacket>,
    run::<PubAckPacket>,
    run::<PubRecPacket>,
    run::<PubRelPacket>,
    run::<PubCompPacket>,
    run::<SubscribePacket>,
    run::<SubAckPacket>,
    run::<UnsubscribePacket>,
    run::<UnsubAckPacket>,
    run::<PingReqPacket>,
    run::<PingRespPacket>,
    run::<DisconnectPacket>,
    run::<AuthPacket>,
];

const VERSIONS: [ProtocolVersion; 3] = [
    ProtocolVersion::V3_1,
    ProtocolVersion::V3_1_1,
    ProtocolVersion::V5,
];

fuzz_target!(|data: &[u8]| {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };

    let _ = ControlPacket::check(input);
    let _ = ControlPacket::connect_version(input);

    let mut buffer = Bytes::copy_from_slice(input);
    let before = buffer.len();
    let selector = usize::from(selector);
    let decode = DECODERS[selector % DECODERS.len()];
    let version = VERSIONS[selector / DECODERS.len() % VERSIONS.len()];
    if decode(&mut buffer, version) {
        // A decoded packet is at least a first byte and a Remaining Length,
        // and never reads past the frame that length gives
        let frame_len = ControlPacket::frame_len(input).expect("a decoded packet has a header");
        assert_eq!(before - buffer.len(), frame_len);
    }
});
