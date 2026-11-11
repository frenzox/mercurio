# mercurio-scram

SCRAM-SHA-256 (RFC 5802, RFC 7677) for the [Mercurio](https://github.com/frenzox/mercurio)
MQTT crates, without channel binding: the messages of both sides, the key
derivation and the proofs, with no I/O. The broker,
[`mercurio-server`](https://crates.io/crates/mercurio-server), and the
client, [`mercurio-client`](https://crates.io/crates/mercurio-client), carry
the messages in MQTT 5 enhanced authentication under the method name
`SCRAM-SHA-256`; most users want those crates instead.

```rust
use mercurio_scram::{ClientFirst, ScramClient, StoredCredentials, MIN_ITERATIONS};

// The server keeps these, set when the password is
let stored = StoredCredentials::generate("pencil", MIN_ITERATIONS).unwrap();

let mut client = ScramClient::new("user", "pencil");
let client_first = client.start().unwrap();

let first = ClientFirst::parse(client_first.as_bytes()).unwrap();
assert_eq!(first.username(), "user");
let (server_first, awaiting) = first.respond(&stored).unwrap();

let client_final = client.respond(server_first.as_bytes()).unwrap();
let server_final = awaiting.verify(client_final.as_bytes()).unwrap();
client.finish(server_final.as_bytes()).unwrap();
```

Proofs and signatures are compared in constant time, and nonces and salts
come from the operating system's random source. Stored credentials are
written in the form of RFC 5803,
`SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`.

## License

MIT or Apache-2.0, at your option.
