//! JSON Web Tokens as CONNECT passwords, against a running daemon.
#![cfg(unix)]

use std::{
    net::{TcpListener, TcpStream},
    path::PathBuf,
    process::{Child, Command, Output},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use argon2::{Argon2, PasswordHasher};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use mercurio_client::{ClientError, ConnectOptions, MqttClient, ReasonCode};
use serde_json::json;

const SECRET: &str = "a secret of at least thirty-two bytes";
const OTHER_SECRET: &str = "another secret, also thirty-two bytes long";

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!("mercuriod-jwt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("jwt.secret"), format!("{SECRET}\n")).unwrap();
        TempDir(dir)
    }

    fn secret(&self) -> PathBuf {
        self.0.join("jwt.secret")
    }

    /// A configuration with JWT authentication by `SECRET` and no leeway,
    /// with `server`, `auth` and `jwt` added to their tables.
    fn config(&self, port: u16, server: &str, auth: &str, jwt: &str) -> PathBuf {
        let path = self.0.join("config.toml");
        std::fs::write(
            &path,
            format!(
                "[server]\nhost = \"127.0.0.1\"\nport = {port}\nplugin_tick_interval = 1\n{server}\n\
                 [admin]\nsocket = \"\"\n\
                 [auth]\nenabled = true\n{auth}\n\
                 [auth.jwt]\nsecret_file = \"{}\"\nalgorithms = [\"HS256\"]\nleeway_seconds = 0\n{jwt}\n",
                self.secret().display()
            ),
        )
        .unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Broker {
    child: Child,
    port: u16,
    dir: TempDir,
}

impl Broker {
    fn start(name: &str, server: &str, auth: &str, jwt: &str) -> Broker {
        let dir = TempDir::new(name);
        let port = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let config = dir.config(port, server, auth, jwt);
        let log = std::fs::File::create(dir.0.join("log")).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_mercuriod"))
            .arg("--config")
            .arg(&config)
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut broker = Broker { child, port, dir };
        let deadline = Instant::now() + Duration::from_secs(10);
        while TcpStream::connect(("127.0.0.1", port)).is_err() {
            if let Some(status) = broker.child.try_wait().unwrap() {
                panic!("the broker exited: {status}");
            }
            assert!(Instant::now() < deadline, "the broker never listened");
            std::thread::sleep(Duration::from_millis(50));
        }
        broker
    }

    async fn connect(
        &self,
        client_id: &str,
        username: &str,
        password: &str,
    ) -> Result<MqttClient, ClientError> {
        MqttClient::connect(
            ConnectOptions::new("127.0.0.1", self.port)
                .client_id(client_id)
                .username(username)
                .password(password.to_string()),
        )
        .await
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.0.join("log")).unwrap()
    }

    fn hangup(&self) {
        // SAFETY: kill has no memory safety preconditions
        unsafe { libc::kill(self.child.id() as libc::pid_t, libc::SIGHUP) };
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn check(name: &str, auth: &str, jwt: &str) -> Output {
    let dir = TempDir::new(name);
    let config = dir.config(1883, "", auth, jwt);
    Command::new(env!("CARGO_BIN_EXE_mercuriod"))
        .arg("--config")
        .arg(&config)
        .arg("--check")
        .output()
        .unwrap()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn token_with(secret: &str, sub: &str, exp: u64) -> String {
    encode(
        &Header::new(Algorithm::HS256),
        &json!({ "sub": sub, "exp": exp }),
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .unwrap()
}

fn token(sub: &str, exp: u64) -> String {
    token_with(SECRET, sub, exp)
}

fn refused(result: Result<MqttClient, ClientError>) -> ReasonCode {
    match result {
        Err(ClientError::ConnectionRefused(reason)) => reason,
        Err(e) => panic!("failed otherwise: {e}"),
        Ok(_) => panic!("connected"),
    }
}

#[tokio::test]
async fn a_valid_token_with_its_user_name_connects() {
    let broker = Broker::start("valid", "", "", "");
    let client = broker
        .connect("c1", "alice", &token("alice", now() + 60))
        .await;
    assert!(client.is_ok(), "{:?}", client.err());
}

#[tokio::test]
async fn a_password_that_is_no_token_is_refused_without_other_backends() {
    let broker = Broker::start("no-token", "", "", "");
    assert_eq!(
        refused(broker.connect("c1", "alice", "not a token").await),
        ReasonCode::BadUserNameOrPassword
    );
}

#[tokio::test]
async fn a_valid_token_with_another_user_name_is_refused_as_a_wrong_password() {
    let broker = Broker::start("other-name", "", "", "");
    let wrong_password = refused(broker.connect("c0", "alice", "not a token").await);
    let reason = refused(
        broker
            .connect("c1", "bob", &token("alice", now() + 60))
            .await,
    );
    assert_eq!(reason, wrong_password);
}

#[tokio::test]
async fn an_expired_token_is_refused_as_a_failed_login() {
    let broker = Broker::start("expired", "", "", "");
    let expired = token("alice", now() - 10);
    let reason = refused(broker.connect("c1", "alice", &expired).await);
    assert_eq!(reason, ReasonCode::BadUserNameOrPassword);
    // Bans spare loopback addresses, so the failure shows in the log only:
    // the broker counts the failed logins it logs this way
    let log = broker.log();
    assert!(
        log.contains("Authentication failed for user `alice`"),
        "{log}"
    );
    assert!(log.contains("expired"), "{log}");
    assert!(!log.contains(&expired), "the token was logged: {log}");
}

#[tokio::test]
async fn password_file_users_still_log_in_next_to_tokens() {
    let files = TempDir::new("passwd-file");
    let passwd = files.0.join("passwd");
    let hash = Argon2::default().hash_password(b"hunter2").unwrap();
    std::fs::write(&passwd, format!("bob:{hash}\n")).unwrap();
    let broker = Broker::start(
        "passwd",
        "",
        &format!("password_file = \"{}\"", passwd.display()),
        "",
    );
    let client = broker.connect("c1", "bob", "hunter2").await;
    assert!(client.is_ok(), "{:?}", client.err());
    let client = broker
        .connect("c2", "alice", &token("alice", now() + 60))
        .await;
    assert!(client.is_ok(), "{:?}", client.err());
    assert_eq!(
        refused(broker.connect("c3", "bob", "wrong").await),
        ReasonCode::BadUserNameOrPassword
    );
}

#[test]
fn check_refuses_jwt_without_authentication() {
    let output = check("disabled", "", "").status;
    assert!(output.success(), "the base configuration must pass");
    let dir = TempDir::new("disabled-config");
    let config = dir.0.join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[auth]\nenabled = false\n[auth.jwt]\nsecret_file = \"{}\"\nalgorithms = [\"HS256\"]\n",
            dir.secret().display()
        ),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_mercuriod"))
        .arg("--config")
        .arg(&config)
        .arg("--check")
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("jwt"), "{stderr}");
}

#[test]
fn check_refuses_two_key_sources() {
    let output = check("two-keys", "", "public_key_file = \"/dev/null\"");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("exactly one"), "{stderr}");
}

#[test]
fn check_refuses_jwt_with_dynamic_security() {
    let output = check(
        "dynsec",
        "dynamic_security = \"/nonexistent/dynsec.json\"",
        "",
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("dynamic_security"), "{stderr}");
    assert!(stderr.contains("jwt"), "{stderr}");
}

#[tokio::test]
async fn sighup_rereads_the_secret() {
    let broker = Broker::start("sighup", "", "", "");
    std::fs::write(broker.dir.secret(), OTHER_SECRET).unwrap();
    broker.hangup();
    let new = token_with(OTHER_SECRET, "alice", now() + 60);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match broker.connect("c1", "alice", &new).await {
            Ok(_) => break,
            Err(e) if Instant::now() > deadline => panic!("the new secret never took: {e}"),
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    assert_eq!(
        refused(
            broker
                .connect("c2", "alice", &token("alice", now() + 60))
                .await
        ),
        ReasonCode::BadUserNameOrPassword
    );
}
