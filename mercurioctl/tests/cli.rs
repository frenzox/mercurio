//! The mercurioctl binary against an in-process broker, reached through a
//! trusted Unix socket as on the broker's host.
#![cfg(unix)]

use std::{path::PathBuf, process::Output, sync::Arc};

use mercurio_client::{ConnectOptions, MqttClient, QoS};
use mercurio_server::{
    admin::{AdminError, AdminExtension},
    plugin::{ready, BoxFuture},
    server::{ListenerConfig, Server, ServerConfig, Transport},
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
    let listeners = vec![admin_socket];
    config.sys_interval = None;
    config.admin_extension = Some(extension);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(
        Server::builder()
            .config(config)
            .listener(ListenerConfig::new(listener, Transport::Mqtt))
            .listeners(listeners)
            .run(std::future::pending::<()>()),
    );
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
        broker
            .json(&["retained", "delete", "home/#", "--yes"])
            .await,
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
async fn destructive_commands_need_yes_off_a_terminal() {
    let broker = start("confirm").await;
    let sensor = broker.client("sensor-1").await;
    for topic in ["home/a", "home/b"] {
        sensor
            .publish_with_retain(topic, "1", QoS::AtLeastOnce, true)
            .await
            .unwrap();
    }

    let unconfirmed = broker.ctl(&["retained", "delete", "home/#"]).await;
    assert_eq!(
        unconfirmed.status.code(),
        Some(3),
        "{}",
        stderr(&unconfirmed)
    );
    assert!(
        stderr(&unconfirmed).contains("--yes"),
        "{}",
        stderr(&unconfirmed)
    );
    assert_eq!(
        broker.json(&["retained", "list"]).await["messages"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // A topic without wildcards deletes one message, with no question
    assert_eq!(
        broker.json(&["retained", "delete", "home/a"]).await,
        json!({ "deleted": 1 })
    );
    assert_eq!(
        broker.json(&["retained", "delete", "home/+", "-y"]).await,
        json!({ "deleted": 1 })
    );

    let unconfirmed = broker.ctl(&["client", "delete-session", "sensor-1"]).await;
    assert_eq!(
        unconfirmed.status.code(),
        Some(3),
        "{}",
        stderr(&unconfirmed)
    );
    let confirmed = broker
        .ctl(&[
            "client",
            "delete-session",
            "sensor-1",
            "--disconnect",
            "--yes",
        ])
        .await;
    assert_eq!(confirmed.status.code(), Some(0), "{}", stderr(&confirmed));
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_with_json_are_json_on_standard_error() {
    let broker = start("json-errors").await;
    let missing = broker.ctl(&["client", "show", "nobody", "--json"]).await;
    assert_eq!(missing.status.code(), Some(1));
    assert!(missing.stdout.is_empty(), "{}", stdout(&missing));
    let error: Value = serde_json::from_slice(&missing.stderr)
        .unwrap_or_else(|e| panic!("{e}: {}", stderr(&missing)));
    assert_eq!(error["code"], "notFound", "{error}");
    assert!(
        error["error"].as_str().unwrap().contains("nobody"),
        "{error}"
    );

    let unreachable = mercurioctl(
        &[
            "--unix",
            "/nonexistent/mercurio.sock",
            "--json",
            "broker",
            "info",
        ],
        &[],
    )
    .await;
    assert_eq!(unreachable.status.code(), Some(2));
    let error: Value = serde_json::from_slice(&unreachable.stderr)
        .unwrap_or_else(|e| panic!("{e}: {}", stderr(&unreachable)));
    assert_eq!(error["code"], Value::Null, "{error}");
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("/nonexistent/mercurio.sock"),
        "{error}"
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
        stderr(&refused).contains("(notAuthorized)"),
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

#[tokio::test(flavor = "multi_thread")]
async fn dynsec_commands_without_dynamic_security_time_out() {
    let broker = start("no-dynsec").await;
    let output = broker
        .ctl(&["--timeout", "1", "dynsec", "list-clients"])
        .await;
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("dynamic security"),
        "{}",
        stderr(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_options_come_from_the_environment() {
    let broker = start("env").await;
    let socket = broker.socket.to_str().unwrap();
    let output = mercurioctl(
        &["--json", "broker", "info"],
        &[("MERCURIOCTL_UNIX", socket)],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    let info: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(info["version"].is_string(), "{info}");

    let help = mercurioctl(&["--help"], &[("MERCURIOCTL_PASSWORD", "hunter2")]).await;
    assert!(!stdout(&help).contains("hunter2"), "{}", stdout(&help));
}

#[tokio::test(flavor = "multi_thread")]
async fn host_and_port_from_the_environment_select_tcp() {
    let broker = start("tcp-env").await;
    let port = broker.port.to_string();
    let output = mercurioctl(
        &["broker", "info"],
        &[
            ("MERCURIOCTL_HOST", "127.0.0.1"),
            ("MERCURIOCTL_PORT", &port),
        ],
    )
    .await;
    // The TCP listener doesn't allow admin commands, which the admin socket does
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_variables_are_unset_and_flags_take_boolean_words() {
    let broker = start("env-empty").await;
    let socket = broker.socket.to_str().unwrap();
    // An empty host, port or user would otherwise conflict with the socket
    let output = mercurioctl(
        &["--json", "broker", "info"],
        &[
            ("MERCURIOCTL_UNIX", socket),
            ("MERCURIOCTL_HOST", ""),
            ("MERCURIOCTL_PORT", ""),
            ("MERCURIOCTL_USERNAME", ""),
            ("MERCURIOCTL_TLS", ""),
            ("MERCURIOCTL_INSECURE", ""),
            ("MERCURIOCTL_TIMEOUT", ""),
        ],
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));

    // False words are read, and the TCP listener refuses admin commands
    let port = broker.port.to_string();
    for (tls, insecure) in [("0", "no"), ("false", "off")] {
        let output = mercurioctl(
            &["broker", "info"],
            &[
                ("MERCURIOCTL_HOST", "127.0.0.1"),
                ("MERCURIOCTL_PORT", &port),
                ("MERCURIOCTL_TLS", tls),
                ("MERCURIOCTL_INSECURE", insecure),
            ],
        )
        .await;
        assert_eq!(output.status.code(), Some(1), "{tls}: {}", stderr(&output));
    }

    // 1 and yes are true: --unix conflicts with --tls
    for (variable, value) in [("MERCURIOCTL_TLS", "1"), ("MERCURIOCTL_INSECURE", "yes")] {
        let output = mercurioctl(
            &["broker", "info"],
            &[("MERCURIOCTL_UNIX", socket), (variable, value)],
        )
        .await;
        let message = stderr(&output);
        assert_eq!(output.status.code(), Some(3), "{message}");
        assert!(message.contains("cannot be used with"), "{message}");
    }
}
