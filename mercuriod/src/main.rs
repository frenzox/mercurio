//! Mercuriod - MQTT broker daemon

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHasher};
use clap::{Parser, Subcommand};
use mercurio_server::acl::AclFile;
use mercurio_server::admin::AdminExtension;
use mercurio_server::error::ServerError;
use mercurio_server::plugin::Plugin;
use mercurio_server::server::{
    AuthConfig as ServerAuthConfig, ConnectPolicy, ListenerConfig, LocalConnector, Redirect,
    Server, ServerConfig, Socket, Transport,
};
use mercurio_storage::{
    memory::MemoryStore,
    postgres::{DatabaseLock, PostgresStore},
    sqlite::SqliteStore,
    MqttStore,
};
use tokio::{net::TcpListener, signal};
use tracing::{error, info};

#[cfg(target_os = "linux")]
mod admin_socket;
mod bridge;
mod commands;
mod config;
mod credentials;
mod dynsec;
mod http_auth;
mod logging;
mod password_file;
mod reload;
#[cfg(unix)]
mod syslog;
#[cfg(feature = "wasm")]
mod wasm;

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

    // Before any wait, as SIGHUP's default action ends the process. Signals
    // that come before the broker serves wait here and cause one reload
    // once it does.
    #[cfg(unix)]
    let hangup = signal::unix::signal(signal::unix::SignalKind::hangup())?;

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

    let log_level = logging::init(&config.logging, cli.verbose)?;
    // Now, before parts of the configuration are moved out
    let shown_config = serde_json::to_value(&config)
        .inspect_err(|e| tracing::warn!("getConfig will be unavailable: {}", e))
        .ok();

    // Determine listen address (CLI overrides config)
    let interface = config.server.bind_interface.as_deref();
    let listen_addr = cli
        .listen
        .unwrap_or_else(|| address(&config.server.host, config.server.port));

    // Before binding, so a standby doesn't accept connections it can't serve
    let (postgres, database_lock) = match &config.storage {
        StorageConfig::Postgres { url, standby, .. } => {
            info!("Using PostgreSQL storage");
            let options = config
                .storage
                .postgres_options()?
                .expect("the backend is postgres");
            let held = match PostgresStore::connect_exclusive(url, &options, false).await? {
                Some(held) => held,
                None if *standby => {
                    info!("Another broker is using the database; standing by to take over");
                    let held = PostgresStore::connect_exclusive(url, &options, true)
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
    let (auth_config, builtins, dynsec) = build_auth(&config.auth, &mut reloadable, true)?;
    let wasm = load_plugins(
        &config.plugins,
        config.server.plugin_timeout(),
        &mut reloadable,
    )
    .await?;
    // Listeners naming plugins share these copies of `[auth]`'s built-ins
    // and of the plugins with the broker's chain
    let chain = |builtins: &[Arc<dyn Plugin>], names: Option<&[String]>| {
        let named = wasm
            .iter()
            .filter(|(name, _)| names.is_none_or(|names| names.contains(name)))
            .map(|(_, plugin)| Arc::clone(plugin));
        builtins.iter().cloned().chain(named).collect::<Vec<_>>()
    };
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
            let names = extra.plugins.as_deref();
            if let Some(auth) = &extra.auth {
                let (auth, plugins, _) = build_auth(auth, &mut reloadable, false)?;
                value.auth = Some(auth);
                value.plugins = Some(chain(&plugins, names));
            } else if names.is_some() {
                value.plugins = Some(chain(&builtins, names));
            }
            value
        });
    }

    #[cfg(target_os = "linux")]
    let admin =
        admin_socket::open(&config.admin.clone().unwrap_or_default())?.map(|(listener, socket)| {
            listeners.push(listener);
            socket
        });
    #[cfg(not(target_os = "linux"))]
    if config
        .admin
        .as_ref()
        .is_some_and(|admin| !admin.socket.is_empty())
    {
        tracing::warn!("Ignoring [admin]: the admin socket needs Linux");
    }

    let (connector, local_listener) = LocalConnector::new();
    let plugins = chain(&builtins, None);

    let bridges = config
        .bridges
        .into_iter()
        .map(|bridge| {
            info!("Starting bridge {}", bridge.name);
            bridge::spawn(bridge, connector.clone())
        })
        .collect::<Result<Vec<_>, _>>()?;

    let reloadable: Arc<[Arc<dyn Reload>]> = reloadable.into();
    let commands = Arc::new(
        commands::DaemonCommands::new(log_level)
            .with_reload(Arc::clone(&reloadable))
            .with_config(shown_config)
            .with_bridges(bridges),
    );

    let mut server_config = ServerConfig::default();
    server_config.tls = tls_config;
    server_config.auth = auth_config;
    server_config.plugins = plugins;
    server_config.limits = limits;
    server_config.policy = policy;
    server_config.listeners = listeners;
    server_config.local_listener = Some(local_listener);
    let extension: Arc<dyn AdminExtension> = commands.clone();
    server_config.admin_extension = Some(extension);
    server_config.sys_interval =
        (config.server.sys_interval > 0).then(|| Duration::from_secs(config.server.sys_interval));
    server_config.plugin_timeout = config.server.plugin_timeout();
    server_config.plugin_tick_interval = config.server.plugin_tick_interval();
    #[cfg(unix)]
    tokio::spawn(reload_on_sighup(hangup, reloadable));

    let result = match config.storage {
        StorageConfig::Memory {} => {
            let storage = Arc::new(MemoryStore::new());
            serve(
                listener,
                server_config,
                storage,
                &commands,
                dynsec,
                shutdown,
            )
            .await
        }
        StorageConfig::Sqlite { path } => {
            info!("Using SQLite storage at {}", path);
            let storage = Arc::new(SqliteStore::new(&path)?);
            serve(
                listener,
                server_config,
                storage,
                &commands,
                dynsec,
                shutdown,
            )
            .await
        }
        StorageConfig::Postgres { .. } => {
            let storage = Arc::new(postgres.expect("opened before binding"));
            serve(
                listener,
                server_config,
                storage,
                &commands,
                dynsec,
                shutdown,
            )
            .await
        }
    };
    // Removed now, as exiting skips destructors
    #[cfg(target_os = "linux")]
    drop(admin);

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

/// Sets the broker up, hands its `Admin` to the daemon's commands and to
/// dynamic security, and serves until `shutdown`.
async fn serve<St: MqttStore + 'static>(
    listener: TcpListener,
    config: ServerConfig,
    storage: Arc<St>,
    commands: &commands::DaemonCommands,
    dynsec: Option<Arc<DynamicSecurity>>,
    shutdown: impl Future,
) -> Result<(), ServerError> {
    let server = Server::new(listener, config, storage).await?;
    commands.set_admin(server.admin());
    if let Some(dynsec) = dynsec {
        dynsec.set_admin(server.admin());
    }
    server.run(shutdown).await
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
        #[cfg(target_os = "linux")]
        (None, Some(path)) => {
            let socket = admin_socket::bind_listener(
                std::path::Path::new(path),
                listener.socket_mode()?,
                listener.group.as_deref(),
            )?;
            Ok((socket.into(), path.clone()))
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        (None, Some(path)) => {
            use std::os::unix::fs::FileTypeExt;
            if listener.mode.is_some() || listener.group.is_some() {
                return Err(format!(
                    "{path}: [[listeners]] `mode` and `group` need Linux"
                ));
            }
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

/// What `build_auth` sets up: the settings, the plugins that check
/// credentials, and dynamic security when it is one of them.
type Auth = (
    ServerAuthConfig,
    Vec<Arc<dyn Plugin>>,
    Option<Arc<DynamicSecurity>>,
);

/// The authentication and access control of `config`, adding the files it
/// loads to `reloadable`.
fn build_auth(
    config: &config::AuthConfig,
    reloadable: &mut Vec<Arc<dyn Reload>>,
    main: bool,
) -> Result<Auth, String> {
    let table = if main { "[auth]" } else { "[listeners.auth]" };
    // Without authentication these would be ignored, letting everyone in
    let authenticating = [
        ("password_file", config.password_file.is_some()),
        ("dynamic_security", config.dynamic_security.is_some()),
        (
            "user_url",
            config.http.as_ref().is_some_and(|h| h.user_url.is_some()),
        ),
    ];
    if let Some((key, _)) = authenticating
        .iter()
        .find(|(_, set)| *set && !config.enabled)
    {
        return Err(format!(
            "`{key}` is set in {table} but `enabled` is false, so no client would be \
             authenticated; set `enabled = true` or remove `{key}`"
        ));
    }

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
        return Ok((value, vec![security.clone()], Some(security)));
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
    if let Some(path) = &config.password_file {
        let validator = Arc::new(Reloadable::load(path, load_credentials)?);
        info!("Loaded credentials from {}", path);
        reloadable.push(validator.clone());
        plugins.push(validator);
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
    Ok((value, plugins, None))
}

/// Loads the `[[plugins]]` in order, with their names, adding each to
/// `reloadable`.
#[cfg(feature = "wasm")]
async fn load_plugins(
    plugins: &[config::PluginConfig],
    plugin_timeout: Duration,
    reloadable: &mut Vec<Arc<dyn Reload>>,
) -> Result<Vec<(String, Arc<dyn Plugin>)>, String> {
    if plugins.is_empty() {
        return Ok(Vec::new());
    }
    let runtime =
        wasm::Runtime::new().map_err(|e| format!("Failed to start WebAssembly: {e:#}"))?;
    let cpus = std::thread::available_parallelism().map_or(1, Into::into);
    let mut loaded = Vec::new();
    for plugin in plugins {
        let name = plugin.name().into_owned();
        let settings = wasm::PluginSettings {
            name: name.clone(),
            path: plugin.path.clone(),
            config: plugin.config.clone().into_iter().collect(),
            allow_http: plugin.allow_http.clone(),
            allow_sockets: plugin.allow_sockets.clone(),
            instances: plugin.instances.unwrap_or(cpus),
            timeout: plugin
                .timeout_ms
                .map_or(plugin_timeout, Duration::from_millis),
            memory: plugin.memory_mb << 20,
        };
        let loaded_plugin = wasm::WasmPlugin::load(&runtime, settings)
            .await
            .map_err(|e| format!("Failed to load plugin `{name}`: {e:#}"))?;
        info!("Loaded plugin `{}` from {}", name, plugin.path.display());
        reloadable.push(Arc::new(wasm::Reloader::new(
            Arc::clone(&loaded_plugin),
            runtime.clone(),
        )));
        loaded.push((name, loaded_plugin as Arc<dyn Plugin>));
    }
    Ok(loaded)
}

#[cfg(not(feature = "wasm"))]
async fn load_plugins(
    plugins: &[config::PluginConfig],
    _plugin_timeout: Duration,
    _reloadable: &mut Vec<Arc<dyn Reload>>,
) -> Result<Vec<(String, Arc<dyn Plugin>)>, String> {
    if plugins.is_empty() {
        Ok(Vec::new())
    } else {
        Err(
            "[[plugins]] is configured, but this mercuriod was built without \
             WebAssembly plugin support"
                .to_string(),
        )
    }
}

/// Rereads the ACL, password and dynamic security files and the
/// WebAssembly plugins on SIGHUP, as the `reload` command does. ACL and
/// plugin changes apply to every later check or call; password changes
/// apply to later connections.
#[cfg(unix)]
async fn reload_on_sighup(mut hangup: signal::unix::Signal, files: Arc<[Arc<dyn Reload>]>) {
    while hangup.recv().await.is_some() {
        info!("Received SIGHUP, reloading");
        reload::reload_all(&files);
    }
}

fn load_acl_file(path: &str) -> Result<AclFile, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read ACL file {}: {}", path, e))?;
    AclFile::parse(&contents).map_err(|e| format!("Invalid ACL file {}: {}", path, e))
}

fn load_credentials(path: &str) -> Result<HashedCredentialValidator, String> {
    Ok(HashedCredentialValidator::new(password_file::load(path)?))
}

/// Add or update a user in the password file.
fn add_or_update_user(path: &str, username: &str) -> Result<(), Box<dyn std::error::Error>> {
    password_file::check_username(username)?;
    let mut users = password_file::read_users(path)?;
    let password = prompt_new_password(username)?;

    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("Failed to hash password: {}", e))?
        .to_string();

    users.insert(username.to_string(), hash);
    password_file::write_users(path, &users)?;

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
    let mut users = password_file::read_users(path)?;
    if users.remove(username).is_none() {
        return Err(format!("User `{}` not found in `{}`", username, path).into());
    }
    password_file::write_users(path, &users)?;
    eprintln!("User `{}` deleted from `{}`", username, path);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{address, build_auth, config};

    fn auth(toml: &str) -> config::AuthConfig {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn test_authentication_settings_need_authentication_enabled() {
        for (toml, key) in [
            ("password_file = \"/nonexistent/passwd\"\n", "password_file"),
            (
                "dynamic_security = \"/nonexistent/dynsec.json\"\n",
                "dynamic_security",
            ),
            ("[http]\nuser_url = \"http://h/user\"\n", "user_url"),
        ] {
            for main in [true, false] {
                let Err(error) = build_auth(&auth(toml), &mut Vec::new(), main) else {
                    panic!("{toml}: accepted");
                };
                assert!(
                    error.contains(&format!("`{key}`")) && error.contains("`enabled`"),
                    "{toml}: {error}"
                );
            }
        }
    }

    #[test]
    fn test_access_control_works_without_authentication() {
        let (value, plugins, _) = build_auth(
            &auth("[http]\nacl_url = \"http://h/acl\"\n"),
            &mut Vec::new(),
            true,
        )
        .map_err(|e| panic!("{e}"))
        .unwrap();
        assert!(!value.require_auth);
        assert_eq!(plugins.len(), 1);

        let acl = std::env::temp_dir().join(format!("mercuriod-acl-{}", std::process::id()));
        std::fs::write(&acl, "topic read public/#\n").unwrap();
        let result = build_auth(
            &auth(&format!("acl_file = \"{}\"\n", acl.display())),
            &mut Vec::new(),
            true,
        );
        std::fs::remove_file(&acl).unwrap();
        let (value, plugins, _) = result.map_err(|e| panic!("{e}")).unwrap();
        assert!(!value.require_auth);
        assert_eq!(plugins.len(), 1);
    }

    #[test]
    fn test_ipv6_hosts_are_bracketed() {
        assert_eq!(address("::", 1883), "[::]:1883");
        assert_eq!(address("[::1]", 1883), "[::1]:1883");
        assert_eq!(address("0.0.0.0", 1883), "0.0.0.0:1883");
        assert_eq!(address("localhost", 1883), "localhost:1883");
    }
}
