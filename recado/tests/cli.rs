//! The recado binary against an in-process broker.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Output, Stdio},
    sync::Arc,
};

use mercurio_client::{ConnectOptions, MqttClient, QoS};
use mercurio_server::{
    auth::SimpleCredentialValidator,
    server::{run_with_config, ServerConfig},
};
use tokio::net::TcpListener;

struct Broker {
    port: u16,
}

async fn start() -> Broker {
    start_with(ServerConfig::default()).await
}

/// A broker that only lets alice in, with the password `secret`.
async fn start_with_login() -> Broker {
    let credentials = [("alice".to_string(), "secret".to_string())];
    let mut config = ServerConfig::default();
    config.auth.require_auth = true;
    config.plugins = vec![Arc::new(SimpleCredentialValidator::new(
        credentials.into_iter().collect(),
    ))];
    start_with(config).await
}

async fn start_with(mut config: ServerConfig) -> Broker {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    config.sys_interval = None;
    tokio::spawn(run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));
    Broker { port }
}

impl Broker {
    /// `recado` with this broker's address before `args`, and nothing from
    /// the environment.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_recado"));
        let mut args = args.iter().copied();
        command
            .env_clear()
            .args(args.next())
            .args(["-H", "127.0.0.1", "-p", &self.port.to_string()])
            .args(args)
            .stdin(Stdio::null());
        command
    }

    /// Runs recado to completion.
    async fn run(&self, args: &[&str]) -> Output {
        self.run_with_input(args, b"").await
    }

    /// Runs recado to completion with `input` on its standard input.
    async fn run_with_input(&self, args: &[&str], input: &[u8]) -> Output {
        self.run_with(args, input, &[]).await
    }

    /// Runs recado to completion with `input` on its standard input and
    /// `env` as its environment.
    async fn run_with(&self, args: &[&str], input: &[u8], env: &[(&str, &str)]) -> Output {
        let mut command = self.command(args);
        command
            .envs(env.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let input = input.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut child = command.spawn().unwrap();
            watchdog(&child);
            let mut stdin = child.stdin.take().unwrap();
            stdin.write_all(&input).unwrap();
            drop(stdin);
            child.wait_with_output().unwrap()
        })
        .await
        .unwrap()
    }

    /// Starts `recado sub` and waits until it is subscribed: it receives a
    /// retained message published on `ready` first, which `args` must
    /// subscribe to.
    async fn subscriber(&self, args: &[&str], ready: &str) -> Subscriber {
        let client = self.client("ready").await;
        client
            .publish_with_retain(ready, "ready", QoS::AtLeastOnce, true)
            .await
            .unwrap();
        let mut child = self
            .command(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        watchdog(&child);
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let stdout = tokio::task::spawn_blocking(move || {
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            assert!(line.contains("ready"), "{line:?}");
            stdout
        })
        .await
        .unwrap();
        client
            .publish_with_retain(ready, "", QoS::AtLeastOnce, true)
            .await
            .unwrap();
        Subscriber { child, stdout }
    }

    async fn client(&self, id: &str) -> MqttClient {
        MqttClient::connect(ConnectOptions::new("127.0.0.1", self.port).client_id(id))
            .await
            .unwrap()
    }
}

/// A running `recado sub`.
struct Subscriber {
    child: Child,
    stdout: BufReader<std::process::ChildStdout>,
}

impl Subscriber {
    /// Waits for recado to exit, and returns its status, the rest of its
    /// output and its standard error.
    async fn finish(self) -> (Option<i32>, Vec<u8>, String) {
        let Subscriber { mut child, stdout } = self;
        tokio::task::spawn_blocking(move || {
            let mut rest = Vec::new();
            std::io::Read::read_to_end(&mut stdout.into_inner(), &mut rest).unwrap();
            let mut stderr = String::new();
            std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr).unwrap();
            (child.wait().unwrap().code(), rest, stderr)
        })
        .await
        .unwrap()
    }
}

/// Kills `child` if it still runs after 20 s, so a recado that never exits
/// fails its test instead of hanging it.
fn watchdog(child: &Child) {
    let pid = child.id().to_string();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(20));
        let _ = Command::new("kill")
            .args(["-9", &pid])
            .stderr(Stdio::null())
            .status();
    });
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn logs_go_to_standard_error() {
    let broker = start().await;
    let output = broker.run(&["pub", "-v", "-t", "a", "-m", "x"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(stderr(&output).contains("Connect"), "{}", stderr(&output));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_subscription_fails() {
    let broker = start().await;
    let output = broker.run(&["sub", "-t", "a/#/b"]).await;
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(message.contains("a/#/b"), "{message}");
    assert!(message.contains("Topic filter invalid"), "{message}");
    assert_eq!(message.lines().count(), 1, "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn losing_the_connection_fails() {
    let broker = start().await;
    let subscriber = broker
        .subscriber(&["sub", "-i", "taken", "-t", "lost/ready"], "lost/ready")
        .await;
    let _thief = broker.client("taken").await;
    let (status, _, message) = subscriber.finish().await;
    assert_eq!(status, Some(2), "{message}");
    assert!(message.contains("Session taken over"), "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn failures_print_one_line_and_set_the_exit_status() {
    let closed = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = closed.local_addr().unwrap().port();
    drop(closed);
    let unreachable = Broker { port }.run(&["pub", "-t", "a", "-m", "x"]).await;
    let message = stderr(&unreachable);
    assert_eq!(unreachable.status.code(), Some(2), "{message}");
    assert!(message.starts_with("recado: "), "{message}");
    assert_eq!(message.lines().count(), 1, "{message}");

    let broker = start().await;
    let usage = broker.run(&["pub", "-t", "a", "-m", "x", "-q", "3"]).await;
    assert_eq!(usage.status.code(), Some(3), "{}", stderr(&usage));
    let help = broker.run(&["sub", "--help"]).await;
    assert_eq!(help.status.code(), Some(0), "{}", stderr(&help));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_password_can_come_from_the_environment() {
    let broker = start_with_login().await;
    let publish = ["pub", "-u", "alice", "-t", "a", "-m", "x"];
    let output = broker
        .run_with(&publish, b"", &[("RECADO_PASSWORD", "secret")])
        .await;
    assert!(output.status.success(), "{}", stderr(&output));

    // -P wins over the variable
    let wrong = [&publish[..], &["-P", "wrong"]].concat();
    let output = broker
        .run_with(&wrong, b"", &[("RECADO_PASSWORD", "secret")])
        .await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));

    // Off a terminal nothing asks, and the broker refuses the login
    let output = broker.run(&publish).await;
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));

    let help = broker
        .run_with(&["pub", "--help"], b"", &[("RECADO_PASSWORD", "hunter2")])
        .await;
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("RECADO_PASSWORD"), "{help}");
    assert!(!help.contains("hunter2"), "{help}");
}
