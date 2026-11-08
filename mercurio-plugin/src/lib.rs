#![doc = include_str!("../README.md")]

/// The generated bindings, which [`export_plugin!`] wraps.
#[doc(hidden)]
pub mod bindings {
    wit_bindgen::generate!({
        path: "wit",
        world: "plugin",
        pub_export_macro: true,
        default_bindings_module: "mercurio_plugin::bindings",
        export_macro_name: "export_guest",
    });
}

#[doc(hidden)]
pub use bindings::exports::mercurio::plugin::guest::Guest;
pub use bindings::mercurio::plugin::host;
pub use bindings::mercurio::plugin::types::{
    Access, AuthResult, Challenge, ClientInfo, Decision, Disconnected, Event, Hooks, Level,
    Message, MessageAction, Qos, Registration, Subscribed, Unsubscribed,
};

/// A broker plugin. Every hook but `init` has a default that does nothing;
/// `init` says which ones the broker calls. Hooks are plain functions:
/// the broker may run several copies of a plugin at once and keeps none of
/// them in particular, so state shared between calls belongs in
/// [`host::kv_set`].
pub trait Plugin {
    /// Called once per copy of the plugin, before any other hook, with the
    /// plugin's settings from the broker configuration. Returns the hooks
    /// the broker calls and the enhanced authentication methods the plugin
    /// implements; an error stops the broker from loading the plugin.
    fn init(config: Vec<(String, String)>) -> Result<Registration, String>;

    /// Checks the user name and password of a CONNECT, when the plugin
    /// registered [`Hooks::AUTHENTICATE`] and the broker requires
    /// authentication.
    ///
    /// `client.client_id` is empty when the client asked the server to
    /// assign one, because assignment happens after authentication.
    /// `username` is the one the client claims; it becomes the verified user
    /// only when the chain answers [`Decision::Allow`].
    ///
    /// The first plugin to answer [`Decision::Allow`] or [`Decision::Deny`]
    /// decides; [`Decision::Defer`], the default, leaves the decision to the
    /// next plugin. A client is accepted only on `Allow`.
    fn authenticate(_client: ClientInfo, _username: String, _password: Vec<u8>) -> Decision {
        Decision::Defer
    }

    /// Carries out one step of MQTT 5 enhanced authentication of `client`
    /// with `method`, one of the [`Registration::auth_methods`] the plugin
    /// registered. `data` is what the client sent in CONNECT or AUTH, if
    /// anything; `state` is None on the first step and otherwise the state
    /// of the last [`AuthResult::Continue`].
    ///
    /// During CONNECT, `client.username` is the user name the client
    /// claims, if any, and `client.client_id` is empty when the server is
    /// to assign one. On re-authentication they are the client identifier
    /// and the verified user.
    ///
    /// The default fails with reason code 0x8C, Bad Authentication Method.
    fn auth_step(
        _client: ClientInfo,
        _method: String,
        _state: Option<Vec<u8>>,
        _data: Option<Vec<u8>>,
    ) -> AuthResult {
        AuthResult::Failed(0x8C)
    }

    /// Whether `client` may have `access` to `topic`, a topic filter when
    /// subscribing. Called when the plugin registered [`Hooks::AUTHORIZE`].
    ///
    /// The first plugin to answer [`Decision::Allow`] or [`Decision::Deny`]
    /// decides; [`Decision::Defer`], the default, leaves the decision to the
    /// next plugin.
    fn authorize(_client: ClientInfo, _topic: String, _access: Access) -> Decision {
        Decision::Defer
    }

    /// Sees each message a client publishes, once it may publish it and
    /// before it is retained or sent on, when the plugin registered
    /// [`Hooks::ON_MESSAGE`]. It may keep, drop or replace the message; the
    /// default keeps it.
    ///
    /// Changes are trusted: a new topic isn't checked against access
    /// control, and a changed `retain` or `qos` isn't checked against the
    /// retain and QoS limits. The one exception is a topic changed to one
    /// starting with `$`, such as `$SYS/...` or `$CONTROL/...`: the broker
    /// drops such a message, still acknowledging it, and logs a warning.
    /// Will messages aren't passed to this hook.
    fn on_message(_client: ClientInfo, _message: Message) -> MessageAction {
        MessageAction::Keep
    }

    /// Hears about connections, subscriptions and expired sessions, when
    /// the plugin registered [`Hooks::ON_EVENT`].
    ///
    /// Events wait in a bounded queue; when plugins can't keep up with
    /// them, new events are dropped.
    fn on_event(_event: Event) {}

    /// Called at the broker's plugin tick interval, when the plugin
    /// registered [`Hooks::ON_TICK`].
    fn on_tick() {}

    /// Handles a message published to `$CONTROL/<name>/<path>`, once the
    /// client may publish there, when the plugin registered
    /// [`Hooks::ON_CONTROL`]. A returned payload is published to
    /// `$CONTROL/<name>/<path>/response`; the default returns none.
    ///
    /// A client publishing to `$CONTROL/<name>/x/response` reaches this
    /// hook with path `"x/response"`, so a plugin that only answers
    /// requests should ignore such paths.
    fn on_control(_client: ClientInfo, _path: String, _payload: Vec<u8>) -> Option<Vec<u8>> {
        None
    }
}

/// Sends panic messages to the broker's log, which would otherwise be lost
/// with the discarded standard error. Called by [`export_plugin!`].
#[doc(hidden)]
pub fn __log_panics() {
    static HOOK: std::sync::Once = std::sync::Once::new();
    HOOK.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            host::log(Level::Error, &info.to_string());
        }));
    });
}

/// Exports `$plugin`, a type implementing [`Plugin`], as the component's
/// `guest` interface. Invoke it once, at module level.
///
/// A panic in the plugin is logged by the broker at error level, then
/// fails the call.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty) => {
        const _: () = {
            struct __MercurioGuest;

            impl $crate::Guest for __MercurioGuest {
                fn init(
                    config: ::std::vec::Vec<(::std::string::String, ::std::string::String)>,
                ) -> ::core::result::Result<$crate::Registration, ::std::string::String> {
                    $crate::__log_panics();
                    <$plugin as $crate::Plugin>::init(config)
                }
                fn authenticate(
                    client: $crate::ClientInfo,
                    username: ::std::string::String,
                    password: ::std::vec::Vec<u8>,
                ) -> $crate::Decision {
                    <$plugin as $crate::Plugin>::authenticate(client, username, password)
                }
                fn auth_step(
                    client: $crate::ClientInfo,
                    method: ::std::string::String,
                    state: ::core::option::Option<::std::vec::Vec<u8>>,
                    data: ::core::option::Option<::std::vec::Vec<u8>>,
                ) -> $crate::AuthResult {
                    <$plugin as $crate::Plugin>::auth_step(client, method, state, data)
                }
                fn authorize(
                    client: $crate::ClientInfo,
                    topic: ::std::string::String,
                    access: $crate::Access,
                ) -> $crate::Decision {
                    <$plugin as $crate::Plugin>::authorize(client, topic, access)
                }
                fn on_message(
                    client: $crate::ClientInfo,
                    message: $crate::Message,
                ) -> $crate::MessageAction {
                    <$plugin as $crate::Plugin>::on_message(client, message)
                }
                fn on_event(event: $crate::Event) {
                    <$plugin as $crate::Plugin>::on_event(event)
                }
                fn on_tick() {
                    <$plugin as $crate::Plugin>::on_tick()
                }
                fn on_control(
                    client: $crate::ClientInfo,
                    path: ::std::string::String,
                    payload: ::std::vec::Vec<u8>,
                ) -> ::core::option::Option<::std::vec::Vec<u8>> {
                    <$plugin as $crate::Plugin>::on_control(client, path, payload)
                }
            }

    $crate::bindings::export_guest!(__MercurioGuest with_types_in $crate::bindings);
        };
    };
}
