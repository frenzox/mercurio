# mercurio-core

Core types shared by the [Mercurio](https://github.com/frenzox/mercurio) MQTT crates: protocol versions, QoS,
reason codes, MQTT 5 properties, messages and topic matching.

Most users want one of the crates built on it instead:
[`mercurio-client`](https://crates.io/crates/mercurio-client) to talk to a
broker, or [`mercurio-server`](https://crates.io/crates/mercurio-server) to
run one.

```rust
use mercurio_core::topic::{topic_matches_filter, validate_subscribe_filter};

assert!(validate_subscribe_filter("sensors/+/temperature").is_ok());
assert!(topic_matches_filter("sensors/hall/temperature", "sensors/+/temperature"));
assert!(!topic_matches_filter("$SYS/broker/uptime", "#"));
```

## Features

- `std` (default): standard library support. Without it the crate is
  `no_std` and needs only `alloc`.

## License

MIT or Apache-2.0, at your option.
