//! Topic access control.
//!
//! [`AclFile`] reads the access control list format of mosquitto's
//! `acl_file`, so existing files can be reused:
//!
//! ```text
//! # Rules before any `user` line apply to anonymous clients
//! topic read public/#
//!
//! user alice
//! topic readwrite alice/#
//! topic deny alice/secret
//!
//! # Patterns apply to every client; %u is the user name, %c the client id
//! pattern write devices/%c/status
//! ```
//!
//! Anything not granted is denied, and `deny` overrides any grant.

use std::collections::HashMap;

use mercurio_core::topic::{topic_matches_filter, validate_subscribe_filter};

/// What a client wants to do with a topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Access {
    /// Receive a message published to it
    Read,
    /// Publish to it
    Write,
    /// Subscribe to it, `topic` being a topic filter
    Subscribe,
}

/// Who is asking for access.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientIdentity {
    pub client_id: String,
    /// Set only for a user the broker verified, by password, certificate
    /// or enhanced authentication; a claimed user name doesn't count
    pub username: Option<String>,
}

impl ClientIdentity {
    pub fn new(client_id: impl Into<String>, username: Option<String>) -> Self {
        ClientIdentity {
            client_id: client_id.into(),
            username,
        }
    }
}

/// Decides which topics a client may read and write.
pub trait Authorizer: Send + Sync {
    /// `topic` is a topic name, or a topic filter when subscribing.
    fn authorize(&self, client: &ClientIdentity, topic: &str, access: Access) -> bool;

    /// Whether clients may publish to `$SYS` topics, which hold the broker's
    /// own statistics and are refused to everyone else.
    fn allows_sys_publish(&self) -> bool {
        false
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grant {
    Read,
    Write,
    ReadWrite,
    Deny,
}

impl Grant {
    fn allows(self, access: Access) -> bool {
        matches!(
            (self, access),
            (Grant::ReadWrite, _)
                | (Grant::Read, Access::Read | Access::Subscribe)
                | (Grant::Write, Access::Write)
        )
    }
}

#[derive(Debug, Clone)]
struct Rule {
    grant: Grant,
    filter: String,
}

/// Access control list in mosquitto's `acl_file` format.
#[derive(Debug, Default)]
pub struct AclFile {
    anonymous: Vec<Rule>,
    users: HashMap<String, Vec<Rule>>,
    /// Rules for every client, with `%u` and `%c` still to substitute
    patterns: Vec<Rule>,
}

impl AclFile {
    /// Reads rules in mosquitto's ACL file format: `user`, `topic` and
    /// `pattern` lines, `%c` and `%u` standing for the client id and user
    /// name in patterns. Fails on a line it doesn't understand, or a topic
    /// that isn't a valid filter.
    pub fn parse(contents: &str) -> Result<Self, String> {
        let mut acl = AclFile::default();
        let mut current_user: Option<String> = None;

        for (number, line) in contents.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let (keyword, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
            let rest = rest.trim();
            let error = |message: &str| format!("line {}: {}", number + 1, message);

            match keyword {
                "user" => {
                    if rest.is_empty() {
                        return Err(error("`user` needs a user name"));
                    }
                    current_user = Some(rest.to_string());
                }
                "topic" | "pattern" => {
                    let rule = parse_rule(rest).map_err(|e| error(&e))?;
                    match (keyword, &current_user) {
                        ("pattern", _) => acl.patterns.push(rule),
                        (_, Some(user)) => acl.users.entry(user.clone()).or_default().push(rule),
                        (_, None) => acl.anonymous.push(rule),
                    }
                }
                _ => return Err(error(&format!("unknown keyword `{keyword}`"))),
            }
        }

        Ok(acl)
    }

    /// The rules that apply to `client`, patterns already substituted.
    fn rules_for(&self, client: &ClientIdentity) -> Vec<Rule> {
        let own = match &client.username {
            Some(username) => self.users.get(username).map(Vec::as_slice),
            None => Some(self.anonymous.as_slice()),
        };
        let mut rules: Vec<Rule> = own.unwrap_or_default().to_vec();

        for pattern in &self.patterns {
            // A pattern naming the user can't apply to anonymous clients
            if pattern.filter.contains("%u") && client.username.is_none() {
                continue;
            }
            let username = client.username.as_deref().unwrap_or_default();
            // An identity with wildcards or levels in it would widen the
            // pattern, `+` as a client id granting every client's topics
            // (mosquitto's CVE-2017-7650). Denials stay, as widening one
            // only denies more.
            let widens = |identity: &str| identity.contains(['+', '#', '/']);
            if pattern.grant != Grant::Deny
                && ((pattern.filter.contains("%c") && widens(&client.client_id))
                    || (pattern.filter.contains("%u") && widens(username)))
            {
                continue;
            }
            let filter = pattern
                .filter
                .replace("%c", &client.client_id)
                .replace("%u", username);
            rules.push(Rule {
                grant: pattern.grant,
                filter,
            });
        }

        rules
    }
}

fn parse_rule(rest: &str) -> Result<Rule, String> {
    if matches!(rest, "" | "read" | "write" | "readwrite" | "deny") {
        return Err("rule needs a topic".to_string());
    }
    // The access is optional and defaults to readwrite, as in mosquitto
    let (grant, filter) = match rest.split_once(char::is_whitespace) {
        Some(("read", filter)) => (Grant::Read, filter),
        Some(("write", filter)) => (Grant::Write, filter),
        Some(("readwrite", filter)) => (Grant::ReadWrite, filter),
        Some(("deny", filter)) => (Grant::Deny, filter),
        _ => (Grant::ReadWrite, rest),
    };
    let filter = filter.trim();
    // A misplaced wildcard, as in `a/#/b`, would match more than it reads
    validate_subscribe_filter(filter).map_err(|e| format!("invalid topic `{filter}`: {e}"))?;
    Ok(Rule {
        grant,
        filter: filter.to_string(),
    })
}

impl Authorizer for AclFile {
    fn authorize(&self, client: &ClientIdentity, topic: &str, access: Access) -> bool {
        let rules = self.rules_for(client);
        // A subscription filter is checked like a topic, so its wildcards
        // only pass where a rule's own wildcards cover them
        let matching = rules
            .iter()
            .filter(|rule| topic_matches_filter(topic, &rule.filter));

        let mut allowed = false;
        for rule in matching {
            if rule.grant == Grant::Deny {
                return false;
            }
            allowed |= rule.grant.allows(access);
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACL: &str = "
        # Anonymous clients
        topic read public/#

        user alice
        topic readwrite alice/#
        topic deny alice/secret
        topic sensors/+/temperature

        pattern write devices/%c/status
        pattern read users/%u/inbox
    ";

    fn client(client_id: &str, username: Option<&str>) -> ClientIdentity {
        ClientIdentity {
            client_id: client_id.into(),
            username: username.map(Into::into),
        }
    }

    #[test]
    fn test_rules_by_user() {
        let acl = AclFile::parse(ACL).unwrap();
        let anonymous = client("c1", None);
        let alice = client("c2", Some("alice"));

        assert!(acl.authorize(&anonymous, "public/news", Access::Read));
        assert!(!acl.authorize(&anonymous, "public/news", Access::Write));
        assert!(!acl.authorize(&anonymous, "alice/notes", Access::Read));

        assert!(acl.authorize(&alice, "alice/notes", Access::Write));
        assert!(!acl.authorize(&alice, "alice/secret", Access::Read));
        assert!(acl.authorize(&alice, "sensors/kitchen/temperature", Access::Write));
        // Rules for anonymous clients don't apply to named users
        assert!(!acl.authorize(&alice, "public/news", Access::Read));
    }

    #[test]
    fn test_patterns_substitute_client_and_user() {
        let acl = AclFile::parse(ACL).unwrap();

        assert!(acl.authorize(&client("dev7", None), "devices/dev7/status", Access::Write));
        assert!(!acl.authorize(&client("dev7", None), "devices/dev8/status", Access::Write));
        assert!(acl.authorize(&client("x", Some("bob")), "users/bob/inbox", Access::Read));
        assert!(!acl.authorize(&client("x", None), "users//inbox", Access::Read));
    }

    #[test]
    fn test_identities_with_wildcards_or_levels_gain_nothing_from_patterns() {
        let acl = AclFile::parse(ACL).unwrap();

        for client_id in ["+", "#", "dev7/../dev8", "a/b"] {
            let client = client(client_id, None);
            assert!(!acl.authorize(&client, "devices/dev8/status", Access::Write));
            assert!(!acl.authorize(&client, "devices/a/b/status", Access::Write));
        }
        let bob = client("x", Some("+"));
        assert!(!acl.authorize(&bob, "users/alice/inbox", Access::Read));

        // A pattern's denial still applies, however wide it becomes
        let acl =
            AclFile::parse("topic readwrite devices/#\npattern deny devices/%c/secret").unwrap();
        assert!(!acl.authorize(&client("+", None), "devices/dev8/secret", Access::Read));
        assert!(acl.authorize(&client("+", None), "devices/dev8/status", Access::Read));
    }

    #[test]
    fn test_rules_with_misplaced_wildcards_are_refused() {
        assert!(AclFile::parse("topic read devices/#/status").is_err());
        assert!(AclFile::parse("pattern write devices/a+/status").is_err());
    }

    #[test]
    fn test_subscription_filters_need_covering_rules() {
        let acl = AclFile::parse(ACL).unwrap();
        let alice = client("c", Some("alice"));

        assert!(acl.authorize(&alice, "alice/#", Access::Subscribe));
        assert!(acl.authorize(&alice, "sensors/+/temperature", Access::Subscribe));
        // Would also match topics no rule grants
        assert!(!acl.authorize(&alice, "#", Access::Subscribe));
        assert!(!acl.authorize(&alice, "sensors/#", Access::Subscribe));
    }

    #[test]
    fn test_parse_errors_name_the_line() {
        assert_eq!(
            AclFile::parse("user alice\nallow everything").unwrap_err(),
            "line 2: unknown keyword `allow`"
        );
        assert_eq!(
            AclFile::parse("topic read").unwrap_err(),
            "line 1: rule needs a topic"
        );
    }
}
