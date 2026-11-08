use std::{
    collections::HashSet,
    path::PathBuf,
    process::Command,
    sync::{Mutex, OnceLock},
    time::Duration,
};

use mercurio_server::plugin::{Access, ClientInfo, Decision, Plugin};

use super::{PluginSettings, Runtime, WasmPlugin};

/// Builds a plugin crate for wasm32-wasip2 once per test run.
pub(crate) fn example(name: &str) -> PathBuf {
    static BUILT: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = crate_dir.join("..");
    let manifest = [
        root.join("examples/plugins").join(name),
        crate_dir.join("test-plugins").join(name),
    ]
    .into_iter()
    .map(|dir| dir.join("Cargo.toml"))
    .find(|manifest| manifest.exists())
    .unwrap_or_else(|| panic!("no plugin crate named {name}"));
    let target = root.join("target/plugins");
    let mut built = BUILT.get_or_init(Default::default).lock().unwrap();
    if built.insert(name.to_string()) {
        let status = Command::new(env!("CARGO"))
            .args(["build", "-q", "--release", "--target", "wasm32-wasip2"])
            .arg("--manifest-path")
            .arg(&manifest)
            .arg("--target-dir")
            .arg(&target)
            .env("CARGO_INCREMENTAL", "0")
            .status()
            .unwrap();
        assert!(status.success(), "building {name} failed");
    }
    target
        .join("wasm32-wasip2/release")
        .join(format!("{}.wasm", name.replace('-', "_")))
}

pub(crate) fn settings(name: &str, config: &[(&str, &str)]) -> PluginSettings {
    PluginSettings {
        name: name.to_string(),
        path: example(name),
        config: config
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        allow_http: Vec::new(),
        allow_sockets: Vec::new(),
        instances: 2,
        timeout: Duration::from_secs(5),
        memory: 64 << 20,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_deny_topic_decides_through_the_sandbox() {
    let runtime = Runtime::new().unwrap();
    let plugin = WasmPlugin::load(&runtime, settings("deny-topic", &[("prefix", "secret/")]))
        .await
        .unwrap();
    let client = ClientInfo::new("c", None);
    assert_eq!(
        plugin.authorize(&client, "secret/x", Access::Write).await,
        Decision::Deny
    );
    assert_eq!(
        plugin.authorize(&client, "public/x", Access::Write).await,
        Decision::Defer
    );
    assert_eq!(
        plugin.authorize(&client, "secret/x", Access::Read).await,
        Decision::Defer
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_init_errors_refuse_the_plugin() {
    let runtime = Runtime::new().unwrap();
    let error = WasmPlugin::load(&runtime, settings("deny-topic", &[]))
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("missing `prefix`"), "{error}");
}

async fn control(plugin: &WasmPlugin, path: &str, payload: &[u8]) -> Option<String> {
    let client = ClientInfo::new("c", None);
    let payload = bytes::Bytes::copy_from_slice(payload);
    plugin
        .on_control(&client, path, payload)
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

async fn control_within(plugin: &WasmPlugin, path: &str, payload: &[u8]) -> Option<String> {
    tokio::time::timeout(Duration::from_secs(2), control(plugin, path, payload))
        .await
        .expect("the call was not stopped in time")
}

async fn misbehave(adjust: impl FnOnce(&mut PluginSettings)) -> std::sync::Arc<WasmPlugin> {
    let mut settings = settings("misbehave", &[]);
    adjust(&mut settings);
    WasmPlugin::load(&Runtime::new().unwrap(), settings)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_plugin_stuck_in_a_loop_is_stopped() {
    let plugin = misbehave(|s| s.timeout = Duration::from_millis(200)).await;
    assert_eq!(control_within(&plugin, "spin", b"").await, None);
    assert_eq!(control(&plugin, "kv-count", b"").await.unwrap(), "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_plugin_stuck_in_a_host_call_is_stopped() {
    let plugin = misbehave(|s| s.timeout = Duration::from_millis(200)).await;
    assert_eq!(control_within(&plugin, "sleep", b"").await, None);
    assert_eq!(control(&plugin, "kv-count", b"").await.unwrap(), "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_plugin_that_sleeps_in_init_fails_to_load() {
    let mut settings = settings("misbehave", &[("init_sleep_ms", "120000")]);
    settings.timeout = Duration::from_millis(200);
    let runtime = Runtime::new().unwrap();
    let started = std::time::Instant::now();
    let loading = WasmPlugin::load(&runtime, settings);
    // Compiling the plugin counts too, and takes seconds when many tests
    // compile at once; a hang would last the two minutes of the sleep
    let result = tokio::time::timeout(Duration::from_secs(30), loading)
        .await
        .expect("loading hung");
    assert!(started.elapsed() < Duration::from_secs(30));
    let error = result.err().expect("the load succeeded").to_string();
    assert!(error.contains("took longer"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_memory_beyond_the_limit_traps() {
    let plugin = misbehave(|s| s.memory = 16 << 20).await;
    assert_eq!(control(&plugin, "grow", b"4").await.unwrap(), "4194304");
    assert_eq!(control(&plugin, "grow", b"64").await, None);
    assert!(control(&plugin, "kv-count", b"").await.is_some());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_http_outside_the_allowlist_is_refused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let allowed = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                    .await;
            });
        }
    });
    let other = free_port().await;
    let plugin = misbehave(|s| s.allow_http = vec![allowed.to_string()]).await;
    let url = |port: u16| format!("http://127.0.0.1:{port}/");
    assert_eq!(
        control(&plugin, "http", url(allowed.port()).as_bytes())
            .await
            .unwrap(),
        "ok"
    );
    let refused = control(&plugin, "http", url(other).as_bytes())
        .await
        .unwrap();
    assert!(refused.contains("HttpRequestDenied"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_sockets_outside_the_allowlist_are_refused() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let allowed = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let _ = listener.accept().await;
        }
    });
    let other = free_port().await;
    let plugin = misbehave(|s| s.allow_sockets = vec![allowed.to_string()]).await;
    assert_eq!(
        control(&plugin, "tcp", allowed.to_string().as_bytes())
            .await
            .unwrap(),
        "ok"
    );
    let refused = control(&plugin, "tcp", format!("127.0.0.1:{other}").as_bytes())
        .await
        .unwrap();
    assert!(refused.contains("PermissionDenied"), "{refused}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_sockets_need_an_allowlist() {
    let plugin = misbehave(|_| ()).await;
    let made = control(&plugin, "sockets", b"20000").await.unwrap();
    assert!(made.starts_with("0 "), "{made}");
    assert!(made.contains("access-denied"), "{made}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_plugin_cant_hold_sockets_without_end() {
    let plugin = misbehave(|s| {
        s.allow_sockets = vec!["127.0.0.1:9".into()];
        s.instances = 1;
    })
    .await;
    // Past the resource table's capacity the call traps and the copy, with
    // its sockets, is dropped
    assert_eq!(control(&plugin, "sockets", b"20000").await, None);
    let made = control(&plugin, "sockets", b"100").await.unwrap();
    assert_eq!(made, "100 none");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_http_resources_share_the_resource_limit() {
    let plugin = misbehave(|s| s.instances = 1).await;
    assert_eq!(control(&plugin, "fields", b"20000").await, None);
    assert_eq!(control(&plugin, "fields", b"100").await.unwrap(), "100");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_kv_is_shared_by_instances() {
    let plugin = misbehave(|s| s.instances = 2).await;
    let marks = tokio::join!(
        control(&plugin, "kv-mark", b"a"),
        control(&plugin, "kv-mark", b"b"),
        control(&plugin, "kv-mark", b"c"),
        control(&plugin, "kv-mark", b"d"),
    );
    assert!([marks.0, marks.1, marks.2, marks.3]
        .iter()
        .all(Option::is_some));
    assert_eq!(control(&plugin, "kv-marks", b"").await.unwrap(), "4");
}

async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

#[tokio::test(flavor = "multi_thread")]
async fn test_reload_keeps_the_previous_code_when_the_new_fails() {
    let dir = std::env::temp_dir().join(format!("mercuriod-wasm-reload-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("deny-topic.wasm");
    std::fs::copy(example("deny-topic"), &path).unwrap();
    let mut settings = settings("deny-topic", &[("prefix", "a/")]);
    settings.path = path.clone();
    let runtime = Runtime::new().unwrap();
    let plugin = WasmPlugin::load(&runtime, settings).await.unwrap();
    let client = ClientInfo::new("c", None);
    let decide = || plugin.authorize(&client, "a/x", Access::Write);
    assert_eq!(decide().await, Decision::Deny);

    std::fs::write(&path, b"not a component").unwrap();
    let error = plugin.reload(&runtime).await.unwrap_err();
    assert!(format!("{error:#}").contains("compiling"), "{error:#}");
    assert_eq!(decide().await, Decision::Deny);

    std::fs::copy(example("deny-topic"), &path).unwrap();
    plugin.reload(&runtime).await.unwrap();
    assert_eq!(decide().await, Decision::Deny);

    // The SIGHUP loop calls reloads synchronously from a runtime thread
    let reloader = super::Reloader::new(std::sync::Arc::clone(&plugin), runtime);
    assert_eq!(
        crate::reload::Reload::path(&reloader),
        path.to_str().unwrap()
    );
    crate::reload::Reload::reload(&reloader).unwrap();
    assert_eq!(decide().await, Decision::Deny);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn test_payload_rewrite_tags_and_drops() {
    use mercurio_core::{message::MessageProperties, qos::QoS};
    use mercurio_server::plugin::{MessageAction, PluginMessage};

    let runtime = Runtime::new().unwrap();
    let plugin = WasmPlugin::load(&runtime, settings("payload-rewrite", &[]))
        .await
        .unwrap();
    let client = ClientInfo::new("tagger", None);
    let message = |payload: &'static [u8]| {
        PluginMessage::new(
            "t",
            bytes::Bytes::from_static(payload),
            QoS::AtMostOnce,
            false,
            MessageProperties::default(),
        )
    };

    let mut tagged = message(b"hello");
    assert_eq!(
        plugin.on_message(&client, &mut tagged).await,
        MessageAction::Continue
    );
    let properties = tagged.properties.user_property.unwrap();
    assert_eq!(properties.len(), 1);
    assert_eq!(properties[0].key, "seen-by");
    assert_eq!(properties[0].value, "tagger");
    assert_eq!(tagged.payload.as_ref(), b"hello");

    let mut empty = message(b"");
    assert_eq!(
        plugin.on_message(&client, &mut empty).await,
        MessageAction::Drop
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_webhook_audit_posts_events() {
    use std::sync::Arc;

    use mercurio_server::server::ServerConfig;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let hook = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let hook_addr = hook.local_addr().unwrap();
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let recorded = Arc::clone(&bodies);
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = hook.accept().await.unwrap();
            let recorded = Arc::clone(&recorded);
            let index = {
                let mut recorded = recorded.lock().unwrap();
                recorded.push(String::new());
                recorded.len() - 1
            };
            tokio::spawn(async move {
                let mut seen = Vec::new();
                let mut buffer = [0u8; 1024];
                let mut answered = false;
                // The body is chunked and may arrive in several reads
                while let Ok(n) = socket.read(&mut buffer).await {
                    if n == 0 {
                        return;
                    }
                    seen.extend_from_slice(&buffer[..n]);
                    recorded.lock().unwrap()[index] = String::from_utf8_lossy(&seen).into_owned();
                    if !answered && seen.windows(4).any(|w| w == b"\r\n\r\n") {
                        answered = true;
                        let _ = socket
                            .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                            .await;
                    }
                }
            });
        }
    });

    let mut audit = settings(
        "webhook-audit",
        &[("url", &format!("http://{hook_addr}/audit"))],
    );
    audit.allow_http = vec![hook_addr.to_string()];
    let plugin = WasmPlugin::load(&Runtime::new().unwrap(), audit)
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.plugins = vec![plugin];
    config.sys_interval = None;
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    let id = b"audited-client";
    let mut connect = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 5, 0x02, 0x00, 60, 0x00];
    connect.extend_from_slice(&(id.len() as u16).to_be_bytes());
    connect.extend_from_slice(id);
    let mut packet = vec![0x10, connect.len() as u8];
    packet.extend(connect);
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(&packet).await.unwrap();
    let mut connack = [0u8; 4];
    stream.read_exact(&mut connack).await.unwrap();
    assert_eq!(connack[0], 0x20);

    let found = async {
        loop {
            if let Some(body) = bodies
                .lock()
                .unwrap()
                .iter()
                .find(|body| body.contains("audited-client"))
            {
                return body.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let request = tokio::time::timeout(Duration::from_secs(2), found)
        .await
        .expect("the webhook never received the event");
    assert!(request.starts_with("POST /audit "), "{request}");
    assert!(request.contains(r#""event":"connected""#), "{request}");
}
