//! Shared utilities for mercurio CLI tools.

use clap::Args;
use mercurio_client::ConnectOptions;
use mercurio_core::protocol::ProtocolVersion;

/// Common connection arguments shared between pub and sub tools.
#[derive(Args, Debug, Clone)]
pub struct ConnectionArgs {
    /// MQTT broker hostname
    #[arg(short = 'H', long, default_value = "localhost")]
    pub host: String,

    /// MQTT broker port
    #[arg(short = 'p', long, default_value = "1883")]
    pub port: u16,

    /// Client ID (auto-generated if not specified)
    #[arg(short = 'i', long)]
    pub client_id: Option<String>,

    /// Username for authentication
    #[arg(short = 'u', long)]
    pub username: Option<String>,

    /// Password for authentication
    #[arg(short = 'P', long)]
    pub password: Option<String>,

    /// Keep-alive interval in seconds
    #[arg(short = 'k', long, default_value = "60")]
    pub keep_alive: u16,

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
        let mut opts = ConnectOptions::new(&self.host, self.port)
            .keep_alive(self.keep_alive)
            .protocol_version(self.parse_protocol_version());

        if let Some(ref id) = self.client_id {
            opts = opts.client_id(id);
        }

        if let Some(ref user) = self.username {
            opts = opts.username(user);
        }

        if let Some(ref pass) = self.password {
            opts = opts.password(pass.as_bytes().to_vec());
        }

        opts
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
