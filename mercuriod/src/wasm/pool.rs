//! Copies of one plugin, each serving one call at a time. A copy that
//! trapped, or whose call was cancelled by a timeout, can't be entered
//! again, so it is dropped and a fresh one made when needed.

use std::{
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
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
    _live: Live,
}

/// Counts a copy among the live ones for as long as it exists, wherever
/// it is dropped: back in the pool, after a trap, or with a cancelled call.
struct Live(Arc<AtomicUsize>);

impl Live {
    fn new(count: &Arc<AtomicUsize>) -> Live {
        count.fetch_add(1, Ordering::Relaxed);
        Live(Arc::clone(count))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The calls of a plugin that failed since the broker started; shared by
/// the code a reload replaces and the new code.
#[derive(Default)]
pub(crate) struct Health {
    failures: AtomicU64,
    last_error: Mutex<Option<String>>,
}

impl Health {
    fn failed(&self, error: String) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        *self.last_error.lock().unwrap() = Some(error);
    }

    pub(crate) fn failures(&self) -> u64 {
        self.failures.load(Ordering::Relaxed)
    }

    pub(crate) fn last_error(&self) -> Option<String> {
        self.last_error.lock().unwrap().clone()
    }
}

pub(crate) struct Pool {
    name: Arc<str>,
    pre: bindings::PluginPre<State>,
    make: Box<dyn Fn() -> State + Send + Sync>,
    config: Vec<(String, String)>,
    timeout: Duration,
    idle: Mutex<Vec<Instance>>,
    permits: Semaphore,
    live: Arc<AtomicUsize>,
    health: Arc<Health>,
}

impl Pool {
    pub(crate) fn new(
        name: Arc<str>,
        pre: bindings::PluginPre<State>,
        make: Box<dyn Fn() -> State + Send + Sync>,
        config: Vec<(String, String)>,
        timeout: Duration,
        instances: usize,
        health: Arc<Health>,
    ) -> Pool {
        Pool {
            name,
            pre,
            make,
            config,
            timeout,
            idle: Mutex::default(),
            permits: Semaphore::new(instances.max(1)),
            live: Arc::default(),
            health,
        }
    }

    /// Copies alive now, idle or in a call.
    pub(crate) fn live(&self) -> usize {
        self.live.load(Ordering::Relaxed)
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
        // Epochs count only time spent running; a plugin waiting in a host
        // call, such as an HTTP request, needs a clock too
        let init = async {
            let plugin = self.pre.instantiate_async(&mut store).await?;
            let guest = plugin.mercurio_plugin_guest().clone();
            let registration = guest.call_init(&mut store, &self.config).await?;
            Ok::<_, wasmtime::Error>((guest, registration))
        };
        let (guest, registration) =
            tokio::time::timeout(self.timeout, init)
                .await
                .map_err(|_| {
                    anyhow::anyhow!(
                        "plugin `{}`: init took longer than {:?}",
                        self.name,
                        self.timeout
                    )
                })??;
        let registration =
            registration.map_err(|message| anyhow::anyhow!("plugin `{}`: {message}", self.name))?;
        let instance = Instance {
            store,
            guest,
            _live: Live::new(&self.live),
        };
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
                    self.failed(format!("Couldn't start a copy for {hook}: {error:#}"));
                    return None;
                }
            },
        };
        self.arm(&mut instance.store);
        // When this future is dropped mid-call the copy goes with it
        let cancelled = Cancelled { pool: self, hook };
        let result = tokio::time::timeout(self.timeout, call(&mut instance)).await;
        std::mem::forget(cancelled);
        match result {
            Ok(Ok(value)) => {
                self.put(instance);
                Some(value)
            }
            Err(_) => {
                self.failed(format!("Took longer than {:?} in {hook}", self.timeout));
                None
            }
            Ok(Err(error)) => {
                self.failed(format!("Failed in {hook}: {error:#}"));
                None
            }
        }
    }

    fn failed(&self, error: String) {
        tracing::warn!(plugin = %self.name, "{error}");
        self.health.failed(error);
    }

    fn arm(&self, store: &mut Store<State>) {
        store.data_mut().ticks_left = (self.timeout.as_millis() / TICK.as_millis()) as u64;
        store.set_epoch_deadline(1);
    }
}

/// Counts a call as failed when it is dropped before it finishes, as the
/// broker drops a hook call that outlives its `plugin_timeout`; forgotten
/// once the call returns.
struct Cancelled<'a> {
    pool: &'a Pool,
    hook: &'a str,
}

impl Drop for Cancelled<'_> {
    fn drop(&mut self) {
        self.pool.failed(format!(
            "Cancelled in {} before it finished, as when the broker's plugin_timeout runs out",
            self.hook
        ));
    }
}
