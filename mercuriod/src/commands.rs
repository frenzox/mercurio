//! The `$CONTROL/broker/v1` commands only the daemon can serve: the ones
//! about its configuration, files, logs and bridges.

use mercurio_server::{
    admin::{AdminError, AdminExtension},
    plugin::BoxFuture,
};
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use tracing::info;

use crate::logging::LogLevel;

/// Serves the daemon's commands, as the broker's `AdminExtension`.
pub struct DaemonCommands {
    log_level: LogLevel,
}

#[derive(Deserialize)]
struct LevelArgs {
    level: String,
}

impl DaemonCommands {
    pub fn new(log_level: LogLevel) -> Self {
        DaemonCommands { log_level }
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
            Some(match command {
                "getLogLevel" => Ok(json!({ "level": self.log_level.get() })),
                "setLogLevel" => self.set_log_level(args),
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
        admin::AdminExtension,
        server::{LocalConnector, LocalListener, Server, ServerConfig},
    };
    use mercurio_storage::memory::MemoryStore;
    use serde_json::{json, Value};
    use tokio::net::TcpListener;

    use super::DaemonCommands;
    use crate::logging::LogLevel;

    /// Serves a broker with `commands` and an in-process listener.
    async fn start(commands: Arc<DaemonCommands>, local_listener: LocalListener) {
        let mut config = ServerConfig::default();
        config.local_listener = Some(local_listener);
        let extension: Arc<dyn AdminExtension> = commands;
        config.admin_extension = Some(extension);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Server::new(listener, config, Arc::new(MemoryStore::new()))
            .await
            .unwrap();
        tokio::spawn(server.run(std::future::pending::<()>()));
    }

    /// Runs `command` as an in-process client, which may run every
    /// command, and returns its response.
    async fn request(connector: &LocalConnector, command: Value) -> Value {
        let connector = connector.clone();
        let options = ConnectOptions::new("localhost", 0)
            .client_id("ctl")
            .connector(move || connector.connect());
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
}
