//! Mercurio - MQTT command-line client

use std::io::{self, Read};

use bytes::Bytes;
use clap::{Parser, Subcommand};
use mercurio_client::{Event, MqttClient, Will};
use mercurio_core::qos::QoS;
use tokio::signal;

mod common;
use common::{init_logging, ConnectionArgs};

#[derive(Parser, Debug)]
#[command(name = "mercurio")]
#[command(about = "MQTT command-line client")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Publish a message to a topic
    Pub {
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
    },
    /// Subscribe to topics and print messages
    Sub {
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

        /// Will message topic (sent by broker if client disconnects unexpectedly)
        #[arg(long)]
        will_topic: Option<String>,

        /// Will message payload
        #[arg(long)]
        will_message: Option<String>,

        /// Will message QoS level (0, 1, or 2)
        #[arg(long, default_value = "0")]
        will_qos: u8,

        /// Retain the will message on the broker
        #[arg(long)]
        will_retain: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Pub {
            connection,
            topic,
            message,
            qos,
            retain,
        } => {
            init_logging(connection.verbose);
            run_publish(connection, topic, message, qos, retain).await?;
        }
        Commands::Sub {
            connection,
            topic,
            qos,
            print_topic,
            will_topic,
            will_message,
            will_qos,
            will_retain,
        } => {
            init_logging(connection.verbose);
            run_subscribe(
                connection,
                topic,
                qos,
                print_topic,
                will_topic,
                will_message,
                will_qos,
                will_retain,
            )
            .await?;
        }
    }

    Ok(())
}

async fn run_publish(
    connection: ConnectionArgs,
    topic: String,
    message: Option<String>,
    qos_level: u8,
    retain: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Get the message payload
    let payload = match message {
        Some(msg) => msg,
        None => {
            // Read from stdin
            let mut buffer = String::new();
            io::stdin().read_to_string(&mut buffer)?;
            buffer
        }
    };

    // Parse QoS
    let qos = parse_qos(qos_level)?;

    // Connect to broker
    let options = connection.to_connect_options();
    let client = MqttClient::connect(options).await?;

    // Publish the message
    let payload_bytes = Bytes::from(payload);
    if retain {
        client
            .publish_with_retain(&topic, payload_bytes, qos, true)
            .await?;
    } else {
        client.publish(&topic, payload_bytes, qos).await?;
    }

    // Disconnect gracefully
    client.disconnect().await?;

    Ok(())
}

async fn run_subscribe(
    connection: ConnectionArgs,
    topics: Vec<String>,
    qos_level: u8,
    print_topic: bool,
    will_topic: Option<String>,
    will_message: Option<String>,
    will_qos: u8,
    will_retain: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // Parse QoS
    let qos = parse_qos(qos_level)?;

    // Connect to broker
    let mut options = connection.to_connect_options();

    // Configure will message if topic is provided
    if let Some(topic) = will_topic {
        let payload = will_message.unwrap_or_default();
        let will = Will {
            topic,
            payload: Bytes::from(payload),
            qos: parse_qos(will_qos)?,
            retain: will_retain,
        };
        options = options.will(will);
    }

    let client = MqttClient::connect(options).await?;

    // Subscribe to topics
    let subscriptions: Vec<(&str, QoS)> = topics.iter().map(|t| (t.as_str(), qos)).collect();
    client.subscribe(&subscriptions).await?;

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

fn parse_qos(qos: u8) -> Result<QoS, Box<dyn std::error::Error>> {
    match qos {
        0 => Ok(QoS::AtMostOnce),
        1 => Ok(QoS::AtLeastOnce),
        2 => Ok(QoS::ExactlyOnce),
        _ => {
            eprintln!("Invalid QoS level: {}. Must be 0, 1, or 2.", qos);
            std::process::exit(1);
        }
    }
}
