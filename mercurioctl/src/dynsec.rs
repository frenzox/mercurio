//! `mercurioctl dynsec`: the commands of mercuriod's dynamic security
//! plugin, named as `mosquitto_ctrl dynsec` names them, in kebab-case.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;

use clap::{Args, Subcommand};
use serde_json::{json, Value};

use crate::client::Failure;
use crate::commands::named;

/// The ACL types a role's ACL takes.
const ACL_TYPES: [&str; 4] = [
    "publishClientSend",
    "publishClientReceive",
    "subscribeLiteral",
    "subscribePattern",
];
/// The ACL types `set-default-acl-access` takes.
const DEFAULT_ACL_TYPES: [&str; 3] = ["publishClientSend", "publishClientReceive", "subscribe"];

#[derive(Subcommand, Debug)]
pub enum Dynsec {
    /// Create a client, which logs in with its user name and password
    CreateClient {
        username: String,
        /// Its password, which other users can see in `ps`; leave it out to
        /// be asked, and give an empty one for none
        #[arg(long, conflicts_with_all = ["password_file", "password_stdin"])]
        password: Option<String>,
        #[command(flatten)]
        source: PasswordSource,
        /// A role of its own (can be repeated)
        #[arg(long = "role", value_name = "ROLENAME")]
        roles: Vec<String>,
        /// A group it belongs to (can be repeated)
        #[arg(long = "group", value_name = "GROUPNAME")]
        groups: Vec<String>,
    },
    /// Delete a client
    DeleteClient { username: String },
    /// Change a client's password
    SetClientPassword {
        username: String,
        /// The new password, which other users can see in `ps`; leave it out
        /// to be asked
        #[arg(conflicts_with_all = ["password_file", "password_stdin"])]
        password: Option<String>,
        #[command(flatten)]
        source: PasswordSource,
    },
    /// Let a disabled client log in again
    EnableClient { username: String },
    /// Stop a client from logging in, and take its access away at once
    DisableClient { username: String },
    /// Give a client a role, or change the role's priority
    AddClientRole {
        username: String,
        rolename: String,
        /// Its roles are checked highest priority first [default: -1]
        #[arg(allow_negative_numbers = true)]
        priority: Option<i64>,
    },
    /// Take a role from a client
    RemoveClientRole { username: String, rolename: String },
    /// Show a client with its roles and groups
    GetClient { username: String },
    /// List the clients' user names
    ListClients,
    /// Create a group
    CreateGroup {
        groupname: String,
        /// A role of the group (can be repeated)
        #[arg(long = "role", value_name = "ROLENAME")]
        roles: Vec<String>,
    },
    /// Delete a group
    DeleteGroup { groupname: String },
    /// Show a group with its roles and clients
    GetGroup { groupname: String },
    /// List the groups
    ListGroups,
    /// Give a group a role, or change the role's priority
    AddGroupRole {
        groupname: String,
        rolename: String,
        /// Its roles are checked highest priority first [default: -1]
        #[arg(allow_negative_numbers = true)]
        priority: Option<i64>,
    },
    /// Take a role from a group
    RemoveGroupRole { groupname: String, rolename: String },
    /// Add a client to a group, or change the group's priority for it
    AddGroupClient {
        groupname: String,
        username: String,
        /// The client's groups are checked highest priority first
        /// [default: -1]
        #[arg(allow_negative_numbers = true)]
        priority: Option<i64>,
    },
    /// Remove a client from a group
    RemoveGroupClient { groupname: String, username: String },
    /// Give clients without a user name the roles of this group
    SetAnonymousGroup { groupname: String },
    /// Show the group of clients without a user name
    GetAnonymousGroup,
    /// Create a role, without ACLs
    CreateRole { rolename: String },
    /// Delete a role
    DeleteRole { rolename: String },
    /// Show a role with its ACLs
    GetRole { rolename: String },
    /// List the roles
    ListRoles,
    /// Add an ACL to a role, replacing one of the same type and topic
    AddRoleAcl {
        rolename: String,
        #[arg(value_parser = ACL_TYPES)]
        acltype: String,
        /// Topic filter
        topic: String,
        #[arg(value_parser = ["allow", "deny"])]
        access: String,
        /// A role's ACLs are checked highest priority first [default: 0]
        #[arg(allow_negative_numbers = true)]
        priority: Option<i64>,
    },
    /// Remove a role's ACL
    RemoveRoleAcl {
        rolename: String,
        #[arg(value_parser = ACL_TYPES)]
        acltype: String,
        topic: String,
    },
    /// Set what clients may do where no ACL decides
    SetDefaultAclAccess {
        #[arg(value_parser = DEFAULT_ACL_TYPES)]
        acltype: String,
        #[arg(value_parser = ["allow", "deny"])]
        access: String,
    },
    /// Show what clients may do where no ACL decides
    GetDefaultAclAccess,
}

/// Where a new client password comes from, for scripts, instead of the
/// command line or a prompt.
#[derive(Args, Debug)]
pub struct PasswordSource {
    /// Read the password from the first line of this file
    #[arg(long, value_name = "PATH", conflicts_with = "password_stdin")]
    password_file: Option<PathBuf>,
    /// Read the password from the first line of standard input
    #[arg(long)]
    password_stdin: bool,
}

impl PasswordSource {
    /// The password from the file or standard input, if one was named.
    fn read(&self) -> Result<Option<String>, Failure> {
        match &self.password_file {
            Some(path) => {
                let file = std::fs::File::open(path)
                    .map_err(|e| Failure::Usage(format!("can't read {}: {e}", path.display())))?;
                self.read_from(&mut BufReader::new(file))
            }
            None => self.read_from(&mut std::io::stdin().lock()),
        }
    }

    /// The first line of `input`, when the password comes from a file or
    /// standard input; `input` is standard input or the opened file.
    pub fn read_from(&self, input: &mut dyn BufRead) -> Result<Option<String>, Failure> {
        if self.password_file.is_none() && !self.password_stdin {
            return Ok(None);
        }
        let mut line = String::new();
        input
            .read_line(&mut line)
            .map_err(|e| Failure::Usage(format!("can't read the password: {e}")))?;
        let line = line.strip_suffix('\n').unwrap_or(&line);
        Ok(Some(line.strip_suffix('\r').unwrap_or(line).to_string()))
    }
}

/// The password given on the command line, read from where `source` names,
/// or asked for with `read_password`.
fn new_password(
    given: Option<String>,
    source: &PasswordSource,
    username: &str,
    read_password: impl FnOnce(&str) -> Result<String, Failure>,
) -> Result<String, Failure> {
    match given.or(source.read()?) {
        Some(password) => Ok(password),
        None => read_password(username),
    }
}

/// The command object `command` sends. `read_password` asks for a password
/// left out of the command line, for the client it names.
pub fn command(
    command: Dynsec,
    read_password: impl FnOnce(&str) -> Result<String, Failure>,
) -> Result<Value, Failure> {
    let rolenames = |names: Vec<String>| -> Vec<Value> {
        names
            .into_iter()
            .map(|rolename| json!({ "rolename": rolename }))
            .collect()
    };
    Ok(match command {
        Dynsec::CreateClient {
            username,
            password,
            source,
            roles,
            groups,
        } => {
            let password = new_password(password, &source, &username, read_password)?;
            // Without one the client can't log in until it gets one
            let password = (!password.is_empty()).then_some(password);
            let groups: Vec<Value> = groups
                .into_iter()
                .map(|groupname| json!({ "groupname": groupname }))
                .collect();
            named(
                "createClient",
                json!({
                    "username": username,
                    "password": password,
                    "roles": rolenames(roles),
                    "groups": groups,
                }),
            )
        }
        Dynsec::DeleteClient { username } => named("deleteClient", json!({ "username": username })),
        Dynsec::SetClientPassword {
            username,
            password,
            source,
        } => {
            let password = new_password(password, &source, &username, read_password)?;
            if password.is_empty() {
                return Err(Failure::Usage("the password can't be empty".to_string()));
            }
            named(
                "setClientPassword",
                json!({ "username": username, "password": password }),
            )
        }
        Dynsec::EnableClient { username } => named("enableClient", json!({ "username": username })),
        Dynsec::DisableClient { username } => {
            named("disableClient", json!({ "username": username }))
        }
        Dynsec::AddClientRole {
            username,
            rolename,
            priority,
        } => named(
            "addClientRole",
            json!({ "username": username, "rolename": rolename, "priority": priority }),
        ),
        Dynsec::RemoveClientRole { username, rolename } => named(
            "removeClientRole",
            json!({ "username": username, "rolename": rolename }),
        ),
        Dynsec::GetClient { username } => named("getClient", json!({ "username": username })),
        Dynsec::ListClients => named("listClients", json!({})),
        Dynsec::CreateGroup { groupname, roles } => named(
            "createGroup",
            json!({ "groupname": groupname, "roles": rolenames(roles) }),
        ),
        Dynsec::DeleteGroup { groupname } => {
            named("deleteGroup", json!({ "groupname": groupname }))
        }
        Dynsec::GetGroup { groupname } => named("getGroup", json!({ "groupname": groupname })),
        Dynsec::ListGroups => named("listGroups", json!({})),
        Dynsec::AddGroupRole {
            groupname,
            rolename,
            priority,
        } => named(
            "addGroupRole",
            json!({ "groupname": groupname, "rolename": rolename, "priority": priority }),
        ),
        Dynsec::RemoveGroupRole {
            groupname,
            rolename,
        } => named(
            "removeGroupRole",
            json!({ "groupname": groupname, "rolename": rolename }),
        ),
        Dynsec::AddGroupClient {
            groupname,
            username,
            priority,
        } => named(
            "addGroupClient",
            json!({ "groupname": groupname, "username": username, "priority": priority }),
        ),
        Dynsec::RemoveGroupClient {
            groupname,
            username,
        } => named(
            "removeGroupClient",
            json!({ "groupname": groupname, "username": username }),
        ),
        Dynsec::SetAnonymousGroup { groupname } => {
            named("setAnonymousGroup", json!({ "groupname": groupname }))
        }
        Dynsec::GetAnonymousGroup => named("getAnonymousGroup", json!({})),
        Dynsec::CreateRole { rolename } => named("createRole", json!({ "rolename": rolename })),
        Dynsec::DeleteRole { rolename } => named("deleteRole", json!({ "rolename": rolename })),
        Dynsec::GetRole { rolename } => named("getRole", json!({ "rolename": rolename })),
        Dynsec::ListRoles => named("listRoles", json!({})),
        Dynsec::AddRoleAcl {
            rolename,
            acltype,
            topic,
            access,
            priority,
        } => named(
            "addRoleACL",
            json!({
                "rolename": rolename,
                "acltype": acltype,
                "topic": topic,
                "allow": (access == "allow"),
                "priority": priority,
            }),
        ),
        Dynsec::RemoveRoleAcl {
            rolename,
            acltype,
            topic,
        } => named(
            "removeRoleACL",
            json!({ "rolename": rolename, "acltype": acltype, "topic": topic }),
        ),
        Dynsec::SetDefaultAclAccess { acltype, access } => named(
            "setDefaultACLAccess",
            json!({ "acls": [{ "acltype": acltype, "allow": (access == "allow") }] }),
        ),
        Dynsec::GetDefaultAclAccess => named("getDefaultACLAccess", json!({})),
    })
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::{json, Value};

    use super::Dynsec;
    use crate::cli::{Cli, Command};
    use crate::client::Failure;

    fn parse(args: &[&str]) -> Dynsec {
        let argv = ["mercurioctl", "dynsec"]
            .into_iter()
            .chain(args.iter().copied());
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Dynsec(command) => command,
            other => panic!("parsed as {other:?}"),
        }
    }

    /// The command object `args` send, when no password has to be asked for.
    fn sent(args: &[&str]) -> Value {
        super::command(parse(args), |_| panic!("asked for a password")).unwrap()
    }

    #[test]
    fn client_commands() {
        assert_eq!(
            sent(&[
                "create-client",
                "alice",
                "--password",
                "secret",
                "--role",
                "sensors",
                "--group",
                "field",
            ]),
            json!({
                "command": "createClient", "username": "alice", "password": "secret",
                "roles": [{"rolename": "sensors"}], "groups": [{"groupname": "field"}],
            })
        );
        assert_eq!(
            sent(&["delete-client", "alice"]),
            json!({"command": "deleteClient", "username": "alice"})
        );
        assert_eq!(
            sent(&["set-client-password", "alice", "n3w"]),
            json!({"command": "setClientPassword", "username": "alice", "password": "n3w"})
        );
        assert_eq!(
            sent(&["enable-client", "alice"]),
            json!({"command": "enableClient", "username": "alice"})
        );
        assert_eq!(
            sent(&["disable-client", "alice"]),
            json!({"command": "disableClient", "username": "alice"})
        );
        assert_eq!(
            sent(&["add-client-role", "alice", "sensors"]),
            json!({"command": "addClientRole", "username": "alice", "rolename": "sensors"})
        );
        assert_eq!(
            sent(&["remove-client-role", "alice", "sensors"]),
            json!({"command": "removeClientRole", "username": "alice", "rolename": "sensors"})
        );
        assert_eq!(
            sent(&["get-client", "alice"]),
            json!({"command": "getClient", "username": "alice"})
        );
        assert_eq!(sent(&["list-clients"]), json!({"command": "listClients"}));
    }

    #[test]
    fn group_commands() {
        assert_eq!(
            sent(&["create-group", "field", "--role", "sensors"]),
            json!({"command": "createGroup", "groupname": "field", "roles": [{"rolename": "sensors"}]})
        );
        assert_eq!(
            sent(&["delete-group", "field"]),
            json!({"command": "deleteGroup", "groupname": "field"})
        );
        assert_eq!(
            sent(&["get-group", "field"]),
            json!({"command": "getGroup", "groupname": "field"})
        );
        assert_eq!(sent(&["list-groups"]), json!({"command": "listGroups"}));
        assert_eq!(
            sent(&["add-group-role", "field", "sensors"]),
            json!({"command": "addGroupRole", "groupname": "field", "rolename": "sensors"})
        );
        assert_eq!(
            sent(&["remove-group-role", "field", "sensors"]),
            json!({"command": "removeGroupRole", "groupname": "field", "rolename": "sensors"})
        );
        assert_eq!(
            sent(&["add-group-client", "field", "alice"]),
            json!({"command": "addGroupClient", "groupname": "field", "username": "alice"})
        );
        assert_eq!(
            sent(&["remove-group-client", "field", "alice"]),
            json!({"command": "removeGroupClient", "groupname": "field", "username": "alice"})
        );
        assert_eq!(
            sent(&["set-anonymous-group", "guests"]),
            json!({"command": "setAnonymousGroup", "groupname": "guests"})
        );
        assert_eq!(
            sent(&["get-anonymous-group"]),
            json!({"command": "getAnonymousGroup"})
        );
    }

    #[test]
    fn role_and_acl_commands() {
        assert_eq!(
            sent(&["create-role", "sensors"]),
            json!({"command": "createRole", "rolename": "sensors"})
        );
        assert_eq!(
            sent(&["delete-role", "sensors"]),
            json!({"command": "deleteRole", "rolename": "sensors"})
        );
        assert_eq!(
            sent(&["get-role", "sensors"]),
            json!({"command": "getRole", "rolename": "sensors"})
        );
        assert_eq!(sent(&["list-roles"]), json!({"command": "listRoles"}));
        assert_eq!(
            sent(&[
                "add-role-acl",
                "sensors",
                "publishClientSend",
                "sensors/#",
                "allow",
                "5"
            ]),
            json!({
                "command": "addRoleACL", "rolename": "sensors", "acltype": "publishClientSend",
                "topic": "sensors/#", "allow": true, "priority": 5,
            })
        );
        assert_eq!(
            sent(&[
                "add-role-acl",
                "sensors",
                "subscribePattern",
                "admin/#",
                "deny"
            ]),
            json!({
                "command": "addRoleACL", "rolename": "sensors", "acltype": "subscribePattern",
                "topic": "admin/#", "allow": false,
            })
        );
        assert_eq!(
            sent(&["remove-role-acl", "sensors", "subscribePattern", "admin/#"]),
            json!({
                "command": "removeRoleACL", "rolename": "sensors", "acltype": "subscribePattern",
                "topic": "admin/#",
            })
        );
        assert_eq!(
            sent(&["set-default-acl-access", "subscribe", "allow"]),
            json!({"command": "setDefaultACLAccess", "acls": [{"acltype": "subscribe", "allow": true}]})
        );
        assert_eq!(
            sent(&["get-default-acl-access"]),
            json!({"command": "getDefaultACLAccess"})
        );
        for wrong in [
            &["add-role-acl", "r", "publish", "t", "allow"][..],
            &["add-role-acl", "r", "subscribePattern", "t", "maybe"],
            &["set-default-acl-access", "subscribePattern", "allow"],
        ] {
            let argv = ["mercurioctl", "dynsec"]
                .into_iter()
                .chain(wrong.iter().copied());
            assert!(Cli::try_parse_from(argv).is_err(), "{wrong:?}");
        }
    }

    #[test]
    fn roles_and_groups_take_a_priority() {
        assert_eq!(
            sent(&["add-client-role", "alice", "sensors", "5"]),
            json!({
                "command": "addClientRole", "username": "alice", "rolename": "sensors",
                "priority": 5,
            })
        );
        assert_eq!(
            sent(&["add-group-role", "field", "sensors", "-2"]),
            json!({
                "command": "addGroupRole", "groupname": "field", "rolename": "sensors",
                "priority": -2,
            })
        );
        assert_eq!(
            sent(&["add-group-client", "field", "alice", "3"]),
            json!({
                "command": "addGroupClient", "groupname": "field", "username": "alice",
                "priority": 3,
            })
        );
        assert_eq!(
            sent(&["add-role-acl", "r", "subscribePattern", "t", "deny", "-1"])["priority"],
            -1
        );
        let argv = ["mercurioctl", "dynsec", "add-client-role", "a", "r", "high"];
        assert!(Cli::try_parse_from(argv).is_err());
    }

    #[test]
    fn passwords_come_from_a_file_or_standard_input() {
        let file = std::env::temp_dir().join(format!("mercurioctl-pw-{}", std::process::id()));
        std::fs::write(&file, "from-file\r\nignored\n").unwrap();
        let path = file.to_str().unwrap();
        let created = super::command(
            parse(&["create-client", "alice", "--password-file", path]),
            |_| panic!("asked for a password"),
        )
        .unwrap();
        assert_eq!(created["password"], "from-file");
        let changed = super::command(
            parse(&["set-client-password", "alice", "--password-file", path]),
            |_| panic!("asked for a password"),
        )
        .unwrap();
        std::fs::remove_file(&file).unwrap();
        assert_eq!(changed["password"], "from-file");

        let missing = super::command(
            parse(&[
                "set-client-password",
                "alice",
                "--password-file",
                "/nonexistent/pw",
            ]),
            |_| panic!("asked for a password"),
        );
        assert!(matches!(missing, Err(Failure::Usage(_))), "{missing:?}");

        let source = match parse(&["create-client", "alice", "--password-stdin"]) {
            Dynsec::CreateClient { source, .. } => source,
            other => panic!("parsed as {other:?}"),
        };
        let mut stdin = std::io::Cursor::new(b"typed\nrest".to_vec());
        assert_eq!(
            source.read_from(&mut stdin).unwrap().as_deref(),
            Some("typed")
        );

        for both in [
            &["create-client", "a", "--password", "x", "--password-stdin"][..],
            &[
                "create-client",
                "a",
                "--password-file",
                "f",
                "--password-stdin",
            ],
            &["set-client-password", "a", "x", "--password-file", "f"],
        ] {
            let argv = ["mercurioctl", "dynsec"]
                .into_iter()
                .chain(both.iter().copied());
            assert!(Cli::try_parse_from(argv).is_err(), "{both:?}");
        }
    }

    #[test]
    fn passwords_left_out_are_asked_for() {
        let typed = super::command(parse(&["create-client", "alice"]), |username| {
            assert_eq!(username, "alice");
            Ok("typed".to_string())
        })
        .unwrap();
        assert_eq!(typed["password"], "typed");

        let without =
            super::command(parse(&["create-client", "alice"]), |_| Ok(String::new())).unwrap();
        assert_eq!(
            without,
            json!({"command": "createClient", "username": "alice", "roles": [], "groups": []})
        );

        assert_eq!(
            super::command(parse(&["set-client-password", "alice"]), |_| Ok(
                String::new()
            )),
            Err(Failure::Usage("the password can't be empty".to_string()))
        );
    }
}
