//! The mercurioctl binary against an in-process broker, reached through a
//! trusted Unix socket as on the broker's host.
#![cfg(unix)]

use std::{path::PathBuf, process::Output, sync::Arc};

use mercurio_client::{ConnectOptions, MqttClient, QoS};
use mercurio_server::{
    admin::{AdminError, AdminExtension},
    plugin::{ready, BoxFuture},
    server::{run_with_config, ListenerConfig, ServerConfig, Transport},
};
use serde_json::{json, Value};
use tokio::net::{TcpListener, UnixListener};

/// Answers the commands only mercuriod serves, shaped as mercuriod shapes
/// them.
struct Daemon;

impl AdminExtension for Daemon {
    fn handle<'a>(
        &'a self,
        command: &'a str,
        _: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, AdminError>>> {
        ready(match command {
            "getLogLevel" => Some(Ok(json!({ "level": "info" }))),
            "setLogLevel" => Some(Ok(Value::Null)),
            "getConfig" => Some(Ok(json!({ "storage": { "backend": "memory" } }))),
            "reload" => Some(Ok(json!({ "items": [
                { "item": "password-file", "path": "/etc/mercurio/passwd", "ok": true },
                {
                    "item": "acl-file", "path": "/etc/mercurio/acl", "ok": false,
                    "error": "syntax error at line 3",
                },
            ] }))),
            "listBridges" => Some(Ok(json!({
                "bridges": [{
                    "name": "cloud", "connected": true, "remoteAddress": "203.0.113.5:8883",
                    "connectedSince": 1794340800, "queued": 0,
                }],
                "totalCount": 1,
            }))),
            _ => None,
        })
    }
}

struct Broker {
    socket: PathBuf,
    port: u16,
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// A broker with a trusted Unix socket, its path unique to `name` and this
/// test run, and a TCP listener where nobody may run admin commands.
async fn start(name: &str) -> Broker {
    let socket =
        std::env::temp_dir().join(format!("mercurioctl-{name}-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&socket);
    let mut admin_socket =
        ListenerConfig::new(UnixListener::bind(&socket).unwrap(), Transport::Mqtt);
    admin_socket.trusted = true;
    let extension: Arc<dyn AdminExtension> = Arc::new(Daemon);
    let mut config = ServerConfig::default();
    config.listeners = vec![admin_socket];
    config.sys_interval = None;
    config.admin_extension = Some(extension);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    Broker { socket, port }
}

impl Broker {
    /// Runs mercurioctl through the broker's socket.
    async fn ctl(&self, args: &[&str]) -> Output {
        let mut all = vec!["--unix", self.socket.to_str().unwrap()];
        all.extend_from_slice(args);
        mercurioctl(&all, &[]).await
    }

    /// Runs mercurioctl through the broker's socket, expecting success, and
    /// returns what it printed, as JSON.
    async fn json(&self, args: &[&str]) -> Value {
        let output = self.ctl(&[args, &["--json"][..]].concat()).await;
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", stdout(&output)))
    }

    /// A client connected over TCP.
    async fn client(&self, id: &str) -> MqttClient {
        MqttClient::connect(ConnectOptions::new("127.0.0.1", self.port).client_id(id))
            .await
            .unwrap()
    }
}

/// Runs mercurioctl with `args` and `env` and nothing else from the
/// environment, `MERCURIOCTL_*` variables included.
async fn mercurioctl(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_mercurioctl"));
    command.env_clear().args(args).envs(env.iter().copied());
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn broker_commands_print_their_data() {
    let broker = start("data").await;
    let sensor = broker.client("sensor-1").await;
    sensor
        .publish_with_retain("home/temp", "21.5", QoS::AtLeastOnce, true)
        .await
        .unwrap();

    let clients = broker.json(&["client", "list"]).await;
    let ids: Vec<&str> = clients["clients"]
        .as_array()
        .unwrap()
        .iter()
        .map(|client| client["clientid"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&"sensor-1"), "{clients}");

    let retained = broker.json(&["retained", "list"]).await;
    assert_eq!(retained["messages"][0]["topic"], "home/temp", "{retained}");
    assert_eq!(
        broker.json(&["retained", "delete", "home/#"]).await,
        json!({ "deleted": 1 })
    );
    assert_eq!(
        broker.json(&["broker", "log-level"]).await,
        json!({ "level": "info" })
    );
    assert_eq!(
        broker.json(&["bridge", "list"]).await["bridges"][0]["name"],
        "cloud"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_exit_status_tells_failures_apart() {
    let broker = start("status").await;

    let set = broker.ctl(&["broker", "log-level", "debug"]).await;
    assert_eq!(set.status.code(), Some(0), "{}", stderr(&set));
    assert!(set.stdout.is_empty(), "{}", stdout(&set));

    let missing = broker.ctl(&["client", "show", "nobody"]).await;
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stderr(&missing).contains("(notFound)"),
        "{}",
        stderr(&missing)
    );

    // Nothing allows admin commands on the TCP listener
    let port = broker.port.to_string();
    let refused = mercurioctl(&["-H", "127.0.0.1", "-p", &port, "broker", "info"], &[]).await;
    assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("NotAuthorized"),
        "{}",
        stderr(&refused)
    );

    let unreachable = mercurioctl(
        &["--unix", "/nonexistent/mercurio.sock", "broker", "info"],
        &[],
    )
    .await;
    assert_eq!(unreachable.status.code(), Some(2));
    assert!(
        stderr(&unreachable).contains("/nonexistent/mercurio.sock"),
        "{}",
        stderr(&unreachable)
    );

    assert_eq!(
        mercurioctl(&["client", "frobnicate"], &[])
            .await
            .status
            .code(),
        Some(3)
    );
    assert_eq!(mercurioctl(&["client"], &[]).await.status.code(), Some(3));
    assert_eq!(mercurioctl(&["--help"], &[]).await.status.code(), Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_broker_that_never_answers_times_out() {
    // Accepts connections into its backlog, and never reads them
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = silent.local_addr().unwrap().port().to_string();
    let output = mercurioctl(
        &[
            "-H",
            "127.0.0.1",
            "-p",
            &port,
            "--timeout",
            "1",
            "broker",
            "info",
        ],
        &[],
    )
    .await;
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("no answer"), "{}", stderr(&output));
}
#[tokio::test(flavor = "multi_thread")]
async fn answers_print_as_tables_by_default() {
    let broker = start("tables").await;
    let _sensor = broker.client("sensor-1").await;

    let output = broker.ctl(&["client", "list"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.starts_with("CLIENT ID"), "{text}");
    assert!(
        text.lines().any(|line| line.starts_with("sensor-1 ")),
        "{text}"
    );

    let reload = broker.ctl(&["broker", "reload"]).await;
    assert_eq!(reload.status.code(), Some(1));
    assert!(
        stdout(&reload).contains("syntax error at line 3"),
        "{}",
        stdout(&reload)
    );
    assert!(
        stderr(&reload).contains("previous contents"),
        "{}",
        stderr(&reload)
    );
}
