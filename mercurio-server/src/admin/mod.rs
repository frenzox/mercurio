//! Administering a running broker: [`Admin`], from Rust, and the
//! `$CONTROL/broker/v1` MQTT API served on top of it.
//!
//! `getSubscribers`, the subscriptions of `getClient` and the responses to
//! a batch of commands aren't paged: a response larger than the client's
//! Maximum Packet Size is dropped by the broker.

mod protocol;
mod types;

pub(crate) use protocol::handle as handle_request;
pub use types::{
    Ban, BanKind, BrokerInfo, ClientDetails, ClientSubscription, ClientSummary, ListenerInfo,
    RetainedMessage, RetainedProperties, RetainedSummary, SubscriberInfo, SubscriptionSummary,
};

use std::{
    collections::HashMap,
    sync::{atomic::Ordering, Arc},
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use mercurio_core::{
    message::unix_millis,
    reason::ReasonCode,
    topic::{parse_shared_subscription, validate_publish_topic, validate_subscribe_filter},
};
use mercurio_storage::{BatchStore, RetainedMessageStore, StorageError};
use serde::Serialize;
use serde_json::Value;
use tracing::info;

use crate::{
    bans::{Bans, Candidate},
    plugin::{BoxFuture, Plugin, PluginStatus},
    queue::Subscriber,
    server::{AddressRange, ListenerState},
    session::SessionSnapshot,
    session_manager::{ClientEntry, Close, DeleteRefused, SessionManager},
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
    plugins: Vec<Arc<dyn Plugin>>,
    bans: Arc<Bans>,
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

/// How long [`Admin::delete_session`] waits for a client to close.
const DELETE_TIMEOUT: Duration = Duration::from_secs(5);

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

/// Which clients `listClients` returns.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct ClientQuery {
    pub connected: Option<bool>,
    pub client_id_prefix: Option<String>,
    /// The user the session belongs to, exactly
    pub username: Option<String>,
    pub address: Option<AddressRange>,
    pub paging: Paging,
}

/// Which ban: `removeBan`'s arguments, and what `addBan` adds or replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct BanKey {
    pub kind: BanKind,
    /// A client id or user name, or an IP address or CIDR block
    pub value: String,
    /// Match client ids or user names starting with `value`; not for
    /// addresses
    pub prefix: bool,
}

impl BanKey {
    /// A ban on exactly `value`.
    pub fn new(kind: BanKind, value: impl Into<String>) -> Self {
        BanKey {
            kind,
            value: value.into(),
            prefix: false,
        }
    }
}

/// A ban to add: `addBan`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct NewBan {
    pub key: BanKey,
    /// Seconds until the ban expires, counted from now; it never does when
    /// None
    pub duration: Option<u32>,
    pub reason: Option<String>,
}

impl NewBan {
    /// A ban that never expires, without a reason.
    pub fn new(key: BanKey) -> Self {
        NewBan {
            key,
            duration: None,
            reason: None,
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
    pub plugins: Vec<Arc<dyn Plugin>>,
    pub bans: Arc<Bans>,
}

/// The sessions as the admin API reaches them, whatever the storage the
/// session manager is generic over.
pub(crate) trait Directory: Send + Sync {
    fn session_count(&self) -> BoxFuture<'_, usize>;
    fn expired_count(&self) -> u64;
    fn storage_failures(&self) -> (u64, usize);
    fn entries(&self) -> BoxFuture<'_, Vec<ClientEntry>>;
    fn entry<'a>(&'a self, client_id: &'a str) -> BoxFuture<'a, Option<ClientEntry>>;
    fn close_connection<'a>(&'a self, client_id: &'a str, close: Close) -> BoxFuture<'a, bool>;
    fn close_generation<'a>(
        &'a self,
        client_id: &'a str,
        generation: u64,
        close: Close,
    ) -> BoxFuture<'a, bool>;
    fn delete_session<'a>(
        &'a self,
        client_id: &'a str,
        disconnect: bool,
    ) -> BoxFuture<'a, Result<(), DeleteRefused>>;
}

impl<S: BatchStore + 'static> Directory for SessionManager<S> {
    fn session_count(&self) -> BoxFuture<'_, usize> {
        Box::pin(SessionManager::session_count(self))
    }

    fn expired_count(&self) -> u64 {
        SessionManager::expired_count(self)
    }

    fn storage_failures(&self) -> (u64, usize) {
        SessionManager::storage_failures(self)
    }

    fn entries(&self) -> BoxFuture<'_, Vec<ClientEntry>> {
        Box::pin(SessionManager::entries(self))
    }

    fn entry<'a>(&'a self, client_id: &'a str) -> BoxFuture<'a, Option<ClientEntry>> {
        Box::pin(SessionManager::entry(self, client_id))
    }

    fn close_connection<'a>(&'a self, client_id: &'a str, close: Close) -> BoxFuture<'a, bool> {
        Box::pin(SessionManager::close_connection(self, client_id, close))
    }

    fn close_generation<'a>(
        &'a self,
        client_id: &'a str,
        generation: u64,
        close: Close,
    ) -> BoxFuture<'a, bool> {
        Box::pin(SessionManager::close_generation(
            self, client_id, generation, close,
        ))
    }

    fn delete_session<'a>(
        &'a self,
        client_id: &'a str,
        disconnect: bool,
    ) -> BoxFuture<'a, Result<(), DeleteRefused>> {
        Box::pin(SessionManager::delete_session(self, client_id, disconnect))
    }
}

/// The reason codes of 0x80 and above a DISCONNECT may carry, MQTT 5
/// section 3.14.2.1.
const DISCONNECT_ERRORS: [u8; 27] = [
    0x80, 0x81, 0x82, 0x83, 0x87, 0x89, 0x8B, 0x8D, 0x8E, 0x8F, 0x90, 0x93, 0x94, 0x95, 0x96, 0x97,
    0x98, 0x99, 0x9A, 0x9B, 0x9C, 0x9D, 0x9E, 0x9F, 0xA0, 0xA1, 0xA2,
];

/// How `disconnectClient` closes a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct DisconnectOptions {
    /// Publish the client's will, as for any connection lost
    pub send_will: bool,
    /// Sent to MQTT 5 clients in the DISCONNECT; one of its error reason
    /// codes, 0x80 or above
    pub reason: ReasonCode,
}

impl Default for DisconnectOptions {
    fn default() -> Self {
        DisconnectOptions {
            send_will: true,
            reason: ReasonCode::AdministrativeAction,
        }
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
                plugins: parts.plugins,
                bans: parts.bans,
            }),
        }
    }

    /// Every plugin, each once: the main chain's in the order they run, then
    /// those only a listener runs: `listPlugins`. A listener's own plugins
    /// replace the main chain for its clients.
    pub async fn plugins(&self) -> Vec<PluginStatus> {
        self.inner
            .plugins
            .iter()
            .map(|plugin| plugin.status())
            .collect()
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
            inner.directory.storage_failures(),
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

    /// Every session, connected or not, sorted by client id: `listClients`.
    pub async fn clients(&self, query: &ClientQuery) -> Page<ClientSummary> {
        let mut entries = self.inner.directory.entries().await;
        entries.sort_by(|a, b| a.client_id.cmp(&b.client_id));
        let mut found = Vec::new();
        for entry in entries {
            if query.connected.is_some_and(|c| c != entry.connected)
                || query
                    .client_id_prefix
                    .as_deref()
                    .is_some_and(|prefix| !entry.client_id.starts_with(prefix))
            {
                continue;
            }
            let snapshot = entry.session.snapshot().await;
            if query
                .username
                .as_ref()
                .is_some_and(|user| snapshot.username.as_ref() != Some(user))
            {
                continue;
            }
            if let Some(range) = &query.address {
                if !snapshot.peer.is_some_and(|peer| range.contains(peer.ip())) {
                    continue;
                }
            }
            found.push(summary(&entry, &snapshot));
        }
        Page::of(found, query.paging)
    }

    /// Closes the connection of `client_id`; its session stays:
    /// `disconnectClient`.
    pub async fn disconnect(
        &self,
        client_id: &str,
        options: DisconnectOptions,
    ) -> Result<(), AdminError> {
        if !DISCONNECT_ERRORS.contains(&options.reason.get_code()) {
            return Err(AdminError::invalid_argument(format!(
                "0x{:02x} is not an error reason code a DISCONNECT may carry",
                options.reason.get_code()
            )));
        }
        let close = Close::Admin {
            reason: options.reason,
            send_will: options.send_will,
        };
        if !self
            .inner
            .directory
            .close_connection(client_id, close)
            .await
        {
            return Err(AdminError::not_found(format!(
                "client `{client_id}` isn't connected"
            )));
        }
        info!("Disconnecting client `{}` for the admin API", client_id);
        Ok(())
    }

    /// Removes the session of `client_id`, its subscriptions, queue and
    /// will: `deleteSession`. A connected client is closed first with
    /// `disconnect`, and is a `conflict` otherwise.
    ///
    /// Closing a client waits for its connection to finish, for 5 seconds
    /// at most before failing with `unavailable`; the session is still
    /// deleted once the connection has closed. Code running in a client's
    /// own connection, such as a plugin hook, must not delete that client's
    /// session: the connection can't close while it waits.
    pub async fn delete_session(
        &self,
        client_id: &str,
        disconnect: bool,
    ) -> Result<(), AdminError> {
        // The caller is often a connection's task, which can't close while
        // it waits here, so the wait gets a bound. The deletion runs on in
        // its own task past it: given up half way, it would leave the client
        // id claimed by no connection.
        let directory = Arc::clone(&self.inner.directory);
        let id = client_id.to_string();
        let deletion = tokio::spawn(async move { directory.delete_session(&id, disconnect).await });
        let unavailable = |message: &str| AdminError::new(ErrorCode::Unavailable, message);
        let deleted = match tokio::time::timeout(DELETE_TIMEOUT, deletion).await {
            Ok(Ok(deleted)) => deleted,
            Ok(Err(_)) => return Err(unavailable("deleting the session failed")),
            Err(_) => return Err(unavailable("timed out waiting for the client to close")),
        };
        deleted.map_err(|refused| match refused {
            DeleteRefused::NotFound => {
                AdminError::not_found(format!("no session for client `{client_id}`"))
            }
            DeleteRefused::Connected => AdminError::new(
                ErrorCode::Conflict,
                format!("client `{client_id}` is connected; set `disconnect` to close it first"),
            ),
        })
    }

    /// Refuses clients matching `ban` from now on, and closes the
    /// connections of those connected with Banned, without their will:
    /// `addBan`. A ban with the same type, value and prefix is replaced,
    /// keeping when it was created.
    pub async fn add_ban(&self, ban: NewBan) -> Result<(), AdminError> {
        let ban = self.inner.bans.add(ban).await?;
        info!(
            "Banned the {} `{}` for the admin API",
            ban.ban.kind.as_str(),
            ban.ban.value
        );
        // Walked once the ban is in force: a client connecting meanwhile
        // either checks again after this and sees the ban, or is listed here
        let close = Close::Admin {
            reason: ReasonCode::AdministrativeAction,
            send_will: false,
        };
        for entry in self.inner.directory.entries().await {
            let Some(generation) = entry.generation else {
                continue;
            };
            let identity = entry.session.identity().await;
            let client = Candidate {
                client_id: &entry.client_id,
                username: identity.username.as_deref(),
                ip: identity.peer.map(|peer| peer.ip()),
            };
            if ban.matches(&client) {
                self.inner
                    .directory
                    .close_generation(&entry.client_id, generation, close)
                    .await;
            }
        }
        Ok(())
    }

    /// Lifts the ban `key` names: `removeBan`.
    pub async fn remove_ban(&self, key: &BanKey) -> Result<(), AdminError> {
        self.inner.bans.remove(key).await?;
        info!(
            "Lifted the ban on the {} `{}` for the admin API",
            key.kind.as_str(),
            key.value
        );
        Ok(())
    }

    /// The bans in force, by type, then value, then prefix: `listBans`.
    pub async fn bans(&self, paging: Paging) -> Page<Ban> {
        Page::of(self.inner.bans.list(), paging)
    }

    /// One session in detail: `getClient`.
    pub async fn client(&self, client_id: &str) -> Result<ClientDetails, AdminError> {
        let entry =
            self.inner.directory.entry(client_id).await.ok_or_else(|| {
                AdminError::not_found(format!("no session for client `{client_id}`"))
            })?;
        let s = entry.session.snapshot().await;
        let summary = summary(&entry, &s);
        let now = unix_millis();
        let get = |counter: &std::sync::atomic::AtomicU64| counter.load(Ordering::Relaxed);
        let mut subscriptions: Vec<ClientSubscription> = s
            .subscriptions
            .iter()
            .map(|sub| ClientSubscription {
                filter: sub.topic_filter.clone(),
                qos: sub.qos as u8,
                no_local: sub.no_local,
                retain_as_published: sub.retain_as_published,
                subscription_identifier: sub.subscription_identifier,
            })
            .collect();
        subscriptions.sort_by(|a, b| a.filter.cmp(&b.filter));
        Ok(ClientDetails {
            client_id: summary.client_id,
            username: summary.username,
            connected: summary.connected,
            address: summary.address,
            listener: summary.listener,
            protocol_version: summary.protocol_version,
            connected_since: summary.connected_since,
            disconnected_since: summary.disconnected_since,
            subscriptions,
            queued: summary.queued,
            inflight: summary.inflight,
            keep_alive: s.keepalive,
            clean_start: s.clean_start,
            session_expiry: s.session_expiry.map(|e| e.as_secs()),
            session_expires_in: s
                .expires_at
                .filter(|_| !entry.connected)
                .map(|at| at.saturating_sub(now) / 1000),
            will_pending: s.will_pending || entry.restored_will,
            messages_in: get(&s.counters.messages_in),
            messages_out: get(&s.counters.messages_out),
            bytes_in: get(&s.counters.bytes_in),
            bytes_out: get(&s.counters.bytes_out),
        })
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

    /// The retained messages matching `filter`, sorted by topic:
    /// `listRetained`.
    pub async fn retained(
        &self,
        filter: &str,
        paging: Paging,
    ) -> Result<Page<RetainedSummary>, AdminError> {
        retained_filter(filter)?;
        let mut messages = self
            .inner
            .retained
            .get_retained(filter)
            .await
            .map_err(storage_failed)?;
        messages.sort_by(|a, b| a.topic.cmp(&b.topic));
        let now = unix_millis();
        let all = messages
            .into_iter()
            .map(|message| {
                let payload = message.payload.unwrap_or_default();
                RetainedSummary {
                    topic: message.topic.to_string(),
                    qos: message.qos as u8,
                    size: payload.len(),
                    expires_in: expires_in(message.expires_at, now),
                    preview: preview(&payload),
                }
            })
            .collect();
        Ok(Page::of(all, paging))
    }

    /// The retained message on `topic`, whole: `getRetained`.
    pub async fn retained_message(&self, topic: &str) -> Result<RetainedMessage, AdminError> {
        validate_publish_topic(topic).map_err(|e| {
            AdminError::invalid_argument(format!("`{topic}` is not a topic name: {e}"))
        })?;
        let message = self
            .inner
            .retained
            .get_retained(topic)
            .await
            .map_err(storage_failed)?
            .into_iter()
            .find(|message| &*message.topic == topic)
            .ok_or_else(|| AdminError::not_found(format!("no retained message on `{topic}`")))?;
        let payload = message.payload.clone().unwrap_or_default();
        let text = std::str::from_utf8(&payload).ok().map(str::to_string);
        let properties = &message.properties;
        Ok(RetainedMessage {
            topic: topic.to_string(),
            qos: message.qos as u8,
            payload_base64: text.is_none().then(|| STANDARD.encode(&payload)),
            payload: text,
            properties: RetainedProperties {
                payload_format_indicator: properties
                    .payload_format_indicator
                    .as_ref()
                    .map(|p| p.value),
                content_type: properties.content_type.as_ref().map(|p| p.value.clone()),
                response_topic: properties.response_topic.as_ref().map(|p| p.value.clone()),
                correlation_data_base64: properties
                    .correlation_data
                    .as_ref()
                    .map(|p| STANDARD.encode(&p.value)),
                user_properties: properties
                    .user_property
                    .iter()
                    .flatten()
                    .map(|p| (p.key.clone(), p.value.clone()))
                    .collect(),
            },
            expires_in: expires_in(message.expires_at, unix_millis()),
        })
    }

    /// Deletes the retained message of every topic `filter` matches, and
    /// returns how many: `deleteRetained`.
    pub async fn delete_retained(&self, filter: &str) -> Result<usize, AdminError> {
        retained_filter(filter)?;
        let messages = self
            .inner
            .retained
            .get_retained(filter)
            .await
            .map_err(storage_failed)?;
        for message in &messages {
            self.inner
                .retained
                .store_retained(&message.topic, None)
                .await
                .map_err(storage_failed)?;
        }
        info!(
            "Deleted {} retained messages matching `{}` for the admin API",
            messages.len(),
            filter
        );
        Ok(messages.len())
    }

    /// Every subscribed filter starting with `prefix`, sorted, with how
    /// many sessions subscribe to it: `listSubscriptions`.
    pub async fn subscriptions(&self, prefix: &str, paging: Paging) -> Page<SubscriptionSummary> {
        let mut found: Vec<SubscriptionSummary> = self
            .inner
            .topics
            .filters()
            .into_iter()
            .filter(|(filter, _)| filter.starts_with(prefix))
            .map(|(filter, subscribers)| SubscriptionSummary {
                filter,
                subscribers,
            })
            .collect();
        found.sort_by(|a, b| a.filter.cmp(&b.filter));
        Page::of(found, paging)
    }

    /// The clients subscribed to exactly `filter`, with their options:
    /// `getSubscribers`.
    pub async fn subscribers(&self, filter: &str) -> Result<Vec<SubscriberInfo>, AdminError> {
        validate_subscribe_filter(filter).map_err(|e| {
            AdminError::invalid_argument(format!("`{filter}` is not a topic filter: {e}"))
        })?;
        let clients: HashMap<u64, String> = self
            .inner
            .directory
            .entries()
            .await
            .into_iter()
            .map(|entry| (entry.session.subscriber_id(), entry.client_id))
            .collect();
        let mut found: Vec<SubscriberInfo> = self
            .inner
            .topics
            .subscribers_of(filter)
            .into_iter()
            .filter_map(|s| {
                Some(SubscriberInfo {
                    client_id: clients.get(&s.session_id)?.clone(),
                    qos: s.granted_qos as u8,
                    no_local: s.no_local,
                    retain_as_published: s.retain_as_published,
                    subscription_identifier: s.subscription_identifier,
                })
            })
            .collect();
        found.sort_by(|a, b| a.client_id.cmp(&b.client_id));
        Ok(found)
    }
}

fn summary(entry: &ClientEntry, s: &SessionSnapshot) -> ClientSummary {
    ClientSummary {
        client_id: entry.client_id.clone(),
        username: s.username.clone(),
        connected: entry.connected,
        address: s.peer.map(|peer| peer.to_string()),
        listener: s.listener.as_deref().map(str::to_string),
        protocol_version: s.protocol_version.map(|v| v as u8),
        connected_since: s.connected_at.filter(|_| entry.connected),
        disconnected_since: s.disconnected_at.filter(|_| !entry.connected),
        subscriptions: s.subscriptions.len(),
        queued: s.queued,
        inflight: s.inflight,
    }
}

/// A topic filter retained messages can be looked up by; shared
/// subscriptions name no topics.
fn retained_filter(filter: &str) -> Result<(), AdminError> {
    if parse_shared_subscription(filter).is_some() || validate_subscribe_filter(filter).is_err() {
        return Err(AdminError::invalid_argument(format!(
            "`{filter}` is not a topic filter"
        )));
    }
    Ok(())
}

pub(crate) fn storage_failed(e: StorageError) -> AdminError {
    AdminError::new(ErrorCode::Unavailable, format!("storage failed: {e}"))
}

/// Seconds until `expires_at`, in Unix milliseconds.
fn expires_in(expires_at: Option<u64>, now: u64) -> Option<u64> {
    expires_at.map(|at| at.saturating_sub(now) / 1000)
}

/// The first 64 bytes of `payload` as text; a character cut by the 64th
/// byte is left out, other invalid UTF-8 gives None.
fn preview(payload: &[u8]) -> Option<String> {
    let cut = payload.len() > 64;
    let head = &payload[..payload.len().min(64)];
    match std::str::from_utf8(head) {
        Ok(text) => Some(text.to_string()),
        Err(e) if cut && e.error_len().is_none() => {
            Some(String::from_utf8_lossy(&head[..e.valid_up_to()]).into_owned())
        }
        Err(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_previews_stop_at_64_bytes_without_cutting_characters() {
        assert_eq!(preview(b"hello").as_deref(), Some("hello"));
        // 1 + 2 * 40 bytes: the 64th byte starts a character
        let text = format!("a{}", "é".repeat(40));
        assert_eq!(
            preview(text.as_bytes()),
            Some(format!("a{}", "é".repeat(31)))
        );
        assert_eq!(preview(&[0xff, 0xfe]), None);
        // Only a character the 64 bytes cut may be left unfinished
        assert_eq!(preview(b"abc\xc3"), None);
    }

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
