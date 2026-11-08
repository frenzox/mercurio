# mercurio-server

The [Mercurio](https://github.com/frenzox/mercurio) MQTT broker as a library: run a full MQTT 3.1, 3.1.1 and 5.0
broker inside your own Tokio program, with your own authentication,
access control and storage. The
[`mercuriod`](https://crates.io/crates/mercuriod) daemon is built on it.

```rust,no_run
use mercurio_server::server::{run_with_config, ServerConfig};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("0.0.0.0:1883").await?;
    run_with_config(listener, ServerConfig::default(), tokio::signal::ctrl_c()).await?;
    Ok(())
}
```

It supports:

- TCP, TLS, WebSocket and Unix socket listeners, any number at once,
  each with its own TLS, authentication, mount point and limits.
- The PROXY protocol, for brokers behind a load balancer.
- Authentication through a `CredentialValidator` trait, MQTT 5 enhanced
  authentication, and TLS client certificates.
- Topic access control through an `Authorizer` trait, and mosquitto's
  ACL file format.
- Persistent sessions, retained messages, wills, shared subscriptions,
  topic aliases, message expiry and `$SYS` statistics.
- In-process client connections, without TCP.

The [embedding guide](https://frenzox.github.io/mercurio/embedding.html)
covers each of these with examples.

## License

MIT or Apache-2.0, at your option.
