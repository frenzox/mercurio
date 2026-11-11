//! Readiness notifications to systemd, as `Type=notify` services send them.
#![cfg(unix)]

use std::{
    net::TcpListener,
    os::unix::net::UnixDatagram,
    path::PathBuf,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn temp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("mercuriod-notify-{name}-{}", std::process::id()))
}

/// Starts the broker with `config`, telling it to notify `socket`.
fn start(config: &str, name: &str, socket: &str) -> (Child, PathBuf) {
    let path = temp(&format!("{name}.toml"));
    std::fs::write(&path, config).unwrap();
    let broker = Command::new(env!("CARGO_BIN_EXE_mercuriod"))
        .arg("--config")
        .arg(&path)
        .env("NOTIFY_SOCKET", socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    (broker, path)
}

/// The notifications received until one has the line `until`, for `wait`
/// at most.
fn received(socket: &UnixDatagram, until: &str, wait: Duration) -> Vec<String> {
    let deadline = Instant::now() + wait;
    let mut messages: Vec<String> = Vec::new();
    let mut buffer = [0u8; 4096];
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        socket
            .set_read_timeout(Some(left.max(Duration::from_millis(1))))
            .unwrap();
        match socket.recv(&mut buffer) {
            Ok(n) => messages.push(String::from_utf8_lossy(&buffer[..n]).into_owned()),
            Err(_) => break,
        }
        if messages.iter().any(|m| m.lines().any(|l| l == until)) {
            break;
        }
    }
    messages
}

fn has(messages: &[String], line: &str) -> bool {
    messages.iter().any(|m| m.lines().any(|l| l == line))
}

fn memory_config() -> String {
    let port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    format!("[server]\nport = {port}\n[admin]\nsocket = \"\"\n")
}

fn terminate(broker: &Child) {
    // SAFETY: kill has no memory safety preconditions
    unsafe { libc::kill(broker.id() as libc::pid_t, libc::SIGTERM) };
}

#[test]
fn ready_once_serving_and_stopping_on_shutdown() {
    let path = temp("socket");
    let _ = std::fs::remove_file(&path);
    let socket = UnixDatagram::bind(&path).unwrap();
    let (mut broker, config) = start(&memory_config(), "ready", path.to_str().unwrap());

    let ready = received(&socket, "READY=1", Duration::from_secs(10));
    terminate(&broker);
    let stopping = received(&socket, "STOPPING=1", Duration::from_secs(10));
    let _ = broker.wait();
    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_file(&path);

    assert!(has(&ready, "READY=1"), "{ready:?}");
    assert!(has(&stopping, "STOPPING=1"), "{stopping:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn abstract_socket_addresses_are_reached() {
    use std::os::{linux::net::SocketAddrExt, unix::net::SocketAddr};

    let name = format!("mercuriod-notify-{}", std::process::id());
    let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
    let socket = UnixDatagram::bind_addr(&address).unwrap();
    let (mut broker, config) = start(&memory_config(), "abstract", &format!("@{name}"));

    let ready = received(&socket, "READY=1", Duration::from_secs(10));
    let _ = broker.kill();
    let _ = broker.wait();
    let _ = std::fs::remove_file(&config);

    assert!(has(&ready, "READY=1"), "{ready:?}");
}

#[test]
fn not_ready_while_waiting_for_the_database() {
    // A database that accepts connections and never answers, so the broker
    // waits at startup, as a standby does
    let database = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = database.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in database.incoming() {
            held.push(stream);
        }
    });
    let path = temp("waiting");
    let _ = std::fs::remove_file(&path);
    let socket = UnixDatagram::bind(&path).unwrap();
    let config = format!(
        "[storage]\nbackend = \"postgres\"\n\
         url = \"postgres://u:p@127.0.0.1:{port}/db?sslmode=disable\"\n\
         [admin]\nsocket = \"\"\n"
    );
    let (mut broker, config) = start(&config, "waiting", path.to_str().unwrap());

    let messages = received(&socket, "READY=1", Duration::from_secs(2));
    let _ = broker.kill();
    let _ = broker.wait();
    let _ = std::fs::remove_file(&config);
    let _ = std::fs::remove_file(&path);

    assert!(!has(&messages, "READY=1"), "{messages:?}");
}
