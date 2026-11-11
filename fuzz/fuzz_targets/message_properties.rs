//! Storage backends decode message properties from bytes they read back
//! from disk, which may be truncated or corrupt: decoding must never panic,
//! and what decodes must encode back to the same properties.
#![no_main]

use bytes::{Bytes, BytesMut};
use libfuzzer_sys::fuzz_target;
use mercurio_core::{
    codec::{Decoder, Encoder},
    message::MessageProperties,
};

fuzz_target!(|data: &[u8]| {
    let Ok(properties) = MessageProperties::decode(&mut Bytes::copy_from_slice(data)) else {
        return;
    };

    let mut encoded = BytesMut::new();
    properties
        .encode(&mut encoded)
        .expect("decoded properties encode");
    assert_eq!(encoded.len(), properties.encoded_size());

    let decoded = MessageProperties::decode(&mut encoded).expect("encoded properties decode");
    assert_eq!(decoded, properties);
});
