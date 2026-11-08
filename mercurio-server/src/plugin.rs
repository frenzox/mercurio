//! Extending the broker: authentication, access control, message hooks,
//! lifecycle events, periodic work and `$CONTROL` commands.
//!
//! A plugin implements the hooks it needs; every other one does nothing.
//! The broker runs its plugins as a chain, in the order configured:
//!
//! - [`Plugin::authenticate`] and [`Plugin::authorize`]: the first
//!   [`Decision::Allow`] or [`Decision::Deny`] decides, and
//!   [`Decision::Defer`] asks the next plugin. When every plugin defers,
//!   a client with credentials is refused, an anonymous one follows
//!   `allow_anonymous`, and access is allowed, except publishing to
//!   `$CONTROL`, which needs an explicit `Allow`.
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
use tracing::{error, warn};

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
pub enum MessageAction {
    Continue,
    /// Drop the message. A QoS 1 or 2 publisher is still acknowledged.
    Drop,
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
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct PluginMessage {
    pub topic: String,
    pub payload: Bytes,
    pub qos: QoS,
    pub retain: bool,
    pub properties: MessageProperties,
}

/// Something that happened to a client, for [`Plugin::on_event`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Event {
    /// The client was accepted.
    Connected { client: ClientInfo },
    /// The connection ended; `reason` is the DISCONNECT reason the server
    /// sent, if any.
    Disconnected {
        client: ClientInfo,
        reason: Option<ReasonCode>,
    },
    /// A subscription was granted at `qos`.
    Subscribed {
        client: ClientInfo,
        filter: String,
        qos: QoS,
    },
    /// A subscription was removed.
    Unsubscribed { client: ClientInfo, filter: String },
    /// A session ended because its expiry interval passed.
    SessionExpired { client_id: String },
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
    fn name(&self) -> &str;

    /// Checks the user name and password of a CONNECT, when the broker
    /// requires authentication.
    fn authenticate<'a>(
        &'a self,
        _client: &'a ClientInfo,
        _credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        ready(Decision::Defer)
    }

    /// Starts MQTT 5 enhanced authentication with `method`, if the plugin
    /// implements it.
    fn begin_auth(&self, _method: &str) -> Option<Box<dyn AuthExchange>> {
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

    /// Sees each message a client publishes, once it may publish it and
    /// before it is retained or sent on. It may change the message or
    /// drop it; a new topic isn't checked against access control again.
    fn on_message<'a>(
        &'a self,
        _client: &'a ClientInfo,
        _message: &'a mut PluginMessage,
    ) -> BoxFuture<'a, MessageAction> {
        ready(MessageAction::Continue)
    }

    /// Hears about connections, subscriptions and expired sessions.
    fn on_event<'a>(&'a self, _event: &'a Event) -> BoxFuture<'a, ()> {
        ready(())
    }

    /// Called every `ServerConfig::plugin_tick_interval`.
    fn on_tick(&self) -> BoxFuture<'_, ()> {
        ready(())
    }

    /// Handles a message published to `$CONTROL/<name>/<path>`, once the
    /// client may publish there. A response is published to
    /// `$CONTROL/<name>/<path>/response`. Such messages aren't sent on to
    /// subscribers.
    fn on_control<'a>(
        &'a self,
        _client: &'a ClientInfo,
        _path: &'a str,
        _payload: Bytes,
    ) -> BoxFuture<'a, Option<Bytes>> {
        ready(None)
    }
}

/// Runs a hook within `limit`. None, logged, if it ran out of time or
/// panicked. A hook that is ready at once, like most access checks, never
/// touches the timer.
#[allow(dead_code)] // used by the server from the next task on
pub(crate) async fn guarded<T>(
    plugin: &str,
    hook: &str,
    limit: Duration,
    future: BoxFuture<'_, T>,
) -> Option<T> {
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
#[allow(dead_code)] // used by the server from the next task on
pub(crate) struct Chain {
    plugins: Arc<[Arc<dyn Plugin>]>,
    timeout: Duration,
    /// In-process connections: every topic is allowed, `$SYS` and
    /// `$CONTROL` included, and nothing is authenticated
    trusted: bool,
}

#[allow(dead_code)] // used by the server from the next task on
impl Chain {
    pub(crate) fn new(plugins: Vec<Arc<dyn Plugin>>, timeout: Duration) -> Chain {
        Chain {
            plugins: plugins.into(),
            timeout,
            trusted: false,
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

    pub(crate) fn plugins(&self) -> &[Arc<dyn Plugin>] {
        &self.plugins
    }

    pub(crate) async fn authenticate(
        &self,
        client: &ClientInfo,
        credentials: &Credentials<'_>,
    ) -> Decision {
        for plugin in self.plugins.iter() {
            let hook = plugin.authenticate(client, credentials);
            match guarded(plugin.name(), "authenticate", self.timeout, hook).await {
                Some(Decision::Defer) => continue,
                Some(decision) => return decision,
                None => return Decision::Deny,
            }
        }
        Decision::Defer
    }

    pub(crate) fn begin_auth(&self, method: &str) -> Option<Box<dyn AuthExchange>> {
        self.plugins
            .iter()
            .find_map(|plugin| plugin.begin_auth(method))
    }

    pub(crate) async fn step(
        &self,
        exchange: &mut dyn AuthExchange,
        data: Option<&Bytes>,
    ) -> AuthResult {
        let hook = exchange.step(data);
        guarded("enhanced authentication", "step", self.timeout, hook)
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
            let hook = plugin.authorize(client, topic, access);
            match guarded(plugin.name(), "authorize", self.timeout, hook).await {
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

    pub(crate) async fn on_message(
        &self,
        client: &ClientInfo,
        message: &mut PluginMessage,
    ) -> MessageAction {
        for plugin in self.plugins.iter() {
            let hook = plugin.on_message(client, message);
            match guarded(plugin.name(), "on_message", self.timeout, hook).await {
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
        let chain = self.clone();
        tokio::spawn(async move {
            for plugin in chain.plugins.iter() {
                let hook = plugin.on_event(&event);
                guarded(plugin.name(), "on_event", chain.timeout, hook).await;
            }
        });
    }

    pub(crate) async fn control(
        &self,
        client: &ClientInfo,
        topic: &str,
        payload: Bytes,
    ) -> Option<Option<Bytes>> {
        let rest = topic.strip_prefix("$CONTROL/")?;
        let (name, path) = rest.split_once('/').unwrap_or((rest, ""));
        let plugin = self.plugins.iter().find(|p| p.name() == name)?;
        let hook = plugin.on_control(client, path, payload);
        Some(
            guarded(plugin.name(), "on_control", self.timeout, hook)
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

    fn client() -> ClientInfo {
        ClientInfo::new("c", None)
    }

    fn message() -> PluginMessage {
        PluginMessage {
            topic: "t".into(),
            payload: Bytes::from_static(b"p"),
            qos: QoS::AtMostOnce,
            retain: false,
            properties: MessageProperties::default(),
        }
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
