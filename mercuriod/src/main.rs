//! Mercuriod - MQTT broker daemon

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher};
use clap::{Parser, Subcommand};
use mercurio_server::server::AuthConfig as ServerAuthConfig;
use mercurio_server::tls::TlsConfig;
use tokio::{net::TcpListener, signal};
use tracing::info;
use tracing_subscriber::{fmt, EnvFilter};

mod config;
mod credentials;

use config::Config;
use credentials::HashedCredentialValidator;

#[derive(Parser, Debug)]
#[command(name = "mercuriod")]
#[command(about = "Mercurio MQTT broker daemon")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

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

#[derive(Subcommand, Debug)]
enum Commands {
    /// Manage password file credentials
    Passwd {
        /// Path to the password file
        #[arg(short = 'f', long)]
        file: String,

        /// Username to add or update
        #[arg(short = 'u', long)]
        username: String,

        /// Delete the user instead of adding/updating
        #[arg(short = 'd', long)]
        delete: bool,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Passwd {
            file,
            username,
            delete,
        }) => {
            if delete {
                delete_user(&file, &username)?;
            } else {
                add_or_update_user(&file, &username)?;
            }
            return Ok(());
        }
        None => {}
    }

    // Load configuration
    let config = if cli.config.exists() {
        Config::from_file(&cli.config)?
    } else {
        if cli.config.to_str() != Some("/etc/mercurio/config.toml") {
            eprintln!("Config file not found: {}", cli.config.display());
            std::process::exit(1);
        }
        Config::default()
    };

    // Initialize logging
    let log_level = if cli.verbose {
        "debug"
    } else {
        &config.logging.level
    };

    let filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).init();

    // Determine listen address (CLI overrides config)
    let listen_addr = cli
        .listen
        .unwrap_or_else(|| format!("{}:{}", config.server.host, config.server.port));

    info!("Starting Mercurio MQTT broker on {}", listen_addr);

    let listener = TcpListener::bind(&listen_addr).await?;

    info!("Mercurio MQTT broker listening on {}", listen_addr);

    // Build TLS configuration if enabled
    let tls_config = if config.server.tls.enabled {
        let cert_path = config
            .server
            .tls
            .cert_path
            .ok_or_else(|| "TLS enabled but cert_path not specified in configuration")?;
        let key_path = config
            .server
            .tls
            .key_path
            .ok_or_else(|| "TLS enabled but key_path not specified in configuration")?;

        let mut tls = TlsConfig::new(cert_path, key_path);
        if let Some(ca_path) = config.server.tls.ca_path {
            tls = tls.with_client_auth(ca_path);
        }

        info!("TLS enabled");
        Some(tls)
    } else {
        None
    };

    // Build authentication configuration
    let auth_config = if config.auth.enabled {
        let credential_validator = if let Some(ref password_file) = config.auth.password_file {
            let credentials = load_password_file(password_file)?;
            info!(
                "Loaded {} credentials from password file",
                credentials.len()
            );
            Some(Arc::new(HashedCredentialValidator::new(credentials))
                as Arc<dyn mercurio_server::auth::CredentialValidator>)
        } else {
            None
        };

        info!("Authentication enabled");
        ServerAuthConfig {
            require_auth: true,
            credential_validator,
            auth_manager: None,
        }
    } else {
        ServerAuthConfig::default()
    };

    mercurio_server::server::run_with_tls(listener, tls_config, auth_config, signal::ctrl_c())
        .await;

    info!("Mercurio MQTT broker stopped");

    Ok(())
}

/// Load a password file with format: `username:hash` per line.
/// Lines starting with `#` and empty lines are ignored.
fn load_password_file(path: &str) -> Result<HashMap<String, String>, Box<dyn std::error::Error>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read password file `{}`: {}", path, e))?;

    let mut credentials = HashMap::new();
    for (line_num, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((username, password_hash)) = line.split_once(':') else {
            return Err(format!(
                "Invalid format in password file `{}` at line {}: expected `username:hash`",
                path,
                line_num + 1
            )
            .into());
        };
        credentials.insert(username.to_string(), password_hash.to_string());
    }

    Ok(credentials)
}

/// Add or update a user in the password file.
fn add_or_update_user(path: &str, username: &str) -> Result<(), Box<dyn std::error::Error>> {
    let password = rpassword::prompt_password(format!("Password for `{}`: ", username))?;
    if password.is_empty() {
        return Err("Password cannot be empty".into());
    }
    let confirm = rpassword::prompt_password("Confirm password: ")?;
    if password != confirm {
        return Err("Passwords do not match".into());
    }

    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("Failed to hash password: {}", e))?
        .to_string();

    let mut entries = load_password_file_entries(path);
    entries.insert(username.to_string(), hash);
    write_password_file(path, &entries)?;

    eprintln!("User `{}` added/updated in `{}`", username, path);
    Ok(())
}

/// Delete a user from the password file.
fn delete_user(path: &str, username: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut entries = load_password_file_entries(path);
    if entries.remove(username).is_none() {
        return Err(format!("User `{}` not found in `{}`", username, path).into());
    }
    write_password_file(path, &entries)?;
    eprintln!("User `{}` deleted from `{}`", username, path);
    Ok(())
}

/// Load password file entries, returning empty map if file doesn't exist.
fn load_password_file_entries(path: &str) -> HashMap<String, String> {
    load_password_file(path).unwrap_or_default()
}

/// Write entries to a password file.
fn write_password_file(
    path: &str,
    entries: &HashMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "# Mercurio password file")?;
    writeln!(file, "# Managed by `mercuriod passwd`. Do not edit manually.")?;
    for (username, hash) in entries {
        writeln!(file, "{}:{}", username, hash)?;
    }
    Ok(())
}
