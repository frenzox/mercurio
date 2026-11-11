//! Dynamic security: clients, roles and access control managed while the
//! broker runs, with the JSON commands of mosquitto's dynamic security
//! plugin published to `$CONTROL/dynamic-security/v1`. The state is kept in
//! a JSON file, saved after every change.
//!
//! A client's roles are its own and those of its groups; clients without a
//! user name get the roles of the anonymous group, when one is set. A user
//! name with no client gets no access.

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Mutex, OnceLock, RwLock},
};

use argon2::{Argon2, PasswordVerifier};
use bytes::Bytes;
use mercurio_core::topic::{filter_covers, topic_matches_filter};
use mercurio_server::admin::{Admin, ClientQuery, DisconnectOptions, Paging, SessionKey, MAX_PAGE};
use mercurio_server::auth::{ScramCredentialStore, StoredCredentials};
use mercurio_server::plugin::{
    ready, Access, BoxFuture, ClientInfo, Credentials, Decision, Plugin, PluginKind, PluginStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::error;

use crate::credentials::{self, unknown_user_hash};
use crate::reload::Reload;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Data {
    #[serde(default)]
    default_acl_access: DefaultAccess,
    #[serde(default)]
    clients: Vec<Client>,
    #[serde(default)]
    roles: Vec<Role>,
    #[serde(default)]
    groups: Vec<Group>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    anonymous_group: Option<String>,
}

/// Access of clients no ACL decides for, anonymous ones included; by
/// default, as in mosquitto, only receiving is allowed. It never lets a
/// client publish to `$CONTROL` topics, which stay closed unless an ACL
/// opens them.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct DefaultAccess {
    publish_client_send: bool,
    publish_client_receive: bool,
    subscribe: bool,
}

impl Default for DefaultAccess {
    fn default() -> Self {
        Self {
            publish_client_send: false,
            publish_client_receive: true,
            subscribe: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Group {
    groupname: String,
    #[serde(flatten)]
    description: Description,
    /// By priority, highest first
    #[serde(default)]
    roles: Vec<Ranked>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Client {
    username: String,
    #[serde(flatten)]
    description: Description,
    /// Argon2 hash, or the `$7$` PBKDF2 hash of a client imported from
    /// mosquitto; a client without one can't log in
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    /// SCRAM-SHA-256 credentials set with the password, see
    /// [`credentials::scram_entry`]; only for clients whose password was
    /// set since SCRAM support, and ignored once the password changes
    /// without them
    #[serde(default, skip_serializing_if = "Option::is_none")]
    scram: Option<String>,
    /// The only client id it may connect with, when set
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clientid: Option<String>,
    #[serde(default)]
    disabled: bool,
    /// By priority, highest first
    #[serde(default)]
    roles: Vec<Ranked>,
    /// By priority, highest first
    #[serde(default)]
    groups: Vec<Ranked>,
}

/// The free text mosquitto lets clients, groups and roles carry.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Description {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    textname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    textdescription: Option<String>,
}

impl Description {
    /// This one with the `textname` and `textdescription` of `args`, those
    /// absent left as they are and empty ones cleared.
    fn modified(&self, args: &Value) -> Result<Self, String> {
        let field = |name: &str, current: &Option<String>| match args.get(name) {
            None | Some(Value::Null) => Ok(current.clone()),
            Some(Value::String(text)) => Ok((!text.is_empty()).then(|| text.clone())),
            Some(_) => Err(format!("Invalid {name}")),
        };
        Ok(Self {
            textname: field("textname", &self.textname)?,
            textdescription: field("textdescription", &self.textdescription)?,
        })
    }
}

/// The priority of a role or group that mosquitto gives none.
const DEFAULT_PRIORITY: i64 = -1;

/// A role or group held by a client or group, with the priority that
/// orders it among the others held. Stored as its bare name when the
/// priority is the default, as files of mercuriod 0.4.5 and earlier hold
/// every one.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ranked {
    name: String,
    priority: i64,
}

impl Ranked {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            priority: DEFAULT_PRIORITY,
        }
    }

    /// The entry of a `get` answer, under `key`; the default priority is
    /// left out, as mosquitto does.
    fn to_json(&self, key: &str) -> Value {
        let mut entry = json!({ key: self.name });
        if self.priority != DEFAULT_PRIORITY {
            entry["priority"] = self.priority.into();
        }
        entry
    }
}

impl Serialize for Ranked {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if self.priority == DEFAULT_PRIORITY {
            return serializer.serialize_str(&self.name);
        }
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry("name", &self.name)?;
        map.serialize_entry("priority", &self.priority)?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for Ranked {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Stored {
            Name(String),
            Ranked {
                name: String,
                #[serde(default = "default_priority")]
                priority: i64,
            },
        }
        Ok(match Stored::deserialize(deserializer)? {
            Stored::Name(name) => Ranked::new(name),
            Stored::Ranked { name, priority } => Ranked { name, priority },
        })
    }
}

fn default_priority() -> i64 {
    DEFAULT_PRIORITY
}

/// Adds `name` to `list` with `priority`, or gives it that priority when
/// it is there, keeping `list` ordered.
fn rank(list: &mut Vec<Ranked>, name: String, priority: i64) {
    list.retain(|entry| entry.name != name);
    list.push(Ranked { name, priority });
    by_priority(list);
}

/// Orders `list` by priority, highest first; entries of equal priority
/// keep the order they were added in.
fn by_priority(list: &mut [Ranked]) {
    list.sort_by_key(|entry| std::cmp::Reverse(entry.priority));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Role {
    rolename: String,
    #[serde(flatten)]
    description: Description,
    /// By priority, highest first
    #[serde(default)]
    acls: Vec<Acl>,
}

impl Role {
    fn order(&mut self) {
        self.acls.sort_by_key(|acl| std::cmp::Reverse(acl.priority));
    }

    /// What the first of its ACLs that applies says, if any.
    fn decide(&self, topic: &str, access: Access) -> Option<bool> {
        self.acls
            .iter()
            .find(|acl| acl.applies(topic, access))
            .map(|acl| acl.allow)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Acl {
    acltype: AclType,
    topic: String,
    #[serde(default)]
    priority: i64,
    allow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum AclType {
    PublishClientSend,
    PublishClientReceive,
    SubscribePattern,
    SubscribeLiteral,
}

impl Acl {
    fn applies(&self, topic: &str, access: Access) -> bool {
        match (self.acltype, access) {
            (AclType::PublishClientSend, Access::Write)
            | (AclType::PublishClientReceive, Access::Read) => {
                topic_matches_filter(topic, &self.topic)
            }
            (AclType::SubscribePattern, Access::Subscribe) => filter_covers(&self.topic, topic),
            (AclType::SubscribeLiteral, Access::Subscribe) => topic == self.topic,
            _ => false,
        }
    }
}

impl Data {
    fn client(&mut self, username: &str) -> Result<&mut Client, String> {
        self.clients
            .iter_mut()
            .find(|c| c.username == username)
            .ok_or_else(|| "Client not found".to_string())
    }

    fn role(&mut self, rolename: &str) -> Result<&mut Role, String> {
        self.roles
            .iter_mut()
            .find(|r| r.rolename == rolename)
            .ok_or_else(|| "Role not found".to_string())
    }

    fn group(&mut self, groupname: &str) -> Result<&mut Group, String> {
        self.groups
            .iter_mut()
            .find(|g| g.groupname == groupname)
            .ok_or_else(|| "Group not found".to_string())
    }

    /// Orders every list a check walks, as a file written by hand or by an
    /// earlier version may not be.
    fn order(&mut self) {
        for role in &mut self.roles {
            role.order();
        }
        for client in &mut self.clients {
            by_priority(&mut client.roles);
            by_priority(&mut client.groups);
        }
        for group in &mut self.groups {
            by_priority(&mut group.roles);
        }
    }

    /// The roles of `client` in the order mosquitto checks them: its own,
    /// then those of each of its groups; without a client, those of the
    /// anonymous group. Each list is ordered by priority.
    fn roles_of<'a>(&'a self, client: Option<&'a Client>) -> impl Iterator<Item = &'a Role> {
        let own = client.into_iter().flat_map(|c| &c.roles);
        let groups: Vec<&str> = match client {
            Some(client) => client.groups.iter().map(|g| g.name.as_str()).collect(),
            None => self.anonymous_group.as_deref().into_iter().collect(),
        };
        let grouped = groups
            .into_iter()
            .filter_map(|name| self.groups.iter().find(|g| g.groupname == name))
            .flat_map(|group| &group.roles);
        own.chain(grouped)
            .filter_map(|held| self.roles.iter().find(|r| r.rolename == held.name))
    }

    fn authorize(&self, username: Option<&str>, topic: &str, access: Access) -> bool {
        self.decide(username, topic, access)
            .unwrap_or(match access {
                Access::Write => self.default_acl_access.publish_client_send,
                Access::Read => self.default_acl_access.publish_client_receive,
                Access::Subscribe => self.default_acl_access.subscribe,
                // A kind of access added later is denied until handled
                _ => false,
            })
    }

    /// Whether the client may publish to `$CONTROL` topic `topic`: only an
    /// ACL opens it, never the default access, which may be permissive for
    /// ordinary topics.
    fn authorize_control(&self, username: Option<&str>, topic: &str) -> bool {
        self.decide(username, topic, Access::Write).unwrap_or(false)
    }

    /// What the client's ACLs say about `access` to `topic`, None when no
    /// ACL applies. As in mosquitto, the first role in [`Self::roles_of`]'s
    /// order with an ACL that applies decides, by its first such ACL.
    fn decide(&self, username: Option<&str>, topic: &str, access: Access) -> Option<bool> {
        // A client disabled or deleted while connected loses its access at
        // once; a user name from elsewhere, such as a client certificate,
        // needs a client of that name, as in mosquitto
        let client = match username {
            Some(username) => match self.clients.iter().find(|c| c.username == username) {
                Some(client) if !client.disabled => Some(client),
                _ => return Some(false),
            },
            None => None,
        };
        self.roles_of(client)
            .find_map(|role| role.decide(topic, access))
    }

    /// The clients whose access `command` may change, to disconnect once it
    /// is applied, as mosquitto does: they log in again under the access
    /// they now have, and their subscriptions are checked again.
    fn affected_by(&self, command: &str, args: &Value) -> Affected {
        let text = |name: &str| args.get(name).and_then(Value::as_str);
        let mut affected = Affected::default();
        match command {
            "deleteClient" | "disableClient" | "setClientPassword" | "addClientRole"
            | "removeClientRole" | "addGroupClient" | "removeGroupClient" => {
                affected.users.extend(text("username").map(str::to_string));
            }
            // Only its text leaves access as it is
            "modifyClient"
                if ["password", "clientid", "roles", "groups"]
                    .iter()
                    .any(|name| present(args, name).is_some()) =>
            {
                affected.users.extend(text("username").map(str::to_string));
            }
            "modifyGroup" => {
                if let Some(group) = text("groupname") {
                    if present(args, "roles").is_some() || present(args, "clients").is_some() {
                        self.add_members(group, &mut affected);
                    }
                }
                // Those it gains, as well as those it loses
                let gained = present(args, "clients")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|entry| entry.get("username").and_then(Value::as_str));
                affected.users.extend(gained.map(str::to_string));
            }
            "addGroupRole" | "removeGroupRole" | "deleteGroup" => {
                if let Some(group) = text("groupname") {
                    self.add_members(group, &mut affected);
                }
            }
            "modifyRole" if present(args, "acls").is_none() => {}
            "deleteRole" | "addRoleACL" | "removeRoleACL" | "modifyRole" => {
                if let Some(role) = text("rolename") {
                    let holders = self
                        .clients
                        .iter()
                        .filter(|c| c.roles.iter().any(|r| r.name == role));
                    affected.users.extend(holders.map(|c| c.username.clone()));
                    for group in self
                        .groups
                        .iter()
                        .filter(|g| g.roles.iter().any(|r| r.name == role))
                    {
                        self.add_members(&group.groupname, &mut affected);
                    }
                }
            }
            "setAnonymousGroup" => affected.anonymous = true,
            _ => {}
        }
        affected
    }

    fn add_members(&self, group: &str, affected: &mut Affected) {
        let members = self
            .clients
            .iter()
            .filter(|c| c.groups.iter().any(|g| g.name == group));
        affected.users.extend(members.map(|c| c.username.clone()));
        affected.anonymous |= self.anonymous_group.as_deref() == Some(group);
    }

    /// Applies one command. Returns its response data, if any, and whether
    /// it changed anything.
    fn apply(&mut self, command: &str, args: &Value) -> Result<(Option<Value>, bool), String> {
        let text = |name: &str| {
            args.get(name)
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| format!("Invalid/missing {name}"))
        };
        match command {
            "createClient" => {
                let username = text("username")?;
                if self.clients.iter().any(|c| c.username == username) {
                    return Err("Client already exists".to_string());
                }
                if let Some(prepared) = mercurio_scram::prepare_username(&username) {
                    if self.clients.iter().any(|c| {
                        mercurio_scram::prepare_username(&c.username).as_ref() == Some(&prepared)
                    }) {
                        return Err("Client is the same user as another after SASLprep".to_string());
                    }
                }
                let roles = self.ranked_roles(args.get("roles"))?;
                let groups = self.ranked_groups(args.get("groups"))?;
                // Empty, as mosquitto takes it, binds no client id
                let clientid = match args.get("clientid") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(id)) if id.is_empty() => None,
                    Some(Value::String(id)) => Some(id.clone()),
                    Some(_) => return Err("Invalid clientid".to_string()),
                };
                let (password, scram) = match args.get("password").and_then(Value::as_str) {
                    Some(password) => {
                        let (hash, scram) = hash_password(&username, password)?;
                        (Some(hash), Some(scram))
                    }
                    None => (None, None),
                };
                let description = Description::default().modified(args)?;
                self.clients.push(Client {
                    username,
                    description,
                    password,
                    scram,
                    clientid,
                    disabled: false,
                    roles,
                    groups,
                });
                Ok((None, true))
            }
            "deleteClient" => {
                let username = text("username")?;
                self.client(&username)?;
                self.clients.retain(|c| c.username != username);
                Ok((None, true))
            }
            "setClientPassword" => {
                let username = text("username")?;
                let (hash, scram) = hash_password(&username, &text("password")?)?;
                let client = self.client(&username)?;
                client.password = Some(hash);
                client.scram = Some(scram);
                Ok((None, true))
            }
            "modifyClient" => {
                let username = text("username")?;
                let client = self.client(&username)?;
                let description = client.description.modified(args)?;
                let roles = present(args, "roles")
                    .map(|roles| self.ranked_roles(Some(roles)))
                    .transpose()?;
                let groups = present(args, "groups")
                    .map(|groups| self.ranked_groups(Some(groups)))
                    .transpose()?;
                let clientid = match present(args, "clientid") {
                    None => None,
                    Some(Value::String(id)) => Some((!id.is_empty()).then(|| id.clone())),
                    Some(_) => return Err("Invalid clientid".to_string()),
                };
                // Empty, as mosquitto takes it, leaves the password as it is
                let password = match present(args, "password") {
                    None => None,
                    Some(Value::String(password)) if password.is_empty() => None,
                    Some(Value::String(password)) => Some(hash_password(&username, password)?),
                    Some(_) => return Err("Invalid password".to_string()),
                };
                let client = self.client(&username)?;
                client.description = description;
                if let Some(roles) = roles {
                    client.roles = roles;
                }
                if let Some(groups) = groups {
                    client.groups = groups;
                }
                if let Some(clientid) = clientid {
                    client.clientid = clientid;
                }
                if let Some((hash, scram)) = password {
                    client.password = Some(hash);
                    client.scram = Some(scram);
                }
                Ok((None, true))
            }
            "enableClient" | "disableClient" => {
                self.client(&text("username")?)?.disabled = command == "disableClient";
                Ok((None, true))
            }
            "addClientRole" => {
                let priority = priority(args)?;
                let rolename = text("rolename")?;
                self.role(&rolename)?;
                let client = self.client(&text("username")?)?;
                rank(&mut client.roles, rolename, priority);
                Ok((None, true))
            }
            "removeClientRole" => {
                let rolename = text("rolename")?;
                self.client(&text("username")?)?
                    .roles
                    .retain(|r| r.name != rolename);
                Ok((None, true))
            }
            "getClient" => {
                let client = self.client(&text("username")?)?;
                let roles: Vec<Value> =
                    client.roles.iter().map(|r| r.to_json("rolename")).collect();
                let groups: Vec<Value> = client
                    .groups
                    .iter()
                    .map(|g| g.to_json("groupname"))
                    .collect();
                let data = json!({ "client": {
                    "username": client.username,
                    "textname": client.description.textname,
                    "textdescription": client.description.textdescription,
                    "clientid": client.clientid,
                    "disabled": client.disabled,
                    "roles": roles,
                    "groups": groups,
                }});
                Ok((Some(data), false))
            }
            "listClients" => {
                let names: Vec<&str> = self.clients.iter().map(|c| c.username.as_str()).collect();
                let data = json!({ "totalCount": names.len(), "clients": names });
                Ok((Some(data), false))
            }
            "createRole" => {
                let rolename = text("rolename")?;
                if self.roles.iter().any(|r| r.rolename == rolename) {
                    return Err("Role already exists".to_string());
                }
                let acls = match args.get("acls") {
                    Some(acls) => serde_json::from_value(acls.clone())
                        .map_err(|e| format!("Invalid acls: {e}"))?,
                    None => Vec::new(),
                };
                let mut role = Role {
                    rolename,
                    description: Description::default().modified(args)?,
                    acls,
                };
                role.order();
                self.roles.push(role);
                Ok((None, true))
            }
            "deleteRole" => {
                let rolename = text("rolename")?;
                self.role(&rolename)?;
                self.roles.retain(|r| r.rolename != rolename);
                for client in &mut self.clients {
                    client.roles.retain(|r| r.name != rolename);
                }
                for group in &mut self.groups {
                    group.roles.retain(|r| r.name != rolename);
                }
                Ok((None, true))
            }
            "createGroup" => {
                let groupname = text("groupname")?;
                if self.groups.iter().any(|g| g.groupname == groupname) {
                    return Err("Group already exists".to_string());
                }
                let roles = self.ranked_roles(args.get("roles"))?;
                let description = Description::default().modified(args)?;
                self.groups.push(Group {
                    groupname,
                    description,
                    roles,
                });
                Ok((None, true))
            }
            "deleteGroup" => {
                let groupname = text("groupname")?;
                self.group(&groupname)?;
                self.groups.retain(|g| g.groupname != groupname);
                for client in &mut self.clients {
                    client.groups.retain(|g| g.name != groupname);
                }
                if self.anonymous_group.as_ref() == Some(&groupname) {
                    self.anonymous_group = None;
                }
                Ok((None, true))
            }
            "addGroupRole" => {
                let priority = priority(args)?;
                let rolename = text("rolename")?;
                self.role(&rolename)?;
                let group = self.group(&text("groupname")?)?;
                rank(&mut group.roles, rolename, priority);
                Ok((None, true))
            }
            "removeGroupRole" => {
                let rolename = text("rolename")?;
                self.group(&text("groupname")?)?
                    .roles
                    .retain(|r| r.name != rolename);
                Ok((None, true))
            }
            "addGroupClient" => {
                let priority = priority(args)?;
                let groupname = text("groupname")?;
                self.group(&groupname)?;
                let client = self.client(&text("username")?)?;
                rank(&mut client.groups, groupname, priority);
                Ok((None, true))
            }
            "removeGroupClient" => {
                let groupname = text("groupname")?;
                self.client(&text("username")?)?
                    .groups
                    .retain(|g| g.name != groupname);
                Ok((None, true))
            }
            "getGroup" => {
                let groupname = text("groupname")?;
                let members: Vec<Value> = self
                    .clients
                    .iter()
                    .filter_map(|c| {
                        let membership = c.groups.iter().find(|g| g.name == groupname)?;
                        let mut member = json!({ "username": c.username });
                        if membership.priority != DEFAULT_PRIORITY {
                            member["priority"] = membership.priority.into();
                        }
                        Some(member)
                    })
                    .collect();
                let group = self.group(&groupname)?;
                let roles: Vec<Value> = group.roles.iter().map(|r| r.to_json("rolename")).collect();
                let data = json!({ "group": {
                    "groupname": group.groupname,
                    "textname": group.description.textname,
                    "textdescription": group.description.textdescription,
                    "roles": roles,
                    "clients": members,
                }});
                Ok((Some(data), false))
            }
            "modifyGroup" => {
                let groupname = text("groupname")?;
                let description = self.group(&groupname)?.description.modified(args)?;
                let roles = present(args, "roles")
                    .map(|roles| self.ranked_roles(Some(roles)))
                    .transpose()?;
                let clients = match present(args, "clients") {
                    Some(clients) => {
                        let clients = ranked(Some(clients), "username")?;
                        if let Some(missing) = clients
                            .iter()
                            .find(|c| !self.clients.iter().any(|client| client.username == c.name))
                        {
                            return Err(format!("Client {} not found", missing.name));
                        }
                        Some(clients)
                    }
                    None => None,
                };
                let group = self.group(&groupname)?;
                group.description = description;
                if let Some(roles) = roles {
                    group.roles = roles;
                }
                if let Some(members) = clients {
                    for client in &mut self.clients {
                        client.groups.retain(|g| g.name != groupname);
                        if let Some(member) = members.iter().find(|m| m.name == client.username) {
                            rank(&mut client.groups, groupname.clone(), member.priority);
                        }
                    }
                }
                Ok((None, true))
            }
            "listGroups" => {
                let names: Vec<&str> = self.groups.iter().map(|g| g.groupname.as_str()).collect();
                let data = json!({ "totalCount": names.len(), "groups": names });
                Ok((Some(data), false))
            }
            "setAnonymousGroup" => {
                let groupname = text("groupname")?;
                self.group(&groupname)?;
                self.anonymous_group = Some(groupname);
                Ok((None, true))
            }
            "getAnonymousGroup" => {
                let data = json!({ "group": { "groupname": self.anonymous_group } });
                Ok((Some(data), false))
            }
            "addRoleACL" => {
                let acl: Acl = serde_json::from_value(args.clone())
                    .map_err(|e| format!("Invalid ACL: {e}"))?;
                let role = self.role(&text("rolename")?)?;
                role.acls
                    .retain(|a| !(a.acltype == acl.acltype && a.topic == acl.topic));
                role.acls.push(acl);
                role.order();
                Ok((None, true))
            }
            "removeRoleACL" => {
                let acltype: AclType = serde_json::from_value(args["acltype"].clone())
                    .map_err(|_| "Invalid/missing acltype".to_string())?;
                let topic = text("topic")?;
                let role = self.role(&text("rolename")?)?;
                let before = role.acls.len();
                role.acls
                    .retain(|a| !(a.acltype == acltype && a.topic == topic));
                if role.acls.len() == before {
                    return Err("ACL not found".to_string());
                }
                Ok((None, true))
            }
            "modifyRole" => {
                let rolename = text("rolename")?;
                let description = self.role(&rolename)?.description.modified(args)?;
                let acls = match present(args, "acls") {
                    Some(acls) => {
                        let listed: Vec<Acl> = serde_json::from_value(acls.clone())
                            .map_err(|e| format!("Invalid acls: {e}"))?;
                        // One per type and topic, the last given, as
                        // addRoleACL keeps
                        let mut acls: Vec<Acl> = Vec::new();
                        for acl in listed {
                            acls.retain(|a| !(a.acltype == acl.acltype && a.topic == acl.topic));
                            acls.push(acl);
                        }
                        Some(acls)
                    }
                    None => None,
                };
                let role = self.role(&rolename)?;
                role.description = description;
                if let Some(acls) = acls {
                    role.acls = acls;
                    role.order();
                }
                Ok((None, true))
            }
            "getRole" => {
                let role = self.role(&text("rolename")?)?;
                Ok((Some(json!({ "role": role })), false))
            }
            "listRoles" => {
                let names: Vec<&str> = self.roles.iter().map(|r| r.rolename.as_str()).collect();
                let data = json!({ "totalCount": names.len(), "roles": names });
                Ok((Some(data), false))
            }
            "setDefaultACLAccess" => {
                let acls = args
                    .get("acls")
                    .and_then(Value::as_array)
                    .ok_or("Invalid/missing acls")?;
                // Applied to a copy, so an invalid entry leaves no half of
                // the change behind for a later command of the batch to save
                let mut access = self.default_acl_access;
                for acl in acls {
                    let allow = acl.get("allow").and_then(Value::as_bool);
                    let field = match acl.get("acltype").and_then(Value::as_str) {
                        Some("publishClientSend") => &mut access.publish_client_send,
                        Some("publishClientReceive") => &mut access.publish_client_receive,
                        Some("subscribe") => &mut access.subscribe,
                        _ => return Err("Invalid/missing acltype".to_string()),
                    };
                    *field = allow.ok_or("Invalid/missing allow")?;
                }
                self.default_acl_access = access;
                Ok((None, true))
            }
            "getDefaultACLAccess" => {
                let access = self.default_acl_access;
                let data = json!({ "acls": [
                    { "acltype": "publishClientSend", "allow": access.publish_client_send },
                    { "acltype": "publishClientReceive", "allow": access.publish_client_receive },
                    { "acltype": "subscribe", "allow": access.subscribe },
                ]});
                Ok((Some(data), false))
            }
            _ => Err("Unknown command".to_string()),
        }
    }
}

impl Data {
    /// The roles of a `roles` argument, which must exist.
    fn ranked_roles(&self, list: Option<&Value>) -> Result<Vec<Ranked>, String> {
        let roles = ranked(list, "rolename")?;
        match roles
            .iter()
            .find(|r| !self.roles.iter().any(|role| role.rolename == r.name))
        {
            Some(missing) => Err(format!("Role {} not found", missing.name)),
            None => Ok(roles),
        }
    }

    /// The groups of a `groups` argument, which must exist.
    fn ranked_groups(&self, list: Option<&Value>) -> Result<Vec<Ranked>, String> {
        let groups = ranked(list, "groupname")?;
        match groups
            .iter()
            .find(|g| !self.groups.iter().any(|group| group.groupname == g.name))
        {
            Some(missing) => Err(format!("Group {} not found", missing.name)),
            None => Ok(groups),
        }
    }
}

/// Names given as `[{"rolename": ..., "priority": ...}]`, under `key`, as
/// mosquitto does, ordered by priority.
fn ranked(list: Option<&Value>, key: &str) -> Result<Vec<Ranked>, String> {
    let entries = match list {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err(format!("Invalid {key} list")),
    };
    let mut names = Vec::new();
    for entry in entries {
        let name = entry
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("Invalid/missing {key}"))?;
        rank(&mut names, name.to_string(), priority(entry)?);
    }
    Ok(names)
}

/// The argument `name` of `args`, unless it is absent or null.
fn present<'a>(args: &'a Value, name: &str) -> Option<&'a Value> {
    args.get(name).filter(|value| !value.is_null())
}

/// The `priority` of `args`, mosquitto's default when it gives none.
fn priority(args: &Value) -> Result<i64, String> {
    match args.get("priority") {
        None | Some(Value::Null) => Ok(DEFAULT_PRIORITY),
        Some(priority) => priority
            .as_i64()
            .ok_or_else(|| "Invalid priority".to_string()),
    }
}

/// The Argon2 hash of `password` and its SCRAM-SHA-256 credentials, which
/// are found by the prepared form of `username`, so one that cannot be
/// prepared is refused.
fn hash_password(username: &str, password: &str) -> Result<(String, String), String> {
    mercurio_scram::prepare_username(username)
        .ok_or("The user name cannot be prepared with SASLprep, for SCRAM-SHA-256")?;
    credentials::hash_new_password(password)
}

/// Connected clients a change of the state concerns.
#[derive(Debug, Default)]
struct Affected {
    users: BTreeSet<String>,
    /// Clients without a user name
    anonymous: bool,
}

impl Affected {
    fn is_empty(&self) -> bool {
        self.users.is_empty() && !self.anonymous
    }
}

/// The dynamic security state, checking passwords and topic access.
pub struct DynamicSecurity {
    path: String,
    data: RwLock<Data>,
    /// Held while a batch or a reload runs, so concurrent batches don't
    /// overwrite each other's changes or the temporary file, and a reload
    /// doesn't land between a batch's copy of the state and its save
    commands: Mutex<()>,
    /// Disconnects the clients a change affects, once the broker runs
    admin: OnceLock<Admin>,
}

impl DynamicSecurity {
    pub fn load(path: &str) -> Result<Self, String> {
        Ok(Self {
            path: path.to_string(),
            data: RwLock::new(read(path)?),
            commands: Mutex::new(()),
            admin: OnceLock::new(),
        })
    }

    /// Hands over the broker's `Admin`, which exists only once the broker
    /// does.
    pub fn set_admin(&self, admin: Admin) {
        let _ = self.admin.set(admin);
    }

    /// Writes a new state file with an administrator client, `admin`, whose
    /// role may send every dynamic security and broker admin command and
    /// read `$SYS`. Created by root in another user's directory, such as the
    /// service's state directory, the file is given to that directory's
    /// owner and group, so the service can read it.
    pub fn init(path: &str, admin: &str, password: &str) -> Result<(), String> {
        if Path::new(path).exists() {
            return Err(format!("{path} already exists"));
        }
        let acl = |acltype, topic: &str| Acl {
            acltype,
            topic: topic.to_string(),
            priority: 0,
            allow: true,
        };
        let (hash, scram) = hash_password(admin, password)?;
        let data = Data {
            clients: vec![Client {
                username: admin.to_string(),
                description: Description::default(),
                password: Some(hash),
                scram: Some(scram),
                clientid: None,
                disabled: false,
                roles: vec![Ranked::new("admin")],
                groups: Vec::new(),
            }],
            roles: vec![Role {
                rolename: "admin".to_string(),
                description: Description::default(),
                acls: vec![
                    acl(AclType::PublishClientSend, "$CONTROL/dynamic-security/#"),
                    acl(AclType::SubscribePattern, "$CONTROL/dynamic-security/#"),
                    acl(AclType::PublishClientReceive, "$CONTROL/dynamic-security/#"),
                    acl(AclType::PublishClientSend, "$CONTROL/broker/v1/#"),
                    acl(AclType::SubscribePattern, "$CONTROL/broker/v1/#"),
                    acl(AclType::PublishClientReceive, "$CONTROL/broker/v1/#"),
                    acl(AclType::SubscribePattern, "$SYS/#"),
                    acl(AclType::PublishClientReceive, "$SYS/#"),
                ],
            }],
            ..Default::default()
        };
        create(path, &data)
    }

    /// Writes a new state file from `source`, the `dynamic-security.json`
    /// of mosquitto's plugin, with its clients, groups, roles, default
    /// access and anonymous group, owned as [`Self::init`] owns its file.
    /// Returns what could not be carried over, one line each.
    pub fn import(source: &str, path: &str) -> Result<Vec<String>, String> {
        if Path::new(path).exists() {
            return Err(format!("{path} already exists"));
        }
        let contents =
            std::fs::read_to_string(source).map_err(|e| format!("Failed to read {source}: {e}"))?;
        // serde_json's errors quote values, which can be hashes
        let file: Value = serde_json::from_str(&contents).map_err(|e| {
            format!(
                "{source} is not JSON: error at line {} column {}",
                e.line(),
                e.column()
            )
        })?;
        let (data, notes) = from_mosquitto(&file).map_err(|e| format!("{source}: {e}"))?;
        create(path, &data)?;
        Ok(notes)
    }

    /// Runs a batch of commands, `{"commands": [...]}`, and returns the
    /// response batch with the clients the changes concern. The file is
    /// saved once if any command changed the state; if saving fails, the
    /// changes are undone.
    fn handle(&self, request: &[u8]) -> (Value, Affected) {
        let request: Value = match serde_json::from_slice(request) {
            Ok(request) => request,
            Err(e) => {
                let error = format!("Invalid JSON: {e}");
                return (
                    json!({ "responses": [{ "error": error }] }),
                    Affected::default(),
                );
            }
        };
        let commands = request
            .get("commands")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        // Applied to a copy, so hashing passwords doesn't hold the lock every
        // authorization takes; batches are serialized so nothing else changes
        // the state in between
        let _batch = self.commands.lock().unwrap_or_else(|e| e.into_inner());
        let mut data = self.data.read().unwrap().clone();
        let mut changed = false;
        let mut affected = Affected::default();
        let mut responses = Vec::new();
        for args in &commands {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            let mut response = json!({ "command": command });
            // Before applying it: a deleted role or group no longer names
            // its holders
            let concerned = data.affected_by(command, args);
            match data.apply(command, args) {
                Ok((reply, change)) => {
                    changed |= change;
                    affected.users.extend(concerned.users);
                    affected.anonymous |= concerned.anonymous;
                    if let Some(reply) = reply {
                        response["data"] = reply;
                    }
                }
                Err(e) => response["error"] = Value::String(e),
            }
            if let Some(correlation) = args.get("correlationData") {
                response["correlationData"] = correlation.clone();
            }
            responses.push(response);
        }
        if changed {
            match write(&self.path, &data) {
                Ok(()) => *self.data.write().unwrap() = data,
                Err(e) => {
                    error!("Dynamic security: {e}");
                    responses = vec![json!({ "error": "Failed to save the changes" })];
                    affected = Affected::default();
                }
            }
        }
        (json!({ "responses": responses }), affected)
    }
}

/// Disconnects the connected clients `affected` names, with Administrative
/// Action and without their wills, as mosquitto does. Anonymous clients are
/// those with a network address: in-process clients and those of Unix
/// sockets, the trusted admin socket among them, are left alone. Each is
/// addressed by its client id and mount point, so a client of another user
/// with the same id on another mount point stays connected.
async fn disconnect(admin: Admin, affected: Affected) {
    let mut query = ClientQuery::default();
    query.connected = Some(true);
    let mut targets = Vec::new();
    loop {
        query.paging = Paging::new(query.paging.offset, MAX_PAGE).expect("a page allowed");
        let page = admin.clients(&query).await;
        if page.items.is_empty() {
            break;
        }
        query.paging.offset += page.items.len();
        let concerned = page
            .items
            .into_iter()
            .filter(|client| match &client.username {
                Some(username) => affected.users.contains(username),
                None => affected.anonymous && client.address.is_some(),
            });
        targets
            .extend(concerned.map(|client| SessionKey::new(client.mount_point, client.client_id)));
    }
    let mut options = DisconnectOptions::default();
    options.send_will = false;
    for key in targets {
        // It may have left meanwhile
        let _ = admin.disconnect(&key, options).await;
    }
}

fn read(path: &str) -> Result<Data, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read dynamic security file {path}: {e}"))?;
    let mut data: Data = serde_json::from_str(&contents).map_err(|e| {
        // serde_json quotes the offending value, which can be a hash or a
        // salt, and the returned text reaches admin clients
        error!("Invalid dynamic security file {path}: {e}");
        let category = match e.classify() {
            serde_json::error::Category::Io => "io",
            serde_json::error::Category::Syntax => "syntax",
            serde_json::error::Category::Data => "data",
            serde_json::error::Category::Eof => "eof",
        };
        format!(
            "Invalid dynamic security file {path}: {category} error at line {} column {}",
            e.line(),
            e.column()
        )
    })?;
    data.order();
    Ok(data)
}

/// Replaces the file in one step, so a crash never leaves half of it. The
/// new contents reach the disk before the rename, and the rename before
/// returning: after a power loss the file is the old one or the new one,
/// never empty, and it is the only copy.
fn write(path: &str, data: &Data) -> Result<(), String> {
    let temporary = format!("{path}.new");
    let contents = serde_json::to_string_pretty(data).expect("serializable");
    // A leftover from a crash would keep its own permissions
    let _ = std::fs::remove_file(&temporary);
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create(true).truncate(true);
    // It holds password hashes: for the broker's user only
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut file, 0o600);
    file.open(&temporary)
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, contents.as_bytes())?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&temporary, path))
        .and_then(|()| sync_directory(path))
        .map_err(|e| format!("Failed to write {path}: {e}"))
}

/// Writes `data` to a new file at `path`, given to the owner of its
/// directory when [`adopted_owner`] says so.
fn create(path: &str, data: &Data) -> Result<(), String> {
    write(path, data)?;
    #[cfg(unix)]
    give_to_directory_owner(path)
        .map_err(|e| format!("Failed to give {path} to its directory's owner: {e}"))?;
    Ok(())
}

/// The state a mosquitto `dynamic-security.json` describes, with what
/// could not be carried over. Names a client, group or role refers to that
/// the file does not define are left out, as are ACL types Mercurio does
/// not check. Errors name where the file is wrong without quoting it.
fn from_mosquitto(file: &Value) -> Result<(Data, Vec<String>), String> {
    let mut notes = Vec::new();
    let mut data = Data::default();
    let list = |value: &Value, key: &str, context: &str| -> Result<Vec<Value>, String> {
        match value.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(entries)) => Ok(entries.clone()),
            Some(_) => Err(format!("{context}`{key}` is not a list")),
        }
    };
    let name = |entry: &Value, key: &str, context: &str| -> Result<String, String> {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| format!("{context}: `{key}` is missing or not text"))
    };

    for (n, entry) in list(file, "roles", "")?.iter().enumerate() {
        let rolename = name(entry, "rolename", &format!("roles[{n}]"))?;
        if data.roles.iter().any(|r| r.rolename == rolename) {
            notes.push(format!("role {rolename}: defined twice; the first is kept"));
            continue;
        }
        let context = format!("role {rolename}");
        let mut acls = Vec::new();
        for (n, acl) in list(entry, "acls", &format!("{context}: "))?
            .iter()
            .enumerate()
        {
            let at = format!("{context}, acls[{n}]");
            let acltype = name(acl, "acltype", &at)?;
            let topic = name(acl, "topic", &at)?;
            let Ok(parsed) = serde_json::from_value::<AclType>(json!(acltype)) else {
                notes.push(format!(
                    "{context}: ACL {acltype} on {topic} left out, as Mercurio has no such check"
                ));
                continue;
            };
            let allow = match acl.get("allow") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(allow)) => *allow,
                Some(_) => return Err(format!("{at}: `allow` is not true or false")),
            };
            let priority = match acl.get("priority") {
                None | Some(Value::Null) => 0,
                Some(priority) => priority
                    .as_i64()
                    .ok_or_else(|| format!("{at}: `priority` is not a whole number"))?,
            };
            acls.retain(|a: &Acl| !(a.acltype == parsed && a.topic == topic));
            acls.push(Acl {
                acltype: parsed,
                topic,
                priority,
                allow,
            });
        }
        data.roles.push(Role {
            rolename,
            description: mosquitto_description(entry),
            acls,
        });
    }

    let known = |names: Vec<Ranked>,
                 kind: &str,
                 exists: &dyn Fn(&str) -> bool,
                 notes: &mut Vec<String>,
                 context: &str| {
        names
            .into_iter()
            .filter(|ranked| {
                let found = exists(&ranked.name);
                if !found {
                    notes.push(format!(
                        "{context}: {kind} {} is not defined, so it is left out",
                        ranked.name
                    ));
                }
                found
            })
            .collect::<Vec<_>>()
    };

    let mut members = Vec::new();
    for (n, entry) in list(file, "groups", "")?.iter().enumerate() {
        let groupname = name(entry, "groupname", &format!("groups[{n}]"))?;
        if data.groups.iter().any(|g| g.groupname == groupname) {
            notes.push(format!(
                "group {groupname}: defined twice; the first is kept"
            ));
            continue;
        }
        let context = format!("group {groupname}");
        let roles =
            ranked(entry.get("roles"), "rolename").map_err(|e| format!("{context}: {e}"))?;
        let roles = known(
            roles,
            "role",
            &|name| data.roles.iter().any(|r| r.rolename == name),
            &mut notes,
            &context,
        );
        let clients =
            ranked(entry.get("clients"), "username").map_err(|e| format!("{context}: {e}"))?;
        members.push((groupname.clone(), clients));
        data.groups.push(Group {
            groupname,
            description: mosquitto_description(entry),
            roles,
        });
    }

    for (n, entry) in list(file, "clients", "")?.iter().enumerate() {
        let username = name(entry, "username", &format!("clients[{n}]"))?;
        if data.clients.iter().any(|c| c.username == username) {
            notes.push(format!(
                "client {username}: defined twice; the first is kept"
            ));
            continue;
        }
        let context = format!("client {username}");
        let roles =
            ranked(entry.get("roles"), "rolename").map_err(|e| format!("{context}: {e}"))?;
        let roles = known(
            roles,
            "role",
            &|name| data.roles.iter().any(|r| r.rolename == name),
            &mut notes,
            &context,
        );
        // Mosquitto keeps memberships with the groups; taken here too
        let groups =
            ranked(entry.get("groups"), "groupname").map_err(|e| format!("{context}: {e}"))?;
        let groups = known(
            groups,
            "group",
            &|name| data.groups.iter().any(|g| g.groupname == name),
            &mut notes,
            &context,
        );
        let clientid = match entry.get("clientid") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) => (!id.is_empty()).then(|| id.clone()),
            Some(_) => return Err(format!("{context}: `clientid` is not text")),
        };
        let disabled = match entry.get("disabled") {
            None | Some(Value::Null) => false,
            Some(Value::Bool(disabled)) => *disabled,
            Some(_) => return Err(format!("{context}: `disabled` is not true or false")),
        };
        let password = mosquitto_password(entry);
        if password.is_none() && entry.get("password").is_some_and(|p| !p.is_null()) {
            notes.push(format!(
                "{context}: its password is in a form Mercurio cannot check, so it has none \
                 and cannot log in until one is set"
            ));
        }
        data.clients.push(Client {
            username,
            description: mosquitto_description(entry),
            password,
            scram: None,
            clientid,
            disabled,
            roles,
            groups,
        });
    }

    for (groupname, clients) in members {
        for member in clients {
            match data.clients.iter_mut().find(|c| c.username == member.name) {
                Some(client) => rank(&mut client.groups, groupname.clone(), member.priority),
                None => notes.push(format!(
                    "group {groupname}: client {} is not defined, so it is left out",
                    member.name
                )),
            }
        }
    }

    match file.get("anonymousGroup") {
        None | Some(Value::Null) => {}
        Some(Value::String(group)) if data.groups.iter().any(|g| &g.groupname == group) => {
            data.anonymous_group = Some(group.clone());
        }
        Some(Value::String(group)) => notes.push(format!(
            "anonymous group {group} is not defined, so none is set"
        )),
        Some(_) => return Err("`anonymousGroup` is not text".to_string()),
    }

    if let Some(access) = file.get("defaultACLAccess").filter(|a| !a.is_null()) {
        let flag = |key: &str, default: bool| match access.get(key) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(allow)) => Ok(*allow),
            Some(_) => Err(format!("defaultACLAccess: `{key}` is not true or false")),
        };
        let defaults = DefaultAccess::default();
        data.default_acl_access = DefaultAccess {
            publish_client_send: flag("publishClientSend", defaults.publish_client_send)?,
            publish_client_receive: flag("publishClientReceive", defaults.publish_client_receive)?,
            subscribe: flag("subscribe", defaults.subscribe)?,
        };
        if !flag("unsubscribe", true)? {
            notes.push(
                "defaultACLAccess: unsubscribe is denied, but Mercurio lets every client \
                 unsubscribe"
                    .to_string(),
            );
        }
    }

    data.order();
    Ok((data, notes))
}

/// The text of a client, group or role of mosquitto, which spells it
/// `textname` and, in the file `mosquitto_ctrl dynsec init` writes,
/// `textName`.
fn mosquitto_description(entry: &Value) -> Description {
    let text = |keys: [&str; 2]| {
        keys.iter()
            .find_map(|key| entry.get(*key).and_then(Value::as_str))
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    Description {
        textname: text(["textname", "textName"]),
        textdescription: text(["textdescription", "textDescription"]),
    }
}

/// The password of a mosquitto client as a `$7$` hash: mosquitto stores
/// the PBKDF2-SHA512 hash and its salt in base64 with the iteration count
/// beside them, the form `mosquitto_passwd` writes in one string. None
/// when it has none or it is in another form.
fn mosquitto_password(entry: &Value) -> Option<String> {
    let hash = entry.get("password")?.as_str()?;
    let salt = entry.get("salt")?.as_str()?;
    let iterations = entry.get("iterations")?.as_u64()?;
    let stored = format!("$7${iterations}${salt}${hash}");
    credentials::check_hash(&stored).ok().map(|()| stored)
}

/// The directory holding `path`.
#[cfg(unix)]
fn directory_of(path: &str) -> &Path {
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// Flushes the directory holding `path`, where a rename is recorded.
#[cfg(unix)]
fn sync_directory(path: &str) -> std::io::Result<()> {
    std::fs::File::open(directory_of(path))?.sync_all()
}

/// Gives the file at `path` to the owner and group of its directory, when
/// [`adopted_owner`] says so.
#[cfg(unix)]
fn give_to_directory_owner(path: &str) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;

    let file = std::fs::metadata(path)?;
    let directory = std::fs::metadata(directory_of(path))?;
    match adopted_owner(file.uid(), (directory.uid(), directory.gid())) {
        Some((uid, gid)) => std::os::unix::fs::chown(path, Some(uid), Some(gid)),
        None => Ok(()),
    }
}

/// The owner and group a file created by `file_uid` takes from its
/// directory's: those of the directory when root created the file in a
/// directory of another user. The service's state directory belongs to its
/// user, and systemd doesn't change the owner of files in a directory whose
/// owner is already right, so a file root leaves there is unreadable to the
/// service.
#[cfg(unix)]
fn adopted_owner(file_uid: u32, directory: (u32, u32)) -> Option<(u32, u32)> {
    (file_uid == 0 && directory.0 != 0).then_some(directory)
}

#[cfg(not(unix))]
fn sync_directory(_: &str) -> std::io::Result<()> {
    Ok(())
}

impl DynamicSecurity {
    /// Whether `username` may log in with `password` as `client_id`; an
    /// empty `client_id` is one the broker is to assign.
    fn check_password(&self, username: &str, password: &[u8], client_id: &str) -> bool {
        // Copied out so Argon2, which is slow on purpose, runs without the lock
        let (hash, bound) = {
            let data = self.data.read().unwrap();
            let client = data.clients.iter().find(|c| c.username == username);
            match client.filter(|c| !c.disabled) {
                Some(Client {
                    password: Some(hash),
                    clientid,
                    ..
                }) => (Some(hash.clone()), clientid.clone()),
                _ => (None, None),
            }
        };
        match hash {
            // Argon2, or the PBKDF2 of a client imported from mosquitto
            // that has not set its password since
            Some(hash) => {
                credentials::verify(&hash, password) && bound.is_none_or(|bound| bound == client_id)
            }
            None => {
                // As slow as a wrong password, so timing doesn't tell which
                // users exist or can log in
                let _ = Argon2::default().verify_password(password, &unknown_user_hash());
                false
            }
        }
    }
}

impl ScramCredentialStore for DynamicSecurity {
    /// The SCRAM credentials of an enabled client, set with its current
    /// password, that may connect as `client`.
    fn scram_credentials<'a>(
        &'a self,
        client: &'a ClientInfo,
        username: &'a str,
    ) -> BoxFuture<'a, Option<StoredCredentials>> {
        let data = self.data.read().unwrap();
        let credentials = data
            .clients
            .iter()
            .find(|c| {
                !c.disabled
                    && mercurio_scram::prepare_username(&c.username).as_deref() == Some(username)
            })
            .filter(|c| c.clientid.as_ref().is_none_or(|id| *id == client.client_id))
            .and_then(|c| match (&c.password, &c.scram) {
                (Some(hash), Some(scram)) => {
                    credentials::scram_credentials(hash, scram).ok().flatten()
                }
                _ => None,
            });
        ready(credentials)
    }
}

impl Plugin for DynamicSecurity {
    fn name(&self) -> &str {
        "dynamic-security"
    }

    fn status(&self) -> PluginStatus {
        PluginStatus::new(self.name(), PluginKind::Builtin).with_hooks(&[
            "authenticate",
            "authorize",
            "authorize_control",
            "on_control",
        ])
    }

    fn on_control<'a>(
        &'a self,
        _: &'a ClientInfo,
        path: &'a str,
        payload: Bytes,
    ) -> BoxFuture<'a, Option<Bytes>> {
        if path != "v1" {
            return ready(None);
        }
        Box::pin(async move {
            // Hashing passwords takes a while; keep it off the runtime
            let (response, affected) = tokio::task::block_in_place(|| self.handle(&payload));
            // Apart, so a long list of sessions doesn't hold the answer up
            let admin = self.admin.get().cloned();
            if let Some(admin) = admin.filter(|_| !affected.is_empty()) {
                tokio::spawn(disconnect(admin, affected));
            }
            Some(Bytes::from(response.to_string()))
        })
    }

    fn authenticate<'a>(
        &'a self,
        client: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        // Hashes take tens of milliseconds of CPU; keep them off the runtime
        let (username, password) = (
            credentials.username.to_string(),
            credentials.password.to_vec(),
        );
        Box::pin(async move {
            let valid = tokio::task::block_in_place(|| {
                self.check_password(&username, &password, &client.client_id)
            });
            if valid {
                Decision::Allow
            } else {
                Decision::Deny
            }
        })
    }

    fn authorize<'a>(
        &'a self,
        client: &'a ClientInfo,
        topic: &'a str,
        access: Access,
    ) -> BoxFuture<'a, Decision> {
        let data = self.data.read().unwrap();
        let allowed = data.authorize(client.username.as_deref(), topic, access);
        ready(if allowed {
            Decision::Allow
        } else {
            Decision::Deny
        })
    }

    fn authorize_control<'a>(
        &'a self,
        client: &'a ClientInfo,
        topic: &'a str,
    ) -> BoxFuture<'a, Decision> {
        let data = self.data.read().unwrap();
        let allowed = data.authorize_control(client.username.as_deref(), topic);
        ready(if allowed {
            Decision::Allow
        } else {
            Decision::Deny
        })
    }
}

impl Reload for DynamicSecurity {
    fn reload(&self) -> Result<(), String> {
        let _batch = self.commands.lock().unwrap_or_else(|e| e.into_inner());
        *self.data.write().unwrap() = read(&self.path)?;
        Ok(())
    }

    fn path(&self) -> &str {
        &self.path
    }

    fn item(&self) -> &str {
        self.name()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND_TOPIC: &str = "$CONTROL/dynamic-security/v1";

    #[tokio::test(flavor = "multi_thread")]
    async fn test_commands_arrive_through_control() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-control-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();

        let request = br#"{"commands":[{"command":"listClients"}]}"#;
        let answer = security
            .on_control(
                &ClientInfo::new("c", Some("admin".into())),
                "v1",
                Bytes::from_static(request),
            )
            .await
            .expect("an answer");
        assert!(String::from_utf8_lossy(&answer).contains("listClients"));
        assert!(security
            .on_control(&ClientInfo::new("c", None), "v2", Bytes::new())
            .await
            .is_none());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_reload_error_does_not_quote_the_file() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-secret-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = std::sync::Arc::new(DynamicSecurity::load(path).unwrap());
        std::fs::write(
            path,
            "{\n \"clients\": [{\"username\": \"a\", \"disabled\": \"SECRET-hash\"}]\n}",
        )
        .unwrap();

        let files: Vec<std::sync::Arc<dyn Reload>> = vec![security];
        let items = crate::reload::reload_all(&files);
        let error = items[0].error.as_deref().unwrap();
        assert!(!error.contains("SECRET-hash"), "{error}");
        assert!(error.contains("data error at line 2"), "{error}");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_concurrent_batches_keep_every_change() {
        let path = std::env::temp_dir().join(format!(
            "mercuriod-dynsec-concurrent-{}",
            std::process::id()
        ));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();

        std::thread::scope(|scope| {
            for n in 0..8 {
                let security = &security;
                scope.spawn(move || {
                    let name = format!("c{n}");
                    run(
                        security,
                        json!([{ "command": "createClient", "username": name, "password": "p" }]),
                    );
                });
            }
        });
        let listed = run(&security, json!([{ "command": "listClients" }]));
        assert_eq!(listed[0]["data"]["totalCount"], 9);
        let _ = std::fs::remove_file(path);
    }

    fn identity(username: &str) -> ClientInfo {
        ClientInfo::new("c", Some(username.to_string()))
    }

    fn run(security: &DynamicSecurity, commands: Value) -> Vec<Value> {
        let request = json!({ "commands": commands }).to_string();
        let (response, _) = security.handle(request.as_bytes());
        response["responses"].as_array().unwrap().clone()
    }

    #[tokio::test]
    async fn test_disabled_clients_lose_access_and_the_file_stays_private() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-off-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        run(
            &security,
            json!([
                { "command": "createClient", "username": "s1", "password": "pw" },
                { "command": "disableClient", "username": "s1" },
            ]),
        );

        // Still connected, it may no longer even receive by default
        assert_eq!(
            security
                .authorize(&identity("s1"), "anything", Access::Read)
                .await,
            Decision::Deny
        );
        assert_eq!(
            security
                .authorize_control(&identity("admin"), COMMAND_TOPIC)
                .await,
            Decision::Allow
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn test_commands_manage_clients_and_access() {
        let path = std::env::temp_dir().join(format!("mercuriod-dynsec-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        assert!(security.check_password("admin", b"secret", "c"));
        assert_eq!(
            security
                .authorize_control(&identity("admin"), COMMAND_TOPIC)
                .await,
            Decision::Allow
        );

        let responses = run(
            &security,
            json!([
                { "command": "createRole", "rolename": "sensor" },
                { "command": "addRoleACL", "rolename": "sensor", "acltype": "publishClientSend",
                  "topic": "sensors/#", "allow": true },
                { "command": "addRoleACL", "rolename": "sensor", "acltype": "publishClientSend",
                  "topic": "sensors/secret", "priority": 1, "allow": false },
                { "command": "createClient", "username": "s1", "password": "pw",
                  "roles": [{ "rolename": "sensor" }], "correlationData": "42" },
                { "command": "createClient", "username": "s1" },
                { "command": "listClients" },
            ]),
        );
        assert!(
            responses[..4].iter().all(|r| r.get("error").is_none()),
            "{responses:?}"
        );
        assert_eq!(responses[3]["correlationData"], "42");
        assert_eq!(responses[4]["error"], "Client already exists");
        assert_eq!(responses[5]["data"]["clients"], json!(["admin", "s1"]));

        let s1 = identity("s1");
        assert!(security.check_password("s1", b"pw", "c"));
        assert_eq!(
            security.authorize(&s1, "sensors/t1", Access::Write).await,
            Decision::Allow
        );
        assert_eq!(
            security
                .authorize(&s1, "sensors/secret", Access::Write)
                .await,
            Decision::Deny
        );
        assert_eq!(
            security
                .authorize(&s1, "sensors/t1", Access::Subscribe)
                .await,
            Decision::Deny
        );
        assert_eq!(
            security.authorize_control(&s1, COMMAND_TOPIC).await,
            Decision::Deny
        );

        // Receiving is allowed by default, subscribing isn't
        assert_eq!(
            security.authorize(&s1, "anything", Access::Read).await,
            Decision::Allow
        );
        assert_eq!(
            security.authorize(&s1, "anything", Access::Subscribe).await,
            Decision::Deny
        );

        // Groups give their roles to members, and to anonymous clients when
        // set as the anonymous group
        let anonymous = ClientInfo::new("a", None);
        let responses = run(
            &security,
            json!([
                { "command": "createRole", "rolename": "reader" },
                { "command": "addRoleACL", "rolename": "reader", "acltype": "subscribePattern",
                  "topic": "news/#", "allow": true },
                { "command": "createGroup", "groupname": "readers",
                  "roles": [{ "rolename": "reader" }] },
                { "command": "addGroupClient", "groupname": "readers", "username": "s1" },
                { "command": "setAnonymousGroup", "groupname": "readers" },
                { "command": "getGroup", "groupname": "readers" },
            ]),
        );
        assert!(
            responses.iter().all(|r| r.get("error").is_none()),
            "{responses:?}"
        );
        assert_eq!(
            responses[5]["data"]["group"]["clients"],
            json!([{ "username": "s1" }])
        );
        assert_eq!(
            security.authorize(&s1, "news/#", Access::Subscribe).await,
            Decision::Allow
        );
        assert_eq!(
            security
                .authorize(&anonymous, "news/today", Access::Subscribe)
                .await,
            Decision::Allow
        );
        run(
            &security,
            json!([{ "command": "deleteGroup", "groupname": "readers" }]),
        );
        assert_eq!(
            security.authorize(&s1, "news/#", Access::Subscribe).await,
            Decision::Deny
        );
        assert_eq!(
            security
                .authorize(&anonymous, "news/today", Access::Subscribe)
                .await,
            Decision::Deny
        );

        run(
            &security,
            json!([{ "command": "disableClient", "username": "s1" }]),
        );
        assert!(!security.check_password("s1", b"pw", "c"));

        // Saved, so a reload or a restart keeps the changes
        security.reload().unwrap();
        assert!(!security.check_password("s1", b"pw", "c"));
        run(
            &security,
            json!([{ "command": "enableClient", "username": "s1" }]),
        );
        assert_eq!(
            security.authorize(&s1, "sensors/t1", Access::Write).await,
            Decision::Allow
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_status_is_builtin() {
        use mercurio_server::plugin::{PluginKind, PluginStatus};

        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-status-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        assert_eq!(
            security.status(),
            PluginStatus::new("dynamic-security", PluginKind::Builtin).with_hooks(&[
                "authenticate",
                "authorize",
                "authorize_control",
                "on_control"
            ])
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn test_permissive_defaults_never_open_control() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-open-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        let responses = run(
            &security,
            json!([
                { "command": "setDefaultACLAccess", "acls": [
                    { "acltype": "publishClientSend", "allow": true },
                    { "acltype": "publishClientReceive", "allow": true },
                    { "acltype": "subscribe", "allow": true },
                ]},
                { "command": "createClient", "username": "s1", "password": "pw" },
            ]),
        );
        assert!(
            responses.iter().all(|r| r.get("error").is_none()),
            "{responses:?}"
        );

        let anonymous = ClientInfo::new("a", None);
        for client in [&identity("s1"), &anonymous] {
            for topic in [
                "$CONTROL",
                COMMAND_TOPIC,
                "$CONTROL/broker/v1",
                "$CONTROL/dynamic-security/v1/response",
            ] {
                assert_eq!(
                    security.authorize_control(client, topic).await,
                    Decision::Deny,
                    "{:?} {topic}",
                    client.username
                );
            }
            assert_eq!(
                security.authorize(client, "anything", Access::Write).await,
                Decision::Allow
            );
        }
        assert_eq!(
            security
                .authorize_control(&identity("admin"), COMMAND_TOPIC)
                .await,
            Decision::Allow
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn test_subscribe_patterns_cover_only_narrower_filters() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-pattern-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        let responses = run(
            &security,
            json!([
                { "command": "createRole", "rolename": "tenant" },
                { "command": "addRoleACL", "rolename": "tenant", "acltype": "subscribePattern",
                  "topic": "tenants/acme/+", "allow": true },
                { "command": "createClient", "username": "acme", "password": "pw",
                  "roles": [{ "rolename": "tenant" }] },
            ]),
        );
        assert!(
            responses.iter().all(|r| r.get("error").is_none()),
            "{responses:?}"
        );

        let acme = identity("acme");
        for (filter, decision) in [
            ("tenants/acme/a", Decision::Allow),
            ("tenants/acme/+", Decision::Allow),
            ("tenants/acme/#", Decision::Deny),
            ("tenants/acme/a/#", Decision::Deny),
            ("tenants/+/a", Decision::Deny),
        ] {
            assert_eq!(
                security.authorize(&acme, filter, Access::Subscribe).await,
                decision,
                "{filter}"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_a_client_id_binds_the_client() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-bound-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        let responses = run(
            &security,
            json!([
                { "command": "createClient", "username": "s1", "password": "pw",
                  "clientid": "dev1" },
                { "command": "getClient", "username": "s1" },
            ]),
        );
        assert!(responses[0].get("error").is_none(), "{responses:?}");
        assert_eq!(responses[1]["data"]["client"]["clientid"], "dev1");

        let credentials = Credentials::new("s1", b"pw");
        // An empty client id is one the broker is to assign
        for (client_id, decision) in [
            ("dev1", Decision::Allow),
            ("dev2", Decision::Deny),
            ("", Decision::Deny),
        ] {
            let client = ClientInfo::new(client_id, Some("s1".to_string()));
            assert_eq!(
                security.authenticate(&client, &credentials).await,
                decision,
                "{client_id:?}"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    /// Connects `username`, password `pw`, as client `username`.
    async fn connect(port: u16, username: &str) -> mercurio_client::MqttClient {
        let options = mercurio_client::ConnectOptions::new("127.0.0.1", port)
            .client_id(username)
            .username(username)
            .password("pw");
        mercurio_client::MqttClient::connect(options)
            .await
            .unwrap_or_else(|e| panic!("{username} failed to connect: {e}"))
    }

    /// Whether the broker disconnected `client` for an administrative
    /// action within a while.
    async fn kicked(client: &mercurio_client::MqttClient) -> bool {
        use mercurio_client::{DisconnectReason, Event};
        use mercurio_core::reason::ReasonCode;

        loop {
            let event =
                tokio::time::timeout(std::time::Duration::from_millis(500), client.recv()).await;
            match event {
                Err(_) => return false,
                Ok(Some(Event::Disconnected {
                    reason: DisconnectReason::ServerInitiated { reason, .. },
                })) => return reason == ReasonCode::AdministrativeAction,
                Ok(Some(Event::Disconnected { reason })) => panic!("{reason:?}"),
                Ok(Some(_)) => continue,
                Ok(None) => panic!("ended without a disconnect event"),
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_changes_disconnect_the_clients_they_affect() {
        use mercurio_server::server::{ListenerConfig, Server, ServerConfig, Transport};

        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-kick-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = std::sync::Arc::new(DynamicSecurity::load(path).unwrap());
        let mut config = ServerConfig::default();
        config.plugins = vec![security.clone()];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Server::builder()
            .config(config)
            .listener(ListenerConfig::new(listener, Transport::Mqtt))
            .storage(std::sync::Arc::new(
                mercurio_storage::memory::MemoryStore::new(),
            ))
            .build()
            .await
            .unwrap();
        security.set_admin(server.admin());
        tokio::spawn(server.run(std::future::pending::<()>()));

        let admin = ClientInfo::new("admin", Some("admin".to_string()));
        let command = |commands: Value| {
            let security = security.clone();
            let admin = admin.clone();
            async move {
                let request = json!({ "commands": commands }).to_string();
                let answer = security
                    .on_control(&admin, "v1", Bytes::from(request))
                    .await
                    .unwrap();
                let answer: Value = serde_json::from_slice(&answer).unwrap();
                for response in answer["responses"].as_array().unwrap() {
                    assert!(response.get("error").is_none(), "{answer}");
                }
            }
        };

        // Each case: setup, then the command that must disconnect `user`
        // and no one else
        let cases = [
            (
                json!([]),
                json!({ "command": "deleteClient", "username": "user" }),
            ),
            (
                json!([]),
                json!({ "command": "disableClient", "username": "user" }),
            ),
            (
                json!([]),
                json!({ "command": "setClientPassword", "username": "user", "password": "x" }),
            ),
            (
                json!([{ "command": "addClientRole", "username": "user", "rolename": "r" }]),
                json!({ "command": "removeClientRole", "username": "user", "rolename": "r" }),
            ),
            (
                json!([]),
                json!({ "command": "addClientRole", "username": "user", "rolename": "r" }),
            ),
            (
                json!([{ "command": "addGroupClient", "username": "user", "groupname": "g" }]),
                json!({ "command": "removeGroupClient", "username": "user", "groupname": "g" }),
            ),
            (
                json!([]),
                json!({ "command": "addGroupClient", "username": "user", "groupname": "g" }),
            ),
            (
                json!([{ "command": "addGroupClient", "username": "user", "groupname": "g" }]),
                json!({ "command": "removeGroupRole", "groupname": "g", "rolename": "r" }),
            ),
            (
                json!([{ "command": "addGroupClient", "username": "user", "groupname": "g" },
                       { "command": "removeGroupRole", "groupname": "g", "rolename": "r" }]),
                json!({ "command": "addGroupRole", "groupname": "g", "rolename": "r" }),
            ),
            (
                json!([{ "command": "addGroupClient", "username": "user", "groupname": "g" }]),
                json!({ "command": "deleteGroup", "groupname": "g" }),
            ),
            (
                json!([{ "command": "addGroupClient", "username": "user", "groupname": "g" }]),
                json!({ "command": "deleteRole", "rolename": "r" }),
            ),
            (
                json!([{ "command": "addClientRole", "username": "user", "rolename": "r" }]),
                json!({ "command": "removeRoleACL", "rolename": "r",
                        "acltype": "subscribePattern", "topic": "a/#" }),
            ),
            (
                json!([{ "command": "addClientRole", "username": "user", "rolename": "r" }]),
                json!({ "command": "addRoleACL", "rolename": "r",
                        "acltype": "subscribePattern", "topic": "b/#", "allow": true }),
            ),
            (
                json!([]),
                json!({ "command": "modifyClient", "username": "user", "password": "x" }),
            ),
            (
                json!([]),
                json!({ "command": "modifyClient", "username": "user",
                        "roles": [{ "rolename": "r" }] }),
            ),
            (
                json!([{ "command": "addGroupClient", "username": "user", "groupname": "g" }]),
                json!({ "command": "modifyGroup", "groupname": "g", "roles": [] }),
            ),
            (
                json!([{ "command": "addGroupClient", "username": "user", "groupname": "g" }]),
                json!({ "command": "modifyGroup", "groupname": "g", "clients": [] }),
            ),
            (
                json!([]),
                json!({ "command": "modifyGroup", "groupname": "g",
                        "clients": [{ "username": "user" }] }),
            ),
            (
                json!([{ "command": "addClientRole", "username": "user", "rolename": "r" }]),
                json!({ "command": "modifyRole", "rolename": "r", "acls": [] }),
            ),
        ];
        for (n, (setup, change)) in cases.into_iter().enumerate() {
            let user = format!("user{n}");
            let bystander = format!("bystander{n}");
            let rename = |value: Value| {
                serde_json::from_str::<Value>(
                    &value.to_string().replace("\"user\"", &format!("{user:?}")),
                )
                .unwrap()
            };
            command(json!([
                { "command": "createRole", "rolename": "r", "acls": [
                    { "acltype": "subscribePattern", "topic": "a/#", "allow": true },
                ]},
                { "command": "createGroup", "groupname": "g", "roles": [{ "rolename": "r" }] },
                { "command": "createClient", "username": user, "password": "pw" },
                { "command": "createClient", "username": bystander, "password": "pw" },
            ]))
            .await;
            command(rename(setup)).await;
            let affected = connect(port, &user).await;
            let other = connect(port, &bystander).await;

            command(json!([rename(change.clone())])).await;
            assert!(kicked(&affected).await, "{change}");
            assert!(!kicked(&other).await, "{change}");

            let _ = other.disconnect().await;
            let cleanup = [
                json!({ "command": "deleteClient", "username": user }),
                json!({ "command": "deleteClient", "username": bystander }),
                json!({ "command": "deleteGroup", "groupname": "g" }),
                json!({ "command": "deleteRole", "rolename": "r" }),
            ];
            let request = json!({ "commands": cleanup }).to_string();
            security.handle(request.as_bytes());
        }

        // Anonymous clients have the anonymous group's roles
        command(json!([
            { "command": "createGroup", "groupname": "guests" },
            { "command": "createClient", "username": "named", "password": "pw" },
        ]))
        .await;
        let options = mercurio_client::ConnectOptions::new("127.0.0.1", port).client_id("anon");
        let anonymous = mercurio_client::MqttClient::connect(options).await.unwrap();
        let named = connect(port, "named").await;
        command(json!([{ "command": "setAnonymousGroup", "groupname": "guests" }])).await;
        assert!(kicked(&anonymous).await);
        assert!(!kicked(&named).await);
        std::fs::remove_file(path).unwrap();
    }

    /// A change disconnects the affected user's client, and leaves another
    /// user's client with the same id on another mount point connected.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_changes_disconnect_only_the_affected_mount_point() {
        use mercurio_server::server::{ListenerConfig, Server, ServerConfig, Transport};

        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-mounted-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = std::sync::Arc::new(DynamicSecurity::load(path).unwrap());
        let mut config = ServerConfig::default();
        config.plugins = vec![security.clone()];
        let mounted = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mounted_port = mounted.local_addr().unwrap().port();
        let mut mounted = ListenerConfig::new(mounted, Transport::Mqtt);
        mounted.mount_point = Some("tenant/".into());
        let listeners = vec![mounted];
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = Server::builder()
            .config(config)
            .listener(ListenerConfig::new(listener, Transport::Mqtt))
            .listeners(listeners)
            .storage(std::sync::Arc::new(
                mercurio_storage::memory::MemoryStore::new(),
            ))
            .build()
            .await
            .unwrap();
        security.set_admin(server.admin());
        tokio::spawn(server.run(std::future::pending::<()>()));
        run(
            &security,
            json!([
                { "command": "createClient", "username": "victim", "password": "pw" },
                { "command": "createClient", "username": "other", "password": "pw" },
            ]),
        );

        let as_shared = |port: u16, username: &str| {
            mercurio_client::ConnectOptions::new("127.0.0.1", port)
                .client_id("shared")
                .username(username)
                .password("pw")
        };
        let affected = mercurio_client::MqttClient::connect(as_shared(port, "victim"))
            .await
            .unwrap();
        let bystander = mercurio_client::MqttClient::connect(as_shared(mounted_port, "other"))
            .await
            .unwrap();
        let admin = ClientInfo::new("admin", Some("admin".to_string()));
        let request = json!({ "commands": [{ "command": "disableClient", "username": "victim" }] });
        security
            .on_control(&admin, "v1", Bytes::from(request.to_string()))
            .await
            .unwrap();
        assert!(kicked(&affected).await);
        assert!(!kicked(&bystander).await);
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn test_users_it_does_not_know_get_no_access() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-gone-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        run(
            &security,
            json!([
                { "command": "setDefaultACLAccess", "acls": [
                    { "acltype": "publishClientSend", "allow": true },
                    { "acltype": "subscribe", "allow": true },
                ]},
                { "command": "createClient", "username": "s1", "password": "pw" },
                // A user name from a client certificate, with no password
                { "command": "createClient", "username": "device-7" },
            ]),
        );
        let all = [Access::Write, Access::Read, Access::Subscribe];
        for username in ["s1", "device-7"] {
            for access in all {
                assert_eq!(
                    security
                        .authorize(&identity(username), "anything", access)
                        .await,
                    Decision::Allow,
                    "{username} {access:?}"
                );
            }
        }

        // Deleted while connected, or never created, as a user name from a
        // certificate or another plugin may be
        run(
            &security,
            json!([{ "command": "deleteClient", "username": "s1" }]),
        );
        for username in ["s1", "stranger"] {
            for access in all {
                assert_eq!(
                    security
                        .authorize(&identity(username), "anything", access)
                        .await,
                    Decision::Deny,
                    "{username} {access:?}"
                );
            }
        }
        // Anonymous clients keep the default access
        let anonymous = ClientInfo::new("a", None);
        for access in all {
            assert_eq!(
                security.authorize(&anonymous, "anything", access).await,
                Decision::Allow,
                "{access:?}"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_root_gives_the_new_file_to_the_owner_of_its_directory() {
        // Root creating it in the service's state directory
        assert_eq!(adopted_owner(0, (998, 997)), Some((998, 997)));
        // Root's own directory, or a user creating it: kept as created
        assert_eq!(adopted_owner(0, (0, 0)), None);
        assert_eq!(adopted_owner(1000, (998, 997)), None);

        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-owner-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::metadata(path).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        let directory = std::fs::metadata(std::env::temp_dir()).unwrap();
        let expected = adopted_owner(metadata.uid(), (directory.uid(), directory.gid()));
        assert!(expected.is_none_or(|owner| owner == (metadata.uid(), metadata.gid())));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn test_unknown_users_cost_a_password_check() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-timing-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        run(
            &security,
            json!([
                { "command": "createClient", "username": "off", "password": "pw" },
                { "command": "disableClient", "username": "off" },
                { "command": "createClient", "username": "nopassword" },
            ]),
        );
        let time = |username: &str| {
            let started = std::time::Instant::now();
            assert!(!security.check_password(username, b"wrong", "c"));
            started.elapsed()
        };
        let wrong = time("admin");
        for username in ["nobody", "off", "nopassword"] {
            let elapsed = time(username);
            assert!(
                elapsed * 4 > wrong,
                "{username}: {elapsed:?}, a wrong password {wrong:?}"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    /// A state with the administrator of `init`, at a path of its own.
    struct Fixture {
        path: String,
        security: DynamicSecurity,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("mercuriod-dynsec-{name}-{}", std::process::id()))
                .to_str()
                .unwrap()
                .to_string();
            let _ = std::fs::remove_file(&path);
            DynamicSecurity::init(&path, "admin", "secret").unwrap();
            let security = DynamicSecurity::load(&path).unwrap();
            Self { path, security }
        }

        /// Runs `commands`, failing on any error.
        fn run_ok(&self, commands: Value) -> Vec<Value> {
            let responses = run(&self.security, commands);
            for response in &responses {
                assert!(response.get("error").is_none(), "{responses:?}");
            }
            responses
        }

        /// Whether `username` may publish to `topic`.
        fn publishes(&self, username: &str, topic: &str) -> bool {
            let data = self.security.data.read().unwrap();
            data.authorize(Some(username), topic, Access::Write)
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// A role allowing or denying publishing to `t/#` with ACL priority
    /// `acl_priority`.
    fn publishing_role(rolename: &str, allow: bool, acl_priority: i64) -> Value {
        json!({ "command": "createRole", "rolename": rolename, "acls": [
            { "acltype": "publishClientSend", "topic": "t/#", "allow": allow,
              "priority": acl_priority },
        ]})
    }

    #[test]
    fn test_own_roles_decide_before_group_roles() {
        let fixture = Fixture::new("own-first");
        fixture.run_ok(json!([
            publishing_role("own", false, 0),
            publishing_role("grouped", true, 100),
            { "command": "createGroup", "groupname": "g", "roles": [{ "rolename": "grouped" }] },
            { "command": "createClient", "username": "c",
              "roles": [{ "rolename": "own" }], "groups": [{ "groupname": "g" }] },
        ]));
        // As in mosquitto, the ACL priority orders a role's ACLs only
        assert!(!fixture.publishes("c", "t/1"));
    }

    #[test]
    fn test_roles_are_checked_by_priority() {
        let fixture = Fixture::new("role-priority");
        fixture.run_ok(json!([
            publishing_role("low", true, 100),
            publishing_role("high", false, 0),
            { "command": "createClient", "username": "c", "roles": [
                { "rolename": "low", "priority": 1 },
                { "rolename": "high", "priority": 5 },
            ]},
        ]));
        assert!(!fixture.publishes("c", "t/1"));

        fixture.run_ok(json!([
            { "command": "addClientRole", "username": "c", "rolename": "low", "priority": 9 },
        ]));
        assert!(fixture.publishes("c", "t/1"));
    }

    #[test]
    fn test_groups_and_their_roles_are_checked_by_priority() {
        let fixture = Fixture::new("group-priority");
        fixture.run_ok(json!([
            publishing_role("allow", true, 0),
            publishing_role("deny", false, 0),
            { "command": "createGroup", "groupname": "allowing" },
            { "command": "addGroupRole", "groupname": "allowing", "rolename": "allow" },
            { "command": "createGroup", "groupname": "denying" },
            { "command": "addGroupRole", "groupname": "denying", "rolename": "deny" },
            { "command": "createClient", "username": "c" },
            { "command": "addGroupClient", "groupname": "allowing", "username": "c",
              "priority": 1 },
            { "command": "addGroupClient", "groupname": "denying", "username": "c",
              "priority": 2 },
        ]));
        assert!(!fixture.publishes("c", "t/1"));
        fixture.run_ok(json!([
            { "command": "addGroupClient", "groupname": "allowing", "username": "c",
              "priority": 3 },
        ]));
        assert!(fixture.publishes("c", "t/1"));

        // Within a group, its roles by their priority
        fixture.run_ok(json!([
            { "command": "createGroup", "groupname": "both", "roles": [
                { "rolename": "allow", "priority": 1 },
                { "rolename": "deny", "priority": 2 },
            ]},
            { "command": "createClient", "username": "d", "groups": [{ "groupname": "both" }] },
        ]));
        assert!(!fixture.publishes("d", "t/1"));
        fixture.run_ok(json!([
            { "command": "addGroupRole", "groupname": "both", "rolename": "allow",
              "priority": 3 },
        ]));
        assert!(fixture.publishes("d", "t/1"));
    }

    #[test]
    fn test_equal_priorities_keep_the_order_added_and_roles_without_a_match_pass_on() {
        let fixture = Fixture::new("ties");
        fixture.run_ok(json!([
            { "command": "createRole", "rolename": "other", "acls": [
                { "acltype": "publishClientSend", "topic": "elsewhere/#", "allow": false,
                  "priority": 50 },
            ]},
            publishing_role("deny", false, 0),
            publishing_role("allow", true, 0),
            { "command": "createClient", "username": "c", "roles": [
                { "rolename": "other" }, { "rolename": "deny" }, { "rolename": "allow" },
            ]},
        ]));
        assert!(!fixture.publishes("c", "t/1"));
    }

    #[test]
    fn test_priorities_are_shown_and_must_be_numbers() {
        let fixture = Fixture::new("priority-shown");
        fixture.run_ok(json!([
            { "command": "createRole", "rolename": "r" },
            { "command": "createRole", "rolename": "s" },
            { "command": "createGroup", "groupname": "g",
              "roles": [{ "rolename": "r", "priority": 4 }] },
            { "command": "createClient", "username": "c", "roles": [
                { "rolename": "r" }, { "rolename": "s", "priority": 2 },
            ]},
            { "command": "addGroupClient", "groupname": "g", "username": "c", "priority": 7 },
        ]));
        let shown = fixture.run_ok(json!([
            { "command": "getClient", "username": "c" },
            { "command": "getGroup", "groupname": "g" },
        ]));
        // Mosquitto's default, -1, is left out
        assert_eq!(
            shown[0]["data"]["client"]["roles"],
            json!([{ "rolename": "s", "priority": 2 }, { "rolename": "r" }])
        );
        assert_eq!(
            shown[0]["data"]["client"]["groups"],
            json!([{ "groupname": "g", "priority": 7 }])
        );
        assert_eq!(
            shown[1]["data"]["group"]["roles"],
            json!([{ "rolename": "r", "priority": 4 }])
        );

        let refused = run(
            &fixture.security,
            json!([
                { "command": "addClientRole", "username": "c", "rolename": "r",
                  "priority": "high" },
                { "command": "createClient", "username": "d",
                  "roles": [{ "rolename": "r", "priority": 1.5 }] },
                { "command": "createClient", "username": "e", "roles": [{ "priority": 1 }] },
            ]),
        );
        for response in &refused {
            assert!(response.get("error").is_some(), "{refused:?}");
        }
        let listed = fixture.run_ok(json!([{ "command": "listClients" }]));
        assert_eq!(listed[0]["data"]["clients"], json!(["admin", "c"]));
    }

    #[tokio::test]
    async fn test_scram_credentials_are_found_by_the_prepared_name() {
        let fixture = Fixture::new("scram-prepared");
        fixture.run_ok(json!([
            { "command": "createClient", "username": "us\u{00AD}er", "password": "pw" },
        ]));
        let found = fixture
            .security
            .scram_credentials(&ClientInfo::new("any", None), "user")
            .await;
        assert!(found.is_some());
        let refused = run(
            &fixture.security,
            json!([
                { "command": "createClient", "username": "\u{00AD}", "password": "pw" },
            ]),
        );
        assert!(refused[0].get("error").is_some(), "{refused:?}");
    }

    #[tokio::test]
    async fn test_clients_with_one_prepared_name_are_refused() {
        let fixture = Fixture::new("scram-collide");
        fixture.run_ok(json!([{ "command": "createClient", "username": "user" }]));
        let refused = run(
            &fixture.security,
            json!([{ "command": "createClient", "username": "us\u{00AD}er" }]),
        );
        assert!(refused[0].get("error").is_some(), "{refused:?}");
    }

    #[tokio::test]
    async fn test_setting_a_password_sets_scram_credentials() {
        let fixture = Fixture::new("scram");
        let scram = |username: &'static str, client_id: &'static str| {
            let security = &fixture.security;
            async move {
                security
                    .scram_credentials(&ClientInfo::new(client_id, None), username)
                    .await
            }
        };
        assert!(scram("admin", "any").await.is_some());

        fixture.run_ok(json!([
            { "command": "createClient", "username": "c", "password": "pw", "clientid": "dev1" },
            { "command": "createClient", "username": "nopassword" },
        ]));
        let first = scram("c", "dev1").await.unwrap();
        assert_eq!(scram("c", "other").await, None, "bound to its client id");
        assert_eq!(scram("nopassword", "any").await, None);
        assert_eq!(scram("nobody", "any").await, None);

        fixture.run_ok(json!([
            { "command": "setClientPassword", "username": "c", "password": "pw2" },
        ]));
        let second = scram("c", "dev1").await.unwrap();
        assert_ne!(first, second);
        fixture.run_ok(json!([
            { "command": "modifyClient", "username": "c", "password": "pw3" },
        ]));
        assert_ne!(scram("c", "dev1").await.unwrap(), second);

        fixture.run_ok(json!([{ "command": "disableClient", "username": "c" }]));
        assert_eq!(scram("c", "dev1").await, None);

        // A password changed by a release that knows nothing of SCRAM
        // leaves keys that no longer hold
        {
            let mut data = fixture.security.data.write().unwrap();
            let admin = data
                .clients
                .iter_mut()
                .find(|c| c.username == "admin")
                .unwrap();
            admin.password = Some(hash_password("admin", "changed").unwrap().0);
        }
        assert_eq!(scram("admin", "any").await, None);

        // The keys survive saving and loading
        fixture.run_ok(json!([{ "command": "enableClient", "username": "c" }]));
        let reloaded = DynamicSecurity::load(&fixture.path).unwrap();
        assert!(reloaded
            .scram_credentials(&ClientInfo::new("dev1", None), "c")
            .await
            .is_some());
    }

    #[test]
    fn test_modify_client_changes_what_it_is_given() {
        let fixture = Fixture::new("modify-client");
        fixture.run_ok(json!([
            publishing_role("allow", true, 0),
            publishing_role("deny", false, 0),
            { "command": "createGroup", "groupname": "g1" },
            { "command": "createGroup", "groupname": "g2" },
            { "command": "createClient", "username": "c", "password": "old",
              "clientid": "dev1", "textname": "Sensor", "roles": [{ "rolename": "deny" }],
              "groups": [{ "groupname": "g1" }] },
        ]));
        assert!(fixture.security.check_password("c", b"old", "dev1"));

        // Absent arguments leave things as they are
        fixture.run_ok(json!([
            { "command": "modifyClient", "username": "c", "textdescription": "Roof" },
        ]));
        let shown = fixture.run_ok(json!([{ "command": "getClient", "username": "c" }]));
        let client = &shown[0]["data"]["client"];
        assert_eq!(client["textname"], "Sensor");
        assert_eq!(client["textdescription"], "Roof");
        assert_eq!(client["clientid"], "dev1");
        assert_eq!(client["roles"], json!([{ "rolename": "deny" }]));
        assert!(fixture.security.check_password("c", b"old", "dev1"));

        // Lists replace the whole list; an empty password changes nothing
        // and an empty client id unbinds it
        fixture.run_ok(json!([
            { "command": "modifyClient", "username": "c", "password": "new", "clientid": "",
              "textname": "Probe", "roles": [{ "rolename": "allow", "priority": 2 }],
              "groups": [{ "groupname": "g2" }] },
        ]));
        let shown = fixture.run_ok(json!([{ "command": "getClient", "username": "c" }]));
        let client = &shown[0]["data"]["client"];
        assert_eq!(client["textname"], "Probe");
        assert_eq!(client["clientid"], Value::Null);
        assert_eq!(
            client["roles"],
            json!([{ "rolename": "allow", "priority": 2 }])
        );
        assert_eq!(client["groups"], json!([{ "groupname": "g2" }]));
        assert!(!fixture.security.check_password("c", b"old", "any"));
        assert!(fixture.security.check_password("c", b"new", "any"));
        assert!(fixture.publishes("c", "t/1"));
        fixture.run_ok(json!([{ "command": "modifyClient", "username": "c", "password": "" }]));
        assert!(fixture.security.check_password("c", b"new", "any"));

        // One bad argument and nothing changes, the password included
        for bad in [
            json!({ "command": "modifyClient", "username": "c", "password": "other",
                    "roles": [{ "rolename": "missing" }] }),
            json!({ "command": "modifyClient", "username": "c", "password": "other",
                    "groups": [{ "groupname": "missing" }] }),
            json!({ "command": "modifyClient", "username": "c", "password": "other",
                    "clientid": 7 }),
            json!({ "command": "modifyClient", "username": "c", "password": "other",
                    "textname": false }),
            json!({ "command": "modifyClient", "username": "nobody", "password": "other" }),
        ] {
            let responses = run(&fixture.security, json!([bad]));
            assert!(responses[0].get("error").is_some(), "{bad}");
        }
        assert!(fixture.security.check_password("c", b"new", "any"));
        let shown = fixture.run_ok(json!([{ "command": "getClient", "username": "c" }]));
        assert_eq!(shown[0]["data"]["client"]["textname"], "Probe");
    }

    #[test]
    fn test_modify_role_replaces_its_acls() {
        let fixture = Fixture::new("modify-role");
        fixture.run_ok(json!([
            publishing_role("r", false, 0),
            { "command": "createClient", "username": "c", "roles": [{ "rolename": "r" }] },
        ]));
        assert!(!fixture.publishes("c", "t/1"));

        fixture.run_ok(json!([
            { "command": "modifyRole", "rolename": "r", "textname": "Writers" },
        ]));
        assert!(!fixture.publishes("c", "t/1"));

        fixture.run_ok(json!([
            { "command": "modifyRole", "rolename": "r", "textdescription": "May write", "acls": [
                { "acltype": "publishClientSend", "topic": "t/#", "allow": true },
                { "acltype": "publishClientSend", "topic": "t/secret", "allow": false,
                  "priority": 1 },
            ]},
        ]));
        assert!(fixture.publishes("c", "t/1"));
        assert!(!fixture.publishes("c", "t/secret"));
        let shown = fixture.run_ok(json!([{ "command": "getRole", "rolename": "r" }]));
        let role = &shown[0]["data"]["role"];
        assert_eq!(role["textname"], "Writers");
        assert_eq!(role["textdescription"], "May write");
        assert_eq!(role["acls"].as_array().unwrap().len(), 2);

        for bad in [
            json!({ "command": "modifyRole", "rolename": "r", "textname": "X",
                    "acls": [{ "acltype": "bogus", "topic": "t", "allow": true }] }),
            json!({ "command": "modifyRole", "rolename": "r", "textname": "X", "acls": 5 }),
            json!({ "command": "modifyRole", "rolename": "missing", "textname": "X" }),
        ] {
            let responses = run(&fixture.security, json!([bad]));
            assert!(responses[0].get("error").is_some(), "{bad}");
        }
        assert!(fixture.publishes("c", "t/1"));
        let shown = fixture.run_ok(json!([{ "command": "getRole", "rolename": "r" }]));
        assert_eq!(shown[0]["data"]["role"]["textname"], "Writers");
    }

    #[test]
    fn test_modify_group_replaces_its_roles_and_clients() {
        let fixture = Fixture::new("modify-group");
        fixture.run_ok(json!([
            publishing_role("allow", true, 0),
            publishing_role("deny", false, 0),
            { "command": "createGroup", "groupname": "g", "roles": [{ "rolename": "deny" }] },
            { "command": "createClient", "username": "in" },
            { "command": "createClient", "username": "out" },
            { "command": "addGroupClient", "groupname": "g", "username": "in" },
        ]));
        assert!(!fixture.publishes("in", "t/1"));

        fixture.run_ok(json!([
            { "command": "modifyGroup", "groupname": "g", "textname": "Field",
              "roles": [{ "rolename": "allow" }],
              "clients": [{ "username": "out", "priority": 4 }] },
        ]));
        let shown = fixture.run_ok(json!([
            { "command": "getGroup", "groupname": "g" },
            { "command": "getClient", "username": "out" },
            { "command": "getClient", "username": "in" },
        ]));
        let group = &shown[0]["data"]["group"];
        assert_eq!(group["textname"], "Field");
        assert_eq!(group["roles"], json!([{ "rolename": "allow" }]));
        assert_eq!(
            group["clients"],
            json!([{ "username": "out", "priority": 4 }])
        );
        assert_eq!(
            shown[1]["data"]["client"]["groups"],
            json!([{ "groupname": "g", "priority": 4 }])
        );
        assert_eq!(shown[2]["data"]["client"]["groups"], json!([]));
        assert!(fixture.publishes("out", "t/1"));

        // Absent lists stay as they are
        fixture.run_ok(json!([
            { "command": "modifyGroup", "groupname": "g", "textdescription": "Outdoors" },
        ]));
        assert!(fixture.publishes("out", "t/1"));

        for bad in [
            json!({ "command": "modifyGroup", "groupname": "g", "textname": "X",
                    "roles": [{ "rolename": "deny" }], "clients": [{ "username": "nobody" }] }),
            json!({ "command": "modifyGroup", "groupname": "g", "textname": "X",
                    "clients": [{ "username": "in" }], "roles": [{ "rolename": "missing" }] }),
            json!({ "command": "modifyGroup", "groupname": "missing", "textname": "X" }),
        ] {
            let responses = run(&fixture.security, json!([bad]));
            assert!(responses[0].get("error").is_some(), "{bad}");
        }
        let shown = fixture.run_ok(json!([{ "command": "getGroup", "groupname": "g" }]));
        let group = &shown[0]["data"]["group"];
        assert_eq!(group["textname"], "Field");
        assert_eq!(group["textdescription"], "Outdoors");
        assert_eq!(
            group["clients"],
            json!([{ "username": "out", "priority": 4 }])
        );
        assert!(fixture.publishes("out", "t/1"));
    }

    /// A file as mosquitto's plugin writes it. `admin`'s password is
    /// "secret", hashed by mosquitto_passwd 2.0.18 as in `credentials.rs`.
    const MOSQUITTO_FILE: &str = r##"{
      "defaultACLAccess": {
        "publishClientSend": true, "publishClientReceive": true,
        "subscribe": false, "unsubscribe": false
      },
      "clients": [{
        "username": "admin", "textName": "Dynsec admin user",
        "roles": [{ "rolename": "admin" }],
        "password": "1FhG/H49+FnZ9qx8YFDOUZOiE+Yd+yOKjQ9AzfsTHwepsiXOTbzCSojC7kA3U/VicSJdksyipSowQpvFhpo0iQ==",
        "salt": "lT8RRynk8Dr5uL1b", "iterations": 101
      }, {
        "username": "sensor", "clientid": "dev1", "textname": "Sensor",
        "textdescription": "On the roof", "disabled": true,
        "roles": [{ "rolename": "reader", "priority": 3 }, { "rolename": "gone" }]
      }, {
        "username": "legacy", "password": "c2VjcmV0"
      }],
      "groups": [{
        "groupname": "field", "textname": "Field devices",
        "roles": [{ "rolename": "writer", "priority": 2 }],
        "clients": [{ "username": "sensor", "priority": 5 }, { "username": "nobody" }]
      }, {
        "groupname": "guests", "roles": [{ "rolename": "reader" }]
      }],
      "roles": [{
        "rolename": "admin",
        "acls": [
          { "acltype": "publishClientSend", "topic": "$CONTROL/dynamic-security/#",
            "allow": true },
          { "acltype": "unsubscribePattern", "topic": "#", "allow": true }
        ]
      }, {
        "rolename": "reader", "textdescription": "Reads news",
        "acls": [{ "acltype": "subscribePattern", "topic": "news/#", "priority": 1,
                   "allow": true }]
      }, {
        "rolename": "writer",
        "acls": [{ "acltype": "publishClientSend", "topic": "news/#", "allow": false }]
      }],
      "anonymousGroup": "guests"
    }"##;

    /// Paths for a mosquitto file holding `contents` and a target beside it.
    fn import_paths(name: &str, contents: &str) -> (String, String) {
        let dir = std::env::temp_dir();
        let source = dir.join(format!(
            "mercuriod-import-{name}-{}.json",
            std::process::id()
        ));
        let target = dir.join(format!("mercuriod-imported-{name}-{}", std::process::id()));
        std::fs::write(&source, contents).unwrap();
        let _ = std::fs::remove_file(&target);
        (
            source.to_str().unwrap().to_string(),
            target.to_str().unwrap().to_string(),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_import_converts_a_mosquitto_file() {
        let (source, target) = import_paths("full", MOSQUITTO_FILE);
        let notes = DynamicSecurity::import(&source, &target).unwrap();
        let security = DynamicSecurity::load(&target).unwrap();

        // Mosquitto's PBKDF2 hashes keep working
        assert!(security.check_password("admin", b"secret", "any"));
        assert!(!security.check_password("admin", b"wrong", "any"));
        assert_eq!(
            security
                .authorize_control(&identity("admin"), COMMAND_TOPIC)
                .await,
            Decision::Allow
        );

        let shown = run(
            &security,
            json!([
                { "command": "getClient", "username": "sensor" },
                { "command": "getGroup", "groupname": "field" },
                { "command": "getRole", "rolename": "reader" },
                { "command": "getAnonymousGroup" },
                { "command": "getDefaultACLAccess" },
                { "command": "getClient", "username": "admin" },
            ]),
        );
        let sensor = &shown[0]["data"]["client"];
        assert_eq!(sensor["clientid"], "dev1");
        assert_eq!(sensor["textname"], "Sensor");
        assert_eq!(sensor["textdescription"], "On the roof");
        assert_eq!(sensor["disabled"], true);
        assert_eq!(
            sensor["roles"],
            json!([{ "rolename": "reader", "priority": 3 }])
        );
        assert_eq!(
            sensor["groups"],
            json!([{ "groupname": "field", "priority": 5 }])
        );
        let field = &shown[1]["data"]["group"];
        assert_eq!(field["textname"], "Field devices");
        assert_eq!(
            field["roles"],
            json!([{ "rolename": "writer", "priority": 2 }])
        );
        let reader = &shown[2]["data"]["role"];
        assert_eq!(reader["textdescription"], "Reads news");
        assert_eq!(reader["acls"][0]["priority"], 1);
        assert_eq!(shown[3]["data"]["group"]["groupname"], "guests");
        assert_eq!(
            shown[4]["data"]["acls"],
            json!([
                { "acltype": "publishClientSend", "allow": true },
                { "acltype": "publishClientReceive", "allow": true },
                { "acltype": "subscribe", "allow": false },
            ])
        );
        assert_eq!(shown[5]["data"]["client"]["textname"], "Dynsec admin user");

        // Anonymous clients get the anonymous group's roles
        let anonymous = ClientInfo::new("a", None);
        assert_eq!(
            security
                .authorize(&anonymous, "news/today", Access::Subscribe)
                .await,
            Decision::Allow
        );

        // What could not come along is reported
        for expected in [
            "unsubscribe",
            "unsubscribePattern",
            "gone",
            "nobody",
            "legacy",
        ] {
            assert!(
                notes.iter().any(|note| note.contains(expected)),
                "{expected}: {notes:?}"
            );
        }
        assert!(!security.check_password("legacy", b"secret", "any"));

        // A password set again is hashed with Argon2
        run(
            &security,
            json!([{ "command": "setClientPassword", "username": "admin", "password": "n3w" }]),
        );
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&target).unwrap()).unwrap();
        let admin = saved["clients"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["username"] == "admin")
            .unwrap();
        assert!(admin["password"].as_str().unwrap().starts_with("$argon2"));
        assert!(security.check_password("admin", b"n3w", "any"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::remove_file(&source).unwrap();
        std::fs::remove_file(&target).unwrap();
    }

    #[test]
    fn test_import_refuses_to_overwrite_and_does_not_quote_the_file() {
        let (source, target) = import_paths("exists", MOSQUITTO_FILE);
        std::fs::write(&target, "keep").unwrap();
        let error = DynamicSecurity::import(&source, &target).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "keep");
        std::fs::remove_file(&target).unwrap();

        std::fs::write(
            &source,
            r#"{"clients": [{"username": 5, "password": "SECRET"}]}"#,
        )
        .unwrap();
        let error = DynamicSecurity::import(&source, &target).unwrap_err();
        assert!(!error.contains("SECRET"), "{error}");
        assert!(!Path::new(&target).exists());
        std::fs::remove_file(&source).unwrap();
    }

    #[test]
    fn test_files_without_priorities_still_load() {
        let fixture = Fixture::new("released");
        // As the released version writes it
        std::fs::write(
            &fixture.path,
            r#"{
              "defaultAclAccess": { "publishClientSend": false, "publishClientReceive": true,
                                    "subscribe": false },
              "clients": [{ "username": "c", "disabled": false, "roles": ["deny", "allow"],
                            "groups": ["g"] }],
              "roles": [
                { "rolename": "deny", "acls": [{ "acltype": "publishClientSend",
                  "topic": "t/#", "priority": 0, "allow": false }] },
                { "rolename": "allow", "acls": [{ "acltype": "publishClientSend",
                  "topic": "t/#", "priority": 9, "allow": true }] }
              ],
              "groups": [{ "groupname": "g", "roles": ["allow"] }]
            }"#,
        )
        .unwrap();
        fixture.security.reload().unwrap();
        assert!(!fixture.publishes("c", "t/1"));

        // Saved again, roles without a priority stay plain names
        fixture.run_ok(json!([{ "command": "createRole", "rolename": "x" }]));
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&fixture.path).unwrap()).unwrap();
        assert_eq!(saved["clients"][0]["roles"], json!(["deny", "allow"]));
        assert_eq!(saved["groups"][0]["roles"], json!(["allow"]));

        fixture.run_ok(json!([
            { "command": "addClientRole", "username": "c", "rolename": "allow", "priority": 1 },
        ]));
        fixture.security.reload().unwrap();
        assert!(fixture.publishes("c", "t/1"));
    }

    #[test]
    fn test_invalid_default_access_changes_nothing() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-default-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        let responses = run(
            &security,
            json!([
                { "command": "setDefaultACLAccess", "acls": [
                    { "acltype": "publishClientSend", "allow": true },
                    { "acltype": "bogus", "allow": true },
                ]},
                { "command": "createClient", "username": "s1", "password": "pw" },
            ]),
        );
        assert!(responses[0].get("error").is_some(), "{responses:?}");
        assert!(responses[1].get("error").is_none(), "{responses:?}");

        security.reload().unwrap();
        let access = run(&security, json!([{ "command": "getDefaultACLAccess" }]));
        assert_eq!(
            access[0]["data"]["acls"][0],
            json!({ "acltype": "publishClientSend", "allow": false })
        );
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn test_init_grants_the_broker_admin_api() {
        let path =
            std::env::temp_dir().join(format!("mercuriod-dynsec-broker-{}", std::process::id()));
        let path = path.to_str().unwrap();
        let _ = std::fs::remove_file(path);
        DynamicSecurity::init(path, "admin", "secret").unwrap();
        let security = DynamicSecurity::load(path).unwrap();
        for topic in ["$CONTROL/broker/v1", "$CONTROL/broker/v1/listClients"] {
            assert_eq!(
                security.authorize_control(&identity("admin"), topic).await,
                Decision::Allow,
                "{topic}"
            );
        }
        for (topic, access) in [
            ("$CONTROL/broker/v1/response", Access::Subscribe),
            ("$CONTROL/broker/v1/response", Access::Read),
        ] {
            assert_eq!(
                security.authorize(&identity("admin"), topic, access).await,
                Decision::Allow,
                "{topic} {access:?}"
            );
        }
        assert_eq!(
            security
                .authorize_control(&identity("other"), "$CONTROL/broker/v1")
                .await,
            Decision::Deny
        );
        std::fs::remove_file(path).unwrap();
    }
}
