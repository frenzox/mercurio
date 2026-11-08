//! Copies of one plugin, each serving one call at a time. A copy that
//! trapped, or whose call was cancelled by a timeout, can't be entered
//! again, so it is dropped and a fresh one made when needed.

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use mercurio_server::plugin::BoxFuture;
use tokio::sync::Semaphore;
use wasmtime::{Store, UpdateDeadline};

use super::{
    runtime::{
        bindings::{
            self, exports::mercurio::plugin::guest::Guest, mercurio::plugin::types::Registration,
        },
        TICK,
    },
    sandbox::State,
};

pub(crate) struct Instance {
    pub(crate) store: Store<State>,
    pub(crate) guest: Guest,
}

pub(crate) struct Pool {
    name: Arc<str>,
    pre: bindings::PluginPre<State>,
    make: Box<dyn Fn() -> State + Send + Sync>,
    config: Vec<(String, String)>,
    timeout: Duration,
    idle: Mutex<Vec<Instance>>,
    permits: Semaphore,
}

impl Pool {
    pub(crate) fn new(
        name: Arc<str>,
        pre: bindings::PluginPre<State>,
        make: Box<dyn Fn() -> State + Send + Sync>,
        config: Vec<(String, String)>,
        timeout: Duration,
        instances: usize,
    ) -> Pool {
        Pool {
            name,
            pre,
            make,
            config,
            timeout,
            idle: Mutex::default(),
            permits: Semaphore::new(instances.max(1)),
        }
    }

    /// A fresh copy, initialised; its registration is returned too.
    pub(crate) async fn create(&self) -> anyhow::Result<(Instance, Registration)> {
        let mut store = Store::new(self.pre.engine(), (self.make)());
        store.limiter(|state| &mut state.limits);
        // Every tick yields to tokio, so a busy plugin never holds a worker
        // thread for more than one tick; once the call's budget is spent,
        // it traps
        store.epoch_deadline_callback(|mut store| {
            let state = store.data_mut();
            if state.ticks_left == 0 {
                return Ok(UpdateDeadline::Interrupt);
            }
            state.ticks_left -= 1;
            Ok(UpdateDeadline::Yield(1))
        });
        self.arm(&mut store);
        let plugin = self.pre.instantiate_async(&mut store).await?;
        let mut instance = Instance {
            store,
            guest: plugin.mercurio_plugin_guest().clone(),
        };
        let registration = instance
            .guest
            .call_init(&mut instance.store, &self.config)
            .await?
            .map_err(|message| anyhow::anyhow!("plugin `{}`: {message}", self.name))?;
        Ok((instance, registration))
    }

    /// Puts a copy made by [`Pool::create`] outside a call back in the pool.
    pub(crate) fn put(&self, instance: Instance) {
        self.idle.lock().unwrap().push(instance);
    }

    /// Runs `call` on an idle copy, or a new one. The copy returns to the
    /// pool only if the call finished without a trap; None, logged, if it
    /// didn't.
    pub(crate) async fn call<T>(
        &self,
        hook: &str,
        call: impl FnOnce(&mut Instance) -> BoxFuture<'_, wasmtime::Result<T>>,
    ) -> Option<T> {
        let _permit = self.permits.acquire().await.ok()?;
        let idle = self.idle.lock().unwrap().pop();
        let mut instance = match idle {
            Some(instance) => instance,
            None => match self.create().await {
                Ok((instance, _)) => instance,
                Err(error) => {
                    tracing::warn!(plugin = %self.name, "Couldn't start a copy for {hook}: {error:#}");
                    return None;
                }
            },
        };
        self.arm(&mut instance.store);
        // When this future is dropped mid-call the copy goes with it
        match call(&mut instance).await {
            Ok(value) => {
                self.put(instance);
                Some(value)
            }
            Err(error) => {
                tracing::warn!(plugin = %self.name, "Failed in {hook}: {error:#}");
                None
            }
        }
    }

    fn arm(&self, store: &mut Store<State>) {
        store.data_mut().ticks_left = (self.timeout.as_millis() / TICK.as_millis()) as u64;
        store.set_epoch_deadline(1);
    }
}
