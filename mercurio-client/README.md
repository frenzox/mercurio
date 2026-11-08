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
- Automatic reconnection, with in-flight messages sent again and QoS 1
  and 2 publishes held while disconnected.
- MQTT 5 properties, subscription options and identifiers, topic
  aliases, and enhanced authentication.
- TLS with the system's roots or your own CA, and client certificates.
- Follows the limits the broker announces, such as Receive Maximum and
  Maximum Packet Size.

For a command-line client built on this library, see
[`recado`](https://crates.io/crates/recado). The
[client guide](https://frenzox.github.io/mercurio/client.html)
covers every option.

## License

MIT or Apache-2.0, at your option.
