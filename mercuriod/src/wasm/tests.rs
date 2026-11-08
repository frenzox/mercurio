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
    let loading = WasmPlugin::load(&runtime, settings);
    // Compiling the plugin counts too, and takes seconds when many tests
    // compile at once; a hang would last the two minutes of the sleep
    let result = tokio::time::timeout(Duration::from_secs(30), loading)
        .await
        .expect("loading hung");
    assert!(result.is_err());
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
async fn test_kv_is_shared_by_instances() {
    let plugin = misbehave(|s| s.instances = 2).await;
    let (a, b, c, d) = tokio::join!(
        control(&plugin, "kv-count", b""),
        control(&plugin, "kv-count", b""),
        control(&plugin, "kv-count", b""),
        control(&plugin, "kv-count", b""),
    );
    let mut counts: Vec<_> = [a, b, c, d].into_iter().map(Option::unwrap).collect();
    counts.sort();
    assert_eq!(counts, ["1", "2", "3", "4"]);
}

async fn free_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}
