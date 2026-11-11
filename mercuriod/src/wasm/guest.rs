//! Calls into a plugin, whichever interface version it was built for. The
//! broker speaks the current interface; a plugin built for 0.1 gets its
//! arguments in 0.1's types, and its answers come back in the current
//! ones.

use wasmtime::{Engine, Store};

use super::{
    runtime::{
        bindings::{self, exports::mercurio::plugin::guest, mercurio::plugin::types as wit},
        bindings_0_1::{
            self as bindings_old, exports::mercurio::plugin::guest as guest_old,
            mercurio::plugin::types as old,
        },
    },
    sandbox::State,
};

/// A compiled plugin, linked and ready to instantiate.
pub(crate) enum Pre {
    V0_1(bindings_old::PluginPre<State>),
    V0_2(bindings::PluginPre<State>),
}

/// The exports of one copy of a plugin.
pub(crate) enum Guest {
    V0_1(guest_old::Guest),
    V0_2(guest::Guest),
}

impl Pre {
    pub(crate) fn engine(&self) -> &Engine {
        match self {
            Pre::V0_1(pre) => pre.engine(),
            Pre::V0_2(pre) => pre.engine(),
        }
    }

    pub(crate) async fn instantiate(&self, store: &mut Store<State>) -> wasmtime::Result<Guest> {
        Ok(match self {
            Pre::V0_1(pre) => Guest::V0_1(
                pre.instantiate_async(store)
                    .await?
                    .mercurio_plugin_guest()
                    .clone(),
            ),
            Pre::V0_2(pre) => Guest::V0_2(
                pre.instantiate_async(store)
                    .await?
                    .mercurio_plugin_guest()
                    .clone(),
            ),
        })
    }
}

type Result<T> = wasmtime::Result<T>;

impl Guest {
    pub(crate) async fn init(
        &self,
        store: &mut Store<State>,
        config: &[(String, String)],
    ) -> Result<std::result::Result<wit::Registration, String>> {
        Ok(match self {
            Guest::V0_1(guest) => guest.call_init(store, config).await?.map(registration),
            Guest::V0_2(guest) => guest.call_init(store, config).await?,
        })
    }

    pub(crate) async fn authenticate(
        &self,
        store: &mut Store<State>,
        client: &wit::ClientInfo,
        username: &str,
        password: &[u8],
    ) -> Result<wit::Decision> {
        Ok(match self {
            Guest::V0_1(guest) => decision(
                guest
                    .call_authenticate(store, &client_info(client), username, password)
                    .await?,
            ),
            Guest::V0_2(guest) => {
                guest
                    .call_authenticate(store, client, username, password)
                    .await?
            }
        })
    }

    pub(crate) async fn auth_step(
        &self,
        store: &mut Store<State>,
        client: &wit::ClientInfo,
        method: &str,
        state: Option<&[u8]>,
        data: Option<&[u8]>,
    ) -> Result<wit::AuthResult> {
        Ok(match self {
            Guest::V0_1(guest) => auth_result(
                guest
                    .call_auth_step(store, &client_info(client), method, state, data)
                    .await?,
            ),
            Guest::V0_2(guest) => {
                guest
                    .call_auth_step(store, client, method, state, data)
                    .await?
            }
        })
    }

    pub(crate) async fn authorize(
        &self,
        store: &mut Store<State>,
        client: &wit::ClientInfo,
        topic: &str,
        access: wit::Access,
    ) -> Result<wit::Decision> {
        Ok(match self {
            Guest::V0_1(guest) => decision(
                guest
                    .call_authorize(store, &client_info(client), topic, old_access(access))
                    .await?,
            ),
            Guest::V0_2(guest) => guest.call_authorize(store, client, topic, access).await?,
        })
    }

    /// 0.1 has no such hook, nor its flag, so the broker never calls it on
    /// a 0.1 plugin; it would defer.
    pub(crate) async fn authorize_control(
        &self,
        store: &mut Store<State>,
        client: &wit::ClientInfo,
        topic: &str,
    ) -> Result<wit::Decision> {
        Ok(match self {
            Guest::V0_1(_) => wit::Decision::Defer,
            Guest::V0_2(guest) => guest.call_authorize_control(store, client, topic).await?,
        })
    }

    pub(crate) async fn on_message(
        &self,
        store: &mut Store<State>,
        client: &wit::ClientInfo,
        message: &wit::Message,
    ) -> Result<wit::MessageAction> {
        Ok(match self {
            Guest::V0_1(guest) => {
                let message = old_message(message.clone());
                match guest
                    .call_on_message(store, &client_info(client), &message)
                    .await?
                {
                    old::MessageAction::Keep => wit::MessageAction::Keep,
                    old::MessageAction::Drop => wit::MessageAction::Drop,
                    old::MessageAction::Replace(changed) => {
                        wit::MessageAction::Replace(new_message(changed))
                    }
                }
            }
            Guest::V0_2(guest) => guest.call_on_message(store, client, message).await?,
        })
    }

    pub(crate) async fn on_event(
        &self,
        store: &mut Store<State>,
        event: &wit::Event,
    ) -> Result<()> {
        match self {
            Guest::V0_1(guest) => guest.call_on_event(store, &old_event(event)).await,
            Guest::V0_2(guest) => guest.call_on_event(store, event).await,
        }
    }

    pub(crate) async fn on_tick(&self, store: &mut Store<State>) -> Result<()> {
        match self {
            Guest::V0_1(guest) => guest.call_on_tick(store).await,
            Guest::V0_2(guest) => guest.call_on_tick(store).await,
        }
    }

    pub(crate) async fn on_control(
        &self,
        store: &mut Store<State>,
        client: &wit::ClientInfo,
        path: &str,
        payload: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        match self {
            Guest::V0_1(guest) => {
                guest
                    .call_on_control(store, &client_info(client), path, payload)
                    .await
            }
            Guest::V0_2(guest) => guest.call_on_control(store, client, path, payload).await,
        }
    }
}

/// The client as 0.1 knew it, without its connection.
fn client_info(client: &wit::ClientInfo) -> old::ClientInfo {
    old::ClientInfo {
        client_id: client.client_id.clone(),
        username: client.username.clone(),
        peer: client.peer.clone(),
    }
}

fn old_access(access: wit::Access) -> old::Access {
    match access {
        wit::Access::Read => old::Access::Read,
        wit::Access::Write => old::Access::Write,
        wit::Access::Subscribe => old::Access::Subscribe,
    }
}

fn decision(decision: old::Decision) -> wit::Decision {
    match decision {
        old::Decision::Allow => wit::Decision::Allow,
        old::Decision::Deny => wit::Decision::Deny,
        old::Decision::Defer => wit::Decision::Defer,
    }
}

fn old_qos(qos: wit::Qos) -> old::Qos {
    match qos {
        wit::Qos::AtMostOnce => old::Qos::AtMostOnce,
        wit::Qos::AtLeastOnce => old::Qos::AtLeastOnce,
        wit::Qos::ExactlyOnce => old::Qos::ExactlyOnce,
    }
}

fn new_qos(qos: old::Qos) -> wit::Qos {
    match qos {
        old::Qos::AtMostOnce => wit::Qos::AtMostOnce,
        old::Qos::AtLeastOnce => wit::Qos::AtLeastOnce,
        old::Qos::ExactlyOnce => wit::Qos::ExactlyOnce,
    }
}

fn old_message(message: wit::Message) -> old::Message {
    old::Message {
        topic: message.topic,
        payload: message.payload,
        qos: old_qos(message.qos),
        retain: message.retain,
        payload_is_utf8: message.payload_is_utf8,
        message_expiry: message.message_expiry,
        content_type: message.content_type,
        response_topic: message.response_topic,
        correlation_data: message.correlation_data,
        user_properties: message.user_properties,
    }
}

fn new_message(message: old::Message) -> wit::Message {
    wit::Message {
        topic: message.topic,
        payload: message.payload,
        qos: new_qos(message.qos),
        retain: message.retain,
        payload_is_utf8: message.payload_is_utf8,
        message_expiry: message.message_expiry,
        content_type: message.content_type,
        response_topic: message.response_topic,
        correlation_data: message.correlation_data,
        user_properties: message.user_properties,
    }
}

fn old_event(event: &wit::Event) -> old::Event {
    match event {
        wit::Event::Connected(client) => old::Event::Connected(client_info(client)),
        wit::Event::Disconnected(d) => old::Event::Disconnected(old::Disconnected {
            client: client_info(&d.client),
            reason: d.reason,
        }),
        wit::Event::Subscribed(s) => old::Event::Subscribed(old::Subscribed {
            client: client_info(&s.client),
            filter: s.filter.clone(),
            qos: old_qos(s.qos),
        }),
        wit::Event::Unsubscribed(u) => old::Event::Unsubscribed(old::Unsubscribed {
            client: client_info(&u.client),
            filter: u.filter.clone(),
        }),
        wit::Event::SessionExpired(client_id) => old::Event::SessionExpired(client_id.clone()),
    }
}

fn registration(registration: old::Registration) -> wit::Registration {
    let mut hooks = wit::Hooks::empty();
    for (old_hook, hook) in [
        (old::Hooks::AUTHENTICATE, wit::Hooks::AUTHENTICATE),
        (old::Hooks::AUTHORIZE, wit::Hooks::AUTHORIZE),
        (old::Hooks::ON_MESSAGE, wit::Hooks::ON_MESSAGE),
        (old::Hooks::ON_EVENT, wit::Hooks::ON_EVENT),
        (old::Hooks::ON_TICK, wit::Hooks::ON_TICK),
        (old::Hooks::ON_CONTROL, wit::Hooks::ON_CONTROL),
    ] {
        if registration.hooks.contains(old_hook) {
            hooks |= hook;
        }
    }
    wit::Registration {
        hooks,
        auth_methods: registration.auth_methods,
    }
}

fn auth_result(result: old::AuthResult) -> wit::AuthResult {
    match result {
        old::AuthResult::Continue(challenge) => wit::AuthResult::Continue(wit::Challenge {
            data: challenge.data,
            state: challenge.state,
        }),
        old::AuthResult::Success(username) => wit::AuthResult::Success(wit::Success {
            username,
            data: None,
        }),
        old::AuthResult::Failed(code) => wit::AuthResult::Failed(code),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_0_1_registrations_keep_their_hooks() {
        let all = old::Hooks::all();
        let converted = registration(old::Registration {
            hooks: all,
            auth_methods: vec!["M".into()],
        });
        assert_eq!(
            converted.hooks | wit::Hooks::AUTHORIZE_CONTROL,
            wit::Hooks::all()
        );
        assert!(!converted.hooks.contains(wit::Hooks::AUTHORIZE_CONTROL));
        assert_eq!(converted.auth_methods, ["M"]);
        let none = registration(old::Registration {
            hooks: old::Hooks::empty(),
            auth_methods: Vec::new(),
        });
        assert_eq!(none.hooks, wit::Hooks::empty());
    }

    #[test]
    fn test_0_1_messages_survive_the_round_trip() {
        let message = wit::Message {
            topic: "t".into(),
            payload: b"p".to_vec(),
            qos: wit::Qos::ExactlyOnce,
            retain: true,
            payload_is_utf8: true,
            message_expiry: Some(5),
            content_type: Some("text/plain".into()),
            response_topic: Some("r".into()),
            correlation_data: Some(b"c".to_vec()),
            user_properties: vec![("k".into(), "v".into())],
        };
        let back = new_message(old_message(message.clone()));
        assert_eq!(format!("{back:?}"), format!("{message:?}"));
    }
}
