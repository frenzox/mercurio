//! Extending the broker: authentication, access control, message hooks,
//! lifecycle events, periodic work and `$CONTROL` commands.
//!
//! A plugin implements the hooks it needs; every other one does nothing.
//! The broker runs its plugins as a chain, in the order configured:
//!
//! - [`Plugin::authenticate`] and [`Plugin::authorize`]: the first
//!   [`Decision::Allow`] or [`Decision::Deny`] decides, and
//!   [`Decision::Defer`] asks the next plugin. Credentials are checked
//!   whenever a client sends them, even when authentication isn't
//!   required. When every plugin defers, a client with credentials is
//!   refused if authentication is required and let in unverified if not, an
//!   anonymous one follows `allow_anonymous`, and access is allowed.
//! - [`Plugin::authorize_control`]: as `authorize`, but when every plugin
//!   defers the client is refused, so only an explicit `Allow` lets a
//!   client publish to `$CONTROL`.
//! - [`Plugin::on_message`]: every plugin, each seeing the changes of the
//!   ones before; the first [`MessageAction::Drop`] drops the message.
//! - [`Plugin::on_event`] and [`Plugin::on_tick`]: every plugin.
//!
//! Each hook call has a time limit, `ServerConfig::plugin_timeout`. A hook
//! that runs out of time or panics denies, for authentication and access
//! control, and drops, for messages.

use std::{
    future::Future, net::SocketAddr, panic::AssertUnwindSafe, pin::Pin, sync::Arc, time::Duration,
};

use bytes::Bytes;
use futures_util::FutureExt;
use mercurio_core::{message::MessageProperties, qos::QoS, reason::ReasonCode};
use serde::Serialize;
use tracing::{error, warn};

use crate::admin::Admin;

pub use crate::acl::Access;

/// The future a hook returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A hook's answer that is already known, for plugins that don't wait on
/// anything.
pub fn ready<'a, T: Send + 'a>(value: T) -> BoxFuture<'a, T> {
    Box::pin(std::future::ready(value))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Deny,
    /// Leave the decision to the next plugin.
    Defer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MessageAction {
    Continue,
    /// Drop the message. A QoS 1 or 2 publisher is still acknowledged.
    Drop,
}

/// What the admin API's `listPlugins` reports about a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct PluginStatus {
    pub name: String,
    pub kind: PluginKind,
    /// The hooks it implements, by their method name, such as `authorize`
    pub hooks: Vec<String>,
    /// WASM plugins: instances running
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instances: Option<usize>,
    /// WASM plugins: calls that failed since the broker started
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failures: Option<u64>,
    /// WASM plugins: the last failure
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl PluginStatus {
    /// The status of a plugin called `name`, with no hooks listed.
    pub fn new(name: impl Into<String>, kind: PluginKind) -> Self {
        PluginStatus {
            name: name.into(),
            kind,
            hooks: Vec::new(),
            instances: None,
            failures: None,
            last_error: None,
        }
    }

    /// Lists `hooks` as the ones it implements, by their method name.
    pub fn with_hooks(mut self, hooks: &[&str]) -> Self {
        self.hooks = hooks.iter().map(|hook| hook.to_string()).collect();
        self
    }
}

/// How a plugin is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum PluginKind {
    /// Part of the broker or `mercuriod`
    Builtin,
    /// Rust code built into the program
    Rust,
    /// A WebAssembly component
    Wasm,
}

/// The client a hook is about.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClientInfo {
    pub client_id: String,
    /// Set only for a user the broker verified, by password, certificate
    /// or enhanced authentication; a claimed user name doesn't count
    pub username: Option<String>,
    /// The client's address, from the PROXY header when there is one.
    /// None for in-process clients and for the publishers of retained
    /// messages.
    pub peer: Option<SocketAddr>,
}

impl ClientInfo {
    pub fn new(client_id: impl Into<String>, username: Option<String>) -> Self {
        ClientInfo {
            client_id: client_id.into(),
            username,
            peer: None,
        }
    }
}

/// The user name and password of a CONNECT.
#[derive(Debug)]
#[non_exhaustive]
pub struct Credentials<'a> {
    pub username: &'a str,
    pub password: &'a [u8],
}

impl<'a> Credentials<'a> {
    pub fn new(username: &'a str, password: &'a [u8]) -> Self {
        Credentials { username, password }
    }
}

/// A message a client published, as [`Plugin::on_message`] may change it.
/// The changes are trusted: a new topic isn't checked against access
/// control, and a changed `retain` or `qos` isn't checked against the
/// retain and QoS limits. A message moved to a different topic starting
/// with `$` is dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginMessage {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
    pub properties: MessageProperties,
}

impl PluginMessage {
    pub fn new(
        topic: impl Into<String>,
        payload: impl Into<Bytes>,
        qos: QoS,
        retain: bool,
        properties: MessageProperties,
    ) -> Self {
        PluginMessage {
            topic: topic.into(),
            payload: payload.into(),
            qos,
            retain,
            properties,
        }
    }
}

/// Something that happened to a client, for [`Plugin::on_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The client was accepted.
    #[non_exhaustive]
    Connected { client: ClientInfo },
    /// The connection ended; `reason` is the DISCONNECT reason the server
    /// sent, if any.
    #[non_exhaustive]
    Disconnected {
        client: ClientInfo,
        reason: Option<ReasonCode>,
    },
    /// A subscription was granted at `qos`.
    #[non_exhaustive]
    Subscribed {
        client: ClientInfo,
        filter: String,
        qos: QoS,
    },
    /// A subscription was removed.
    #[non_exhaustive]
    Unsubscribed { client: ClientInfo, filter: String },
    /// A session ended because its expiry interval passed.
    #[non_exhaustive]
    SessionExpired { client_id: String },
}

impl Event {
    fn kind(&self) -> &'static str {
        match self {
            Event::Connected { .. } => "Connected",
            Event::Disconnected { .. } => "Disconnected",
            Event::Subscribed { .. } => "Subscribed",
            Event::Unsubscribed { .. } => "Unsubscribed",
            Event::SessionExpired { .. } => "SessionExpired",
        }
    }
}

/// Result of an authentication step.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum AuthResult {
    /// The client is authenticated, as `username` if the method identifies
    /// users.
    Success { username: Option<String> },
    /// Authentication requires another round - send this challenge to client.
    Continue(Bytes),
    /// Authentication failed with the given reason.
    Failed(ReasonCode),
}

/// One client's MQTT 5 enhanced authentication, carried out step by step.
/// It keeps whatever the method needs between steps, such as the nonce of
/// a challenge.
pub trait AuthExchange: Send {
    /// Processes the data the client sent in CONNECT or AUTH, if any.
    fn step<'a>(&'a mut self, data: Option<&'a Bytes>) -> BoxFuture<'a, AuthResult>;
}

/// An extension of the broker. Every hook has a default that does nothing,
/// so a plugin implements only the ones it needs.
pub trait Plugin: Send + Sync + 'static {
    /// Names the plugin in logs and in `$CONTROL/<name>/...` topics.
    ///
    /// Publishes to `$CONTROL/<name>/...` go to the plugin with that name,
    /// see [`Plugin::on_control`], so pick a name no client topic needs.
    ///
    /// The name `broker` is taken: `$CONTROL/broker/...` is the broker's own
    /// admin API, so a plugin with that name never sees those requests.
    fn name(&self) -> &str;

    /// What the admin API reports about the plugin. The default gives its
    /// name, kind [`PluginKind::Rust`] and no hooks.
    fn status(&self) -> PluginStatus {
        PluginStatus::new(self.name(), PluginKind::Rust)
    }

    /// Checks the user name and password of a CONNECT. It runs whenever a
    /// client sends both, whether or not the broker requires
    /// authentication, and [`Decision::Deny`] refuses the client either
    /// way. A plugin that only does access control leaves it deferring.
    ///
    /// `client.client_id` is empty when the client asked the server to
    /// assign one, because assignment happens after authentication.
    ///
    /// The user name in `credentials`, and in `client.username` here, is
    /// the one the client claims; it becomes the verified user only when
    /// the chain answers [`Decision::Allow`].
    fn authenticate<'a>(
        &'a self,
        _client: &'a ClientInfo,
        _credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        ready(Decision::Defer)
    }

    /// Starts MQTT 5 enhanced authentication of `client` with `method`, if
    /// the plugin implements it.
    ///
    /// During CONNECT, `client.username` is the user name the client
    /// claims, if any, and `client.client_id` is empty when the server is
    /// to assign one. When a connected client re-authenticates, they are
    /// its client identifier and verified user.
    fn begin_auth(&self, _client: &ClientInfo, _method: &str) -> Option<Box<dyn AuthExchange>> {
        None
    }

    /// Whether `client` may have `access` to `topic`, a topic filter when
    /// subscribing.
    fn authorize<'a>(
        &'a self,
        _client: &'a ClientInfo,
        _topic: &'a str,
        _access: Access,
    ) -> BoxFuture<'a, Decision> {
        ready(Decision::Defer)
    }

    /// Whether `client` may publish to `topic`, a `$CONTROL/...` topic,
    /// which administers the broker or a plugin. A request to
    /// `$CONTROL/broker/v1` is asked about twice: for that topic, then for
    /// each of its commands as `$CONTROL/broker/v1/<command>`.
    ///
    /// The broker asks this instead of [`Plugin::authorize`] for such
    /// publishes, so a plugin that allows every topic doesn't make every
    /// client an administrator. When every plugin defers, the client is
    /// refused. Receiving and subscribing to `$CONTROL` topics go through
    /// `authorize`, as responses go only to the client that asked.
    fn authorize_control<'a>(
        &'a self,
        _client: &'a ClientInfo,
        _topic: &'a str,
    ) -> BoxFuture<'a, Decision> {
        ready(Decision::Defer)
    }

    /// Sees each message a client publishes, once it may publish it and
    /// before it is retained or sent on. It may change the message or
    /// drop it.
    ///
    /// Changes are trusted: a new topic isn't checked against access
    /// control, and a changed `retain` or `qos` isn't checked against the
    /// retain and QoS limits. The one exception is a topic changed to one
    /// starting with `$`, such as `$SYS/...` or `$CONTROL/...`: the broker
    /// drops such a message, still acknowledging it, and logs a warning.
    ///
    /// Will messages aren't passed to this hook.
    fn on_message<'a>(
        &'a self,
        _client: &'a ClientInfo,
        _message: &'a mut PluginMessage,
    ) -> BoxFuture<'a, MessageAction> {
        ready(MessageAction::Continue)
    }

    /// Hears about connections, subscriptions and expired sessions.
    ///
    /// Events of one connection arrive in the order they happened. Session
    /// expiry is reported separately: it may interleave with the events of
    /// other connections, run concurrently with them, and isn't ordered
    /// relative to the `Disconnected` of the same client.
    ///
    /// Events wait in a bounded queue; when plugins can't keep up with
    /// them, new events are dropped.
    fn on_event<'a>(&'a self, _event: &'a Event) -> BoxFuture<'a, ()> {
        ready(())
    }

    /// Called every `ServerConfig::plugin_tick_interval`.
    fn on_tick(&self) -> BoxFuture<'_, ()> {
        ready(())
    }

    /// Handles a message published to `$CONTROL/<name>/<path>`, once the
    /// client may publish there. A response goes only to that client: on
    /// the request's Response Topic with its Correlation Data, or else on
    /// `$CONTROL/<name>/<path>/response`. Requests aren't sent on to
    /// subscribers, even when the plugin named `<name>` doesn't implement
    /// this hook.
    ///
    /// A client publishing to `$CONTROL/<name>/x/response` reaches this
    /// hook with path `"x/response"`, so a plugin that only answers
    /// requests should ignore such paths.
    fn on_control<'a>(
        &'a self,
        _client: &'a ClientInfo,
        _path: &'a str,
        _payload: Bytes,
    ) -> BoxFuture<'a, Option<Bytes>> {
        ready(None)
    }
}

/// Whether `topic` is `$CONTROL` or a topic below it.
pub(crate) fn is_control(topic: &str) -> bool {
    topic == "$CONTROL" || topic.starts_with("$CONTROL/")
}

/// Runs a hook within `limit`. None, logged, if it ran out of time or
/// panicked, whether making its future or polling it. A hook that is ready
/// at once, like most access checks, never touches the timer.
pub(crate) async fn guarded<'a, T>(
    plugin: &str,
    hook: &str,
    limit: Duration,
    call: impl FnOnce() -> BoxFuture<'a, T>,
) -> Option<T> {
    let Ok(future) = std::panic::catch_unwind(AssertUnwindSafe(call)) else {
        error!("Plugin `{plugin}` panicked in {hook}");
        return None;
    };
    let mut future = AssertUnwindSafe(future).catch_unwind();
    let result = match (&mut future).now_or_never() {
        Some(result) => result,
        None => match tokio::time::timeout(limit, future).await {
            Ok(result) => result,
            Err(_) => {
                warn!("Plugin `{plugin}` took longer than {limit:?} in {hook}");
                return None;
            }
        },
    };
    result
        .map_err(|_| error!("Plugin `{plugin}` panicked in {hook}"))
        .ok()
}

/// The plugins one connection runs, in order.
#[derive(Clone)]
pub(crate) struct Chain {
    plugins: Arc<[Arc<dyn Plugin>]>,
    timeout: Duration,
    /// In-process connections: every topic is allowed, `$SYS` and
    /// `$CONTROL` included, and nothing is authenticated
    trusted: bool,
    /// Shared by the clones of a chain.
    events: Arc<Events>,
    /// Serves `$CONTROL/broker/v1`; set on every chain the server builds
    admin: Option<Admin>,
}

/// An enhanced authentication in progress, with the plugin carrying it out.
pub(crate) struct Exchange {
    plugin: Arc<dyn Plugin>,
    steps: Box<dyn AuthExchange>,
}

/// How many events may wait for the plugins before new ones are dropped.
const EVENT_QUEUE: usize = 10_000;

#[derive(Default)]
struct Events {
    /// Feeds the task that hands events to the plugins, started by the
    /// first event.
    sender: std::sync::OnceLock<tokio::sync::mpsc::Sender<Event>>,
    /// Whether the last event was dropped, so a run of drops is logged once
    dropping: std::sync::atomic::AtomicBool,
}

impl Chain {
    pub(crate) fn new(plugins: Vec<Arc<dyn Plugin>>, timeout: Duration) -> Chain {
        Chain {
            plugins: plugins.into(),
            timeout,
            trusted: false,
            events: Arc::default(),
            admin: None,
        }
    }

    pub(crate) fn with_admin(self, admin: Admin) -> Chain {
        Chain {
            admin: Some(admin),
            ..self
        }
    }

    pub(crate) fn trusted(&self) -> Chain {
        Chain {
            trusted: true,
            ..self.clone()
        }
    }

    pub(crate) fn is_trusted(&self) -> bool {
        self.trusted
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.plugins.is_empty()
    }

    /// The plugins' names, for logs: `a, b`.
    pub(crate) fn names(&self) -> String {
        let names: Vec<&str> = self.plugins.iter().map(|p| p.name()).collect();
        names.join(", ")
    }

    pub(crate) async fn authenticate(
        &self,
        client: &ClientInfo,
        credentials: &Credentials<'_>,
    ) -> Decision {
        for plugin in self.plugins.iter() {
            match guarded(plugin.name(), "authenticate", self.timeout, || {
                plugin.authenticate(client, credentials)
            })
            .await
            {
                Some(Decision::Defer) => continue,
                Some(decision) => return decision,
                None => return Decision::Deny,
            }
        }
        Decision::Defer
    }

    pub(crate) fn begin_auth(&self, client: &ClientInfo, method: &str) -> Option<Exchange> {
        self.plugins.iter().find_map(|plugin| {
            let steps =
                std::panic::catch_unwind(AssertUnwindSafe(|| plugin.begin_auth(client, method)))
                    .unwrap_or_else(|_| {
                        error!("Plugin `{}` panicked in begin_auth", plugin.name());
                        None
                    })?;
            Some(Exchange {
                plugin: Arc::clone(plugin),
                steps,
            })
        })
    }

    pub(crate) async fn step(&self, exchange: &mut Exchange, data: Option<&Bytes>) -> AuthResult {
        let Exchange { plugin, steps } = exchange;
        guarded(
            plugin.name(),
            "enhanced authentication",
            self.timeout,
            || steps.step(data),
        )
        .await
        .unwrap_or(AuthResult::Failed(ReasonCode::NotAuthorized))
    }

    pub(crate) async fn authorize(
        &self,
        client: &ClientInfo,
        topic: &str,
        access: Access,
    ) -> Decision {
        if self.trusted {
            return Decision::Allow;
        }
        for plugin in self.plugins.iter() {
            match guarded(plugin.name(), "authorize", self.timeout, || {
                plugin.authorize(client, topic, access)
            })
            .await
            {
                Some(Decision::Defer) => continue,
                Some(decision) => return decision,
                None => return Decision::Deny,
            }
        }
        Decision::Defer
    }

    pub(crate) async fn may(&self, client: &ClientInfo, topic: &str, access: Access) -> bool {
        self.authorize(client, topic, access).await != Decision::Deny
    }

    /// Whether `client` may publish to the `$CONTROL` topic `topic`: only
    /// when a plugin allows it.
    pub(crate) async fn may_control(&self, client: &ClientInfo, topic: &str) -> bool {
        if self.trusted {
            return true;
        }
        for plugin in self.plugins.iter() {
            match guarded(plugin.name(), "authorize_control", self.timeout, || {
                plugin.authorize_control(client, topic)
            })
            .await
            {
                Some(Decision::Defer) => continue,
                Some(decision) => return decision == Decision::Allow,
                None => return false,
            }
        }
        false
    }

    pub(crate) async fn on_message(
        &self,
        client: &ClientInfo,
        message: &mut PluginMessage,
    ) -> MessageAction {
        for plugin in self.plugins.iter() {
            match guarded(plugin.name(), "on_message", self.timeout, || {
                plugin.on_message(client, message)
            })
            .await
            {
                Some(MessageAction::Continue) => continue,
                Some(MessageAction::Drop) | None => return MessageAction::Drop,
            }
        }
        MessageAction::Continue
    }

    pub(crate) fn emit(&self, event: Event) {
        if self.plugins.is_empty() {
            return;
        }
        let sender = self.events.sender.get_or_init(|| {
            // One task per chain hands events to the plugins in the order
            // they happened. The queue is bounded, so plugins slower than
            // the event rate lose events instead of growing it without end.
            let (sender, mut receiver) = tokio::sync::mpsc::channel::<Event>(EVENT_QUEUE);
            let (plugins, timeout) = (Arc::clone(&self.plugins), self.timeout);
            tokio::spawn(async move {
                while let Some(event) = receiver.recv().await {
                    for plugin in plugins.iter() {
                        guarded(plugin.name(), "on_event", timeout, || {
                            plugin.on_event(&event)
                        })
                        .await;
                    }
                }
            });
            sender
        });
        use std::sync::atomic::Ordering::Relaxed;
        match sender.try_send(event) {
            Ok(()) => self.events.dropping.store(false, Relaxed),
            Err(tokio::sync::mpsc::error::TrySendError::Full(event)) => {
                if !self.events.dropping.swap(true, Relaxed) {
                    warn!(
                        "Plugins can't keep up with events; dropping them, from a {} event",
                        event.kind()
                    );
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {}
        }
    }

    pub(crate) async fn control(
        &self,
        client: &ClientInfo,
        topic: &str,
        payload: Bytes,
    ) -> Option<Option<Bytes>> {
        let rest = topic.strip_prefix("$CONTROL/")?;
        let (name, path) = rest.split_once('/').unwrap_or((rest, ""));
        if let (Some(admin), "broker") = (&self.admin, name) {
            // Anything else under `$CONTROL/broker/`, responses included,
            // is consumed unanswered
            let answer = match path {
                "v1" => Some(crate::admin::handle_request(admin, self, client, &payload).await),
                _ => None,
            };
            return Some(answer);
        }
        let plugin = self.plugins.iter().find(|p| p.name() == name)?;
        Some(
            guarded(plugin.name(), "on_control", self.timeout, || {
                plugin.on_control(client, path, payload)
            })
            .await
            .flatten(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixed {
        name: &'static str,
        decision: Decision,
    }

    impl Plugin for Fixed {
        fn name(&self) -> &str {
            self.name
        }

        fn authorize<'a>(
            &'a self,
            _: &'a ClientInfo,
            _: &'a str,
            _: Access,
        ) -> BoxFuture<'a, Decision> {
            ready(self.decision)
        }
    }

    struct Slow;

    impl Plugin for Slow {
        fn name(&self) -> &str {
            "slow"
        }

        fn authorize<'a>(
            &'a self,
            _: &'a ClientInfo,
            _: &'a str,
            _: Access,
        ) -> BoxFuture<'a, Decision> {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Decision::Allow
            })
        }
    }

    struct Panics;

    impl Plugin for Panics {
        fn name(&self) -> &str {
            "panics"
        }

        fn on_message<'a>(
            &'a self,
            _: &'a ClientInfo,
            _: &'a mut PluginMessage,
        ) -> BoxFuture<'a, MessageAction> {
            Box::pin(async { panic!("broken plugin") })
        }
    }

    fn chain(plugins: Vec<Arc<dyn Plugin>>) -> Chain {
        Chain::new(plugins, Duration::from_millis(50))
    }

    fn fixed(name: &'static str, decision: Decision) -> Arc<dyn Plugin> {
        Arc::new(Fixed { name, decision })
    }

    #[test]
    fn test_plugins_report_rust_without_hooks_by_default() {
        let status = fixed("a", Decision::Allow).status();
        assert_eq!(status, PluginStatus::new("a", PluginKind::Rust));
        assert!(status.hooks.is_empty());
        assert_eq!(status.instances, None);
    }

    fn client() -> ClientInfo {
        ClientInfo::new("c", None)
    }

    fn message() -> PluginMessage {
        PluginMessage::new(
            "t",
            Bytes::from_static(b"p"),
            QoS::AtMostOnce,
            false,
            MessageProperties::default(),
        )
    }

    #[tokio::test]
    async fn test_first_decision_wins_and_defer_passes_on() {
        let deny_first = chain(vec![
            fixed("a", Decision::Defer),
            fixed("b", Decision::Deny),
            fixed("c", Decision::Allow),
        ]);
        assert_eq!(
            deny_first.authorize(&client(), "t", Access::Read).await,
            Decision::Deny
        );

        let allow_first = chain(vec![
            fixed("a", Decision::Allow),
            fixed("b", Decision::Deny),
        ]);
        assert_eq!(
            allow_first.authorize(&client(), "t", Access::Read).await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn test_all_deferring_allows_access() {
        let deferring = chain(vec![fixed("a", Decision::Defer)]);
        assert_eq!(
            deferring.authorize(&client(), "t", Access::Read).await,
            Decision::Defer
        );
        assert!(deferring.may(&client(), "t", Access::Read).await);
        assert!(chain(Vec::new()).may(&client(), "t", Access::Write).await);
    }

    #[tokio::test]
    async fn test_control_needs_an_allow_from_its_own_hook() {
        let topic = "$CONTROL/broker/v1";
        let allows_topics = chain(vec![fixed("a", Decision::Allow)]);
        assert!(!allows_topics.may_control(&client(), topic).await);
        assert!(!chain(Vec::new()).may_control(&client(), topic).await);
        assert!(
            chain(Vec::new())
                .trusted()
                .may_control(&client(), topic)
                .await
        );
    }

    #[tokio::test]
    async fn test_trusted_chains_allow_everything() {
        let denying = chain(vec![fixed("a", Decision::Deny)]).trusted();
        assert_eq!(
            denying.authorize(&client(), "$SYS/x", Access::Write).await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn test_a_hook_out_of_time_denies() {
        let slow = chain(vec![Arc::new(Slow), fixed("b", Decision::Allow)]);
        assert_eq!(
            slow.authorize(&client(), "t", Access::Read).await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn test_a_panicking_hook_drops_the_message() {
        let panics = chain(vec![Arc::new(Panics)]);
        assert_eq!(
            panics.on_message(&client(), &mut message()).await,
            MessageAction::Drop
        );
    }

    /// Panics while making its futures, before any of them is polled.
    struct PanicsAtOnce;

    impl Plugin for PanicsAtOnce {
        fn name(&self) -> &str {
            "panics-at-once"
        }

        fn begin_auth(&self, _: &ClientInfo, _: &str) -> Option<Box<dyn AuthExchange>> {
            panic!("broken plugin")
        }

        fn authorize<'a>(
            &'a self,
            _: &'a ClientInfo,
            _: &'a str,
            _: Access,
        ) -> BoxFuture<'a, Decision> {
            panic!("broken plugin")
        }

        fn on_event<'a>(&'a self, _: &'a Event) -> BoxFuture<'a, ()> {
            panic!("broken plugin")
        }
    }

    /// Counts the events it hears.
    #[derive(Default)]
    struct Counter(std::sync::atomic::AtomicUsize);

    impl Plugin for Counter {
        fn name(&self) -> &str {
            "counter"
        }

        fn on_event<'a>(&'a self, _: &'a Event) -> BoxFuture<'a, ()> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ready(())
        }
    }

    impl Counter {
        async fn wait_for(&self, count: usize) -> usize {
            for _ in 0..200 {
                if self.0.load(std::sync::atomic::Ordering::Relaxed) >= count {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.0.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    fn expired(client_id: &str) -> Event {
        Event::SessionExpired {
            client_id: client_id.into(),
        }
    }

    #[tokio::test]
    async fn test_a_hook_panicking_before_its_future_denies() {
        let panics = chain(vec![Arc::new(PanicsAtOnce), fixed("b", Decision::Allow)]);
        assert_eq!(
            panics.authorize(&client(), "t", Access::Read).await,
            Decision::Deny
        );
        assert!(panics.begin_auth(&client(), "PLAIN").is_none());
    }

    #[tokio::test]
    async fn test_a_panicking_event_hook_doesnt_stop_later_events() {
        let counter = Arc::new(Counter::default());
        let chain = chain(vec![Arc::new(PanicsAtOnce), counter.clone()]);
        chain.emit(expired("a"));
        assert_eq!(counter.wait_for(1).await, 1);
        chain.emit(expired("b"));
        assert_eq!(counter.wait_for(2).await, 2);
    }

    #[tokio::test]
    async fn test_events_beyond_the_queue_are_dropped() {
        let counter = Arc::new(Counter::default());
        let chain = chain(vec![counter.clone()]);
        // The current-thread runtime runs the event task only once this
        // test yields, so the queue fills up first
        for _ in 0..EVENT_QUEUE + 5 {
            chain.emit(expired("a"));
        }
        assert_eq!(counter.wait_for(EVENT_QUEUE).await, EVENT_QUEUE);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            counter.0.load(std::sync::atomic::Ordering::Relaxed),
            EVENT_QUEUE
        );
    }

    #[tokio::test]
    async fn test_control_finds_the_plugin_by_name() {
        struct Echo;
        impl Plugin for Echo {
            fn name(&self) -> &str {
                "echo"
            }
            fn on_control<'a>(
                &'a self,
                _: &'a ClientInfo,
                path: &'a str,
                payload: Bytes,
            ) -> BoxFuture<'a, Option<Bytes>> {
                ready(Some(Bytes::from(format!(
                    "{path}:{}",
                    String::from_utf8_lossy(&payload)
                ))))
            }
        }
        let chain = chain(vec![Arc::new(Echo)]);
        let answer = chain
            .control(&client(), "$CONTROL/echo/v1", Bytes::from_static(b"hi"))
            .await;
        assert_eq!(answer, Some(Some(Bytes::from_static(b"v1:hi"))));
        assert_eq!(
            chain
                .control(&client(), "$CONTROL/other/v1", Bytes::new())
                .await,
            None
        );
    }
}
