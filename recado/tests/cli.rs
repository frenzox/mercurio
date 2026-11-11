//! The recado binary against an in-process broker.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Output, Stdio},
    sync::Arc,
};

use mercurio_client::Bytes;
use mercurio_client::{ConnectOptions, MessageProperties, MqttClient, QoS};
use mercurio_server::{
    auth::SimpleCredentialValidator,
    server::{run_with_config, ServerConfig},
};
use serde_json::{json, Value};
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
    /// The next line recado prints, with its line feed.
    fn next_line(&mut self) -> Vec<u8> {
        let mut line = Vec::new();
        tokio::task::block_in_place(|| self.stdout.read_until(b'\n', &mut line)).unwrap();
        line
    }

    /// Stops recado and returns what [`Subscriber::finish`] does.
    async fn kill(mut self) -> (Option<i32>, Vec<u8>, String) {
        self.child.kill().unwrap();
        self.finish().await
    }

    /// Waits for recado to exit, and returns its status, the rest of its
    /// output and its standard error.
    async fn finish(self) -> (Option<i32>, Vec<u8>, String) {
        let Subscriber {
            mut child,
            mut stdout,
        } = self;
        tokio::task::spawn_blocking(move || {
            let mut rest = Vec::new();
            std::io::Read::read_to_end(&mut stdout, &mut rest).unwrap();
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
async fn mqtt_3_without_a_client_id_still_connects() {
    let broker = start().await;
    for args in [
        &["pub", "-V", "311", "-c", "-t", "a", "-m", "x"][..],
        &["pub", "-V", "31", "-t", "a", "-m", "x"],
        &["pub", "-V", "311", "-t", "a", "-m", "x"],
    ] {
        let output = broker.run(args).await;
        assert!(output.status.success(), "{args:?}: {}", stderr(&output));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_subscription_fails() {
    let mut config = ServerConfig::default();
    config.limits.max_subscriptions = Some(1);
    let broker = start_with(config).await;
    let output = broker.run(&["sub", "-t", "a", "-t", "b"]).await;
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(1), "{message}");
    assert!(message.contains("subscription to b"), "{message}");
    assert_eq!(message.lines().count(), 1, "{message}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invalid_filter_is_a_usage_error() {
    let broker = start().await;
    let output = broker.run(&["sub", "-t", "a/#/b"]).await;
    let message = stderr(&output);
    assert_eq!(output.status.code(), Some(3), "{message}");
    assert!(message.contains("a/#/b"), "{message}");
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

/// Not UTF-8, with a NUL and a line feed in the middle.
const BINARY: &[u8] = &[0xff, 0x00, b'\n', 0xfe, 0x80];

/// The payload of the next message `client` receives within 10 s.
async fn next_payload(client: &MqttClient) -> Vec<u8> {
    let next = async {
        loop {
            if let Some(mercurio_client::Event::Message { payload, .. }) = client.recv().await {
                return payload.to_vec();
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), next)
        .await
        .expect("no message within 10 s")
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_payloads_are_published_from_stdin_and_files() {
    let broker = start().await;
    let receiver = broker.client("receiver").await;
    receiver
        .subscribe(&[("bin", QoS::AtMostOnce)])
        .await
        .unwrap();

    let output = broker.run_with_input(&["pub", "-t", "bin"], BINARY).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(next_payload(&receiver).await, BINARY);

    let file = std::env::temp_dir().join(format!("recado-binary-{}", std::process::id()));
    std::fs::write(&file, BINARY).unwrap();
    let output = broker
        .run(&["pub", "-t", "bin", "-f", file.to_str().unwrap()])
        .await;
    std::fs::remove_file(&file).unwrap();
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(next_payload(&receiver).await, BINARY);

    let both = broker
        .run(&["pub", "-t", "bin", "-m", "x", "-f", "/dev/null"])
        .await;
    assert_eq!(both.status.code(), Some(3), "{}", stderr(&both));
}

#[tokio::test(flavor = "multi_thread")]
async fn pub_sends_a_will() {
    let broker = start().await;
    let receiver = broker.client("receiver").await;
    receiver
        .subscribe(&[("will/#", QoS::AtLeastOnce)])
        .await
        .unwrap();
    let mut publisher = broker
        .command(&[
            "pub",
            "-t",
            "will/line",
            "-l",
            "--will-topic",
            "will/status",
            "--will-message",
            "gone",
            "--will-qos",
            "1",
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    watchdog(&publisher);
    // Connected once its first line arrives
    let mut stdin = publisher.stdin.take().unwrap();
    stdin.write_all(b"up\n").unwrap();
    assert_eq!(next_payload(&receiver).await, b"up");
    publisher.kill().unwrap();
    publisher.wait().unwrap();
    assert_eq!(next_payload(&receiver).await, b"gone");
}

#[tokio::test(flavor = "multi_thread")]
async fn line_mode_publishes_each_line() {
    let broker = start().await;
    let receiver = broker.client("receiver").await;
    receiver
        .subscribe(&[("lines", QoS::AtLeastOnce)])
        .await
        .unwrap();
    let output = broker
        .run_with_input(
            &["pub", "-t", "lines", "-l", "-q", "1"],
            b"one\n\ntwo\r\nthree",
        )
        .await;
    assert!(output.status.success(), "{}", stderr(&output));
    for expected in ["one", "two", "three"] {
        assert_eq!(next_payload(&receiver).await, expected.as_bytes());
    }

    let both = broker.run(&["pub", "-t", "lines", "-l", "-m", "x"]).await;
    assert_eq!(both.status.code(), Some(3), "{}", stderr(&both));
}

#[tokio::test(flavor = "multi_thread")]
async fn binary_payloads_are_printed_as_they_are() {
    let broker = start().await;
    let mut subscriber = broker
        .subscriber(&["sub", "-t", "bin/#", "-T"], "bin/ready")
        .await;
    let sender = broker.client("sender").await;
    sender
        .publish("bin/x", &[0xff, 0x00, 0xfe][..], QoS::AtLeastOnce)
        .await
        .unwrap();
    assert_eq!(subscriber.next_line(), b"bin/x: \xff\x00\xfe\n");
    subscriber.kill().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn json_lines_carry_the_message_and_its_properties() {
    let broker = start().await;
    let mut subscriber = broker
        .subscriber(
            &[
                "sub",
                "-t",
                "json/#",
                "--format",
                "json",
                "--subscription-id",
                "7",
                "-q",
                "1",
            ],
            "json/ready",
        )
        .await;
    let sender = broker.client("sender").await;
    let properties = MessageProperties::default()
        .content_type("text/plain")
        .response_topic("replies/me")
        .correlation_data(Bytes::from_static(&[0x01, 0xff]))
        .user_property("k", "v1")
        .user_property("k", "v2");
    sender
        .publish_with_properties("json/text", "héllo", QoS::AtLeastOnce, false, properties)
        .await
        .unwrap();
    let text: Value = serde_json::from_slice(&subscriber.next_line()).unwrap();
    assert_eq!(
        text,
        json!({
            "topic": "json/text",
            "qos": 1,
            "retain": false,
            "payload": "héllo",
            "payload_base64": "aMOpbGxv",
            "content_type": "text/plain",
            "response_topic": "replies/me",
            "correlation_data_base64": "Af8=",
            "user_properties": [["k", "v1"], ["k", "v2"]],
            "subscription_ids": [7],
        })
    );

    sender
        .publish("json/binary", &[0xff, 0x00][..], QoS::AtMostOnce)
        .await
        .unwrap();
    let binary: Value = serde_json::from_slice(&subscriber.next_line()).unwrap();
    assert_eq!(
        binary,
        json!({
            "topic": "json/binary",
            "qos": 0,
            "retain": false,
            "payload_base64": "/wA=",
            "subscription_ids": [7],
        })
    );
    subscriber.kill().await;
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_disconnects_cleanly() {
    let broker = start().await;
    let receiver = broker.client("receiver").await;
    receiver
        .subscribe(&[("term/status", QoS::AtLeastOnce)])
        .await
        .unwrap();
    let subscriber = broker
        .subscriber(
            &[
                "sub",
                "-t",
                "term/ready",
                "--will-topic",
                "term/status",
                "--will-message",
                "died",
            ],
            "term/ready",
        )
        .await;
    let status = Command::new("kill")
        .args(["-TERM", &subscriber.child.id().to_string()])
        .status()
        .unwrap();
    assert!(status.success());
    let (status, _, message) = subscriber.finish().await;
    assert_eq!(status, Some(0), "{message}");
    // A clean DISCONNECT discards the will
    let will = tokio::time::timeout(std::time::Duration::from_millis(500), receiver.recv()).await;
    assert!(will.is_err(), "{will:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn sub_exits_after_count_messages() {
    let broker = start().await;
    // The retained message that says recado is ready counts as the first
    let subscriber = broker
        .subscriber(&["sub", "-t", "count/#", "-C", "3"], "count/ready")
        .await;
    let sender = broker.client("sender").await;
    for payload in ["a", "b"] {
        sender
            .publish("count/x", payload, QoS::AtLeastOnce)
            .await
            .unwrap();
    }
    let (status, rest, message) = subscriber.finish().await;
    assert_eq!(status, Some(0), "{message}");
    assert_eq!(rest, b"a\nb\n");
}

#[tokio::test(flavor = "multi_thread")]
async fn sub_fails_when_the_timeout_comes_first() {
    let broker = start().await;
    let started = std::time::Instant::now();
    let output = broker.run(&["sub", "-t", "quiet", "-W", "1"]).await;
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));
    assert!(stderr(&output).contains("1 s"), "{}", stderr(&output));
    assert!(started.elapsed() < std::time::Duration::from_secs(10));

    let output = broker
        .run(&["sub", "-t", "quiet", "-C", "2", "-W", "1"])
        .await;
    assert_eq!(output.status.code(), Some(4), "{}", stderr(&output));

    let sender = broker.client("sender").await;
    sender
        .publish_with_retain("kept", "here", QoS::AtLeastOnce, true)
        .await
        .unwrap();
    let output = broker
        .run(&["sub", "-t", "kept", "-C", "1", "-W", "5"])
        .await;
    assert_eq!(output.status.code(), Some(0), "{}", stderr(&output));
    assert_eq!(output.stdout, b"here\n");
}
