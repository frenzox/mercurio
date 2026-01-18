//! MQTT subscribe tool - subscribe to topics and print messages.

use clap::Parser;
use mercurio_cli::{init_logging, ConnectionArgs};
use mercurio_client::{Event, MqttClient};
use mercurio_core::qos::QoS;
use tokio::signal;

#[derive(Parser, Debug)]
#[command(name = "mercurio-sub")]
#[command(about = "Subscribe to topics on an MQTT broker")]
#[command(version)]
struct Args {
    #[command(flatten)]
    connection: ConnectionArgs,

    /// Topic(s) to subscribe to (can be specified multiple times)
    #[arg(short = 't', long, required = true)]
    topic: Vec<String>,

    /// QoS level for subscriptions (0, 1, or 2)
    #[arg(short = 'q', long, default_value = "0")]
    qos: u8,

    /// Print topic name before each message
    #[arg(short = 'T', long)]
    print_topic: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    init_logging(args.connection.verbose);

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

    // Subscribe to topics
    let subscriptions: Vec<(&str, QoS)> = args.topic.iter().map(|t| (t.as_str(), qos)).collect();
    client.subscribe(&subscriptions).await?;

    let print_topic = args.print_topic;

    // Handle Ctrl+C for graceful shutdown
    tokio::select! {
        _ = signal::ctrl_c() => {
            eprintln!("\nDisconnecting...");
        }
        _ = async {
            loop {
                match client.recv().await {
                    Some(Event::Message { topic, payload, .. }) => {
                        if print_topic {
                            println!("{}: {}", topic, String::from_utf8_lossy(&payload));
                        } else {
                            println!("{}", String::from_utf8_lossy(&payload));
                        }
                    }
                    Some(Event::Disconnected { reason }) => {
                        eprintln!("Disconnected: {:?}", reason);
                        break;
                    }
                    None => {
                        break;
                    }
                }
            }
        } => {}
    }

    // Disconnect gracefully
    let _ = client.disconnect().await;

    Ok(())
}
