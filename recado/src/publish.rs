//! `recado pub`.

use std::io::{self, Read};

use bytes::Bytes;
use clap::Args;
use mercurio_client::{MessageProperties, MqttClient, QoS};

use crate::common::{qos_level, ConnectionArgs};
use crate::failure::Failure;

#[derive(Args, Debug)]
pub struct PubArgs {
    #[command(flatten)]
    pub connection: ConnectionArgs,

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
}

/// MQTT 5 properties of a published message.
#[derive(Args, Debug)]
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

fn parse_user_property(value: &str) -> Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got `{value}`"))
}

pub async fn run(mut args: PubArgs) -> Result<(), Failure> {
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

    args.connection.connect.ask_password()?;
    // Connect to broker
    let options = args.connection.to_connect_options();
    let client = MqttClient::connect(options).await?;

    // Publish the message
    client
        .publish_with_properties(
            &args.topic,
            Bytes::from(payload),
            args.qos,
            args.retain,
            args.properties.into(),
        )
        .await?;

    // Disconnect gracefully
    client.disconnect().await?;

    Ok(())
}
