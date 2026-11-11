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
        broker: None,
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
async fn test_auth_steps_see_the_client() {
    use mercurio_server::plugin::AuthResult;

    let plugin = misbehave(|_| ()).await;
    let client = ClientInfo::new("who", Some("claimed".into()));
    let mut exchange = plugin.begin_auth(&client, "WHO").unwrap();
    assert_eq!(
        exchange.step(None).await,
        AuthResult::Success {
            username: Some(r#"who Some("claimed")"#.into()),
            data: None,
        }
    );
    assert!(plugin.begin_auth(&client, "OTHER").is_none());
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

/// Collects what is logged on the test's thread.
#[derive(Clone, Default)]
struct Logs(std::sync::Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// On one thread, so the plugin's calls log where the test listens
#[tokio::test(flavor = "current_thread")]
async fn test_a_rust_plugin_panic_is_logged() {
    let logs = Logs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(move || writer.clone())
        .with_ansi(false)
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    let plugin = misbehave(|_| ()).await;
    // Tracing caches whether a log line has a subscriber when its line
    // first runs. A test on another thread that ran the plugin's log line
    // first, as this subscriber was being set, can leave "none" cached for
    // it, since rebuilding the cache for a new subscriber skips lines still
    // being registered. By now they are registered, so this rebuild sees
    // them.
    tracing::callsite::rebuild_interest_cache();
    assert_eq!(control(&plugin, "panic", b"ouch").await, None);
    let logged = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("misbehaving on purpose: ouch"), "{logged}");
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
async fn test_a_plugin_with_allowed_sockets_cant_listen() {
    let allowed = free_port().await;
    let plugin = misbehave(|s| s.allow_sockets = vec![format!("127.0.0.1:{allowed}")]).await;
    for address in [
        "0.0.0.0:0".to_string(),
        format!("127.0.0.1:{allowed}"),
        format!("0.0.0.0:{allowed}"),
    ] {
        let refused = control(&plugin, "listen", address.as_bytes())
            .await
            .unwrap();
        assert!(refused.contains("PermissionDenied"), "{address}: {refused}");
    }
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
    let message = |payload: &'static [u8], retain: bool| {
        PluginMessage::new(
            "t",
            bytes::Bytes::from_static(payload),
            QoS::AtMostOnce,
            retain,
            MessageProperties::default(),
        )
    };

    let mut tagged = message(b"hello", false);
    assert_eq!(
        plugin.on_message(&client, &mut tagged).await,
        MessageAction::Continue
    );
    let properties = tagged.properties.user_property.unwrap();
    assert_eq!(properties.len(), 1);
    assert_eq!(properties[0].key, "seen-by");
    assert_eq!(properties[0].value, "tagger");
    assert_eq!(tagged.payload.as_ref(), b"hello");

    let mut empty = message(b"", false);
    assert_eq!(
        plugin.on_message(&client, &mut empty).await,
        MessageAction::Drop
    );

    // An empty retained message clears the topic's retained message
    let mut clear = message(b"", true);
    assert_eq!(
        plugin.on_message(&client, &mut clear).await,
        MessageAction::Continue
    );
    assert!(clear.retain && clear.payload.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn test_webhook_audit_posts_events() {
    use std::sync::Arc;

    use mercurio_server::server::{ListenerConfig, ServerConfig, Transport};
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
    let mounted = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mounted_addr = mounted.local_addr().unwrap();
    let mut config = ServerConfig::default();
    config.plugins = vec![plugin];
    config.sys_interval = None;
    config.listeners = vec![{
        let mut mounted = ListenerConfig::new(mounted, Transport::Mqtt);
        mounted.mount_point = Some("tenant/".into());
        mounted
    }];
    tokio::spawn(mercurio_server::server::run_with_config(
        listener,
        config,
        std::future::pending::<()>(),
    ));

    // MQTT 5 CONNECT with clean start and the given properties
    let connect = |id: &[u8], properties: &[u8]| {
        let mut connect = vec![0x00, 0x04, b'M', b'Q', b'T', b'T', 5, 0x02, 0x00, 60];
        connect.push(properties.len() as u8);
        connect.extend_from_slice(properties);
        connect.extend_from_slice(&(id.len() as u16).to_be_bytes());
        connect.extend_from_slice(id);
        let mut packet = vec![0x10, connect.len() as u8];
        packet.extend(connect);
        packet
    };
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&connect(b"audited-client", &[]))
        .await
        .unwrap();
    let mut connack = [0u8; 4];
    stream.read_exact(&mut connack).await.unwrap();
    assert_eq!(connack[0], 0x20);

    let posted = |text: &'static str| {
        let bodies = Arc::clone(&bodies);
        async move {
            loop {
                if let Some(body) = bodies
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|body| body.contains(text))
                {
                    return body.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    };
    let request = tokio::time::timeout(Duration::from_secs(2), posted("audited-client"))
        .await
        .expect("the webhook never received the event");
    assert!(request.starts_with("POST /audit "), "{request}");
    assert!(request.contains(r#""event":"connected""#), "{request}");

    // A session of a mounted listener, expiring a second after its client
    // leaves, is reported with the mount point
    let mut stream = tokio::net::TcpStream::connect(mounted_addr).await.unwrap();
    stream
        .write_all(&connect(b"expiring", &[0x11, 0x00, 0x00, 0x00, 0x01]))
        .await
        .unwrap();
    let mut connack = [0u8; 3];
    stream.read_exact(&mut connack).await.unwrap();
    assert_eq!(connack[0], 0x20);
    drop(stream);
    let request = tokio::time::timeout(Duration::from_secs(5), posted("session-expired"))
        .await
        .expect("the webhook never received the expiry");
    assert!(request.contains(r#""client":"expiring""#), "{request}");
    assert!(request.contains(r#""mountPoint":"tenant/""#), "{request}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_status_reports_hooks_copies_and_failures() {
    use mercurio_server::plugin::PluginKind;

    // Its own runtime, which the reload below uses too
    let runtime = Runtime::new().unwrap();
    let plugin = WasmPlugin::load(&runtime, settings("misbehave", &[]))
        .await
        .unwrap();
    let status = plugin.status();
    assert_eq!(status.name, "misbehave");
    assert_eq!(status.kind, PluginKind::Wasm);
    assert_eq!(
        status.hooks,
        [
            "begin_auth",
            "authorize_control",
            "on_message",
            "on_control"
        ]
    );
    assert_eq!(status.interface.as_deref(), Some("0.2.0"));
    assert_eq!(status.instances, Some(1));
    assert_eq!(status.failures, Some(0));
    assert_eq!(status.last_error, None);

    // The trapped copy is dropped, and the failure is remembered
    assert_eq!(control(&plugin, "panic", b"ouch").await, None);
    let status = plugin.status();
    assert_eq!(status.instances, Some(0));
    assert_eq!(status.failures, Some(1));
    let last_error = status.last_error.expect("the failure");
    assert!(last_error.contains("on_control"), "{last_error}");

    // Copies are made again on demand, and failures outlive a reload
    assert_eq!(control(&plugin, "kv-count", b"").await.unwrap(), "1");
    assert_eq!(plugin.status().instances, Some(1));
    plugin.reload(&runtime).await.unwrap();
    let status = plugin.status();
    assert_eq!((status.instances, status.failures), (Some(1), Some(1)));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_plugins_shows_wasm_plugins() {
    use std::sync::Arc;

    use mercurio_server::{
        plugin::PluginKind,
        server::{Server, ServerConfig},
    };
    use mercurio_storage::memory::MemoryStore;

    let plugin = WasmPlugin::load(
        &Runtime::new().unwrap(),
        settings("deny-topic", &[("prefix", "secret/")]),
    )
    .await
    .unwrap();
    let mut config = ServerConfig::default();
    config.plugins = vec![plugin as Arc<dyn Plugin>];
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let server = Server::new(listener, config, Arc::new(MemoryStore::new()))
        .await
        .unwrap();
    let plugins = server.admin().plugins().await;
    let [status] = &plugins[..] else {
        panic!("expected one plugin: {plugins:?}");
    };
    assert_eq!(status.name, "deny-topic");
    assert_eq!(status.kind, PluginKind::Wasm);
    assert_eq!(status.hooks, ["authorize"]);
    assert_eq!((status.instances, status.failures), (Some(1), Some(0)));
    let json = serde_json::to_value(status).unwrap();
    assert_eq!(json["kind"], "wasm");
    assert_eq!(json["instances"], 1);
    assert_eq!(json["interface"], "0.2.0");
    assert!(json.get("lastError").is_none(), "{json}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_call_the_broker_cancels_counts_as_a_failure() {
    // Like the broker's plugin_timeout, shorter than the plugin's own limit
    let plugin = misbehave(|s| s.instances = 1).await;
    let cancelled =
        tokio::time::timeout(Duration::from_millis(200), control(&plugin, "sleep", b"")).await;
    assert!(cancelled.is_err());
    let status = plugin.status();
    assert_eq!((status.instances, status.failures), (Some(0), Some(1)));
    let last_error = status.last_error.expect("the cancelled call");
    assert!(last_error.contains("on_control"), "{last_error}");
    assert!(last_error.contains("plugin_timeout"), "{last_error}");
    assert_eq!(control(&plugin, "kv-count", b"").await.unwrap(), "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_plugin_built_for_0_1_still_loads() {
    use mercurio_core::{message::MessageProperties, qos::QoS};
    use mercurio_server::plugin::{AuthResult, MessageAction, PluginMessage};

    let plugin = WasmPlugin::load(&Runtime::new().unwrap(), settings("interface-0-1", &[]))
        .await
        .unwrap();
    let status = plugin.status();
    assert_eq!(status.interface.as_deref(), Some("0.1.0"));
    assert_eq!(
        status.hooks,
        ["begin_auth", "authorize", "on_message", "on_control"]
    );

    let client = connected("old", None);
    assert_eq!(
        plugin.authorize(&client, "old/x", Access::Write).await,
        Decision::Deny
    );
    assert_eq!(
        plugin.authorize(&client, "old/x", Access::Read).await,
        Decision::Defer
    );
    assert_eq!(
        plugin
            .authorize_control(&client, "$CONTROL/broker/v1")
            .await,
        Decision::Defer
    );

    let mut exchange = plugin.begin_auth(&client, "OLD").unwrap();
    assert_eq!(
        exchange.step(None).await,
        AuthResult::Success {
            username: Some("old".into()),
            data: None,
        }
    );

    let mut message = PluginMessage::new(
        "t",
        bytes::Bytes::from_static(b"m"),
        QoS::AtLeastOnce,
        false,
        MessageProperties::default(),
    );
    assert_eq!(
        plugin.on_message(&client, &mut message).await,
        MessageAction::Continue
    );
    let tag = &message.properties.user_property.unwrap()[0];
    assert_eq!((tag.key.as_str(), tag.value.as_str()), ("seen-by", "old"));

    assert_eq!(control(&plugin, "kv", b"stored").await.unwrap(), "stored");

    // 0.1 knows nothing of the connection
    let seen = control_as(&plugin, &client, "client").await.unwrap();
    assert!(seen.contains(r#"client-id: "old""#), "{seen}");
    assert!(!seen.contains("connection"), "{seen}");
}

/// A client on a TLS listener, as hooks see it once it connected.
fn connected(client_id: &str, username: Option<&str>) -> ClientInfo {
    use mercurio_core::protocol::ProtocolVersion;
    use mercurio_server::plugin::ConnectionInfo;

    let mut connection = ConnectionInfo::new("0.0.0.0:8883", ProtocolVersion::V3_1_1);
    connection.tls = true;
    connection.certificate = Some("device-7".into());
    connection.mount_point = Some("tenant/".into());
    let mut client = ClientInfo::new(client_id, username.map(Into::into));
    client.peer = Some("10.0.0.1:5555".parse().unwrap());
    client.connection = Some(std::sync::Arc::new(connection));
    client
}

async fn control_as(plugin: &WasmPlugin, client: &ClientInfo, path: &str) -> Option<String> {
    plugin
        .on_control(client, path, bytes::Bytes::new())
        .await
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test(flavor = "multi_thread")]
async fn test_plugins_see_the_connection() {
    let plugin = misbehave(|_| ()).await;
    let seen = control_as(&plugin, &connected("c", Some("u")), "client")
        .await
        .unwrap();
    for part in [
        r#"listener: "0.0.0.0:8883""#,
        "protocol-version: 4",
        "tls: true",
        r#"certificate: Some("device-7")"#,
        r#"mount-point: Some("tenant/")"#,
        r#"peer: Some("10.0.0.1:5555")"#,
    ] {
        assert!(seen.contains(part), "{part} in {seen}");
    }
    let seen = control_as(&plugin, &ClientInfo::new("c", None), "client")
        .await
        .unwrap();
    assert!(seen.contains("connection: None"), "{seen}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_plugins_can_grant_control() {
    let plugin = misbehave(|_| ()).await;
    let topic = "$CONTROL/broker/v1";
    assert_eq!(
        plugin
            .authorize_control(&connected("c", Some("admin")), topic)
            .await,
        Decision::Allow
    );
    assert_eq!(
        plugin
            .authorize_control(&connected("c", Some("bob")), topic)
            .await,
        Decision::Defer
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_authentication_success_sends_final_data() {
    use mercurio_server::plugin::AuthResult;

    let plugin = misbehave(|_| ()).await;
    let client = connected("c", None);
    let mut exchange = plugin.begin_auth(&client, "WHO").unwrap();
    let data = bytes::Bytes::from_static(b"server-final");
    assert_eq!(
        exchange.step(Some(&data)).await,
        AuthResult::Success {
            username: Some(r#"c None"#.into()),
            data: Some(data),
        }
    );
}

/// Starts a broker serving the in-process connections of `listener`.
async fn broker_with(
    plugins: Vec<std::sync::Arc<dyn Plugin>>,
    listener: mercurio_server::server::LocalListener,
) {
    use mercurio_server::server::ServerConfig;

    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut config = ServerConfig::default();
    config.plugins = plugins;
    config.sys_interval = None;
    config.local_listener = Some(listener);
    tokio::spawn(mercurio_server::server::run_with_config(
        tcp,
        config,
        std::future::pending::<()>(),
    ));
}

/// misbehave, publishing into a running broker, and a client of that
/// broker subscribed to `filter`.
async fn publishing(filter: &str) -> (std::sync::Arc<WasmPlugin>, mercurio_client::MqttClient) {
    use mercurio_client::{ConnectOptions, MqttClient, QoS};
    use mercurio_server::server::LocalConnector;

    let (connector, listener) = LocalConnector::new();
    let broker = connector.clone();
    let plugin = misbehave(move |s| s.broker = Some(broker)).await;
    broker_with(vec![plugin.clone()], listener).await;
    let options = ConnectOptions::new("localhost", 0)
        .client_id("watcher")
        .connector(move || std::future::ready(connector.connect()));
    let watcher = MqttClient::connect(options).await.unwrap();
    watcher
        .subscribe(&[(filter, QoS::AtLeastOnce)])
        .await
        .unwrap();
    (plugin, watcher)
}

/// The topic and payload of the next message `client` receives, if one
/// comes within half a second.
async fn next_message(client: &mercurio_client::MqttClient) -> Option<(String, Vec<u8>)> {
    loop {
        let event = tokio::time::timeout(Duration::from_millis(500), client.recv())
            .await
            .ok()??;
        if let mercurio_client::Event::Message { topic, payload, .. } = event {
            return Some((topic, payload.to_vec()));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_plugin_publishes_through_the_broker() {
    let (plugin, watcher) = publishing("out/#").await;
    assert_eq!(
        control(&plugin, "publish", b"out/x\nhello").await.unwrap(),
        "ok"
    );
    let event = tokio::time::timeout(Duration::from_secs(2), watcher.recv())
        .await
        .unwrap()
        .unwrap();
    let mercurio_client::Event::Message {
        topic,
        payload,
        qos,
        properties,
        ..
    } = event
    else {
        panic!("expected the message, got {event:?}");
    };
    assert_eq!(topic, "out/x");
    assert_eq!(payload.as_ref(), b"hello");
    assert_eq!(qos, mercurio_client::QoS::AtLeastOnce);
    assert_eq!(properties.content_type.as_deref(), Some("text/plain"));
    assert_eq!(
        properties.user_properties,
        [("by".to_string(), "misbehave".to_string())]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_plugin_never_sees_its_own_messages() {
    let (plugin, watcher) = publishing("echo/#").await;
    // As a client would publish it, through the plugin's on_message
    let mut message = mercurio_server::plugin::PluginMessage::new(
        "echo/a",
        bytes::Bytes::new(),
        mercurio_core::qos::QoS::AtMostOnce,
        false,
        Default::default(),
    );
    plugin
        .on_message(&ClientInfo::new("someone", None), &mut message)
        .await;
    assert_eq!(
        next_message(&watcher)
            .await
            .map(|(topic, _)| topic)
            .as_deref(),
        Some("echo/echo/a")
    );
    // The copy reached the plugin's on_message as its own, and stopped
    assert_eq!(next_message(&watcher).await, None);
    assert_eq!(plugin.status().failures, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_plugins_may_not_publish_to_dollar_topics() {
    let (plugin, watcher) = publishing("#").await;
    for topic in ["$SYS/broker/uptime", "$CONTROL/broker/v1", "$share/g/t"] {
        let refused = control(&plugin, "publish", format!("{topic}\nx").as_bytes())
            .await
            .unwrap();
        assert!(refused.contains("`$` topics"), "{topic}: {refused}");
    }
    let refused = control(&plugin, "publish", b"a/+\nx").await.unwrap();
    assert!(refused.contains("a/+"), "{refused}");
    assert_eq!(next_message(&watcher).await, None);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_an_oversized_publish_is_refused() {
    let (plugin, watcher) = publishing("big").await;
    let limit = super::publish::PUBLISH_SIZE;
    let refused = control(&plugin, "publish-big", (limit + 1).to_string().as_bytes())
        .await
        .unwrap();
    assert!(refused.contains("allowed"), "{refused}");
    assert_eq!(next_message(&watcher).await, None);
    // The topic counts too
    let fits = limit - "big".len();
    assert_eq!(
        control(&plugin, "publish-big", fits.to_string().as_bytes())
            .await
            .unwrap(),
        "ok"
    );
    let (_, payload) = next_message(&watcher).await.unwrap();
    assert_eq!(payload.len(), fits);
    // A refusal is the plugin's to handle, not a failed call
    assert_eq!(plugin.status().failures, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_publish_flood_is_rate_limited() {
    use super::publish::{PUBLISH_BURST, PUBLISH_RATE};

    // Messages queue for a broker that never serves them
    let (connector, _listener) = mercurio_server::server::LocalConnector::new();
    let plugin = misbehave(|s| s.broker = Some(connector)).await;
    // The bucket refills while the test runs, slowly on a loaded machine
    let refilled = |since: std::time::Instant| {
        (since.elapsed().as_secs_f64() * PUBLISH_RATE as f64).ceil() as u32
    };
    let count = |flood: &str| -> u32 { flood.split_once(' ').unwrap().0.parse().unwrap() };
    let started = std::time::Instant::now();
    let flood = control(&plugin, "publish-flood", b"1000").await.unwrap();
    let published = count(&flood);
    assert!(
        (PUBLISH_BURST..=PUBLISH_BURST + refilled(started)).contains(&published),
        "{flood}"
    );
    assert!(
        flood.contains(&format!("faster than {PUBLISH_RATE} messages a second")),
        "{flood}"
    );
    // The limit is the plugin's, whichever copy publishes
    let started = std::time::Instant::now();
    let (a, b) = tokio::join!(
        control(&plugin, "publish-flood", b"1000"),
        control(&plugin, "publish-flood", b"1000"),
    );
    assert!(count(&a.unwrap()) + count(&b.unwrap()) <= refilled(started) + 1);
    assert_eq!(plugin.status().failures, Some(0));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_a_plugin_publishing_without_end_is_stopped() {
    let (connector, _listener) = mercurio_server::server::LocalConnector::new();
    let plugin = misbehave(|s| {
        s.broker = Some(connector);
        s.timeout = Duration::from_millis(200);
    })
    .await;
    assert_eq!(control_within(&plugin, "publish-spin", b"").await, None);
    let status = plugin.status();
    assert_eq!(status.failures, Some(1));
    assert!(status.last_error.unwrap().contains("on_control"));
    assert_eq!(control(&plugin, "kv-count", b"").await.unwrap(), "1");
}
