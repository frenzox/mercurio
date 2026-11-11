# mercurio-packets

Encoding and decoding of every MQTT control packet, for MQTT 3.1, 3.1.1
and 5.0. Part of [Mercurio](https://github.com/frenzox/mercurio), and `no_std` compatible.

```rust
use bytes::BytesMut;
use mercurio_core::protocol::ProtocolVersion;
use mercurio_packets::ControlPacket;

let mut input = BytesMut::from(&[0xc0, 0x00][..]); // PINGREQ
let packet = ControlPacket::parse(&mut input, ProtocolVersion::V3_1_1).unwrap();

let mut output = BytesMut::new();
packet.encode(&mut output, ProtocolVersion::V3_1_1);
assert_eq!(&output[..], &[0xc0, 0x00]);
```

`ControlPacket::check` tells whether a buffer holds a whole packet yet,
which is what a `tokio_util::codec::Decoder` needs. Decoding refuses
malformed packets with the MQTT reason code a broker should answer with.

## Features

- `std` (default): standard library support. Without it the crate is
  `no_std` and needs only `alloc`.

## License

MIT or Apache-2.0, at your option.
