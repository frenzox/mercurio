//! Refuses publishing under a configured topic prefix, and leaves every
//! other decision to the next plugin.

use std::sync::OnceLock;

use mercurio_plugin::{
    export_plugin, Access, ClientInfo, Config, Decision, Hooks, Plugin, Registration,
};

/// Set by `init`; settings never change, so each copy keeps its own.
static PREFIX: OnceLock<String> = OnceLock::new();

struct DenyTopic;

impl Plugin for DenyTopic {
    fn init(config: Config) -> Result<Registration, String> {
        let prefix = config.require("prefix")?;
        if prefix.is_empty() {
            return Err("`prefix` is empty, which would deny every publish".into());
        }
        let _ = PREFIX.set(prefix.to_string());
        Ok(Registration {
            hooks: Hooks::AUTHORIZE,
            auth_methods: Vec::new(),
        })
    }

    fn authorize(_client: ClientInfo, topic: String, access: Access) -> Decision {
        let Some(prefix) = PREFIX.get() else {
            return Decision::Defer;
        };
        if access == Access::Write && topic.starts_with(prefix.as_str()) {
            Decision::Deny
        } else {
            Decision::Defer
        }
    }
}

export_plugin!(DenyTopic);
