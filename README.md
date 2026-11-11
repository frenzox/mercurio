# Mercurio

[![Build](https://github.com/frenzox/mercurio/actions/workflows/build.yml/badge.svg)](https://github.com/frenzox/mercurio/actions/workflows/build.yml)
[![crates.io](https://img.shields.io/crates/v/mercuriod.svg)](https://crates.io/crates/mercuriod)
[![docs.rs](https://img.shields.io/docsrs/mercurio-client)](https://docs.rs/mercurio-client)
[![License](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

An MQTT broker, client library and command-line client written in Rust on
top of Tokio. It speaks MQTT 3.1, 3.1.1 and 5.0.

- **Ready for mosquitto users.** Mercurio reads mosquitto's password and
  ACL files, takes the commands of its dynamic security plugin, publishes
  `$SYS` statistics with the same names and bridges to other brokers. The
  [migration guide](https://frenzox.github.io/mercurio/migrating.html)
  maps every mosquitto option.
- **Complete MQTT 5.** Shared subscriptions, subscription options, topic
  aliases, message expiry, request/response, enhanced authentication and
  server redirection, in the broker and the client alike.
- **Durable.** Sessions, retained messages and queued messages are kept in
  SQLite or PostgreSQL, and a standby broker takes over a PostgreSQL
  database when the serving one stops.
- **Extensible.** Plugins authenticate clients, check access, rewrite or
  drop messages and react to events. Write them in any language that
  builds WebAssembly components, sandboxed with their own limits, or in
  Rust against the broker library.
- **Embeddable.** The broker is also a library, `mercurio-server`, to run
  inside your own program.
- **Easy to run.** `.deb` and `.rpm` packages set up the broker as a
  sandboxed systemd service, on x86_64, aarch64 and armv7 Linux.
- **Administrable at runtime.** List, disconnect and ban clients, delete
  sessions and retained messages, change the log level and reload files
  while the broker runs, with `mercurioctl` or over MQTT on
  `$CONTROL/broker/v1`.

## Install

Each [release](https://github.com/frenzox/mercurio/releases) of
`mercuriod`, the broker, `mercurioctl`, its administration tool, and
`recado`, the command-line client, has `.deb` and `.rpm` packages and a
`.tar.gz`. Download the ones for your system and install them:

```bash
# Debian, Ubuntu, Raspberry Pi OS
sudo apt install ./mercuriod_*.deb ./mercurioctl_*.deb ./recado_*.deb

# Fedora, RHEL, openSUSE
sudo dnf install ./mercuriod-*.rpm ./mercurioctl-*.rpm ./recado-*.rpm
```

The broker then runs as the `mercuriod` service, configured by
`/etc/mercurio/config.toml`. Or install the binaries with cargo:

```bash
cargo install mercuriod mercurioctl recado
```

## Quick start

Start the broker with its defaults, which listen on `127.0.0.1:1883` and
keep state in memory, or use the service the packages started:

```bash
mercuriod
```

Subscribe in one terminal, and publish from another:

```bash
recado sub -t 'sensors/#' -T
recado pub -t sensors/kitchen -m 21.5 -q 1
```

Any MQTT client works with the broker, mosquitto's included:

```bash
mosquitto_sub -V mqttv5 -t 'sensors/#' -v
```

On the packaged service, `mercurioctl` looks at and changes the running
broker from the same host, as root or a member of the `mercurio` group:

```bash
sudo mercurioctl client list
sudo mercurioctl broker log-level debug
```

## Features

**Protocol**

- MQTT 3.1, 3.1.1 and 5.0; QoS 0, 1 and 2 in both directions, with
  retransmission on reconnect
- Persistent sessions with session expiry, retained messages, wills and will
  delay
- Shared subscriptions, subscription identifiers, No Local, Retain As
  Published and Retain Handling
- Message expiry, content type, response topic, correlation data and user
  properties
- Flow control with Receive Maximum, Maximum Packet Size and topic aliases
- Enhanced authentication and re-authentication, server redirection,
  Server Keep Alive and Response Information

**Broker**

- [Listeners](https://frenzox.github.io/mercurio/listeners.html) on TCP,
  TLS, WebSocket and Unix sockets, with the PROXY protocol, mount points and
  their own authentication
- [Authentication](https://frenzox.github.io/mercurio/security.html) with
  password files (Argon2 or mosquitto's hashes), client certificates, an
  HTTP service or dynamic security managed at runtime
- Topic access control in mosquitto's `acl_file` format
- [Bridges](https://frenzox.github.io/mercurio/bridges.html) to other
  brokers, over MQTT 5 or 3.1.1, with failover and mutual TLS
- State in memory, SQLite or PostgreSQL, with standby failover on
  PostgreSQL
- [WebAssembly plugins](https://frenzox.github.io/mercurio/plugins.html)
  with access to HTTP and sockets limited to allowlists
- [`$SYS` statistics](https://frenzox.github.io/mercurio/operations.html),
  logging to a file or syslog, limits on connections, queues and sessions
- [Administration](https://frenzox.github.io/mercurio/admin.html) at
  runtime on `$CONTROL/broker/v1` and with
  [`mercurioctl`](https://frenzox.github.io/mercurio/mercurioctl.html):
  clients, sessions, bans, retained messages, subscriptions, the log level
- Configuration reloads on SIGHUP or `mercurioctl broker reload`, graceful
  shutdown on SIGTERM

**Client library**

- Async API: `publish` and `subscribe` resolve when the broker acknowledges
  them
- Automatic reconnection that resumes the session and resends
  unacknowledged messages
- Follows the limits the broker announces, and holds the broker to the
  client's own
- TLS through rustls

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

## Documentation

The [documentation](https://frenzox.github.io/mercurio/) is built from
[`docs/`](docs):

| Page | Covers |
|------|--------|
| [Installing & running](https://frenzox.github.io/mercurio/install.html) | Packages, building, the systemd service, signals |
| [Configuration reference](https://frenzox.github.io/mercurio/configuration.html) | Every option of the TOML file |
| [Authentication & access](https://frenzox.github.io/mercurio/security.html) | Passwords, ACLs, TLS, HTTP auth, dynamic security |
| [Listeners & transports](https://frenzox.github.io/mercurio/listeners.html) | TLS, WebSocket, Unix sockets, PROXY protocol, mount points |
| [Bridges](https://frenzox.github.io/mercurio/bridges.html) | Forwarding topics to and from other brokers |
| [Operations & monitoring](https://frenzox.github.io/mercurio/operations.html) | Persistence, limits, statistics, logs |
| [Administration](https://frenzox.github.io/mercurio/admin.html) | The admin socket, permissions, the `$CONTROL/broker/v1` commands, bans, upgrading |
| [mercurioctl](https://frenzox.github.io/mercurio/mercurioctl.html) | Administering the broker from the command line |
| [Command-line client](https://frenzox.github.io/mercurio/cli.html) | `recado pub` and `recado sub` |
| [Rust client library](https://frenzox.github.io/mercurio/client.html) | Connecting, publishing and subscribing from Rust |
| [Embedding the broker](https://frenzox.github.io/mercurio/embedding.html) | Running `mercurio-server` in your program, Rust plugins |
| [WebAssembly plugins](https://frenzox.github.io/mercurio/plugins.html) | Writing, configuring and limiting plugins |
| [Migrating from mosquitto](https://frenzox.github.io/mercurio/migrating.html) | mosquitto's options and their equivalents |

## Crates

| Crate | Contents |
|-------|----------|
| [`mercuriod`](mercuriod) | The broker daemon: configuration, logging, password files, WASM plugins |
| [`recado`](recado) | The `recado pub` and `recado sub` command-line client |
| [`mercurioctl`](mercurioctl) | The administration tool for a running broker |
| [`mercurio-client`](mercurio-client) | The async client library |
| [`mercurio-server`](mercurio-server) | The broker as a library, with the `Plugin` trait |
| [`mercurio-plugin`](mercurio-plugin) | The SDK for writing WASM plugins in Rust |
| [`mercurio-storage`](mercurio-storage) | Storage for sessions, retained, in-flight and queued messages, in memory, SQLite or PostgreSQL |
| [`mercurio-packets`](mercurio-packets) | Encoding and decoding of every MQTT packet, for all three protocol versions |
| [`mercurio-core`](mercurio-core) | Shared types: QoS, reason codes, properties, topic matching |

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

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in the work by you, as defined in the Apache-2.0
license, shall be dual licensed as above, without any additional terms or
conditions.
