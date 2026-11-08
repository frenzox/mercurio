# mercurio-plugin

Write plugins for the [mercurio](https://github.com/frenzox/mercurio) MQTT
broker in Rust. A plugin is a WebAssembly component that `mercuriod` loads
at startup and runs in a sandbox. It can authenticate and authorize clients,
change or drop messages, watch client events, run on a timer and answer
control requests.

This crate generates the bindings for the broker's WIT interface
(`wit/plugin.wit` in the repository) and wraps them in the [`Plugin`] trait.
Every hook except `init` has a default, so a plugin implements only the hooks
it uses. `init` returns the hooks that the broker calls.

## Build a plugin

Make the plugin crate a `cdylib`:

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
mercurio-plugin = "0.1"
```

Then build it for WASI preview 2:

```sh
rustup target add wasm32-wasip2
cargo build --release --target wasm32-wasip2
```

The component is `target/wasm32-wasip2/release/<name>.wasm`.

## Example

This plugin refuses publishing under a configured topic prefix and leaves
every other decision to the next plugin:

```rust,ignore
use mercurio_plugin::{export_plugin, host, Access, ClientInfo, Decision, Hooks, Plugin, Registration};

struct DenyTopic;

impl Plugin for DenyTopic {
    fn init(config: Vec<(String, String)>) -> Result<Registration, String> {
        let prefix = config
            .into_iter()
            .find(|(key, _)| key == "prefix")
            .map(|(_, value)| value)
            .ok_or("missing `prefix`")?;
        host::kv_set("prefix", prefix.as_bytes());
        Ok(Registration { hooks: Hooks::AUTHORIZE, auth_methods: Vec::new() })
    }

    fn authorize(_client: ClientInfo, topic: String, access: Access) -> Decision {
        let prefix = host::kv_get("prefix").unwrap_or_default();
        if access == Access::Write && topic.as_bytes().starts_with(&prefix) {
            Decision::Deny
        } else {
            Decision::Defer
        }
    }
}

export_plugin!(DenyTopic);
```

To register more than one hook, combine the flags: `Hooks::AUTHORIZE | Hooks::ON_MESSAGE`.

## Keep no state in the plugin

The broker can run several copies of a plugin at the same time, and it can
replace a copy at any time. Thus a hook must not keep state in statics or
in the plugin type. Keep shared state in the host store: [`host::kv_get`],
[`host::kv_set`], [`host::kv_delete`] and [`host::kv_list`]. The store is
in memory and is lost when the broker stops.

## More

The broker's plugin documentation is at
<https://frenzox.github.io/mercurio/plugins.html> (`docs/plugins.html`).
