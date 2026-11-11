//! Shared utilities for recado.

use clap::{Args, ValueEnum};
use mercurio_client::{cli::ConnectArgs, ConnectOptions, QoS, Will};
use mercurio_core::protocol::ProtocolVersion;

/// Common connection arguments shared between pub and sub tools.
#[derive(Args, Debug, Clone)]
pub struct ConnectionArgs {
    #[command(flatten)]
    pub connect: ConnectArgs,

    /// Keep-alive interval in seconds
    #[arg(short = 'k', long, default_value = "60")]
    pub keep_alive: u16,

    /// Keep the session after disconnecting instead of starting clean
    #[arg(short = 'c', long)]
    pub no_clean_start: bool,

    /// Seconds the broker keeps the session after the connection closes
    /// (MQTT 5)
    #[arg(short = 'x', long, default_value = "0")]
    pub session_expiry: u32,

    /// MQTT protocol version; mosquitto's mqttv5, mqttv311 and mqttv31 work
    /// too
    #[arg(short = 'V', long, value_enum, default_value_t = Protocol::V5)]
    pub protocol_version: Protocol,

    /// Enable verbose output
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

/// The will, which the broker publishes when the client goes away without
/// disconnecting.
#[derive(Args, Debug)]
pub struct WillArgs {
    /// Will message topic (sent by broker if client disconnects unexpectedly)
    #[arg(long)]
    will_topic: Option<String>,

    /// Will message payload
    #[arg(long, requires = "will_topic")]
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

impl WillArgs {
    /// `options` with the will, when there is one.
    pub fn apply(self, options: ConnectOptions) -> ConnectOptions {
        match self.will_topic {
            Some(topic) => options.will(
                Will::new(topic, self.will_message.unwrap_or_default())
                    .qos(self.will_qos)
                    .retain(self.will_retain)
                    .delay(self.will_delay),
            ),
            None => options,
        }
    }
}

impl ConnectionArgs {
    /// Convert CLI arguments to ConnectOptions.
    pub fn to_connect_options(&self) -> ConnectOptions {
        let options = self
            .connect
            .to_connect_options()
            .keep_alive(self.keep_alive)
            .clean_start(!self.no_clean_start)
            .session_expiry(self.session_expiry)
            .protocol_version(self.protocol_version.into());
        match self.client_id() {
            Some(id) => options.client_id(id),
            None => options,
        }
    }

    /// The client id to send: the one given with -i, or one made up where
    /// MQTT 3 needs an id and none was given. MQTT 3.1 takes no empty id,
    /// and 3.1.1 only with a clean session. Otherwise the id stays empty
    /// and the broker assigns one.
    pub fn client_id(&self) -> Option<String> {
        if let Some(id) = &self.connect.client_id {
            return Some(id.clone());
        }
        let needed = match self.protocol_version {
            Protocol::V31 => true,
            Protocol::V311 => self.no_clean_start,
            Protocol::V5 => false,
        };
        needed.then(made_up_client_id)
    }
}

/// `recado-` and 16 hex digits, 23 characters, the most MQTT 3.1 takes.
/// `RandomState`'s keys are seeded from the operating system, so each call
/// gives another.
fn made_up_client_id() -> String {
    use std::hash::BuildHasher;

    let random = std::collections::hash_map::RandomState::new();
    format!("recado-{:016x}", random.hash_one(std::process::id()))
}

/// An MQTT version, as `-V` names it.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    #[value(name = "5", aliases = ["5.0", "v5", "mqttv5"])]
    V5,
    #[value(name = "311", aliases = ["3.1.1", "v311", "mqttv311"])]
    V311,
    #[value(name = "31", aliases = ["3.1", "v31", "mqttv31"])]
    V31,
}

impl From<Protocol> for ProtocolVersion {
    fn from(protocol: Protocol) -> Self {
        match protocol {
            Protocol::V5 => ProtocolVersion::V5,
            Protocol::V311 => ProtocolVersion::V3_1_1,
            Protocol::V31 => ProtocolVersion::V3_1,
        }
    }
}

/// Parses a QoS level, 0, 1 or 2.
pub fn qos_level(value: &str) -> Result<QoS, String> {
    match value {
        "0" => Ok(QoS::AtMostOnce),
        "1" => Ok(QoS::AtLeastOnce),
        "2" => Ok(QoS::ExactlyOnce),
        _ => Err(format!("`{value}` isn't a QoS level, 0, 1 or 2")),
    }
}

/// Initialize tracing/logging based on verbosity.
pub fn init_logging(verbose: bool) {
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = if verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::new("warn")
    };

    // Standard output carries the payloads
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use mercurio_core::protocol::ProtocolVersion;

    use super::ConnectionArgs;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        connection: ConnectionArgs,
    }

    fn version(value: &str) -> Result<ProtocolVersion, clap::Error> {
        Cli::try_parse_from(["recado", "-V", value])
            .map(|cli| cli.connection.protocol_version.into())
    }

    #[test]
    fn protocol_versions_take_recado_and_mosquitto_spellings() {
        for (spellings, expected) in [
            (&["5", "5.0", "v5", "mqttv5"][..], ProtocolVersion::V5),
            (
                &["311", "3.1.1", "v311", "mqttv311"],
                ProtocolVersion::V3_1_1,
            ),
            (&["31", "3.1", "v31", "mqttv31"], ProtocolVersion::V3_1),
        ] {
            for spelling in spellings {
                assert_eq!(version(spelling).unwrap(), expected, "{spelling}");
            }
        }
    }

    #[test]
    fn a_client_id_is_made_up_only_where_mqtt_3_needs_one() {
        let id = |args: &[&str]| {
            Cli::try_parse_from(std::iter::once("recado").chain(args.iter().copied()))
                .unwrap()
                .connection
                .client_id()
        };
        assert_eq!(id(&[]), None);
        assert_eq!(id(&["-c"]), None);
        assert_eq!(id(&["-V", "311"]), None);
        assert_eq!(id(&["-V", "31", "-i", "me"]).as_deref(), Some("me"));
        for args in [&["-V", "31"][..], &["-V", "311", "-c"]] {
            let made_up = id(args).unwrap();
            // MQTT 3.1 takes 1 to 23 characters
            assert!(made_up.starts_with("recado-"), "{made_up}");
            assert!((1..=23).contains(&made_up.len()), "{made_up}");
            assert_ne!(id(args), Some(made_up), "{args:?}");
        }
    }

    #[test]
    fn unknown_protocol_versions_are_refused() {
        for wrong in ["4", "3", "mqttv4", ""] {
            assert!(version(wrong).is_err(), "{wrong:?}");
        }
    }
}
