//! Refuses publishing under a configured topic prefix, and leaves every
//! other decision to the next plugin.

use mercurio_plugin::{
    export_plugin, host, Access, ClientInfo, Decision, Hooks, Plugin, Registration,
};

struct DenyTopic;

impl Plugin for DenyTopic {
    fn init(config: Vec<(String, String)>) -> Result<Registration, String> {
        let prefix = config
            .into_iter()
            .find(|(key, _)| key == "prefix")
            .map(|(_, value)| value)
            .ok_or("missing `prefix`")?;
        host::kv_set("prefix", prefix.as_bytes());
        Ok(Registration {
            hooks: Hooks::AUTHORIZE,
            auth_methods: Vec::new(),
        })
    }

    fn authorize(_client: ClientInfo, topic: String, access: Access) -> Decision {
        let prefix = host::kv_get("prefix").unwrap_or_default();
        if access == Access::Write && topic.as_bytes().starts_with(&prefix) {
            Decision::Deny
        } else {
            Decision::Defer
        }
    }
}

export_plugin!(DenyTopic);
