//! The recado binary against an in-process broker.

use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, Command, Output, Stdio},
};

use mercurio_client::{ConnectOptions, MqttClient, QoS};
use mercurio_server::server::{run_with_config, ServerConfig};
use tokio::net::TcpListener;

struct Broker {
    port: u16,
}

async fn start() -> Broker {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = ServerConfig::default();
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
        let mut command = self.command(args);
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let input = input.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut child = command.spawn().unwrap();
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
            std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut stderr)
                .unwrap();
            (child.wait().unwrap().code(), rest, stderr)
        })
        .await
        .unwrap()
    }
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
