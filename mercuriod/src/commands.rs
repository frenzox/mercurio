//! The `$CONTROL/broker/v1` commands only the daemon can serve: the ones
//! about its configuration, files, logs and bridges.

use std::sync::{Arc, OnceLock};

use mercurio_server::{
    admin::{check_arguments, Admin, AdminError, AdminExtension, ErrorCode, Paging, SessionKey},
    plugin::BoxFuture,
};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use tracing::info;

use crate::bridge::BridgeStatus;
use crate::logging::LogLevel;
use crate::reload::{self, Reload};

/// Serves the daemon's commands, as the broker's `AdminExtension`.
pub struct DaemonCommands {
    log_level: LogLevel,
    /// The files SIGHUP rereads too
    reloadable: Arc<[Arc<dyn Reload>]>,
    /// The configuration, with its secrets redacted; None when it couldn't
    /// be shown as JSON
    config: Option<Value>,
    bridges: Vec<Arc<BridgeStatus>>,
    /// The broker's, once it is set up
    admin: OnceLock<Admin>,
}

#[derive(Deserialize)]
struct LevelArgs {
    level: String,
}

#[derive(Deserialize)]
struct PageArgs {
    offset: Option<usize>,
    count: Option<usize>,
}

impl PageArgs {
    fn paging(&self) -> Result<Paging, AdminError> {
        let default = Paging::default();
        Paging::new(
            self.offset.unwrap_or(default.offset),
            self.count.unwrap_or(default.count),
        )
    }
}

impl DaemonCommands {
    pub fn new(log_level: LogLevel) -> Self {
        DaemonCommands {
            log_level,
            reloadable: Arc::new([]),
            config: None,
            bridges: Vec::new(),
            admin: OnceLock::new(),
        }
    }

    /// Reloads `files` on `reload`.
    pub fn with_reload(mut self, files: Arc<[Arc<dyn Reload>]>) -> Self {
        self.reloadable = files;
        self
    }

    /// Answers `getConfig` with `config`.
    pub fn with_config(mut self, config: Option<Value>) -> Self {
        self.config = config;
        self
    }

    /// Lists `bridges`, in this order, on `listBridges`.
    pub fn with_bridges(mut self, bridges: Vec<Arc<BridgeStatus>>) -> Self {
        self.bridges = bridges;
        self
    }

    /// Hands over the broker's `Admin`, which exists only once the broker
    /// this extension is part of is set up.
    pub fn set_admin(&self, admin: Admin) {
        let _ = self.admin.set(admin);
    }

    async fn list_bridges(&self, args: &Value) -> Result<Value, AdminError> {
        let paging = arguments::<PageArgs>(args)?.paging()?;
        let mut bridges = Vec::new();
        for status in self.bridges.iter().skip(paging.offset).take(paging.count) {
            // A bridge's local client is in-process, without a mount point
            let queued = match self.admin.get() {
                Some(admin) => admin
                    .client(&SessionKey::unmounted(status.client_id()))
                    .await
                    .map_or(0, |client| client.queued),
                None => 0,
            };
            bridges.push(status.entry(queued));
        }
        Ok(json!({ "bridges": bridges, "totalCount": self.bridges.len() }))
    }

    fn reload(&self) -> Value {
        info!("Reloading on request");
        json!({ "items": reload::reload_all(&self.reloadable) })
    }

    fn set_log_level(&self, args: &Value) -> Result<Value, AdminError> {
        let level = arguments::<LevelArgs>(args)?.level;
        self.log_level.set(&level)?;
        info!("Log level set to `{}` until restart", level);
        Ok(Value::Null)
    }
}

impl AdminExtension for DaemonCommands {
    fn handle<'a>(
        &'a self,
        command: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, AdminError>>> {
        Box::pin(async move {
            let known: &[&str] = match command {
                "listBridges" => &["offset", "count"],
                "setLogLevel" => &["level"],
                "getLogLevel" | "reload" | "getConfig" => &[],
                _ => return None,
            };
            if let Err(e) = check_arguments(args, known) {
                return Some(Err(e));
            }
            Some(match command {
                "listBridges" => self.list_bridges(args).await,
                "getLogLevel" => Ok(json!({ "level": self.log_level.get() })),
                "setLogLevel" => self.set_log_level(args),
                "reload" => Ok(self.reload()),
                "getConfig" => self.config.clone().ok_or_else(|| {
                    AdminError::new(
                        ErrorCode::Unavailable,
                        "the configuration can't be shown as JSON",
                    )
                }),
                _ => return None,
            })
        })
    }
}

fn arguments<T: DeserializeOwned>(args: &Value) -> Result<T, AdminError> {
    T::deserialize(args).map_err(|e| AdminError::invalid_argument(e.to_string()))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use mercurio_client::{ConnectOptions, Event, MqttClient, QoS};
    use mercurio_server::{
        admin::{AdminExtension, ErrorCode},
        server::{LocalConnector, LocalListener, Server, ServerConfig},
    };
    use mercurio_storage::memory::MemoryStore;
    use serde_json::{json, Value};
    use tokio::net::TcpListener;

    use super::DaemonCommands;
    use crate::bridge::{self, BridgeStatus};
    use crate::config::BridgeConfig;
    use crate::logging::LogLevel;
    use crate::reload::{Reload, Reloadable};

    /// Serves a broker with `commands` and an in-process listener.
    async fn start(commands: Arc<DaemonCommands>, local_listener: LocalListener) {
        let mut config = ServerConfig::default();
        config.local_listener = Some(local_listener);
        let extension: Arc<dyn AdminExtension> = commands.clone();
        config.admin_extension = Some(extension);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Server::new(listener, config, Arc::new(MemoryStore::new()))
            .await
            .unwrap();
        commands.set_admin(server.admin());
        tokio::spawn(server.run(std::future::pending::<()>()));
    }

    /// Runs `command` as an in-process client, which may run every
    /// command, and returns its response.
    async fn request(connector: &LocalConnector, command: Value) -> Value {
        let connector = connector.clone();
        let options = ConnectOptions::new("localhost", 0)
            .client_id("ctl")
            .connector(move || std::future::ready(connector.connect()));
        let client = MqttClient::connect(options).await.unwrap();
        client
            .subscribe(&[("$CONTROL/broker/v1/response", QoS::AtMostOnce)])
            .await
            .unwrap();
        let request = json!({ "commands": [command] }).to_string();
        client
            .publish("$CONTROL/broker/v1", request, QoS::AtMostOnce)
            .await
            .unwrap();
        let payload = loop {
            let event = tokio::time::timeout(Duration::from_secs(5), client.recv())
                .await
                .expect("no response in time");
            match event {
                Some(Event::Message { payload, .. }) => break payload,
                Some(_) => continue,
                None => panic!("disconnected before the response"),
            }
        };
        let _ = client.disconnect().await;
        serde_json::from_slice::<Value>(&payload).unwrap()["responses"][0].clone()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_log_level_commands_are_served_through_the_broker() {
        let (log_level, _filter) = LogLevel::new("info,mercurio_server=debug", true);
        let commands = Arc::new(DaemonCommands::new(log_level));
        let (connector, local_listener) = LocalConnector::new();
        start(commands, local_listener).await;

        let response = request(&connector, json!({ "command": "getLogLevel" })).await;
        assert_eq!(response["data"]["level"], "info,mercurio_server=debug");
        let response = request(
            &connector,
            json!({ "command": "setLogLevel", "level": "info,x=loud" }),
        )
        .await;
        assert_eq!(response["code"], "invalidArgument", "{response}");
        let response = request(&connector, json!({ "command": "setLogLevel" })).await;
        assert_eq!(response["code"], "invalidArgument", "{response}");
        let response = request(&connector, json!({ "command": "listBridgez" })).await;
        assert_eq!(response["code"], "invalidArgument", "{response}");
    }

    #[tokio::test]
    async fn test_set_log_level_changes_what_get_log_level_returns() {
        // The filter must stay alive for the level to change
        let (log_level, _filter) = LogLevel::new("info", true);
        let commands = DaemonCommands::new(log_level);
        let args = json!({ "command": "setLogLevel", "level": "warn" });
        assert_eq!(
            commands.handle("setLogLevel", &args).await.unwrap(),
            Ok(Value::Null)
        );
        let level = commands.handle("getLogLevel", &json!({})).await.unwrap();
        assert_eq!(level.unwrap()["level"], "warn");
        assert!(commands.handle("getlogLevel", &json!({})).await.is_none());
    }

    #[tokio::test]
    async fn test_unknown_arguments_are_refused_by_name() {
        let (log_level, _filter) = LogLevel::new("info", true);
        let commands = DaemonCommands::new(log_level);
        for (command, args) in [
            (
                "setLogLevel",
                json!({ "command": "setLogLevel", "level": "warn", "lvl": 1 }),
            ),
            ("reload", json!({ "command": "reload", "lvl": 1 })),
            ("listBridges", json!({ "command": "listBridges", "lvl": 1 })),
        ] {
            let error = commands.handle(command, &args).await.unwrap().unwrap_err();
            assert_eq!(error.code, ErrorCode::InvalidArgument);
            assert!(error.message.contains("`lvl`"), "{}", error.message);
        }
        assert_eq!(log_level_of(&commands).await, "info");
    }

    async fn log_level_of(commands: &DaemonCommands) -> String {
        let level = commands.handle("getLogLevel", &json!({})).await.unwrap();
        level.unwrap()["level"].as_str().unwrap().to_string()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reload_reports_each_file() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-commands-acl-{}", std::process::id()));
        std::fs::write(&path, "topic read public/#\n").unwrap();
        let path_str = path.to_str().unwrap().to_string();
        let acl: Arc<dyn Reload> =
            Arc::new(Reloadable::load(&path_str, crate::load_acl_file).unwrap());
        let (log_level, _filter) = LogLevel::new("info", true);
        let commands = DaemonCommands::new(log_level).with_reload(vec![acl].into());
        let args = json!({ "command": "reload" });

        let result = commands.handle("reload", &args).await.unwrap().unwrap();
        assert_eq!(
            result,
            json!({ "items": [{ "item": "acl-file", "path": path_str, "ok": true }] })
        );

        std::fs::write(&path, "bogus\n").unwrap();
        let result = commands.handle("reload", &args).await.unwrap().unwrap();
        assert_eq!(result["items"][0]["ok"], false, "{result}");
        let error = result["items"][0]["error"].as_str().unwrap();
        assert!(error.contains("unknown keyword"), "{error}");
        std::fs::remove_file(&path).unwrap();

        let (log_level, _filter) = LogLevel::new("info", true);
        let none = DaemonCommands::new(log_level);
        assert_eq!(
            none.handle("reload", &args).await.unwrap(),
            Ok(json!({ "items": [] }))
        );
    }

    #[tokio::test]
    async fn test_get_config_returns_the_config_given() {
        let args = json!({ "command": "getConfig" });
        let config = json!({ "server": { "port": 1883 } });
        // The filter must stay alive for the level to change
        let (log_level, _filter) = LogLevel::new("info", true);
        let commands = DaemonCommands::new(log_level).with_config(Some(config.clone()));
        assert_eq!(
            commands.handle("getConfig", &args).await.unwrap(),
            Ok(config)
        );
        let (log_level, _filter) = LogLevel::new("info", true);
        let without = DaemonCommands::new(log_level);
        let error = without
            .handle("getConfig", &args)
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Unavailable);
    }

    #[tokio::test]
    async fn test_list_bridges_pages_in_configuration_order() {
        let statuses = vec![
            Arc::new(BridgeStatus::new("a", "mercurio-bridge-a")),
            Arc::new(BridgeStatus::new("b", "mercurio-bridge-b")),
        ];
        let (log_level, _filter) = LogLevel::new("info", true);
        let commands = DaemonCommands::new(log_level).with_bridges(statuses);
        let all = commands
            .handle("listBridges", &json!({}))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(all["totalCount"], 2);
        assert_eq!(all["bridges"][0]["name"], "a");
        assert_eq!(all["bridges"][1]["queued"], 0);
        let second = commands
            .handle("listBridges", &json!({ "offset": 1, "count": 1 }))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second["bridges"].as_array().unwrap().len(), 1);
        assert_eq!(second["bridges"][0]["name"], "b");
        assert_eq!(second["totalCount"], 2);
        let error = commands
            .handle("listBridges", &json!({ "count": 1001 }))
            .await
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidArgument);
    }

    /// Polls `listBridges` until its first bridge satisfies `done`.
    async fn bridges_when(commands: &DaemonCommands, done: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            let listed = commands
                .handle("listBridges", &json!({}))
                .await
                .unwrap()
                .unwrap();
            if done(&listed["bridges"][0]) {
                return listed;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out: {listed}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_list_bridges_follows_a_real_bridge() {
        let remote = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = remote.local_addr().unwrap().port();
        let (stop_remote, stopped) = tokio::sync::oneshot::channel::<()>();
        let remote = tokio::spawn(mercurio_server::server::run(remote, async move {
            let _ = stopped.await;
        }));

        let config: BridgeConfig = toml::from_str(&format!(
            "name = \"cloud\"\nhost = \"127.0.0.1\"\nport = {port}\n[[topics]]\npattern = \"a/#\"\n"
        ))
        .unwrap();
        let (connector, local_listener) = LocalConnector::new();
        let status = bridge::spawn(bridge::prepare(&config, connector.clone()).unwrap());
        let (log_level, _filter) = LogLevel::new("info", true);
        let commands = Arc::new(DaemonCommands::new(log_level).with_bridges(vec![status]));
        start(Arc::clone(&commands), local_listener).await;

        let listed = bridges_when(&commands, |b| b["connected"] == true).await;
        assert_eq!(listed["totalCount"], 1);
        let bridge = &listed["bridges"][0];
        assert_eq!(bridge["name"], "cloud");
        assert_eq!(bridge["remoteAddress"], format!("127.0.0.1:{port}"));
        assert!(bridge["connectedSince"].is_u64(), "{bridge}");
        assert_eq!(bridge["queued"], 0);

        // Also served through the broker
        let response = request(&connector, json!({ "command": "listBridges" })).await;
        assert_eq!(
            response["data"]["bridges"][0]["connected"], true,
            "{response}"
        );

        stop_remote.send(()).unwrap();
        remote.await.unwrap().unwrap();
        let listed = bridges_when(&commands, |b| b["connected"] == false).await;
        let bridge = &listed["bridges"][0];
        assert!(bridge["disconnectedSince"].is_u64(), "{bridge}");
        assert_eq!(bridge["remoteAddress"], format!("127.0.0.1:{port}"));
    }
}
