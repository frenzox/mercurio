//! Administering a running broker: [`Admin`], from Rust, and the
//! `$CONTROL/broker/v1` MQTT API served on top of it.
//!
//! `getSubscribers`, the subscriptions of `getClient` and the responses to
//! a batch of commands aren't paged: a response larger than the client's
//! Maximum Packet Size is dropped by the broker.

mod protocol;
mod types;

pub(crate) use protocol::handle as handle_request;
pub use types::{BrokerInfo, ListenerInfo};

use std::{
    sync::{atomic::Ordering, Arc},
    time::Instant,
};

use mercurio_core::message::unix_millis;
use mercurio_storage::{BatchStore, RetainedMessageStore};
use serde::Serialize;
use serde_json::Value;

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
    extension: Option<Arc<dyn AdminExtension>>,
}

/// Why a command failed: a stable [`ErrorCode`] for scripts, and a message
/// for people.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
#[non_exhaustive]
pub struct AdminError {
    pub code: ErrorCode,
    pub message: String,
}

impl AdminError {
    /// An error with `code`, explained by `message`.
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        AdminError {
            code,
            message: message.into(),
        }
    }

    /// The request is wrong: a missing or malformed argument.
    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArgument, message)
    }

    /// What the command names doesn't exist.
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(ErrorCode::NotFound, message)
    }
}

/// The `code` of a failed command's response.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum ErrorCode {
    /// The client isn't allowed to run the command
    NotAuthorized,
    /// What the command names, such as a client or a retained message,
    /// doesn't exist
    NotFound,
    /// The command or its arguments are malformed
    InvalidArgument,
    /// The command can't run in the current state, such as deleting the
    /// session of a connected client without `disconnect`
    Conflict,
    /// The broker can't serve the command now: storage failed, a wait timed
    /// out, or the command is served by something not running
    Unavailable,
}

/// Most items a list command returns at once.
pub const MAX_PAGE: usize = 1000;

/// Which part of a list to return.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Paging {
    /// How many items to skip
    pub offset: usize,
    /// How many items to return at most
    pub count: usize,
}

impl Default for Paging {
    fn default() -> Self {
        Paging {
            offset: 0,
            count: 100,
        }
    }
}

impl Paging {
    /// Fails with `invalidArgument` for more than [`MAX_PAGE`] items.
    pub fn new(offset: usize, count: usize) -> Result<Self, AdminError> {
        if count > MAX_PAGE {
            return Err(AdminError::invalid_argument(format!(
                "`count` is at most {MAX_PAGE}"
            )));
        }
        Ok(Paging { offset, count })
    }
}

/// Part of a list, and how long the whole list is.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Page<T> {
    /// The items `Paging` asked for
    pub items: Vec<T>,
    /// How many items the whole list has
    pub total_count: usize,
}

impl<T> Page<T> {
    pub(crate) fn of(all: Vec<T>, paging: Paging) -> Page<T> {
        Page {
            total_count: all.len(),
            items: all
                .into_iter()
                .skip(paging.offset)
                .take(paging.count)
                .collect(),
        }
    }
}

/// Serves the `$CONTROL/broker/v1` commands the broker doesn't know, such
/// as the ones `mercuriod` adds. Set in `ServerConfig::admin_extension`.
pub trait AdminExtension: Send + Sync + 'static {
    /// Runs `command`; `args` is the whole command object. `None` means the
    /// command is unknown. Returned data other than `null` becomes the
    /// response's `data`. The client's permission for the command was
    /// checked before, on its exact name: match `command` exactly, as
    /// matching it ignoring case would run commands the ACL denies.
    fn handle<'a>(
        &'a self,
        command: &'a str,
        args: &'a Value,
    ) -> BoxFuture<'a, Option<Result<Value, AdminError>>>;
}

/// What `Admin` is built from, by the server.
pub(crate) struct AdminParts {
    pub directory: Arc<dyn Directory>,
    pub retained: Arc<dyn RetainedMessageStore>,
    pub topics: TopicTree<Subscriber>,
    pub stats: Arc<Stats>,
    pub listeners: Vec<Arc<ListenerState>>,
    pub storage: &'static str,
    pub extension: Option<Arc<dyn AdminExtension>>,
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
                extension: parts.extension,
            }),
        }
    }

    pub(crate) fn extension(&self) -> Option<&Arc<dyn AdminExtension>> {
        self.inner.extension.as_ref()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pages_cut_the_list_and_count_all_of_it() {
        let page = Page::of((0..10).collect(), Paging::new(8, 5).unwrap());
        assert_eq!(page.items, vec![8, 9]);
        assert_eq!(page.total_count, 10);
        assert_eq!(
            Paging::new(0, MAX_PAGE + 1).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
}
