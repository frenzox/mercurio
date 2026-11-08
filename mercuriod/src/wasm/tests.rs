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
