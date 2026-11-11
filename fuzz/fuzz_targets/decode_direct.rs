//! The per-packet decoders are public, so they must refuse any input
//! without panicking, not only the exact frames `ControlPacket` hands them.
//! The first byte picks the decoder; the rest is its input.
#![no_main]

use bytes::Bytes;
use libfuzzer_sys::fuzz_target;
use mercurio_core::codec::Decoder;
use mercurio_packets::{
    auth::AuthPacket, connack::ConnAckPacket, connect::ConnectPacket, disconnect::DisconnectPacket,
    pingreq::PingReqPacket, pingresp::PingRespPacket, puback::PubAckPacket, pubcomp::PubCompPacket,
    publish::PublishPacket, pubrec::PubRecPacket, pubrel::PubRelPacket, suback::SubAckPacket,
    subscribe::SubscribePacket, unsuback::UnsubAckPacket, unsubscribe::UnsubscribePacket,
    ControlPacket,
};

type Decode = fn(&mut Bytes) -> bool;

fn run<P: Decoder>(bytes: &mut Bytes) -> bool {
    P::decode(bytes).is_ok()
}

const DECODERS: [Decode; 21] = [
    run::<ConnectPacket>,
    run::<ConnAckPacket>,
    |b| ConnAckPacket::decode_v3(b).is_ok(),
    run::<PublishPacket>,
    |b| PublishPacket::decode_v3(b).is_ok(),
    run::<PubAckPacket>,
    run::<PubRecPacket>,
    run::<PubRelPacket>,
    run::<PubCompPacket>,
    run::<SubscribePacket>,
    |b| SubscribePacket::decode_v3(b).is_ok(),
    run::<SubAckPacket>,
    |b| SubAckPacket::decode_v3(b).is_ok(),
    run::<UnsubscribePacket>,
    |b| UnsubscribePacket::decode_v3(b).is_ok(),
    run::<UnsubAckPacket>,
    |b| UnsubAckPacket::decode_v3(b).is_ok(),
    run::<PingReqPacket>,
    run::<PingRespPacket>,
    run::<DisconnectPacket>,
    run::<AuthPacket>,
];

fuzz_target!(|data: &[u8]| {
    let Some((&selector, input)) = data.split_first() else {
        return;
    };

    let _ = ControlPacket::check(input);
    let _ = ControlPacket::connect_version(input);

    let mut buffer = Bytes::copy_from_slice(input);
    let before = buffer.len();
    if DECODERS[usize::from(selector) % DECODERS.len()](&mut buffer) {
        // A decoded packet is at least a first byte and a Remaining Length,
        // and never reads past the frame that length gives
        let frame_len = ControlPacket::frame_len(input).expect("a decoded packet has a header");
        assert_eq!(before - buffer.len(), frame_len);
    }
});
