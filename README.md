# Mercurio

![Build Status](https://github.com/frenzox/mercurio/actions/workflows/build.yml/badge.svg)

An MQTT broker, client library and command-line client written in Rust on
top of tokio. It speaks MQTT 3.1, 3.1.1 and 5.0.

## Features

### Broker (`mercuriod`)

- QoS 0, 1 and 2 in both directions, with retransmission on reconnect
- Persistent sessions with session expiry, kept in memory or in SQLite so
  they survive restarts
- Retained messages, wills, and will delay
- Shared subscriptions (`$share/group/filter`) and subscription identifiers
- MQTT 5 subscription options: No Local, Retain As Published and Retain
  Handling
- MQTT 5 message properties: message expiry, content type, response topic,
  correlation data and user properties
- Flow control: Receive Maximum, Maximum Packet Size and topic aliases in
  both directions
- TLS, with optional client certificates
- MQTT over WebSocket
- Authentication with a password file (Argon2 hashes) or MQTT 5 enhanced
  authentication, including re-authentication
- Topic access control in mosquitto's `acl_file` format
- `$SYS/broker/` statistics
- Server redirection, Server Keep Alive, Response Information
- Graceful shutdown on Ctrl+C or SIGTERM: clients are told why they are being
  disconnected

### Client library (`mercurio-client`)

- Async API: `publish` and `subscribe` resolve when the broker acknowledges
  them
- Follows the limits the broker announces, and holds the broker to the
  client's own limits
- Optional automatic reconnection that resumes the session and resends
  unacknowledged messages
- MQTT 5 properties, subscription options and enhanced authentication
- TLS through rustls

## Quick start

Build everything:

```bash
cargo build --release
```

Start the broker with its default settings, which listen on
`127.0.0.1:1883` and keep state in memory:

```bash
./target/release/mercuriod
```

Subscribe in one terminal, and publish from another:

```bash
./target/release/mercurio sub -t 'sensors/#' -T
./target/release/mercurio pub -t sensors/kitchen -m 21.5 -q 1
```

Any MQTT client works with the broker, mosquitto's included:

```bash
mosquitto_sub -V mqttv5 -t 'sensors/#' -v
```

## Configuring the broker

`mercuriod` reads a TOML file, `/etc/mercurio/config.toml` by default, or
the path given with `-c`. A minimal configuration that persists state and
requires a password:

```toml
[server]
host = "0.0.0.0"
port = 1883

[storage]
backend = "sqlite"
path = "/var/lib/mercurio/mercurio.db"

[auth]
enabled = true
allow_anonymous = false
password_file = "/etc/mercurio/passwd"
```

Add users to the password file with:

```bash
mercuriod passwd -f /etc/mercurio/passwd -u alice
```

See [`mercuriod/README.md`](mercuriod/README.md) for every option,
including TLS, WebSocket, limits, access control and the systemd unit in
[`mercuriod/contrib`](mercuriod/contrib).

## Command-line client

`mercurio pub` publishes one message, read from `-m` or from standard
input. `mercurio sub` prints messages until interrupted. Both accept the
connection options (`-H`, `-p`, `-u`, `-P`, `-V`, `--tls`, ...); run them
with `--help` for the full list.

```bash
# MQTT 5 request with properties
mercurio pub -t requests/lamp -m on --response-topic replies/me \
    --correlation-data 42 --user-property source=cli

# Persistent subscriber that survives broker restarts
mercurio sub -t 'alerts/#' -i monitor -c -x 3600 --reconnect 5 -q 1
```

## Client library

```rust
use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = ConnectOptions::new("localhost", 1883)
        .client_id("thermostat")
        .reconnect(std::time::Duration::from_secs(5));
    let client = MqttClient::connect(options).await?;

    client.subscribe(&[("sensors/+/temperature", QoS::AtLeastOnce)]).await?;
    client.publish("sensors/hall/temperature", "20.5", QoS::AtLeastOnce).await?;

    while let Some(event) = client.recv().await {
        match event {
            Event::Message { topic, payload, .. } => {
                println!("{topic}: {}", String::from_utf8_lossy(&payload));
            }
            Event::Disconnected { reason } => eprintln!("disconnected: {reason:?}"),
            Event::Reconnected { .. } => eprintln!("reconnected"),
        }
    }
    Ok(())
}
```

`publish_with_properties` and `subscribe_with` give access to MQTT 5
properties and subscription options. `ConnectOptions::authenticator` plugs
in an enhanced authentication method.

## Workspace layout

| Crate | Contents |
|-------|----------|
| [`mercurio-core`](mercurio-core) | Shared types: QoS, reason codes, properties, topic matching, codec primitives |
| [`mercurio-packets`](mercurio-packets) | Encoding and decoding of every MQTT control packet, for all three protocol versions |
| [`mercurio-storage`](mercurio-storage) | Storage traits for sessions, retained messages, wills, in-flight and queued messages, with memory and SQLite backends |
| [`mercurio-server`](mercurio-server) | The broker as a library, to embed or extend with custom authentication and authorization |
| [`mercuriod`](mercuriod) | The broker daemon: configuration, logging, password files |
| [`mercurio-client`](mercurio-client) | The async client library |
| [`mercurio`](mercurio) | The `mercurio pub` and `mercurio sub` command-line client |

## Development

```bash
cargo test --workspace
cargo clippy --all-targets --all-features
cargo fmt --all
```

The tests exercise the broker and client over real connections. Some use
hand-written packets to check how each side reacts to a peer that breaks
the protocol.

## License

MIT; see [LICENSE](LICENSE).
