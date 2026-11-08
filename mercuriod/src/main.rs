//! Mercuriod - MQTT broker daemon

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher};
use clap::{Parser, Subcommand};
use mercurio_server::acl::AclFile;
use mercurio_server::plugin::Plugin;
use mercurio_server::server::{
    AuthConfig as ServerAuthConfig, ConnectPolicy, ListenerConfig, LocalConnector, Redirect,
    ServerConfig, Socket, Transport,
};
use mercurio_storage::{
    postgres::{DatabaseLock, PostgresStore},
    sqlite::SqliteStore,
};
use tokio::{net::TcpListener, signal};
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter};

mod bridge;
mod config;
mod credentials;
mod dynsec;
mod http_auth;
mod reload;
#[cfg(unix)]
mod syslog;

use config::{Config, StorageConfig};
use credentials::HashedCredentialValidator;
use dynsec::DynamicSecurity;
use http_auth::HttpAuth;
use reload::{Reload, Reloadable};

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
    /// Create a dynamic security file with an administrator
    DynsecInit {
        /// Path of the file to create
        #[arg(short = 'f', long)]
        file: String,

        /// User name of the administrator
        #[arg(short = 'u', long, default_value = "admin")]
        username: String,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // glibc gives each thread that allocates an arena of its own and keeps
    // the memory freed there, so under load the broker's memory grew by
    // tens of megabytes and stayed; two arenas keep it flat, with no
    // measurable cost in throughput. Set before the runtime starts threads.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: called before any other thread exists
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    // The HTTP auth client uses the process default provider
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

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
        Some(Commands::DynsecInit { file, username }) => {
            let password = prompt_new_password(&username)?;
            DynamicSecurity::init(&file, &username, &password)?;
            eprintln!("Created `{}` with administrator `{}`", file, username);
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

    let mut filter = EnvFilter::try_new(log_level).unwrap_or_else(|_| EnvFilter::new("info"));
    if !config.logging.connection_messages {
        filter = filter.add_directive(
            format!("{}=off", mercurio_server::CONNECTIONS)
                .parse()
                .expect("valid directive"),
        );
    }
    #[cfg(unix)]
    let syslog = match config.logging.syslog {
        true => Some(
            syslog::Syslog::connect("/dev/log")
                .map_err(|e| format!("Failed to connect to syslog at /dev/log: {}", e))?,
        ),
        false => None,
    };
    #[cfg(not(unix))]
    let syslog: Option<std::io::Sink> = match config.logging.syslog {
        true => return Err("syslog is only supported on Unix".into()),
        false => None,
    };
    match (syslog, &config.logging.file) {
        (Some(syslog), _) => fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .without_time()
            .with_writer(syslog)
            .init(),
        (None, Some(path)) => {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| format!("Failed to open log file {}: {}", path, e))?;
            fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        (None, None) => fmt().with_env_filter(filter).init(),
    }

    // Determine listen address (CLI overrides config)
    let interface = config.server.bind_interface.as_deref();
    let listen_addr = cli
        .listen
        .unwrap_or_else(|| address(&config.server.host, config.server.port));

    // Before binding, so a standby doesn't accept connections it can't serve
    let (postgres, database_lock) = match &config.storage {
        StorageConfig::Postgres {
            url,
            ca_path,
            standby,
        } => {
            info!("Using PostgreSQL storage");
            let ca_path = ca_path.as_deref();
            let held = match PostgresStore::connect_exclusive(url, ca_path, false).await? {
                Some(held) => held,
                None if *standby => {
                    info!("Another broker is using the database; standing by to take over");
                    let held = PostgresStore::connect_exclusive(url, ca_path, true)
                        .await?
                        .expect("waiting returns once the database is taken");
                    info!("Took over the database");
                    held
                }
                None => {
                    return Err("Another broker is using the database; set \
                         `standby = true` in [storage] to stand by for it"
                        .into())
                }
            };
            let (store, lock) = held;
            (Some(store), Some(lock))
        }
        _ => (None, None),
    };
    let shutdown = async {
        match &database_lock {
            Some(lock) => tokio::select! {
                _ = shutdown_signal() => {}
                _ = lock.lost() => error!("Lost hold of the database; stopping, since a standby may take over"),
            },
            None => shutdown_signal().await,
        }
    };

    info!("Starting Mercurio MQTT broker on {}", listen_addr);

    let listener = bind_tcp(&listen_addr, interface).await?;

    let websocket_listener = match config.server.websocket_port {
        Some(port) => {
            let websocket_addr = address(&config.server.host, port);
            let websocket_listener = bind_tcp(&websocket_addr, interface).await?;
            info!("Serving MQTT over WebSocket on {}", websocket_addr);
            Some(websocket_listener)
        }
        None => None,
    };

    info!("Mercurio MQTT broker listening on {}", listen_addr);

    // Build TLS configuration if enabled
    let limits = config.server.limits()?;
    let mut policy = ConnectPolicy::default();
    policy.response_information = config.server.response_information.clone();
    policy.redirect = config
        .server
        .redirect
        .clone()
        .map(|r| Redirect::new(r.server_reference, r.permanent));
    policy.require_client_id = !config.server.allow_zero_length_clientid;
    policy.auto_id_prefix = config.server.auto_id_prefix.clone();
    policy.client_id_prefixes = config.server.clientid_prefixes.clone();

    let tls_config = config.server.tls.build()?;
    // The WebSocket listener shares the main listener's TLS settings
    let mut listeners: Vec<_> = websocket_listener
        .map(|listener| {
            let mut value = ListenerConfig::new(listener, Transport::WebSocket);
            value.tls = tls_config.clone();
            value.http_dir = config.server.http_dir.clone().map(Into::into);
            value
        })
        .into_iter()
        .collect();
    let mut reloadable: Vec<Arc<dyn Reload>> = Vec::new();
    for extra in &config.listeners {
        let (listener, addr) = bind_listener(extra).await?;
        info!(
            "Serving {:?}{} on {}",
            extra.protocol,
            if extra.tls.enabled { " with TLS" } else { "" },
            addr
        );
        listeners.push({
            let mut value = ListenerConfig::new(listener, extra.protocol.into());
            value.tls = extra.tls.build()?;
            value.proxy_protocol = extra.proxy_protocol;
            value.proxy_from = extra
                .proxy_from
                .iter()
                .map(|range| range.parse())
                .collect::<Result<_, _>>()?;
            value.mount_point = extra.mount_point.clone();
            value.max_connections = extra.max_connections;
            value.http_dir = extra.http_dir.clone().map(Into::into);
            if let Some(auth) = &extra.auth {
                let (auth, plugins) = build_auth(auth, &mut reloadable, false)?;
                value.auth = Some(auth);
                value.plugins = Some(plugins);
            }
            value
        });
    }

    let (connector, local_listener) = LocalConnector::new();
    let (auth_config, plugins) = build_auth(&config.auth, &mut reloadable, true)?;

    for bridge in config.bridges {
        info!("Starting bridge {}", bridge.name);
        bridge::spawn(bridge, connector.clone())?;
    }

    let mut server_config = ServerConfig::default();
    server_config.tls = tls_config;
    server_config.auth = auth_config;
    server_config.plugins = plugins;
    server_config.limits = limits;
    server_config.policy = policy;
    server_config.listeners = listeners;
    server_config.local_listener = Some(local_listener);
    server_config.sys_interval =
        (config.server.sys_interval > 0).then(|| Duration::from_secs(config.server.sys_interval));
    #[cfg(unix)]
    tokio::spawn(reload_on_sighup(reloadable));

    let result = match config.storage {
        StorageConfig::Memory => {
            mercurio_server::server::run_with_config(listener, server_config, shutdown).await
        }
        StorageConfig::Sqlite { path } => {
            info!("Using SQLite storage at {}", path);
            let storage = Arc::new(SqliteStore::new(&path)?);
            mercurio_server::server::run_with_storage(listener, server_config, storage, shutdown)
                .await
        }
        StorageConfig::Postgres { .. } => {
            let storage = Arc::new(postgres.expect("opened before binding"));
            mercurio_server::server::run_with_storage(listener, server_config, storage, shutdown)
                .await
        }
    };
    // A non-zero status tells service managers the broker failed to start
    if let Err(e) = result {
        error!("Failed to start the broker: {}", e);
        std::process::exit(1);
    }

    if database_lock.as_ref().is_some_and(DatabaseLock::is_lost) {
        std::process::exit(1);
    }

    info!("Mercurio MQTT broker stopped");

    Ok(())
}

/// `host:port`, with an IPv6 host in brackets.
fn address(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Listens on `addr`, only through `interface` when given.
async fn bind_tcp(addr: &str, interface: Option<&str>) -> Result<TcpListener, String> {
    let failed = |e: std::io::Error| format!("Failed to listen on {}: {}", addr, e);
    let Some(interface) = interface else {
        return TcpListener::bind(addr).await.map_err(failed);
    };
    let socket_addr = tokio::net::lookup_host(addr)
        .await
        .map_err(failed)?
        .next()
        .ok_or_else(|| format!("{addr} resolves to no address"))?;
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(socket_addr),
        socket2::Type::STREAM,
        None,
    )
    .map_err(failed)?;
    #[cfg(any(target_os = "linux", target_os = "android"))]
    socket
        .bind_device(Some(interface.as_bytes()))
        .map_err(|e| format!("Failed to bind to interface {}: {}", interface, e))?;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    return Err(format!(
        "bind_interface {interface} is only supported on Linux"
    ));
    socket.set_reuse_address(true).map_err(failed)?;
    socket.set_nonblocking(true).map_err(failed)?;
    socket.bind(&socket_addr.into()).map_err(failed)?;
    socket.listen(1024).map_err(failed)?;
    TcpListener::from_std(socket.into()).map_err(failed)
}

/// Binds an extra listener, returning it with its address for logs.
async fn bind_listener(listener: &config::ListenerConfig) -> Result<(Socket, String), String> {
    match (listener.port, &listener.path) {
        (Some(port), None) => {
            let addr = address(&listener.host, port);
            let socket = bind_tcp(&addr, listener.bind_interface.as_deref()).await?;
            Ok((socket.into(), addr))
        }
        #[cfg(unix)]
        (None, Some(path)) => {
            use std::os::unix::fs::FileTypeExt;
            // Left behind by a broker that did not stop cleanly
            if std::fs::metadata(path).is_ok_and(|m| m.file_type().is_socket()) {
                let _ = std::fs::remove_file(path);
            }
            let socket = tokio::net::UnixListener::bind(path)
                .map_err(|e| format!("Failed to listen on {}: {}", path, e))?;
            Ok((socket.into(), path.clone()))
        }
        _ => Err("Each [[listeners]] entry needs either `port` or `path`".to_string()),
    }
}

/// Resolves on Ctrl+C or, on Unix, SIGTERM: what service managers and
/// container runtimes send to stop the daemon.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate = signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to listen for SIGTERM");
        tokio::select! {
            _ = signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = signal::ctrl_c().await;
}

/// The authentication and access control of `config`, and the plugins that
/// check credentials, adding the files it loads to `reloadable`.
fn build_auth(
    config: &config::AuthConfig,
    reloadable: &mut Vec<Arc<dyn Reload>>,
    main: bool,
) -> Result<(ServerAuthConfig, Vec<Arc<dyn Plugin>>), String> {
    if let Some(path) = &config.dynamic_security {
        if config.password_file.is_some() || config.acl_file.is_some() {
            return Err("dynamic_security replaces password_file and acl_file".to_string());
        }
        if !main {
            return Err("dynamic_security is only supported in [auth]".to_string());
        }
        let security = Arc::new(DynamicSecurity::load(path)?);
        info!("Loaded dynamic security from {}", path);
        reloadable.push(security.clone());
        let mut value = ServerAuthConfig::default();
        value.require_auth = config.enabled;
        value.allow_anonymous = config.allow_anonymous;
        return Ok((value, vec![security]));
    }

    let http = config.http.as_ref().map(|http| {
        let auth = Arc::new(HttpAuth::new(
            http.user_url.clone(),
            http.acl_url.clone(),
            Duration::from_secs(http.timeout_seconds),
            Duration::from_secs(http.cache_seconds),
        ));
        (auth, http.user_url.is_some(), http.acl_url.is_some())
    });
    let (http_users, http_acl) = http
        .as_ref()
        .map_or((false, false), |(_, users, acl)| (*users, *acl));
    // Each URL replaces the file doing the same check
    if (http_users && config.password_file.is_some()) || (http_acl && config.acl_file.is_some()) {
        return Err("[auth.http] URLs replace password_file and acl_file".to_string());
    }

    let mut plugins: Vec<Arc<dyn Plugin>> = Vec::new();
    if config.enabled {
        if let Some(path) = &config.password_file {
            let validator = Arc::new(Reloadable::load(path, load_credentials)?);
            info!("Loaded credentials from {}", path);
            reloadable.push(validator.clone());
            plugins.push(validator);
        }
    }
    // Without authentication, only its access control is used
    if let Some((auth, _, _)) = http.filter(|_| config.enabled || http_acl) {
        plugins.push(auth);
    }
    if let Some(path) = &config.acl_file {
        let acl = Arc::new(Reloadable::load(path, load_acl_file)?);
        info!("Loaded topic access control from {}", path);
        reloadable.push(acl.clone());
        plugins.push(acl);
    }

    let mut value = ServerAuthConfig::default();
    value.require_auth = config.enabled;
    value.allow_anonymous = config.allow_anonymous;
    Ok((value, plugins))
}

/// Rereads the ACL and password files on SIGHUP. ACL changes apply to every
/// later publish and subscribe; password changes apply to later connections.
#[cfg(unix)]
async fn reload_on_sighup(files: Vec<Arc<dyn Reload>>) {
    let mut hangup = signal::unix::signal(signal::unix::SignalKind::hangup())
        .expect("failed to listen for SIGHUP");
    while hangup.recv().await.is_some() {
        info!("Received SIGHUP, reloading");
        for file in &files {
            match file.reload() {
                Ok(()) => info!("Reloaded {}", file.path()),
                Err(e) => error!("Keeping the previous {}: {}", file.path(), e),
            }
        }
    }
}

fn load_acl_file(path: &str) -> Result<AclFile, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read ACL file {}: {}", path, e))?;
    AclFile::parse(&contents).map_err(|e| format!("Invalid ACL file {}: {}", path, e))
}

fn load_credentials(path: &str) -> Result<HashedCredentialValidator, String> {
    let credentials = load_password_file(path).map_err(|e| e.to_string())?;
    Ok(HashedCredentialValidator::new(credentials))
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
    let password = prompt_new_password(username)?;

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

/// Asks for a new password twice.
fn prompt_new_password(username: &str) -> Result<String, Box<dyn std::error::Error>> {
    let password = rpassword::prompt_password(format!("Password for `{}`: ", username))?;
    if password.is_empty() {
        return Err("Password cannot be empty".into());
    }
    let confirm = rpassword::prompt_password("Confirm password: ")?;
    if password != confirm {
        return Err("Passwords do not match".into());
    }
    Ok(password)
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
    writeln!(
        file,
        "# Managed by `mercuriod passwd`. Do not edit manually."
    )?;
    for (username, hash) in entries {
        writeln!(file, "{}:{}", username, hash)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::address;

    #[test]
    fn test_ipv6_hosts_are_bracketed() {
        assert_eq!(address("::", 1883), "[::]:1883");
        assert_eq!(address("[::1]", 1883), "[::1]:1883");
        assert_eq!(address("0.0.0.0", 1883), "0.0.0.0:1883");
        assert_eq!(address("localhost", 1883), "localhost:1883");
    }
}
