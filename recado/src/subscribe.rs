//! `recado sub`.

use std::io::{self, Write};
use std::time::Duration;

use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use bytes::Bytes;
use clap::{Args, ValueEnum};
use mercurio_client::{
    Event, MessageProperties, MqttClient, QoS, RetainHandling, SubscriptionOptions, Will,
};
use serde_json::{json, Value};
use tokio::signal;

use crate::common::{qos_level, ConnectionArgs};
use crate::failure::Failure;

#[derive(Args, Debug)]
pub struct SubArgs {
    #[command(flatten)]
    pub connection: ConnectionArgs,

    /// Topic(s) to subscribe to (can be specified multiple times)
    #[arg(short = 't', long, required = true)]
    topic: Vec<String>,

    /// QoS level for subscriptions (0, 1, or 2)
    #[arg(short = 'q', long, default_value = "0", value_parser = qos_level)]
    qos: QoS,

    /// Print topic name before each message
    #[arg(short = 'T', long, conflicts_with = "format")]
    print_topic: bool,

    /// How to print each message: its payload as received, or a JSON
    /// object on one line with the topic and the properties
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,

    #[command(flatten)]
    subscription: SubscriptionArgs,

    /// Reconnect after losing the connection, waiting this many seconds
    /// between attempts
    #[arg(long)]
    reconnect: Option<u64>,

    #[command(flatten)]
    will: WillArgs,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Text,
    Json,
}

#[derive(Args, Debug)]
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

/// MQTT 5 subscription options.
#[derive(Args, Debug)]
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

pub async fn run(mut args: SubArgs) -> Result<(), Failure> {
    args.connection.connect.ask_password()?;
    // Connect to broker
    let mut options = args.connection.to_connect_options();

    // Configure will message if topic is provided
    let will = args.will;
    if let Some(topic) = will.will_topic {
        let payload = will.will_message.unwrap_or_default();
        options = options.will(
            Will::new(topic, Bytes::from(payload))
                .qos(will.will_qos)
                .retain(will.will_retain)
                .delay(will.will_delay),
        );
    }
    let reconnect = args.reconnect;
    if let Some(seconds) = reconnect {
        options = options.reconnect(Duration::from_secs(seconds));
    }

    let client = MqttClient::connect(options).await?;

    // Subscribe to topics
    let subscription = args.subscription;
    let subscription_options = SubscriptionOptions {
        qos: args.qos,
        no_local: subscription.no_local,
        retain_as_pub: subscription.retain_as_published,
        retain_handling: RetainHandling::from(subscription.retain_handling),
    };
    let subscriptions: Vec<(&str, SubscriptionOptions)> = args
        .topic
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
                    Some(Event::Message {
                        topic,
                        payload,
                        qos,
                        retain,
                        properties,
                        subscription_ids,
                    }) => {
                        let mut out = io::stdout().lock();
                        let written = match args.format {
                            Format::Text => (|| {
                                if args.print_topic {
                                    write!(out, "{topic}: ")?;
                                }
                                out.write_all(&payload)?;
                                out.write_all(b"\n")
                            })(),
                            Format::Json => {
                                let line = json_line(
                                    &topic,
                                    &payload,
                                    qos,
                                    retain,
                                    &properties,
                                    &subscription_ids,
                                );
                                writeln!(out, "{line}")
                            }
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

/// A message as `--format json` prints it. The payload is always there in
/// base64, and as text too when it is UTF-8; properties the message doesn't
/// have are left out.
fn json_line(
    topic: &str,
    payload: &[u8],
    qos: QoS,
    retain: bool,
    properties: &MessageProperties,
    subscription_ids: &[u32],
) -> Value {
    let mut line = json!({
        "topic": topic,
        "qos": qos as u8,
        "retain": retain,
        "payload_base64": BASE64.encode(payload),
    });
    if let Ok(text) = std::str::from_utf8(payload) {
        line["payload"] = json!(text);
    }
    if properties.payload_is_utf8 {
        line["payload_is_utf8"] = json!(true);
    }
    if let Some(expiry) = properties.message_expiry {
        line["message_expiry"] = json!(expiry);
    }
    if let Some(content_type) = &properties.content_type {
        line["content_type"] = json!(content_type);
    }
    if let Some(response_topic) = &properties.response_topic {
        line["response_topic"] = json!(response_topic);
    }
    if let Some(data) = &properties.correlation_data {
        line["correlation_data_base64"] = json!(BASE64.encode(data));
    }
    if !properties.user_properties.is_empty() {
        line["user_properties"] = json!(properties.user_properties);
    }
    if !subscription_ids.is_empty() {
        line["subscription_ids"] = json!(subscription_ids);
    }
    line
}
