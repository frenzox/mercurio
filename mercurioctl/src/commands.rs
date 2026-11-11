//! What each command line sends: the topic, and the command object with its
//! arguments.

use serde_json::{json, Map, Value};

use crate::cli::{
    BanCommand, BrokerCommand, ClientCommand, Command, ListCommand, PageArgs, RetainedCommand,
    SubscriptionCommand,
};
use crate::client::Failure;
use crate::dynsec;

/// Where the broker's own commands go.
pub const BROKER_TOPIC: &str = "$CONTROL/broker/v1";
/// Where dynamic security commands go.
pub const DYNSEC_TOPIC: &str = "$CONTROL/dynamic-security/v1";

/// One command and the topic it goes to.
#[derive(Debug, PartialEq)]
pub struct Request {
    pub topic: &'static str,
    /// The command object: `command` and its arguments
    pub command: Value,
}

impl Request {
    /// The command's name in the protocol, such as `listClients`.
    pub fn name(&self) -> &str {
        self.command["command"].as_str().unwrap_or_default()
    }
}

/// What `command` sends. `read_password` asks for a password a dynamic
/// security command needs and wasn't given, for the client it names.
pub fn request(
    command: Command,
    read_password: impl FnOnce(&str) -> Result<String, Failure>,
) -> Result<Request, Failure> {
    let command = match command {
        Command::Dynsec(c) => {
            return Ok(Request {
                topic: DYNSEC_TOPIC,
                command: dynsec::command(c, read_password)?,
            })
        }
        Command::Client(c) => client(c),
        Command::Ban(c) => ban(c),
        Command::Retained(c) => retained(c),
        Command::Subscription(c) => subscription(c),
        Command::Broker(c) => broker(c),
        Command::Listener(ListCommand::List) => named("listListeners", json!({})),
        Command::Bridge(ListCommand::List) => named("listBridges", json!({})),
        Command::Plugin(ListCommand::List) => named("listPlugins", json!({})),
        Command::Completions { .. } => {
            return Err(Failure::Usage(
                "completions aren't a broker command".to_string(),
            ))
        }
    };
    Ok(Request {
        topic: BROKER_TOPIC,
        command,
    })
}

/// The command object for `name`, with the members of `args` that aren't
/// null.
pub fn named(name: &str, args: Value) -> Value {
    let mut object = Map::new();
    object.insert("command".to_string(), name.into());
    if let Value::Object(args) = args {
        object.extend(args.into_iter().filter(|(_, value)| !value.is_null()));
    }
    Value::Object(object)
}

fn paged(name: &str, mut args: Value, page: PageArgs) -> Value {
    args["offset"] = json!(page.offset);
    args["count"] = json!(page.count);
    named(name, args)
}

fn client(c: ClientCommand) -> Value {
    match c {
        ClientCommand::List {
            connected,
            offline,
            prefix,
            user,
            address,
            page,
        } => paged(
            "listClients",
            json!({
                "connected": (connected || offline).then_some(connected),
                "clientidPrefix": prefix,
                "username": user,
                "address": address,
            }),
            page,
        ),
        ClientCommand::Show { clientid } => named("getClient", json!({ "clientid": clientid })),
        ClientCommand::Disconnect {
            clientid,
            no_will,
            reason_code,
        } => named(
            "disconnectClient",
            json!({ "clientid": clientid, "sendWill": !no_will, "reasonCode": reason_code }),
        ),
        ClientCommand::DeleteSession {
            clientid,
            disconnect,
            ..
        } => named(
            "deleteSession",
            json!({ "clientid": clientid, "disconnect": disconnect }),
        ),
    }
}

fn ban(c: BanCommand) -> Value {
    match c {
        BanCommand::Add {
            kind,
            value,
            prefix,
            duration,
            reason,
        } => named(
            "addBan",
            json!({
                "type": kind.as_str(),
                "value": value,
                "prefix": prefix,
                "duration": duration,
                "reason": reason,
            }),
        ),
        BanCommand::Remove {
            kind,
            value,
            prefix,
        } => named(
            "removeBan",
            json!({ "type": kind.as_str(), "value": value, "prefix": prefix }),
        ),
        BanCommand::List { page } => paged("listBans", json!({}), page),
    }
}

fn retained(c: RetainedCommand) -> Value {
    match c {
        RetainedCommand::List { filter, page } => {
            paged("listRetained", json!({ "filter": filter }), page)
        }
        RetainedCommand::Show { topic } => named("getRetained", json!({ "topic": topic })),
        RetainedCommand::Delete { filter, .. } => {
            named("deleteRetained", json!({ "filter": filter }))
        }
    }
}

fn subscription(c: SubscriptionCommand) -> Value {
    match c {
        SubscriptionCommand::List { prefix, page } => {
            paged("listSubscriptions", json!({ "filterPrefix": prefix }), page)
        }
        SubscriptionCommand::Subscribers { filter } => {
            named("getSubscribers", json!({ "filter": filter }))
        }
    }
}

fn broker(c: BrokerCommand) -> Value {
    match c {
        BrokerCommand::Info => named("getBrokerInfo", json!({})),
        BrokerCommand::Config => named("getConfig", json!({})),
        BrokerCommand::Reload => named("reload", json!({})),
        BrokerCommand::LogLevel { level: None } => named("getLogLevel", json!({})),
        BrokerCommand::LogLevel { level: Some(level) } => {
            named("setLogLevel", json!({ "level": level }))
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::{json, Value};

    use super::{request, BROKER_TOPIC};
    use crate::cli::Cli;

    /// The command object `args` send to the broker.
    fn sent(args: &[&str]) -> Value {
        let argv = std::iter::once("mercurioctl").chain(args.iter().copied());
        let request = request(Cli::try_parse_from(argv).unwrap().command, |_| {
            panic!("asked for a password")
        })
        .unwrap();
        assert_eq!(request.topic, BROKER_TOPIC);
        request.command
    }

    #[test]
    fn client_commands() {
        assert_eq!(sent(&["client", "list"]), json!({"command": "listClients"}));
        assert_eq!(
            sent(&[
                "client",
                "list",
                "--connected",
                "--prefix",
                "sensor-",
                "--user",
                "alice",
                "--address",
                "10.0.0.0/8",
                "--offset",
                "100",
                "--count",
                "50",
            ]),
            json!({
                "command": "listClients", "connected": true, "clientidPrefix": "sensor-",
                "username": "alice", "address": "10.0.0.0/8", "offset": 100, "count": 50,
            })
        );
        assert_eq!(
            sent(&["client", "list", "--offline"]),
            json!({"command": "listClients", "connected": false})
        );
        assert_eq!(
            sent(&["client", "show", "s1"]),
            json!({"command": "getClient", "clientid": "s1"})
        );
        assert_eq!(
            sent(&["client", "disconnect", "s1"]),
            json!({"command": "disconnectClient", "clientid": "s1", "sendWill": true})
        );
        assert_eq!(
            sent(&[
                "client",
                "disconnect",
                "s1",
                "--no-will",
                "--reason-code",
                "0x8e"
            ]),
            json!({
                "command": "disconnectClient", "clientid": "s1", "sendWill": false,
                "reasonCode": 142,
            })
        );
        assert_eq!(
            sent(&["client", "delete-session", "s1", "--disconnect"]),
            json!({"command": "deleteSession", "clientid": "s1", "disconnect": true})
        );
    }

    #[test]
    fn ban_commands() {
        assert_eq!(
            sent(&[
                "ban",
                "add",
                "clientid",
                "evil-",
                "--prefix",
                "--duration",
                "3600",
                "--reason",
                "flooding",
            ]),
            json!({
                "command": "addBan", "type": "clientid", "value": "evil-", "prefix": true,
                "duration": 3600, "reason": "flooding",
            })
        );
        assert_eq!(
            sent(&["ban", "add", "address", "10.0.0.0/8"]),
            json!({"command": "addBan", "type": "address", "value": "10.0.0.0/8", "prefix": false})
        );
        assert_eq!(
            sent(&["ban", "remove", "username", "bob"]),
            json!({"command": "removeBan", "type": "username", "value": "bob", "prefix": false})
        );
        assert_eq!(sent(&["ban", "list"]), json!({"command": "listBans"}));
        assert!(Cli::try_parse_from(["mercurioctl", "ban", "add", "ip", "10.0.0.1"]).is_err());
    }

    #[test]
    fn retained_and_subscription_commands() {
        assert_eq!(
            sent(&["retained", "list"]),
            json!({"command": "listRetained", "filter": "#"})
        );
        assert_eq!(
            sent(&["retained", "list", "sensors/#", "--count", "10"]),
            json!({"command": "listRetained", "filter": "sensors/#", "count": 10})
        );
        assert_eq!(
            sent(&["retained", "show", "a/b"]),
            json!({"command": "getRetained", "topic": "a/b"})
        );
        assert_eq!(
            sent(&["retained", "delete", "a/#"]),
            json!({"command": "deleteRetained", "filter": "a/#"})
        );
        assert_eq!(
            sent(&["subscription", "list", "--prefix", "sensors/"]),
            json!({"command": "listSubscriptions", "filterPrefix": "sensors/"})
        );
        assert_eq!(
            sent(&["subscription", "subscribers", "a/#"]),
            json!({"command": "getSubscribers", "filter": "a/#"})
        );
    }

    #[test]
    fn broker_and_introspection_commands() {
        assert_eq!(
            sent(&["broker", "info"]),
            json!({"command": "getBrokerInfo"})
        );
        assert_eq!(sent(&["broker", "config"]), json!({"command": "getConfig"}));
        assert_eq!(sent(&["broker", "reload"]), json!({"command": "reload"}));
        assert_eq!(
            sent(&["broker", "log-level"]),
            json!({"command": "getLogLevel"})
        );
        assert_eq!(
            sent(&["broker", "log-level", "info,mercurio_server::session=debug"]),
            json!({"command": "setLogLevel", "level": "info,mercurio_server::session=debug"})
        );
        assert_eq!(
            sent(&["listener", "list"]),
            json!({"command": "listListeners"})
        );
        assert_eq!(sent(&["bridge", "list"]), json!({"command": "listBridges"}));
        assert_eq!(sent(&["plugin", "list"]), json!({"command": "listPlugins"}));
    }

    #[test]
    fn dynsec_commands_go_to_dynamic_security() {
        let cli = Cli::try_parse_from(["mercurioctl", "dynsec", "list-roles"]).unwrap();
        let request = request(cli.command, |_| panic!("asked for a password")).unwrap();
        assert_eq!(request.topic, super::DYNSEC_TOPIC);
        assert_eq!(request.command, json!({"command": "listRoles"}));
    }
}
