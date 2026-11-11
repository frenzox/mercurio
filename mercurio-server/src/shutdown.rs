use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};

use tokio::{
    runtime::Handle,
    sync::{broadcast, watch},
    task::AbortHandle,
};

/// The tasks one server runs, so stopping it ends them all: none outlives
/// `Server::run`, and with them go the handles they hold on its storage.
///
/// Its lock is never held while a task's future is dropped, which can run
/// code that spawns another, as dropping the last handle on a session does.
#[derive(Clone)]
pub(crate) struct Tasks(Arc<TaskSet>);

struct TaskSet {
    state: Mutex<State>,
    /// Tasks spawned, or about to be, that have yet to end
    live: watch::Sender<usize>,
}

struct State {
    /// The tasks running, by id; None once stopped
    running: Option<HashMap<u64, AbortHandle>>,
    next_id: u64,
}

impl Default for Tasks {
    fn default() -> Self {
        Tasks(Arc::new(TaskSet {
            state: Mutex::new(State {
                running: Some(HashMap::new()),
                next_id: 0,
            }),
            live: watch::Sender::new(0),
        }))
    }
}

/// Counts a task as ended once its future is dropped, run or not.
struct Ended {
    tasks: Tasks,
    id: u64,
}

impl Drop for Ended {
    fn drop(&mut self) {
        if let Some(running) = &mut self.tasks.0.state.lock().unwrap().running {
            running.remove(&self.id);
        }
        self.tasks.0.live.send_modify(|live| *live -= 1);
    }
}

impl Tasks {
    /// Runs `task` until it ends or the server stops. Once the server has
    /// stopped, `task` is dropped without running, and None returned.
    pub(crate) fn spawn<F>(&self, task: F) -> Option<AbortHandle>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.spawn_on(task, &Handle::current())
    }

    /// Like `spawn`, on `runtime`, for code without a runtime context
    /// such as `Drop`.
    pub(crate) fn spawn_on<F>(&self, task: F, runtime: &Handle) -> Option<AbortHandle>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let id = {
            let mut state = self.0.state.lock().unwrap();
            if state.running.is_none() {
                None
            } else {
                // Counted before it runs, so `stop` waits for it
                self.0.live.send_modify(|live| *live += 1);
                state.next_id += 1;
                Some(state.next_id)
            }
        }?;
        let ended = Ended {
            tasks: self.clone(),
            id,
        };
        let task = runtime
            .spawn(async move {
                let _ended = ended;
                task.await
            })
            .abort_handle();
        let mut state = self.0.state.lock().unwrap();
        match &mut state.running {
            // A task that ended already removed nothing; such leftovers are
            // swept now and then
            Some(running) if !task.is_finished() => {
                if id.is_multiple_of(1024) {
                    running.retain(|_, task| !task.is_finished());
                }
                running.insert(id, task.clone());
            }
            Some(_) => {}
            None => task.abort(),
        }
        Some(task)
    }

    /// Aborts every task, refuses new ones from then on, and waits until
    /// they have all ended.
    pub(crate) async fn stop(&self) {
        let running = self.0.state.lock().unwrap().running.take();
        for task in running.iter().flat_map(|running| running.values()) {
            task.abort();
        }
        let mut live = self.0.live.subscribe();
        let _ = live.wait_for(|live| *live == 0).await;
    }
}

/// Listens for the server shutdown signal.
///
/// Shutdown is signalled using a `broadcast::Receiver`. Only a single value is
/// ever sent. Once a value has been sent via the broadcast channel, the server
/// should shutdown.
///
/// The `Shutdown` struct listens for the signal and tracks that the signal has
/// been received. Callers may query for whether the shutdown signal has been
/// received or not.
#[derive(Debug)]
pub(crate) struct Shutdown {
    /// `true` if the shutdown signal has been received
    shutdown: bool,

    /// The receive half of the channel used to listen for shutdown.
    notify: broadcast::Receiver<()>,
}

impl Shutdown {
    /// Create a new `Shutdown` backed by the given `broadcast::Receiver`.
    pub(crate) fn new(notify: broadcast::Receiver<()>) -> Shutdown {
        Shutdown {
            shutdown: false,
            notify,
        }
    }

    /// Returns `true` if the shutdown signal has been received.
    pub(crate) fn is_shutdown(&self) -> bool {
        self.shutdown
    }

    /// Whether the shutdown signal has been sent, without waiting for it.
    pub(crate) fn is_signalled(&mut self) -> bool {
        if !self.shutdown {
            self.shutdown = !matches!(
                self.notify.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            );
        }
        self.shutdown
    }

    /// Receive the shutdown notice, waiting if necessary.
    pub(crate) async fn recv(&mut self) {
        // If the shutdown signal has already been received, then return
        // immediately.
        if self.shutdown {
            return;
        }

        // Cannot receive a "lag error" as only one value is ever sent.
        let _ = self.notify.recv().await;

        // Remember that the signal has been received.
        self.shutdown = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signal_is_seen_without_waiting() {
        let (notify, receiver) = broadcast::channel(1);
        let mut shutdown = Shutdown::new(receiver);
        assert!(!shutdown.is_signalled());
        notify.send(()).unwrap();
        assert!(shutdown.is_signalled());
        assert!(shutdown.is_shutdown());
    }

    /// Spawns another task when dropped, as dropping a session can.
    struct SpawnsOnDrop(Tasks, Arc<()>);

    impl Drop for SpawnsOnDrop {
        fn drop(&mut self) {
            let held = Arc::clone(&self.1);
            self.0.spawn(async move { drop(held) });
        }
    }

    #[tokio::test]
    async fn test_stopping_ends_every_task_and_refuses_new_ones() {
        let tasks = Tasks::default();
        let held = Arc::new(());
        let spawns = SpawnsOnDrop(tasks.clone(), Arc::clone(&held));
        tasks.spawn(async move {
            let _spawns = spawns;
            std::future::pending::<()>().await
        });
        let finished = Arc::clone(&held);
        tasks.spawn(async move { drop(finished) });
        tokio::task::yield_now().await;

        tokio::time::timeout(std::time::Duration::from_secs(5), tasks.stop())
            .await
            .expect("stopping waited for a task that never ends");
        assert_eq!(Arc::strong_count(&held), 1);
        assert!(tasks.spawn(async {}).is_none());
    }
}
