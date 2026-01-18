//! Mercuriod - MQTT broker daemon

use std::path::PathBuf;

use clap::Parser;
use tokio::{net::TcpListener, signal};
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

mod config;
use config::Config;

#[derive(Parser, Debug)]
#[command(name = "mercuriod")]
#[command(about = "Mercurio MQTT broker daemon")]
#[command(version)]
struct Args {
    /// Path to configuration file
    #[arg(short = 'c', long, default_value = "/etc/mercurio/config.toml")]
    config: PathBuf,

    /// Override listen address (e.g., 0.0.0.0:1883)
    #[arg(short = 'l', long)]
    listen: Option<String>,

    /// Enable verbose output
    #[arg(short = 'v', long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // Load configuration
    let config = if args.config.exists() {
        Config::from_file(&args.config)?
    } else {
        if args.config.to_str() != Some("/etc/mercurio/config.toml") {
            eprintln!("Config file not found: {}", args.config.display());
            std::process::exit(1);
        }
        // Use defaults if no config file and using default path
        Config::default()
    };

    // Initialize logging
    let log_level = if args.verbose {
        "debug"
    } else {
        &config.logging.level
    };

    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();

    // Determine listen address (CLI overrides config)
    let listen_addr = args
        .listen
        .unwrap_or_else(|| format!("{}:{}", config.server.host, config.server.port));

    info!("Starting Mercurio MQTT broker on {}", listen_addr);

    let listener = TcpListener::bind(&listen_addr).await?;

    info!("Mercurio MQTT broker listening on {}", listen_addr);

    mercurio_server::server::run(listener, signal::ctrl_c()).await;

    info!("Mercurio MQTT broker stopped");

    Ok(())
}
