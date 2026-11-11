//! Drops empty messages, unless retained, and tags the others with the id
//! of their publisher.

use mercurio_plugin::{
    export_plugin, ClientInfo, Config, Hooks, Message, MessageAction, Plugin, Registration,
};

struct PayloadRewrite;

impl Plugin for PayloadRewrite {
    fn init(_config: Config) -> Result<Registration, String> {
        Ok(Registration {
            hooks: Hooks::ON_MESSAGE,
            auth_methods: Vec::new(),
        })
    }

    fn on_message(client: ClientInfo, mut message: Message) -> MessageAction {
        // An empty retained message clears the topic's retained message
        if message.payload.is_empty() && !message.retain {
            return MessageAction::Drop;
        }
        message
            .user_properties
            .push(("seen-by".to_string(), client.client_id));
        MessageAction::Replace(message)
    }
}

export_plugin!(PayloadRewrite);
