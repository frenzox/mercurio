//! Shared utilities for recado.

use clap::{Args, ValueEnum};
use mercurio_client::{cli::ConnectArgs, ConnectOptions};
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

impl ConnectionArgs {
    /// Convert CLI arguments to ConnectOptions.
    pub fn to_connect_options(&self) -> ConnectOptions {
        self.connect
            .to_connect_options()
            .keep_alive(self.keep_alive)
            .clean_start(!self.no_clean_start)
            .session_expiry(self.session_expiry)
            .protocol_version(self.protocol_version.into())
    }
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
    fn unknown_protocol_versions_are_refused() {
        for wrong in ["4", "3", "mqttv4", ""] {
            assert!(version(wrong).is_err(), "{wrong:?}");
        }
    }
}
