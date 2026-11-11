# mercurio-client

An async MQTT client for Tokio, speaking MQTT 3.1, 3.1.1 and 5.0, from the
[Mercurio](https://github.com/frenzox/mercurio) project. It works with any MQTT broker.

```rust,no_run
use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let options = ConnectOptions::new("localhost", 1883).client_id("thermostat");
    let client = MqttClient::connect(options).await?;

    client.subscribe(&[("sensors/+/temperature", QoS::AtLeastOnce)]).await?;
    client.publish("sensors/hall/temperature", "20.5", QoS::AtLeastOnce).await?;

    while let Some(event) = client.recv().await {
        if let Event::Message { topic, payload, .. } = event {
            println!("{topic}: {}", String::from_utf8_lossy(&payload));
        }
    }
    Ok(())
}
```

- `publish` and `subscribe` resolve once the broker acknowledges them.
- Automatic reconnection with exponential backoff, in-flight messages
  sent again, QoS 1 and 2 publishes held while disconnected, and
  optionally the subscriptions made again when the broker lost the
  session.
- Optional manual acknowledgement of received messages, for
  at-least-once processing with the broker's flow control.
- MQTT 5 properties, subscription options and identifiers, topic
  aliases both ways, and enhanced authentication.
- TLS verified with the operating system's certificate store or your
  own CA, client certificates, ALPN, or a rustls `ClientConfig` of
  your own.
- Unix sockets, with `ConnectOptions::unix`, and WebSocket (`ws://` and
  `wss://`) behind the optional `websocket` feature.
- TLS sits behind the default `tls` feature; turn it off for plain TCP
  and Unix sockets without rustls and aws-lc.
- An optional `clap` feature with the connection options of recado and
  mercurioctl, `cli::ConnectArgs`, for your own command-line tools.
- Follows the limits the broker announces, such as Receive Maximum and
  Maximum Packet Size.

For command-line tools built on this library, see
[`recado`](https://crates.io/crates/recado) and
[`mercurioctl`](https://crates.io/crates/mercurioctl). The
[client guide](https://frenzox.github.io/mercurio/client.html)
covers every option.

## License

MIT or Apache-2.0, at your option.
