//! The wasmtime engine and linker shared by every plugin.

use std::time::Duration;

use wasmtime::{
    component::{HasSelf, Linker},
    Config, Engine,
};

use super::sandbox::State;

/// The current interface, 0.2.
pub(crate) mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "plugin",
        imports: { default: async },
        exports: { default: async },
    });
}

/// Interface 0.1, for plugins built before 0.2.
pub(crate) mod bindings_0_1 {
    wasmtime::component::bindgen!({
        path: "wit-0.1",
        world: "plugin",
        imports: { default: async },
        exports: { default: async },
    });
}

/// How often the engine's epoch advances; plugin time limits count in these.
pub(crate) const TICK: Duration = Duration::from_millis(10);

/// The engine plugins run on and the host functions they may import. One
/// is enough for the whole process.
#[derive(Clone)]
pub struct Runtime {
    pub(crate) engine: Engine,
    pub(crate) linker: Linker<State>,
}

impl Runtime {
    pub fn new() -> anyhow::Result<Runtime> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config)?;
        // The ticker stops once every clone of the engine is gone
        let ticker = engine.weak();
        std::thread::Builder::new()
            .name("wasm-epoch".into())
            .spawn(move || {
                while let Some(engine) = ticker.upgrade() {
                    engine.increment_epoch();
                    drop(engine);
                    std::thread::sleep(TICK);
                }
            })?;
        let mut linker = Linker::<State>::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
        wasmtime_wasi_http::p2::add_only_http_to_linker_async(&mut linker)?;
        bindings::Plugin::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)?;
        bindings_0_1::Plugin::add_to_linker::<State, HasSelf<State>>(&mut linker, |state| state)?;
        Ok(Runtime { engine, linker })
    }
}
