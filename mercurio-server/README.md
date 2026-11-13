# mercurio-server

The [Mercurio](https://github.com/frenzox/mercurio) MQTT broker as a library: run a full MQTT 3.1, 3.1.1 and 5.0
broker inside your own Tokio program, with your own authentication,
access control and storage. The
[`mercuriod`](https://crates.io/crates/mercuriod) daemon is built on it.

```rust,no_run
use mercurio_server::server::{ListenerConfig, Server, Transport};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("0.0.0.0:1883").await?;
    Server::builder()
        .listener(ListenerConfig::new(listener, Transport::Mqtt))
        .run(tokio::signal::ctrl_c())
        .await?;
    Ok(())
}
```

It supports:

- TCP, TLS, WebSocket and Unix socket listeners, any number at once,
  each with its own TLS, authentication, mount point and limits.
- The PROXY protocol, for brokers behind a load balancer.
- Plugins: one `Plugin` trait for authentication, MQTT 5 enhanced
  authentication, topic access control, message hooks, lifecycle events,
  periodic work and `$CONTROL` commands, run as a chain. Built in:
  SCRAM-SHA-256 and SCRAM-SHA-256-PLUS (channel binding on TLS 1.3)
  over credentials you store, a password list, PLAIN, and
  mosquitto's `acl_file` format.
- TLS client certificates.
- Persistent sessions, retained messages, wills, shared subscriptions,
  topic aliases, message expiry and `$SYS` statistics.
- In-process client connections, without TCP.
- Administration while it runs: `Server::admin` from Rust, and the
  `$CONTROL/broker/v1` commands over MQTT, extended with `AdminExtension`.

The [embedding guide](https://frenzox.github.io/mercurio/embedding.html)
covers each of these with examples.

## License

MIT or Apache-2.0, at your option.
