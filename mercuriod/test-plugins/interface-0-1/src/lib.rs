//! A plugin built for interface 0.1, which the broker keeps loading.

wit_bindgen::generate!({
    path: "../../wit-0.1",
    world: "plugin",
});

use exports::mercurio::plugin::guest::Guest;
use mercurio::plugin::{
    host,
    types::{
        Access, AuthResult, ClientInfo, Decision, Event, Hooks, Message, MessageAction,
        Registration,
    },
};

struct Old;

impl Guest for Old {
    fn init(_config: Vec<(String, String)>) -> Result<Registration, String> {
        Ok(Registration {
            hooks: Hooks::AUTHORIZE | Hooks::ON_MESSAGE | Hooks::ON_CONTROL,
            auth_methods: vec!["OLD".into()],
        })
    }

    fn authenticate(_client: ClientInfo, _username: String, _password: Vec<u8>) -> Decision {
        Decision::Defer
    }

    fn auth_step(
        client: ClientInfo,
        _method: String,
        _state: Option<Vec<u8>>,
        _data: Option<Vec<u8>>,
    ) -> AuthResult {
        AuthResult::Success(Some(client.client_id))
    }

    /// Refuses publishing under `old/`.
    fn authorize(_client: ClientInfo, topic: String, access: Access) -> Decision {
        match access {
            Access::Write if topic.starts_with("old/") => Decision::Deny,
            _ => Decision::Defer,
        }
    }

    fn on_message(client: ClientInfo, mut message: Message) -> MessageAction {
        message
            .user_properties
            .push(("seen-by".into(), client.client_id));
        MessageAction::Replace(message)
    }

    fn on_event(_event: Event) {}

    fn on_tick() {}

    fn on_control(client: ClientInfo, path: String, payload: Vec<u8>) -> Option<Vec<u8>> {
        match path.as_str() {
            "client" => Some(format!("{client:?}").into_bytes()),
            "kv" => {
                host::kv_set("kept", &payload).ok()?;
                host::kv_get("kept")
            }
            _ => None,
        }
    }
}

export!(Old);
