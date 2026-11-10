//! Dynamic security: clients, roles and access control managed while the
//! broker runs, with the JSON commands of mosquitto's dynamic security
//! plugin published to `$CONTROL/dynamic-security/v1`. The state is kept in
//! a JSON file, saved after every change.
//!
//! A client's roles are its own and those of its groups; clients without a
//! user name get the roles of the anonymous group, when one is set.

use std::{
    path::Path,
    sync::{Mutex, RwLock},
};

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use bytes::Bytes;
use mercurio_core::topic::{filter_covers, topic_matches_filter};
use mercurio_server::plugin::{
    ready, Access, BoxFuture, ClientInfo, Credentials, Decision, Plugin, PluginKind, PluginStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::error;

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
/// default, as in mosquitto, only receiving is allowed. It never applies to
/// `$CONTROL` topics, which stay closed unless an ACL opens them.
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
    #[serde(default)]
    roles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Client {
    username: String,
    /// Argon2 hash; a client without one can't log in
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
    /// The only client id it may connect with, when set
    #[serde(default, skip_serializing_if = "Option::is_none")]
    clientid: Option<String>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    roles: Vec<String>,
    #[serde(default)]
    groups: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Role {
    rolename: String,
    #[serde(default)]
    acls: Vec<Acl>,
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

    /// The names of the roles of a client: its own, then its groups'.
    fn roles_of(&self, username: Option<&str>) -> Vec<&str> {
        let groups: Vec<&str> = match username {
            Some(username) => {
                let Some(client) = self.clients.iter().find(|c| c.username == username) else {
                    return Vec::new();
                };
                let mut roles: Vec<&str> = client.roles.iter().map(String::as_str).collect();
                let groups = client.groups.iter().map(String::as_str);
                roles.extend(self.group_roles(groups));
                return roles;
            }
            None => self.anonymous_group.as_deref().into_iter().collect(),
        };
        self.group_roles(groups.into_iter()).collect()
    }

    fn group_roles<'a>(
        &'a self,
        groups: impl Iterator<Item = &'a str>,
    ) -> impl Iterator<Item = &'a str> {
        groups
            .filter_map(|name| self.groups.iter().find(|g| g.groupname == name))
            .flat_map(|group| group.roles.iter().map(String::as_str))
    }

    fn authorize(&self, username: Option<&str>, topic: &str, access: Access) -> bool {
        // A client disabled while connected loses its access at once
        if let Some(username) = username {
            if self
                .clients
                .iter()
                .any(|c| c.username == username && c.disabled)
            {
                return false;
            }
        }
        let decision = self
            .roles_of(username)
            .into_iter()
            .filter_map(|rolename| self.roles.iter().find(|r| r.rolename == rolename))
            .flat_map(|role| &role.acls)
            .filter(|acl| acl.applies(topic, access))
            // The first of the highest priority wins
            .fold(None::<&Acl>, |best, acl| match best {
                Some(best) if best.priority >= acl.priority => Some(best),
                _ => Some(acl),
            });
        // $CONTROL administers the broker: only an ACL opens it, never the
        // default access, which may be permissive for ordinary topics
        let control = topic == "$CONTROL" || topic.starts_with("$CONTROL/");
        match decision {
            Some(acl) => acl.allow,
            None if control => false,
            None => match access {
                Access::Write => self.default_acl_access.publish_client_send,
                Access::Read => self.default_acl_access.publish_client_receive,
                Access::Subscribe => self.default_acl_access.subscribe,
                // A kind of access added later is denied until handled
                _ => false,
            },
        }
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
                let roles = names(args.get("roles"), "rolename");
                if let Some(missing) = roles.iter().find(|r| self.role(r).is_err()) {
                    return Err(format!("Role {missing} not found"));
                }
                let groups = names(args.get("groups"), "groupname");
                if let Some(missing) = groups.iter().find(|g| self.group(g).is_err()) {
                    return Err(format!("Group {missing} not found"));
                }
                // Empty, as mosquitto takes it, binds no client id
                let clientid = match args.get("clientid") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(id)) if id.is_empty() => None,
                    Some(Value::String(id)) => Some(id.clone()),
                    Some(_) => return Err("Invalid clientid".to_string()),
                };
                let password = match args.get("password").and_then(Value::as_str) {
                    Some(password) => Some(hash_password(password)?),
                    None => None,
                };
                self.clients.push(Client {
                    username,
                    password,
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
                let hash = hash_password(&text("password")?)?;
                self.client(&text("username")?)?.password = Some(hash);
                Ok((None, true))
            }
            "enableClient" | "disableClient" => {
                self.client(&text("username")?)?.disabled = command == "disableClient";
                Ok((None, true))
            }
            "addClientRole" => {
                let rolename = text("rolename")?;
                self.role(&rolename)?;
                let client = self.client(&text("username")?)?;
                if !client.roles.contains(&rolename) {
                    client.roles.push(rolename);
                }
                Ok((None, true))
            }
            "removeClientRole" => {
                let rolename = text("rolename")?;
                self.client(&text("username")?)?
                    .roles
                    .retain(|r| *r != rolename);
                Ok((None, true))
            }
            "getClient" => {
                let client = self.client(&text("username")?)?;
                let roles: Vec<Value> = client
                    .roles
                    .iter()
                    .map(|r| json!({ "rolename": r }))
                    .collect();
                let groups: Vec<Value> = client
                    .groups
                    .iter()
                    .map(|g| json!({ "groupname": g }))
                    .collect();
                let data = json!({ "client": {
                    "username": client.username,
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
                self.roles.push(Role { rolename, acls });
                Ok((None, true))
            }
            "deleteRole" => {
                let rolename = text("rolename")?;
                self.role(&rolename)?;
                self.roles.retain(|r| r.rolename != rolename);
                for client in &mut self.clients {
                    client.roles.retain(|r| *r != rolename);
                }
                for group in &mut self.groups {
                    group.roles.retain(|r| *r != rolename);
                }
                Ok((None, true))
            }
            "createGroup" => {
                let groupname = text("groupname")?;
                if self.groups.iter().any(|g| g.groupname == groupname) {
                    return Err("Group already exists".to_string());
                }
                let roles = names(args.get("roles"), "rolename");
                if let Some(missing) = roles.iter().find(|r| self.role(r).is_err()) {
                    return Err(format!("Role {missing} not found"));
                }
                self.groups.push(Group { groupname, roles });
                Ok((None, true))
            }
            "deleteGroup" => {
                let groupname = text("groupname")?;
                self.group(&groupname)?;
                self.groups.retain(|g| g.groupname != groupname);
                for client in &mut self.clients {
                    client.groups.retain(|g| *g != groupname);
                }
                if self.anonymous_group.as_ref() == Some(&groupname) {
                    self.anonymous_group = None;
                }
                Ok((None, true))
            }
            "addGroupRole" => {
                let rolename = text("rolename")?;
                self.role(&rolename)?;
                let group = self.group(&text("groupname")?)?;
                if !group.roles.contains(&rolename) {
                    group.roles.push(rolename);
                }
                Ok((None, true))
            }
            "removeGroupRole" => {
                let rolename = text("rolename")?;
                self.group(&text("groupname")?)?
                    .roles
                    .retain(|r| *r != rolename);
                Ok((None, true))
            }
            "addGroupClient" => {
                let groupname = text("groupname")?;
                self.group(&groupname)?;
                let client = self.client(&text("username")?)?;
                if !client.groups.contains(&groupname) {
                    client.groups.push(groupname);
                }
                Ok((None, true))
            }
            "removeGroupClient" => {
                let groupname = text("groupname")?;
                self.client(&text("username")?)?
                    .groups
                    .retain(|g| *g != groupname);
                Ok((None, true))
            }
            "getGroup" => {
                let groupname = text("groupname")?;
                let members: Vec<Value> = self
                    .clients
                    .iter()
                    .filter(|c| c.groups.contains(&groupname))
                    .map(|c| json!({ "username": c.username }))
                    .collect();
                let group = self.group(&groupname)?;
                let roles: Vec<Value> = group
                    .roles
                    .iter()
                    .map(|r| json!({ "rolename": r }))
                    .collect();
                let data = json!({ "group": {
                    "groupname": group.groupname,
                    "roles": roles,
                    "clients": members,
                }});
                Ok((Some(data), false))
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

/// Names given as `[{"rolename": ...}]` or `[{"groupname": ...}]`, as
/// mosquitto does.
fn names(list: Option<&Value>, key: &str) -> Vec<String> {
    list.and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| r.get(key).and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

fn hash_password(password: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| format!("Failed to hash password: {e}"))
}

/// The dynamic security state, checking passwords and topic access.
pub struct DynamicSecurity {
    path: String,
    data: RwLock<Data>,
    /// Held while a batch or a reload runs, so concurrent batches don't
    /// overwrite each other's changes or the temporary file, and a reload
    /// doesn't land between a batch's copy of the state and its save
    commands: Mutex<()>,
}

impl DynamicSecurity {
    pub fn load(path: &str) -> Result<Self, String> {
        Ok(Self {
            path: path.to_string(),
            data: RwLock::new(read(path)?),
            commands: Mutex::new(()),
        })
    }

    /// Writes a new state file with an administrator client, `admin`, whose
    /// role may send every dynamic security and broker admin command and
    /// read `$SYS`.
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
        let data = Data {
            clients: vec![Client {
                username: admin.to_string(),
                password: Some(hash_password(password)?),
                clientid: None,
                disabled: false,
                roles: vec!["admin".to_string()],
                groups: Vec::new(),
            }],
            roles: vec![Role {
                rolename: "admin".to_string(),
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
        write(path, &data)
    }

    /// Runs a batch of commands, `{"commands": [...]}`, and returns the
    /// response batch. The file is saved once if any command changed the
    /// state; if saving fails, the changes are undone.
    fn handle(&self, request: &[u8]) -> Value {
        let request: Value = match serde_json::from_slice(request) {
            Ok(request) => request,
            Err(e) => return json!({ "responses": [{ "error": format!("Invalid JSON: {e}") }] }),
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
        let mut responses = Vec::new();
        for args in &commands {
            let command = args.get("command").and_then(Value::as_str).unwrap_or("");
            let mut response = json!({ "command": command });
            match data.apply(command, args) {
                Ok((reply, change)) => {
                    changed |= change;
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
                }
            }
        }
        json!({ "responses": responses })
    }
}

fn read(path: &str) -> Result<Data, String> {
    let contents = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read dynamic security file {path}: {e}"))?;
    serde_json::from_str(&contents).map_err(|e| {
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
    })
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

/// Flushes the directory holding `path`, where a rename is recorded.
#[cfg(unix)]
fn sync_directory(path: &str) -> std::io::Result<()> {
    let directory = match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    std::fs::File::open(directory)?.sync_all()
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
                }) => (hash.clone(), clientid.clone()),
                _ => return false,
            }
        };
        let valid = PasswordHash::new(&hash)
            .is_ok_and(|hash| Argon2::default().verify_password(password, &hash).is_ok());
        valid && bound.is_none_or(|bound| bound == client_id)
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
            let response = tokio::task::block_in_place(|| self.handle(&payload));
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
        let response = security.handle(request.as_bytes());
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
                .authorize(&identity("admin"), COMMAND_TOPIC, Access::Write)
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
                .authorize(&identity("admin"), COMMAND_TOPIC, Access::Write)
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
            security.authorize(&s1, COMMAND_TOPIC, Access::Write).await,
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
                for access in [Access::Write, Access::Read, Access::Subscribe] {
                    assert_eq!(
                        security.authorize(client, topic, access).await,
                        Decision::Deny,
                        "{:?} {topic} {access:?}",
                        client.username
                    );
                }
            }
            assert_eq!(
                security.authorize(client, "anything", Access::Write).await,
                Decision::Allow
            );
            assert_eq!(
                security
                    .authorize(client, "$CONTROLLER", Access::Write)
                    .await,
                Decision::Allow
            );
        }
        assert_eq!(
            security
                .authorize(&identity("admin"), COMMAND_TOPIC, Access::Write)
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

    #[test]
    fn test_invalid_default_access_changes_nothing() {
        let path = std::env::temp_dir().join(format!(
            "mercuriod-dynsec-default-{}",
            std::process::id()
        ));
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
        for (topic, access) in [
            ("$CONTROL/broker/v1", Access::Write),
            ("$CONTROL/broker/v1/listClients", Access::Write),
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
                .authorize(&identity("other"), "$CONTROL/broker/v1", Access::Write)
                .await,
            Decision::Deny
        );
        std::fs::remove_file(path).unwrap();
    }
}
