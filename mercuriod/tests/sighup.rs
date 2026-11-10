//! A reload asked for while the broker is still starting must not stop it.
#![cfg(unix)]

use std::{
    net::TcpListener,
    process::{Command, Stdio},
    sync::mpsc,
    time::Duration,
};

#[test]
fn sighup_while_waiting_for_the_database_keeps_the_broker_running() {
    // A database that accepts connections and never answers, so the broker
    // waits at startup, as a standby does
    let database = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = database.local_addr().unwrap().port();
    let (connected, waiting) = mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in database.incoming() {
            held.push(stream.unwrap());
            let _ = connected.send(());
        }
    });

    let config = std::env::temp_dir().join(format!("mercuriod-sighup-{}.toml", std::process::id()));
    std::fs::write(
        &config,
        format!(
            "[storage]\nbackend = \"postgres\"\n\
             url = \"postgres://u:p@127.0.0.1:{port}/db?sslmode=disable\"\n\
             [admin]\nsocket = \"\"\n"
        ),
    )
    .unwrap();
    let mut broker = Command::new(env!("CARGO_BIN_EXE_mercuriod"))
        .arg("--config")
        .arg(&config)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let started = waiting.recv_timeout(Duration::from_secs(10));
    if started.is_ok() {
        // SAFETY: kill has no memory safety preconditions
        unsafe { libc::kill(broker.id() as libc::pid_t, libc::SIGHUP) };
        std::thread::sleep(Duration::from_millis(500));
    }
    let exited = broker.try_wait().unwrap();
    let _ = broker.kill();
    let _ = broker.wait();
    let _ = std::fs::remove_file(&config);

    started.expect("the broker never connected to the database");
    assert_eq!(exited, None, "SIGHUP stopped the starting broker");
}
