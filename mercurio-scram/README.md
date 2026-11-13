# mercurio-scram

SCRAM-SHA-256 (RFC 5802, RFC 7677) for the [Mercurio](https://github.com/frenzox/mercurio)
MQTT crates, with optional `tls-exporter` channel binding (RFC 9266): the
messages of both sides, the key derivation and the proofs, with no I/O. The
broker, [`mercurio-server`](https://crates.io/crates/mercurio-server), and
the client, [`mercurio-client`](https://crates.io/crates/mercurio-client),
carry the messages in MQTT 5 enhanced authentication under the method names
`SCRAM-SHA-256` and `SCRAM-SHA-256-PLUS`; most users want those crates
instead.

```rust
use mercurio_scram::{ClientFirst, ScramClient, StoredCredentials, MIN_ITERATIONS};

// The server keeps these, set when the password is
let stored = StoredCredentials::generate("pencil", MIN_ITERATIONS).unwrap();

let mut client = ScramClient::new("user", "pencil");
let client_first = client.start().unwrap();

let first = ClientFirst::parse(client_first.as_bytes()).unwrap();
assert_eq!(first.username(), "user");
let (server_first, awaiting) = first.respond(&stored, None).unwrap();

let client_final = client.respond(server_first.as_bytes()).unwrap();
let server_final = awaiting.verify(client_final.as_bytes()).unwrap();
client.finish(server_final.as_bytes()).unwrap();
```

With channel binding, each side passes the TLS connection's exporter
data, 32 bytes exported with the label `EXPORTER-Channel-Binding` and an
empty context. The proof then covers it, so an exchange relayed between two
TLS connections fails:

```rust
use mercurio_scram::{
    ClientBinding, ClientFirst, ScramClient, StoredCredentials, MIN_ITERATIONS,
    TLS_EXPORTER_LEN,
};

let stored = StoredCredentials::generate("pencil", MIN_ITERATIONS).unwrap();
// Exported from the TLS 1.3 connection, on each end
let exporter = [7u8; TLS_EXPORTER_LEN];

let mut client = ScramClient::new("user", "pencil");
client.set_binding(ClientBinding::TlsExporter(exporter));
let client_first = client.start().unwrap();

let first = ClientFirst::parse(client_first.as_bytes()).unwrap();
let (server_first, awaiting) = first.respond(&stored, Some(&exporter)).unwrap();

let client_final = client.respond(server_first.as_bytes()).unwrap();
let server_final = awaiting.verify(client_final.as_bytes()).unwrap();
client.finish(server_final.as_bytes()).unwrap();
```

Only `tls-exporter` is supported, not `tls-unique` or `tls-server-end-point`.
User names are prepared with SASLprep on both sides.

Proofs and signatures are compared in constant time, and nonces and salts
come from the operating system's random source. Stored credentials are
written in the form of RFC 5803,
`SCRAM-SHA-256$<iterations>:<salt>$<StoredKey>:<ServerKey>`.

## License

MIT or Apache-2.0, at your option.
