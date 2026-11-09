//! Mercurio - MQTT command-line client

use std::io::{self, Read, Write};
use std::time::Duration;

use bytes::Bytes;
use clap::{Parser, Subcommand};
use mercurio_client::{
    Event, MessageProperties, MqttClient, RetainHandling, SubscriptionOptions, Will,
};
use mercurio_core::qos::QoS;
use tokio::signal;

mod common;
use common::{init_logging, ConnectionArgs};

#[derive(Parser, Debug)]
#[command(name = "recado")]
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

        #[command(flatten)]
        properties: PropertyArgs,
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

        #[command(flatten)]
        subscription: SubscriptionArgs,

        /// Reconnect after losing the connection, waiting this many seconds
        /// between attempts
        #[arg(long)]
        reconnect: Option<u64>,

        #[command(flatten)]
        will: WillArgs,
    },
}

#[derive(clap::Args, Debug)]
struct WillArgs {
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

    /// Seconds the broker waits before publishing the will (MQTT 5)
    #[arg(long, default_value = "0")]
    will_delay: u32,
}

/// MQTT 5 properties of a published message.
#[derive(clap::Args, Debug)]
struct PropertyArgs {
    /// Content type of the payload, such as a MIME type
    #[arg(long)]
    content_type: Option<String>,

    /// Topic a reply should be published to
    #[arg(long)]
    response_topic: Option<String>,

    /// Data that ties a reply to its request
    #[arg(long)]
    correlation_data: Option<String>,

    /// Seconds the broker keeps the message for subscribers yet to get it
    #[arg(long)]
    message_expiry: Option<u32>,

    /// Mark the payload as UTF-8 text
    #[arg(long)]
    utf8: bool,

    /// User property as key=value (can be specified multiple times)
    #[arg(long = "user-property", value_parser = parse_user_property)]
    user_properties: Vec<(String, String)>,
}

impl From<PropertyArgs> for MessageProperties {
    fn from(args: PropertyArgs) -> Self {
        MessageProperties {
            payload_is_utf8: args.utf8,
            message_expiry: args.message_expiry,
            content_type: args.content_type,
            response_topic: args.response_topic,
            correlation_data: args.correlation_data.map(Bytes::from),
            user_properties: args.user_properties,
        }
    }
}

/// MQTT 5 subscription options.
#[derive(clap::Args, Debug)]
struct SubscriptionArgs {
    /// Don't receive messages this client publishes itself
    #[arg(long)]
    no_local: bool,

    /// Keep the retain flag messages were published with
    #[arg(long)]
    retain_as_published: bool,

    /// Retained messages on subscribing: 0 always, 1 only for new
    /// subscriptions, 2 never
    #[arg(long, default_value = "0", value_parser = clap::value_parser!(u8).range(0..=2))]
    retain_handling: u8,

    /// Identifier the broker attaches to messages matching these
    /// subscriptions
    #[arg(long)]
    subscription_id: Option<u32>,
}

fn parse_user_property(value: &str) -> Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got `{value}`"))
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
            properties,
        } => {
            init_logging(connection.verbose);
            run_publish(connection, topic, message, qos, retain, properties.into()).await?;
        }
        Commands::Sub {
            connection,
            topic,
            qos,
            print_topic,
            subscription,
            reconnect,
            will,
        } => {
            init_logging(connection.verbose);
            run_subscribe(
                connection,
                topic,
                qos,
                print_topic,
                subscription,
                reconnect,
                will,
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
    properties: MessageProperties,
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
    client
        .publish_with_properties(&topic, Bytes::from(payload), qos, retain, properties)
        .await?;

    // Disconnect gracefully
    client.disconnect().await?;

    Ok(())
}

async fn run_subscribe(
    connection: ConnectionArgs,
    topics: Vec<String>,
    qos_level: u8,
    print_topic: bool,
    subscription: SubscriptionArgs,
    reconnect: Option<u64>,
    will: WillArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    // Parse QoS
    let qos = parse_qos(qos_level)?;

    // Connect to broker
    let mut options = connection.to_connect_options();

    // Configure will message if topic is provided
    if let Some(topic) = will.will_topic {
        let payload = will.will_message.unwrap_or_default();
        options = options.will(
            Will::new(topic, Bytes::from(payload))
                .qos(parse_qos(will.will_qos)?)
                .retain(will.will_retain)
                .delay(will.will_delay),
        );
    }
    if let Some(seconds) = reconnect {
        options = options.reconnect(Duration::from_secs(seconds));
    }

    let client = MqttClient::connect(options).await?;

    // Subscribe to topics
    let subscription_options = SubscriptionOptions {
        qos,
        no_local: subscription.no_local,
        retain_as_pub: subscription.retain_as_published,
        retain_handling: RetainHandling::from(subscription.retain_handling),
    };
    let subscriptions: Vec<(&str, SubscriptionOptions)> = topics
        .iter()
        .map(|t| (t.as_str(), subscription_options))
        .collect();
    let subscribe = || client.subscribe_with(&subscriptions, subscription.subscription_id);
    subscribe().await?;

    // Handle Ctrl+C for graceful shutdown
    tokio::select! {
        _ = signal::ctrl_c() => {
            eprintln!("\nDisconnecting...");
        }
        _ = async {
            loop {
                match client.recv().await {
                    Some(Event::Message { topic, payload, .. }) => {
                        let payload = String::from_utf8_lossy(&payload);
                        let mut out = io::stdout();
                        let written = if print_topic {
                            writeln!(out, "{}: {}", topic, payload)
                        } else {
                            writeln!(out, "{}", payload)
                        };
                        // The reader went away, as `head` does once it has
                        // enough lines
                        if written.is_err() {
                            break;
                        }
                    }
                    Some(Event::Disconnected { reason }) => {
                        eprintln!("Disconnected: {:?}", reason);
                        // With reconnection on, the client says when it
                        // gives up by closing the event stream
                        if reconnect.is_none() {
                            break;
                        }
                    }
                    Some(Event::Reconnected { session_present }) => {
                        eprintln!("Reconnected");
                        // A broker that lost the session lost the
                        // subscriptions with it
                        if !session_present {
                            if let Err(e) = subscribe().await {
                                eprintln!("Resubscribing failed: {}", e);
                                break;
                            }
                        }
                    }
                    // Events added in later versions of the client
                    Some(_) => {}
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
