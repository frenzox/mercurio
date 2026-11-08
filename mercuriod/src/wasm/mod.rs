//! WebAssembly plugins: components implementing `wit/plugin.wit`, run in a
//! sandbox and adapted to the server's plugin chain.

mod convert;
mod pool;
mod runtime;
mod sandbox;
#[cfg(test)]
mod tests;

use std::{
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};

use anyhow::Context;
use bytes::Bytes;
use mercurio_core::reason::ReasonCode;
use mercurio_server::plugin::{
    ready, Access, AuthExchange, AuthResult, BoxFuture, ClientInfo, Credentials, Decision, Event,
    MessageAction, Plugin, PluginMessage,
};
use wasmtime::component::Component;

use pool::Pool;
use runtime::bindings::{
    self,
    mercurio::plugin::types::{self as wit, Hooks},
};
pub use runtime::Runtime;
use sandbox::{Allowed, Kv, State};

/// The export a plugin implements, before its version.
const INTERFACE: &str = "mercurio:plugin/guest@";

/// The interface versions a plugin may be built for: wasmtime matches any
/// patch release of a 0.x minor to the version the bindings name.
const VERSION: &str = "0.1";

/// How to load and confine one plugin.
// Built from the configuration (Task 4)
#[cfg_attr(not(test), allow(dead_code))]
pub struct PluginSettings {
    pub name: String,
    /// The component, a `.wasm` file
    pub path: PathBuf,
    /// Passed to the plugin's `init`
    pub config: Vec<(String, String)>,
    /// Hosts it may send HTTP requests to, `host` (HTTPS) or `host:port`
    pub allow_http: Vec<String>,
    /// Addresses it may open sockets to, `address:port`
    pub allow_sockets: Vec<String>,
    /// How many copies may run at once
    pub instances: usize,
    /// How long one call may run
    pub timeout: Duration,
    /// Bytes of memory each copy may use
    pub memory: usize,
}

/// A WebAssembly plugin in the server's plugin chain.
pub struct WasmPlugin {
    name: String,
    loaded: RwLock<Arc<Loaded>>,
    // Read once reloading replaces `loaded` and hands the new code the
    // same store
    #[allow(dead_code)]
    kv: Arc<Kv>,
}

struct Loaded {
    pool: Pool,
    hooks: Hooks,
    auth_methods: Vec<String>,
}

impl WasmPlugin {
    /// Compiles the plugin, makes its first copy and runs its `init`.
    // Used once the configuration loads plugins (Task 4)
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn load(runtime: &Runtime, settings: PluginSettings) -> anyhow::Result<Arc<Self>> {
        let kv = Arc::new(Kv::default());
        let loaded = Loaded::new(runtime, &settings, Arc::clone(&kv)).await?;
        Ok(Arc::new(WasmPlugin {
            name: settings.name,
            loaded: RwLock::new(Arc::new(loaded)),
            kv,
        }))
    }

    /// The current code, which a reload may replace while a call runs.
    fn loaded(&self) -> Arc<Loaded> {
        Arc::clone(&self.loaded.read().unwrap())
    }

    /// The code, if it implements `hook`.
    fn with(&self, hook: Hooks) -> Option<Arc<Loaded>> {
        let loaded = self.loaded();
        loaded.hooks.contains(hook).then_some(loaded)
    }
}

impl Loaded {
    async fn new(
        runtime: &Runtime,
        settings: &PluginSettings,
        kv: Arc<Kv>,
    ) -> anyhow::Result<Self> {
        let path = settings.path.clone();
        let engine = runtime.engine.clone();
        // Compiling takes tens of milliseconds, too long for a runtime
        // thread of a running broker
        let component = tokio::task::spawn_blocking(move || {
            let bytes =
                std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            Component::from_binary(&engine, &bytes)
                .map_err(anyhow::Error::from)
                .with_context(|| format!("compiling {}", path.display()))
        })
        .await??;
        let path = &settings.path;
        check_version(runtime, &component, path)?;
        let pre = runtime
            .linker
            .instantiate_pre(&component)
            .and_then(bindings::PluginPre::new)
            .map_err(anyhow::Error::from)
            .with_context(|| format!("linking {}", path.display()))?;
        let allowed = Allowed::parse(&settings.allow_http, &settings.allow_sockets)?;
        let name: Arc<str> = settings.name.as_str().into();
        let memory = settings.memory;
        let make = {
            let name = Arc::clone(&name);
            Box::new(move || State::new(Arc::clone(&name), &allowed, memory, Arc::clone(&kv)))
        };
        let pool = Pool::new(
            name,
            pre,
            make,
            settings.config.clone(),
            settings.timeout,
            settings.instances,
        );
        let (instance, registration) = pool.create().await?;
        pool.put(instance);
        Ok(Loaded {
            pool,
            hooks: registration.hooks,
            auth_methods: registration.auth_methods,
        })
    }
}

/// Refuses a component that doesn't export the plugin interface, or
/// exports a version of it this broker can't call.
fn check_version(runtime: &Runtime, component: &Component, path: &Path) -> anyhow::Result<()> {
    let names: Vec<String> = component
        .component_type()
        .exports(&runtime.engine)
        .map(|(name, _)| name.to_string())
        .collect();
    let path = path.display();
    let Some(version) = names.iter().find_map(|name| name.strip_prefix(INTERFACE)) else {
        anyhow::bail!(
            "{path}: not a mercurio plugin, it exports {}",
            if names.is_empty() {
                "nothing".to_string()
            } else {
                names.join(", ")
            }
        );
    };
    anyhow::ensure!(
        compatible(version),
        "{path}: built for {INTERFACE}{version}, this broker supports {VERSION}"
    );
    Ok(())
}

/// Whether wasmtime's export lookup matches `version` to the interface
/// this broker was built with: any patch release of the same 0.x minor,
/// with no pre-release or build suffix.
fn compatible(version: &str) -> bool {
    version
        .strip_prefix(VERSION)
        .and_then(|patch| patch.strip_prefix('.'))
        .is_some_and(|patch| !patch.is_empty() && patch.bytes().all(|b| b.is_ascii_digit()))
}

impl Plugin for WasmPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn authenticate<'a>(
        &'a self,
        client: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        let Some(loaded) = self.with(Hooks::AUTHENTICATE) else {
            return ready(Decision::Defer);
        };
        let client = convert::client(client);
        let (username, password) = (
            credentials.username.to_string(),
            credentials.password.to_vec(),
        );
        Box::pin(async move {
            loaded
                .pool
                .call("authenticate", move |i| {
                    Box::pin(async move {
                        i.guest
                            .call_authenticate(&mut i.store, &client, &username, &password)
                            .await
                    })
                })
                .await
                .map_or(Decision::Deny, convert::decision)
        })
    }

    fn begin_auth(&self, method: &str) -> Option<Box<dyn AuthExchange>> {
        let loaded = self.loaded();
        if !loaded.auth_methods.iter().any(|m| m == method) {
            return None;
        }
        Some(Box::new(WasmAuth {
            loaded,
            method: method.to_string(),
            state: None,
        }))
    }

    fn authorize<'a>(
        &'a self,
        client: &'a ClientInfo,
        topic: &'a str,
        access: Access,
    ) -> BoxFuture<'a, Decision> {
        let Some(loaded) = self.with(Hooks::AUTHORIZE) else {
            return ready(Decision::Defer);
        };
        // An access the plugin interface doesn't know yet
        let Some(access) = convert::access(access) else {
            return ready(Decision::Deny);
        };
        let client = convert::client(client);
        let topic = topic.to_string();
        Box::pin(async move {
            loaded
                .pool
                .call("authorize", move |i| {
                    Box::pin(async move {
                        i.guest
                            .call_authorize(&mut i.store, &client, &topic, access)
                            .await
                    })
                })
                .await
                .map_or(Decision::Deny, convert::decision)
        })
    }

    fn on_message<'a>(
        &'a self,
        client: &'a ClientInfo,
        message: &'a mut PluginMessage,
    ) -> BoxFuture<'a, MessageAction> {
        let Some(loaded) = self.with(Hooks::ON_MESSAGE) else {
            return ready(MessageAction::Continue);
        };
        let client = convert::client(client);
        let converted = convert::message(message);
        Box::pin(async move {
            let action = loaded
                .pool
                .call("on_message", move |i| {
                    Box::pin(async move {
                        i.guest
                            .call_on_message(&mut i.store, &client, &converted)
                            .await
                    })
                })
                .await;
            match action {
                Some(wit::MessageAction::Keep) => MessageAction::Continue,
                Some(wit::MessageAction::Replace(changed)) => {
                    *message = convert::plugin_message(changed);
                    MessageAction::Continue
                }
                Some(wit::MessageAction::Drop) | None => MessageAction::Drop,
            }
        })
    }

    fn on_event<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, ()> {
        let Some(loaded) = self.with(Hooks::ON_EVENT) else {
            return ready(());
        };
        let Some(event) = convert::event(event) else {
            return ready(());
        };
        Box::pin(async move {
            loaded
                .pool
                .call("on_event", move |i| {
                    Box::pin(async move { i.guest.call_on_event(&mut i.store, &event).await })
                })
                .await;
        })
    }

    fn on_tick(&self) -> BoxFuture<'_, ()> {
        let Some(loaded) = self.with(Hooks::ON_TICK) else {
            return ready(());
        };
        Box::pin(async move {
            loaded
                .pool
                .call("on_tick", |i| {
                    Box::pin(async move { i.guest.call_on_tick(&mut i.store).await })
                })
                .await;
        })
    }

    fn on_control<'a>(
        &'a self,
        client: &'a ClientInfo,
        path: &'a str,
        payload: Bytes,
    ) -> BoxFuture<'a, Option<Bytes>> {
        let Some(loaded) = self.with(Hooks::ON_CONTROL) else {
            return ready(None);
        };
        let client = convert::client(client);
        let path = path.to_string();
        Box::pin(async move {
            loaded
                .pool
                .call("on_control", move |i| {
                    Box::pin(async move {
                        i.guest
                            .call_on_control(&mut i.store, &client, &path, &payload)
                            .await
                    })
                })
                .await
                .flatten()
                .map(Bytes::from)
        })
    }
}

/// One client's enhanced authentication with a plugin.
struct WasmAuth {
    loaded: Arc<Loaded>,
    method: String,
    /// What the plugin asked to keep after the last step
    state: Option<Vec<u8>>,
}

impl AuthExchange for WasmAuth {
    fn step<'a>(&'a mut self, data: Option<&'a Bytes>) -> BoxFuture<'a, AuthResult> {
        // ponytail: `Plugin::begin_auth` doesn't say which client is
        // authenticating, so the plugin sees an empty client
        let client = wit::ClientInfo {
            client_id: String::new(),
            username: None,
            peer: None,
        };
        let state = self.state.take();
        let data = data.map(|data| data.to_vec());
        let method = self.method.clone();
        Box::pin(async move {
            let result = self
                .loaded
                .pool
                .call("enhanced authentication", move |i| {
                    Box::pin(async move {
                        i.guest
                            .call_auth_step(
                                &mut i.store,
                                &client,
                                &method,
                                state.as_deref(),
                                data.as_deref(),
                            )
                            .await
                    })
                })
                .await;
            let Some(result) = result else {
                return AuthResult::Failed(ReasonCode::NotAuthorized);
            };
            let (result, state) = convert::auth_result(result);
            self.state = state;
            result
        })
    }
}

#[cfg(test)]
mod version_tests {
    use super::compatible;

    #[test]
    fn test_patch_releases_of_the_minor_are_compatible() {
        assert!(compatible("0.1.0"));
        assert!(compatible("0.1.7"));
        assert!(!compatible("0.2.0"));
        assert!(!compatible("0.10.0"));
        assert!(!compatible("0.1.0-rc1"));
        assert!(!compatible("0.1"));
    }
}
