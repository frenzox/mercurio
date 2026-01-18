//! MQTT publish tool - publish messages to topics.

use std::io::{self, Read};

use bytes::Bytes;
use clap::Parser;
use mercurio_cli::{init_logging, ConnectionArgs};
use mercurio_client::MqttClient;
use mercurio_core::qos::QoS;

#[derive(Parser, Debug)]
#[command(name = "mercurio-pub")]
#[command(about = "Publish messages to an MQTT broker")]
#[command(version)]
struct Args {
    #[command(flatten)]
    connection: ConnectionArgs,

    /// Topic to publish to
    #[arg(short = 't', long)]
    topic: String,

    /// Message payload (reads from stdin if not provided)
    #[arg(short = 'm', long)]
    message: Option<String>,

    /// QoS level (0, 1, or 2)
    #[arg(short = 'q', long, default_value = "0")]
    qos: u8,

    /// Retain the message on the broker
    #[arg(short = 'r', long)]
    retain: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    init_logging(args.connection.verbose);

    // Get the message payload
    let payload = match args.message {
        Some(msg) => msg,
        None => {
            // Read from stdin
            let mut buffer = String::new();
            io::stdin().read_to_string(&mut buffer)?;
            buffer
        }
    };

    // Parse QoS
    let qos = match args.qos {
        0 => QoS::AtMostOnce,
        1 => QoS::AtLeastOnce,
        2 => QoS::ExactlyOnce,
        _ => {
            eprintln!("Invalid QoS level: {}. Must be 0, 1, or 2.", args.qos);
            std::process::exit(1);
        }
    };

    // Connect to broker
    let options = args.connection.to_connect_options();
    let client = MqttClient::connect(options).await?;

    // Publish the message
    let payload_bytes = Bytes::from(payload);
    if args.retain {
        client
            .publish_with_retain(&args.topic, payload_bytes, qos, true)
            .await?;
    } else {
        client.publish(&args.topic, payload_bytes, qos).await?;
    }

    // Disconnect gracefully
    client.disconnect().await?;

    Ok(())
}
