//! Administering a running broker: [`Admin`], from Rust, and the
//! `$CONTROL/broker/v1` MQTT API served on top of it.

mod types;

pub use types::{BrokerInfo, ListenerInfo};

use std::{
    sync::{atomic::Ordering, Arc},
    time::Instant,
};

use mercurio_core::message::unix_millis;
use mercurio_storage::{BatchStore, RetainedMessageStore};

use crate::{
    plugin::BoxFuture,
    queue::Subscriber,
    server::ListenerState,
    session_manager::SessionManager,
    sys::{self, Stats},
    topic_tree::TopicTree,
};

/// Administers a running broker. Get one from
/// [`Server::admin`](crate::server::Server::admin); clones share the broker.
#[derive(Clone)]
pub struct Admin {
    inner: Arc<Inner>,
}

struct Inner {
    directory: Arc<dyn Directory>,
    retained: Arc<dyn RetainedMessageStore>,
    topics: TopicTree<Subscriber>,
    stats: Arc<Stats>,
    listeners: Vec<Arc<ListenerState>>,
    storage: &'static str,
    /// Unix seconds
    started_at: u64,
    started: Instant,
}

/// What `Admin` is built from, by the server.
pub(crate) struct AdminParts {
    pub directory: Arc<dyn Directory>,
    pub retained: Arc<dyn RetainedMessageStore>,
    pub topics: TopicTree<Subscriber>,
    pub stats: Arc<Stats>,
    pub listeners: Vec<Arc<ListenerState>>,
    pub storage: &'static str,
}

/// The sessions as the admin API reaches them, whatever the storage the
/// session manager is generic over.
pub(crate) trait Directory: Send + Sync {
    fn session_count(&self) -> BoxFuture<'_, usize>;
    fn expired_count(&self) -> u64;
}

impl<S: BatchStore + 'static> Directory for SessionManager<S> {
    fn session_count(&self) -> BoxFuture<'_, usize> {
        Box::pin(SessionManager::session_count(self))
    }

    fn expired_count(&self) -> u64 {
        SessionManager::expired_count(self)
    }
}

impl Admin {
    pub(crate) fn new(parts: AdminParts) -> Admin {
        Admin {
            inner: Arc::new(Inner {
                directory: parts.directory,
                retained: parts.retained,
                topics: parts.topics,
                stats: parts.stats,
                listeners: parts.listeners,
                storage: parts.storage,
                started_at: unix_millis() / 1000,
                started: Instant::now(),
            }),
        }
    }

    pub(crate) fn started(&self) -> Instant {
        self.inner.started
    }

    /// The broker's version, uptime, storage and `$SYS` counters:
    /// `getBrokerInfo`.
    pub async fn info(&self) -> BrokerInfo {
        let inner = &self.inner;
        let retained = inner.retained.count_retained().await.unwrap_or(0);
        let counters = sys::counters(
            &inner.stats,
            inner.directory.session_count().await,
            inner.directory.expired_count(),
            retained,
            inner.topics.subscription_count(),
        );
        BrokerInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            started_at: inner.started_at,
            uptime: inner.started.elapsed().as_secs(),
            storage: inner.storage.to_string(),
            stats: counters
                .into_iter()
                .map(|(name, value)| (name.to_string(), value))
                .collect(),
        }
    }

    /// The listeners, the main one first: `listListeners`.
    pub async fn listeners(&self) -> Vec<ListenerInfo> {
        self.inner
            .listeners
            .iter()
            .map(|listener| ListenerInfo {
                connections: listener.connections.load(Ordering::Relaxed),
                ..listener.info.clone()
            })
            .collect()
    }
}
