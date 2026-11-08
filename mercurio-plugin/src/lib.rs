#![doc = include_str!("../README.md")]

wit_bindgen::generate!({
    path: "wit",
    world: "plugin",
    pub_export_macro: true,
    default_bindings_module: "mercurio_plugin",
    export_macro_name: "export_guest",
});

pub use exports::mercurio::plugin::guest::Guest;
pub use mercurio::plugin::host;
pub use mercurio::plugin::types::{
    Access, AuthStep, Challenge, ClientInfo, Decision, Disconnected, Event, Hooks, Level, Message,
    MessageAction, Qos, Registration, Subscribed, Unsubscribed,
};

/// A broker plugin. Every hook but `init` has a default that does nothing;
/// `init` says which ones the broker calls. Hooks are plain functions:
/// the broker may run several copies of a plugin at once and keeps none of
/// them in particular, so state shared between calls belongs in
/// [`host::kv_set`].
pub trait Plugin {
    fn init(config: Vec<(String, String)>) -> Result<Registration, String>;

    fn authenticate(_client: ClientInfo, _username: String, _password: Vec<u8>) -> Decision {
        Decision::Defer
    }

    fn auth_step(_method: String, _state: Option<Vec<u8>>, _data: Option<Vec<u8>>) -> AuthStep {
        AuthStep::Failed(0x8C) // Bad Authentication Method
    }

    fn authorize(_client: ClientInfo, _topic: String, _access: Access) -> Decision {
        Decision::Defer
    }

    fn on_message(_client: ClientInfo, _message: Message) -> MessageAction {
        MessageAction::Keep
    }

    fn on_event(_event: Event) {}

    fn on_tick() {}

    fn on_control(_client: ClientInfo, _path: String, _payload: Vec<u8>) -> Option<Vec<u8>> {
        None
    }
}

/// Exports `$plugin`, a type implementing [`Plugin`], as the component's
/// `guest` interface.
#[macro_export]
macro_rules! export_plugin {
    ($plugin:ty) => {
        struct __MercurioGuest;

        impl $crate::Guest for __MercurioGuest {
            fn init(config: Vec<(String, String)>) -> Result<$crate::Registration, String> {
                <$plugin as $crate::Plugin>::init(config)
            }
            fn authenticate(client: $crate::ClientInfo, username: String, password: Vec<u8>) -> $crate::Decision {
                <$plugin as $crate::Plugin>::authenticate(client, username, password)
            }
            fn auth_step(method: String, state: Option<Vec<u8>>, data: Option<Vec<u8>>) -> $crate::AuthStep {
                <$plugin as $crate::Plugin>::auth_step(method, state, data)
            }
            fn authorize(client: $crate::ClientInfo, topic: String, access: $crate::Access) -> $crate::Decision {
                <$plugin as $crate::Plugin>::authorize(client, topic, access)
            }
            fn on_message(client: $crate::ClientInfo, message: $crate::Message) -> $crate::MessageAction {
                <$plugin as $crate::Plugin>::on_message(client, message)
            }
            fn on_event(event: $crate::Event) {
                <$plugin as $crate::Plugin>::on_event(event)
            }
            fn on_tick() {
                <$plugin as $crate::Plugin>::on_tick()
            }
            fn on_control(client: $crate::ClientInfo, path: String, payload: Vec<u8>) -> Option<Vec<u8>> {
                <$plugin as $crate::Plugin>::on_control(client, path, payload)
            }
        }

        $crate::export_guest!(__MercurioGuest with_types_in $crate);
    };
}
