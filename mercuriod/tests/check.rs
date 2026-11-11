//! `mercuriod --check` validates everything a start would, without binding
//! a listener or touching storage, and fails with a start's error.
#![cfg(unix)]

use std::{
    net::TcpListener,
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct TempDir(PathBuf);

impl TempDir {
    fn new(name: &str) -> TempDir {
        let dir =
            std::env::temp_dir().join(format!("mercuriod-check-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    fn write(&self, name: &str, content: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, content).unwrap();
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn mercuriod(config: &Path, check: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mercuriod"));
    command.arg("--config").arg(config);
    if check {
        command.arg("--check");
    }
    command.output().unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// A port this test holds, so a broker that tried to listen on it would fail.
fn taken_port() -> (TcpListener, u16) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    (listener, port)
}

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../mercurio-server/tests/fixtures"
);

#[test]
fn a_valid_configuration_passes_without_binding_or_storage() {
    let dir = TempDir::new("valid");
    let (_held, port) = taken_port();
    let (_held_ws, ws_port) = taken_port();
    let database = dir.0.join("mercurio.db");
    let passwd = dir.write(
        "passwd",
        "alice:$6$pZufTypp2a6RmJv0$0E3Uw01KVS5L6/s+8dWtsoMjdF9mnjUcloZJLTWqsIQbvp1b3crPcunceFO+VmiVpj+Wde8NRnyJs4ZAFgaQUA==\n",
    );
    let acl = dir.write("acl", "user alice\ntopic readwrite #\n");
    let socket = dir.0.join("mqtt.sock");
    let config = dir.write(
        "config.toml",
        &format!(
            r#"
            [server]
            port = {port}
            websocket_port = {ws_port}
            [server.tls]
            enabled = true
            cert_path = "{FIXTURES}/cert.pem"
            key_path = "{FIXTURES}/key.pem"
            [storage]
            backend = "sqlite"
            path = "{database}"
            [auth]
            enabled = true
            password_file = "{passwd}"
            acl_file = "{acl}"
            [admin]
            socket = "{admin}"
            [[listeners]]
            path = "{socket}"
            "#,
            database = database.display(),
            passwd = passwd.display(),
            acl = acl.display(),
            admin = dir.0.join("admin.sock").display(),
            socket = socket.display(),
        ),
    );
    let output = mercuriod(&config, true);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!database.exists(), "storage was opened");
    assert!(!socket.exists(), "a Unix listener was bound");
    assert!(
        !dir.0.join("admin.sock").exists(),
        "the admin socket was bound"
    );
}

#[test]
fn check_does_not_connect_to_postgres() {
    let dir = TempDir::new("postgres");
    let database = TcpListener::bind("127.0.0.1:0").unwrap();
    let db_port = database.local_addr().unwrap().port();
    database.set_nonblocking(true).unwrap();
    let config = dir.write(
        "config.toml",
        &format!(
            "[storage]\nbackend = \"postgres\"\n\
             url = \"postgres://u:p@127.0.0.1:{db_port}/db?sslmode=disable\"\n\
             [admin]\nsocket = \"\"\n"
        ),
    );
    let output = mercuriod(&config, true);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(database.accept().is_err(), "the database was connected to");
}

/// Configurations a start refuses, with what the error must name.
fn broken(dir: &TempDir) -> Vec<(&'static str, PathBuf, Vec<String>)> {
    let (_, port) = taken_port();
    let config =
        |name: &str, body: String| dir.write(name, &format!("{body}\n[admin]\nsocket = \"\"\n"));
    let plaintext = dir.write("passwd", "alice:hunter2\n");
    let missing_cert = dir.0.join("missing.pem");
    let not_a_file = dir.0.join("not-a-file.toml");
    std::fs::create_dir_all(&not_a_file).unwrap();
    let include = dir.0.join("conf.d");
    std::fs::create_dir_all(include.join("bad.toml")).unwrap();
    vec![
        (
            "unknown key",
            config("unknown.toml", "[server]\nprot = 1\n".into()),
            vec!["`prot`".into(), "unknown.toml".into()],
        ),
        (
            "plain-text password",
            config(
                "plaintext.toml",
                format!(
                    "[auth]\nenabled = true\npassword_file = \"{}\"\n",
                    plaintext.display()
                ),
            ),
            vec!["line 1".into(), plaintext.display().to_string()],
        ),
        (
            "missing certificate",
            config(
                "tls.toml",
                format!(
                    "[server.tls]\nenabled = true\ncert_path = \"{}\"\nkey_path = \"{FIXTURES}/key.pem\"\n",
                    missing_cert.display()
                ),
            ),
            vec![missing_cert.display().to_string()],
        ),
        (
            "authentication off with a password file, on a taken port",
            config(
                "auth.toml",
                format!(
                    "[server]\nport = {port}\n[auth]\npassword_file = \"{}\"\n",
                    plaintext.display()
                ),
            ),
            vec!["`enabled`".into()],
        ),
        (
            "HTTP timeout past plugin_timeout",
            config(
                "timeout.toml",
                "[auth.http]\nacl_url = \"http://127.0.0.1:9/acl\"\ntimeout_seconds = 10\n".into(),
            ),
            vec!["`timeout_seconds`".into(), "`plugin_timeout`".into()],
        ),
        (
            "missing HTTP auth CA file",
            config(
                "http-ca.toml",
                format!(
                    "[auth.http]\nacl_url = \"https://127.0.0.1:9/acl\"\nca_path = \"{}\"\n",
                    missing_cert.display()
                ),
            ),
            vec![missing_cert.display().to_string()],
        ),
        (
            "HTTP auth client certificate without its key",
            config(
                "http-cert.toml",
                format!(
                    "[auth.http]\nacl_url = \"https://127.0.0.1:9/acl\"\ncert_path = \"{FIXTURES}/cert.pem\"\n"
                ),
            ),
            vec!["cert_path".into(), "key_path".into()],
        ),
        (
            "unreadable configuration",
            not_a_file.clone(),
            vec![not_a_file.display().to_string()],
        ),
        (
            "unreadable included file",
            config(
                "include.toml",
                format!("include_dir = \"{}\"\n", include.display()),
            ),
            vec![include.join("bad.toml").display().to_string()],
        ),
    ]
}

#[test]
fn broken_configurations_fail_with_the_error_a_start_gives() {
    let dir = TempDir::new("broken");
    // Held through the starts below, which would otherwise serve on it
    let (_held, _) = taken_port();
    for (what, config, named) in broken(&dir) {
        let checked = mercuriod(&config, true);
        let message = stderr(&checked);
        assert!(!checked.status.success(), "{what}: passed the check");
        for name in &named {
            assert!(
                message.contains(name.as_str()),
                "{what}: {name} not in {message}"
            );
        }
        // One readable line, not a Debug string with escapes
        assert_eq!(message.trim_end().lines().count(), 1, "{what}: {message}");
        assert!(!message.contains("Error: \""), "{what}: {message}");
        assert!(!message.contains("\\n"), "{what}: {message}");

        let started = mercuriod(&config, false);
        assert!(!started.status.success(), "{what}: started");
        assert_eq!(stderr(&started), message, "{what}");
    }
}

#[test]
fn the_shipped_configuration_passes() {
    let config = Path::new(env!("CARGO_MANIFEST_DIR")).join("contrib/config.toml");
    let output = mercuriod(&config, true);
    assert!(output.status.success(), "{}", stderr(&output));
}

#[test]
fn a_start_validates_authentication_before_binding() {
    // The auth error, which needs no port, and not the port's
    let dir = TempDir::new("order");
    let (_held, port) = taken_port();
    let passwd = dir.write("passwd", "alice:hunter2\n");
    let config = dir.write(
        "config.toml",
        &format!(
            "[admin]\nsocket = \"\"\n[server]\nport = {port}\n[auth]\npassword_file = \"{}\"\n",
            passwd.display()
        ),
    );
    let output = mercuriod(&config, false);
    let message = stderr(&output);
    assert!(!output.status.success());
    assert!(message.contains("`enabled`"), "{message}");
    assert!(!message.contains("listen"), "{message}");
}

#[test]
fn an_imported_mosquitto_file_passes_and_is_not_overwritten() {
    let dir = TempDir::new("import");
    let mosquitto = dir.write(
        "dynamic-security.json",
        r#"{"clients": [{"username": "u", "roles": [{"rolename": "r"}]}],
            "roles": [{"rolename": "r", "acls": [
                {"acltype": "unsubscribeLiteral", "topic": "t", "allow": true}]}]}"#,
    );
    let imported = dir.0.join("imported.json");
    let import = || {
        Command::new(env!("CARGO_BIN_EXE_mercuriod"))
            .arg("dynsec-import")
            .arg(&mosquitto)
            .arg(&imported)
            .output()
            .unwrap()
    };
    let output = import();
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).contains("unsubscribeLiteral"),
        "{}",
        stderr(&output)
    );

    let config = dir.write(
        "config.toml",
        &format!(
            "[admin]\nsocket = \"\"\n[auth]\nenabled = true\ndynamic_security = \"{}\"\n",
            imported.display()
        ),
    );
    let checked = mercuriod(&config, true);
    assert!(checked.status.success(), "{}", stderr(&checked));

    let again = import();
    assert!(!again.status.success());
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );
}
