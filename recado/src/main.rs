//! Mercurio - MQTT command-line client

use std::io::{self, Read, Write};
use std::process::ExitCode;
use std::time::Duration;

use bytes::Bytes;
use clap::{Parser, Subcommand};
use mercurio_client::{
    Event, MessageProperties, MqttClient, RetainHandling, SubscriptionOptions, Will,
};
use mercurio_core::qos::QoS;
use tokio::signal;

mod common;
mod failure;
use common::{init_logging, ConnectionArgs};
use failure::Failure;

const AFTER_HELP: &str = "\
Exit status: 0 on success, 1 when the broker refused a subscription or a \
message, 2 when the broker couldn't be reached, refused the login or \
closed the connection, 3 on wrong usage.";

#[derive(Parser, Debug)]
#[command(name = "recado")]
#[command(about = "MQTT command-line client")]
#[command(version, after_help = AFTER_HELP)]
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
        #[arg(short = 'q', long, default_value = "0", value_parser = qos_level)]
        qos: QoS,

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
        #[arg(short = 'q', long, default_value = "0", value_parser = qos_level)]
        qos: QoS,

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
    #[arg(long, default_value = "0", value_parser = qos_level)]
    will_qos: QoS,

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
async fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(if e.use_stderr() { failure::USAGE } else { 0 });
        }
    };
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            eprintln!("recado: {failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}

async fn run(cli: Cli) -> Result<(), Failure> {
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
            run_publish(connection, topic, message, qos, retain, properties.into()).await
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
            .await
        }
    }
}

async fn run_publish(
    connection: ConnectionArgs,
    topic: String,
    message: Option<String>,
    qos: QoS,
    retain: bool,
    properties: MessageProperties,
) -> Result<(), Failure> {
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
    qos: QoS,
    print_topic: bool,
    subscription: SubscriptionArgs,
    reconnect: Option<u64>,
    will: WillArgs,
) -> Result<(), Failure> {
    // Connect to broker
    let mut options = connection.to_connect_options();

    // Configure will message if topic is provided
    if let Some(topic) = will.will_topic {
        let payload = will.will_message.unwrap_or_default();
        options = options.will(
            Will::new(topic, Bytes::from(payload))
                .qos(will.will_qos)
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
    let subscribe = || async {
        let granted = client
            .subscribe_with(&subscriptions, subscription.subscription_id)
            .await?;
        match granted.iter().find(|result| !result.success) {
            Some(refused) => Err(Failure::Rejected(format!(
                "the broker refused the subscription to {}: {}",
                refused.topic, refused.reason
            ))),
            None => Ok(()),
        }
    };
    if let Err(failure) = subscribe().await {
        let _ = client.disconnect().await;
        return Err(failure);
    }

    // Handle Ctrl+C for graceful shutdown
    let outcome = tokio::select! {
        _ = signal::ctrl_c() => {
            eprintln!("\nDisconnecting...");
            Ok(())
        }
        outcome = async {
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
                            return Ok(());
                        }
                    }
                    Some(Event::Disconnected { reason }) => {
                        // With reconnection on, the client says when it
                        // gives up by closing the event stream
                        if reconnect.is_none() {
                            return Err(Failure::disconnected(&reason));
                        }
                        eprintln!("recado: {}; reconnecting", Failure::disconnected(&reason));
                    }
                    Some(Event::Reconnected { session_present }) => {
                        eprintln!("recado: reconnected");
                        // A broker that lost the session lost the
                        // subscriptions with it
                        if !session_present {
                            subscribe().await?;
                        }
                    }
                    // Events added in later versions of the client
                    Some(_) => {}
                    None => {
                        return Err(Failure::Connection("the connection was lost".to_string()));
                    }
                }
            }
        } => outcome,
    };

    // Disconnect gracefully
    let _ = client.disconnect().await;

    outcome
}

fn qos_level(value: &str) -> Result<QoS, String> {
    match value {
        "0" => Ok(QoS::AtMostOnce),
        "1" => Ok(QoS::AtLeastOnce),
        "2" => Ok(QoS::ExactlyOnce),
        _ => Err(format!("`{value}` isn't a QoS level, 0, 1 or 2")),
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    use super::{Cli, Commands};

    #[test]
    fn arguments_are_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn connection_flags_keep_their_names() {
        let cli = Cli::try_parse_from([
            "recado",
            "sub",
            "-H",
            "broker",
            "-p",
            "8883",
            "-i",
            "me",
            "-u",
            "alice",
            "-P",
            "secret",
            "--tls",
            "--cafile",
            "ca.pem",
            "--insecure",
            "-k",
            "30",
            "-c",
            "-x",
            "60",
            "-V",
            "311",
            "-v",
            "-t",
            "a",
        ])
        .unwrap();
        let Commands::Sub { connection, .. } = cli.command else {
            panic!("expected sub");
        };
        let connect = &connection.connect;
        assert_eq!(connect.host.as_deref(), Some("broker"));
        assert_eq!(connect.port, Some(8883));
        assert_eq!(connect.client_id.as_deref(), Some("me"));
        assert_eq!(connect.username.as_deref(), Some("alice"));
        assert_eq!(connect.password.as_deref(), Some("secret"));
        assert!(connect.tls && connect.insecure);
        assert_eq!(connect.cafile.as_deref(), Some("ca.pem"));
        assert_eq!(connection.keep_alive, 30);
        assert!(connection.no_clean_start && connection.verbose);
        assert_eq!(connection.session_expiry, 60);
        assert_eq!(connection.protocol_version, "311");
    }

    #[cfg(unix)]
    #[test]
    fn pub_and_sub_connect_through_a_unix_socket() {
        let path = "/run/mercurio/admin.sock";
        for command in ["pub", "sub"] {
            let cli = Cli::try_parse_from(["recado", command, "--unix", path, "-t", "a"]).unwrap();
            let (Commands::Pub { connection, .. } | Commands::Sub { connection, .. }) = cli.command;
            assert_eq!(
                connection.connect.unix.as_deref(),
                Some(std::path::Path::new(path))
            );
            let both = ["recado", command, "--unix", path, "-H", "broker", "-t", "a"];
            assert!(Cli::try_parse_from(both).is_err(), "{command}");
        }
    }
}
