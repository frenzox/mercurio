//! `recado pub`.

use std::io::{self, Read};
use std::path::PathBuf;

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

    /// Message payload, as text; without it or -f, standard input is
    /// published as it is, binary or not
    #[arg(short = 'm', long)]
    message: Option<String>,

    /// Publish the contents of this file, binary or not
    #[arg(short = 'f', long, value_name = "PATH", conflicts_with = "message")]
    file: Option<PathBuf>,

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

    /// Data that ties a reply to its request, as text
    #[arg(long)]
    correlation_data: Option<String>,

    /// Correlation data as bytes, in hex, such as 00ff7a
    #[arg(long, value_name = "HEX", value_parser = parse_hex, conflicts_with = "correlation_data")]
    correlation_data_hex: Option<Bytes>,

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
            correlation_data: args
                .correlation_data
                .map(Bytes::from)
                .or(args.correlation_data_hex),
            user_properties: args.user_properties,
        }
    }
}

fn parse_hex(value: &str) -> Result<Bytes, String> {
    let wrong = || format!("`{value}` isn't bytes in hex, two digits each");
    if !value.len().is_multiple_of(2) || !value.is_ascii() {
        return Err(wrong());
    }
    (0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).map_err(|_| wrong()))
        .collect::<Result<Vec<u8>, _>>()
        .map(Bytes::from)
}

fn parse_user_property(value: &str) -> Result<(String, String), String> {
    value
        .split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got `{value}`"))
}

pub async fn run(mut args: PubArgs) -> Result<(), Failure> {
    let payload = match (args.message, &args.file) {
        (Some(message), _) => Bytes::from(message),
        (None, Some(path)) => std::fs::read(path)
            .map(Bytes::from)
            .map_err(|e| Failure::Usage(format!("can't read {}: {e}", path.display())))?,
        (None, None) => {
            let mut buffer = Vec::new();
            io::stdin().read_to_end(&mut buffer)?;
            Bytes::from(buffer)
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
            payload,
            args.qos,
            args.retain,
            args.properties.into(),
        )
        .await?;

    // Disconnect gracefully
    client.disconnect().await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use clap::Parser;
    use mercurio_client::MessageProperties;

    use super::PropertyArgs;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        properties: PropertyArgs,
    }

    fn correlation(args: &[&str]) -> Result<Option<Bytes>, clap::Error> {
        Cli::try_parse_from(std::iter::once("recado").chain(args.iter().copied()))
            .map(|cli| MessageProperties::from(cli.properties).correlation_data)
    }

    #[test]
    fn correlation_data_is_text_or_hex() {
        assert_eq!(
            correlation(&["--correlation-data", "42"]).unwrap(),
            Some(Bytes::from_static(b"42"))
        );
        assert_eq!(
            correlation(&["--correlation-data-hex", "00fF7a"]).unwrap(),
            Some(Bytes::from_static(&[0x00, 0xff, 0x7a]))
        );
        for wrong in ["0", "zz", "0x00"] {
            assert!(
                correlation(&["--correlation-data-hex", wrong]).is_err(),
                "{wrong}"
            );
        }
        let both = ["--correlation-data", "a", "--correlation-data-hex", "00"];
        assert!(correlation(&both).is_err());
    }
}
