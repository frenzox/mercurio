//! Arbitrary bytes from a client must never panic the decoder, and what
//! decodes must encode back to the same packet.
#![no_main]

use bytes::BytesMut;
use libfuzzer_sys::fuzz_target;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_packets::ControlPacket;

fuzz_target!(|data: &[u8]| {
    let Some((&selector, bytes)) = data.split_first() else {
        return;
    };
    let version = match selector % 3 {
        0 => ProtocolVersion::V3_1,
        1 => ProtocolVersion::V3_1_1,
        _ => ProtocolVersion::V5,
    };
    let mut src = BytesMut::from(bytes);
    while let Ok(packet) = ControlPacket::parse(&mut src, version) {
        let mut encoded = BytesMut::new();
        packet.encode(&mut encoded, version);
        let decoded =
            ControlPacket::parse(&mut encoded, version).expect("an encoded packet decodes");
        assert_eq!(packet, decoded);
        assert!(encoded.is_empty(), "encoding wrote more than one packet");
    }
});
