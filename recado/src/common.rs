//! Shared utilities for recado.

use clap::Args;
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

    /// MQTT protocol version (5, 311, or 31)
    #[arg(short = 'V', long, default_value = "5")]
    pub protocol_version: String,

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
            .protocol_version(self.parse_protocol_version())
    }

    fn parse_protocol_version(&self) -> ProtocolVersion {
        match self.protocol_version.as_str() {
            "5" | "5.0" | "v5" => ProtocolVersion::V5,
            "311" | "3.1.1" | "v311" => ProtocolVersion::V3_1_1,
            "31" | "3.1" | "v31" => ProtocolVersion::V3_1,
            _ => ProtocolVersion::V5,
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

    fmt().with_env_filter(filter).init();
}
