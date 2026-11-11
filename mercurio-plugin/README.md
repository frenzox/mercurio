# mercurio-plugin

Write plugins for the [mercurio](https://github.com/frenzox/mercurio) MQTT
broker in Rust. A plugin is a WebAssembly component that `mercuriod` loads
at startup and runs in a sandbox. It can authenticate and authorize clients,
decide who administers the broker, change, drop or publish messages, watch
client events, run on a timer and answer control requests.

This crate generates the bindings for version 0.2 of the broker's WIT
interface (`wit/plugin.wit` in the repository) and wraps them in the
[`Plugin`] trait. The broker also loads plugins built for 0.1.

| `mercurio-plugin`    | Interface |
|----------------------|-----------|
| 0.1.x and 0.2.0      | 0.1       |
| releases after 0.2.0 | 0.2       |

This README describes interface 0.2.
Every hook except `init` has a default, so a plugin implements only the hooks
it uses. `init` returns the hooks that the broker calls.

## Build a plugin

Make the plugin crate a `cdylib`:

```toml
[lib]
crate-type = ["cdylib"]
```

and add the latest release of this crate:

```sh
cargo add mercurio-plugin
```

Until a release after 0.2.0 is on crates.io, take the crate from the
repository instead, as the releases before it speak interface 0.1:

```toml
[dependencies]
mercurio-plugin = { git = "https://github.com/frenzox/mercurio" }
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
use std::sync::OnceLock;

use mercurio_plugin::{
    export_plugin, Access, ClientInfo, Config, Decision, Hooks, Plugin, Registration,
};

/// Set by `init`; settings never change, so each copy keeps its own.
static PREFIX: OnceLock<String> = OnceLock::new();

struct DenyTopic;

impl Plugin for DenyTopic {
    fn init(config: Config) -> Result<Registration, String> {
        let prefix = config.require("prefix")?;
        if prefix.is_empty() {
            return Err("`prefix` is empty, which would deny every publish".into());
        }
        let _ = PREFIX.set(prefix.to_string());
        Ok(Registration {
            hooks: Hooks::AUTHORIZE,
            auth_methods: Vec::new(),
        })
    }

    fn authorize(_client: ClientInfo, topic: String, access: Access) -> Decision {
        let Some(prefix) = PREFIX.get() else {
            return Decision::Defer;
        };
        if access == Access::Write && topic.starts_with(prefix.as_str()) {
            Decision::Deny
        } else {
            Decision::Defer
        }
    }
}

export_plugin!(DenyTopic);
```

To register more than one hook, combine the flags: `Hooks::AUTHORIZE | Hooks::ON_MESSAGE`.

## Settings

`init` gets the plugin's `config` table from the broker configuration as a
[`Config`]. [`Config::get`] returns a setting if it is there, and
[`Config::require`] returns it or an error that names it, ready to return
from `init`.

## Keep no changing state in the plugin

The broker can run several copies of a plugin at the same time, and it can
replace a copy at any time. Thus a hook must not keep state that changes in
statics or in the plugin type. Keep shared state in the host store:
[`host::kv_get`], [`host::kv_set`], [`host::kv_delete`] and
[`host::kv_list`]. The store is in memory and is lost when the broker
stops. `kv_set` can fail, for example when the plugin is over its quota.

Settings are the exception. They are the same for every copy and never
change, and each copy runs `init` before any other hook, so a copy may keep
them in a `std::sync::OnceLock` set in `init`, as the example does. That
saves a call to the store on every hook.

The store is shared but not transactional: a get followed by a set can race
with another copy, so a counter kept as get-then-set can lose updates.
Prefer writing distinct keys and counting them with `kv_list`.

## Publishing

[`host::publish`] sends a [`Message`] as the client `mercurio-plugin-<name>`,
inside the broker, without access control. It returns once the message is
queued, and fails, publishing nothing, for a topic that starts with `$` or
is not a valid topic name, for a message over 64 KiB counting its topic,
payload and properties, and past 100 messages a second, in bursts of 100,
for all copies together. The plugin's own [`Plugin::on_message`] never
sees what it publishes; the other plugins' do.

## Logging

Standard output and error are discarded, so `println!` shows nothing. Log
with [`host::log`]. A panic is logged by the broker at error level, then the
call fails.

## More

The broker's plugin documentation is at
<https://frenzox.github.io/mercurio/plugins.html> (`docs/plugins.html`).
