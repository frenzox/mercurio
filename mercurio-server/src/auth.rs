//! Built-in authentication plugins for tests and demos: PLAIN enhanced
//! authentication and an in-memory user name and password check.
//!
//! Both keep passwords in plaintext, in memory. They compare them in
//! constant time, and take as long for an unknown user as for a wrong
//! password, but a real deployment wants hashed passwords, such as
//! `mercuriod`'s password file or dynamic security, or a plugin of its own.

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;

use mercurio_core::reason::ReasonCode;

use crate::plugin::{
    ready, AuthExchange, AuthResult, BoxFuture, ClientInfo, Credentials, Decision, Plugin,
    PluginKind, PluginStatus,
};

/// PLAIN authentication method (RFC 4616), for tests and demos.
///
/// Expects authentication data in the format: `\0username\0password`
/// where the first byte is NUL (authorization identity is empty).
pub struct PlainAuth {
    /// Map of username -> password for credential validation.
    credentials: Arc<HashMap<String, String>>,
}

impl PlainAuth {
    /// Create a new PLAIN authenticator with the given credentials.
    pub fn new(credentials: HashMap<String, String>) -> Self {
        PlainAuth {
            credentials: Arc::new(credentials),
        }
    }
}

/// Parse PLAIN authentication data.
///
/// Format: [authzid] NUL authcid NUL passwd
/// We expect authzid to be empty, so format is: NUL username NUL password
fn parse_plain_data(data: &Bytes) -> Option<(String, String)> {
    let data = data.as_ref();

    // Must start with NUL (empty authorization identity)
    if data.is_empty() || data[0] != 0 {
        return None;
    }

    // Find second NUL separating username and password
    let second_nul = data[1..].iter().position(|&b| b == 0)?;
    let username_end = 1 + second_nul;

    // Extract username and password
    let username = std::str::from_utf8(&data[1..username_end]).ok()?;
    let password = std::str::from_utf8(&data[username_end + 1..]).ok()?;

    Some((username.to_string(), password.to_string()))
}

impl Plugin for PlainAuth {
    fn name(&self) -> &str {
        "plain"
    }

    fn status(&self) -> PluginStatus {
        PluginStatus::new(self.name(), PluginKind::Builtin).with_hooks(&["begin_auth"])
    }

    fn begin_auth(&self, _client: &ClientInfo, method: &str) -> Option<Box<dyn AuthExchange>> {
        (method == "PLAIN").then(|| {
            Box::new(PlainExchange {
                credentials: Arc::clone(&self.credentials),
                done: false,
            }) as Box<dyn AuthExchange>
        })
    }
}

struct PlainExchange {
    credentials: Arc<HashMap<String, String>>,
    done: bool,
}

impl AuthExchange for PlainExchange {
    fn step<'a>(&'a mut self, data: Option<&'a Bytes>) -> BoxFuture<'a, AuthResult> {
        ready(self.check(data))
    }
}

impl PlainExchange {
    fn check(&mut self, data: Option<&Bytes>) -> AuthResult {
        // PLAIN is a single step
        if std::mem::replace(&mut self.done, true) {
            return AuthResult::Failed(ReasonCode::ProtocolError);
        }

        let Some(data) = data else {
            // PLAIN requires data in the initial request
            return AuthResult::Failed(ReasonCode::BadAuthenticationMethod);
        };

        let Some((username, password)) = parse_plain_data(data) else {
            return AuthResult::Failed(ReasonCode::MalformedPacket);
        };

        match check_password(&self.credentials, &username, password.as_bytes()) {
            Some(true) => AuthResult::Success {
                username: Some(username),
                data: None,
            },
            _ => AuthResult::Failed(ReasonCode::BadUserNameOrPassword),
        }
    }
}

/// Whether `password` is `username`'s in `credentials`; None for an unknown
/// user, found as slowly as a wrong password so timing doesn't tell which
/// users exist.
fn check_password(
    credentials: &HashMap<String, String>,
    username: &str,
    password: &[u8],
) -> Option<bool> {
    match credentials.get(username) {
        Some(stored) => Some(same_password(stored.as_bytes(), password)),
        None => {
            same_password(UNKNOWN_USER_PASSWORD, password);
            None
        }
    }
}

/// Compared with for unknown users, so they take as long as known ones.
const UNKNOWN_USER_PASSWORD: &[u8] = b"no password matches this one, as the user is unknown";

/// Compares in time that depends on the longer length only, never on where
/// the two differ, so timing tells nothing about the stored password.
fn same_password(stored: &[u8], given: &[u8]) -> bool {
    let mut difference = stored.len() ^ given.len();
    for i in 0..stored.len().max(given.len()) {
        let (a, b) = (stored.get(i), given.get(i));
        difference |= usize::from(a.unwrap_or(&0) ^ b.unwrap_or(&0));
    }
    std::hint::black_box(difference) == 0
}

/// In-memory user name and password check, for tests and demos.
///
/// It answers only for the users it holds: a wrong password is denied, and
/// an unknown user is left to the next plugin, so it can sit in front of
/// another source of users.
pub struct SimpleCredentialValidator {
    credentials: HashMap<String, String>,
}

impl SimpleCredentialValidator {
    /// Create a new validator with the given credentials.
    pub fn new(credentials: HashMap<String, String>) -> Self {
        Self { credentials }
    }
}

impl Plugin for SimpleCredentialValidator {
    fn name(&self) -> &str {
        "credentials"
    }

    fn status(&self) -> PluginStatus {
        PluginStatus::new(self.name(), PluginKind::Builtin).with_hooks(&["authenticate"])
    }

    fn authenticate<'a>(
        &'a self,
        _: &'a ClientInfo,
        credentials: &'a Credentials<'a>,
    ) -> BoxFuture<'a, Decision> {
        ready(
            match check_password(
                &self.credentials,
                credentials.username,
                credentials.password,
            ) {
                Some(true) => Decision::Allow,
                Some(false) => Decision::Deny,
                None => Decision::Defer,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> ClientInfo {
        ClientInfo::new("c", None)
    }

    fn create_plain_data(username: &str, password: &str) -> Bytes {
        let mut data = Vec::new();
        data.push(0); // Empty authorization identity
        data.extend_from_slice(username.as_bytes());
        data.push(0); // Separator
        data.extend_from_slice(password.as_bytes());
        Bytes::from(data)
    }

    #[tokio::test]
    async fn test_plain_auth_success() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("user", "pass");
        assert_eq!(
            auth.begin_auth(&client(), "PLAIN")
                .unwrap()
                .step(Some(&data))
                .await,
            AuthResult::Success {
                username: Some("user".into()),
                data: None,
            }
        );
    }

    #[tokio::test]
    async fn test_plain_auth_wrong_password() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("user", "wrongpass");
        match auth
            .begin_auth(&client(), "PLAIN")
            .unwrap()
            .step(Some(&data))
            .await
        {
            AuthResult::Failed(ReasonCode::BadUserNameOrPassword) => {}
            other => panic!("Expected BadUserNameOrPassword, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_plain_auth_unknown_user() {
        let mut credentials = HashMap::new();
        credentials.insert("user".to_string(), "pass".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("unknown", "pass");
        match auth
            .begin_auth(&client(), "PLAIN")
            .unwrap()
            .step(Some(&data))
            .await
        {
            AuthResult::Failed(ReasonCode::BadUserNameOrPassword) => {}
            other => panic!("Expected BadUserNameOrPassword, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_plain_auth_no_data() {
        let credentials = HashMap::new();
        let auth = PlainAuth::new(credentials);

        match auth
            .begin_auth(&client(), "PLAIN")
            .unwrap()
            .step(None)
            .await
        {
            AuthResult::Failed(ReasonCode::BadAuthenticationMethod) => {}
            other => panic!("Expected BadAuthenticationMethod, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_plain_auth_malformed_data() {
        let credentials = HashMap::new();
        let auth = PlainAuth::new(credentials);

        // Missing initial NUL
        let data = Bytes::from("user\0pass");
        match auth
            .begin_auth(&client(), "PLAIN")
            .unwrap()
            .step(Some(&data))
            .await
        {
            AuthResult::Failed(ReasonCode::MalformedPacket) => {}
            other => panic!("Expected MalformedPacket, got {:?}", other),
        }
    }

    #[test]
    fn test_plain_auth_unsupported_method() {
        assert!(PlainAuth::new(HashMap::new())
            .begin_auth(&client(), "UNKNOWN")
            .is_none());
    }

    #[tokio::test]
    async fn test_plain_auth_exchange_is_single_step() {
        let mut credentials = HashMap::new();
        credentials.insert("admin".to_string(), "secret".to_string());
        let auth = PlainAuth::new(credentials);

        let data = create_plain_data("admin", "secret");
        let mut exchange = auth.begin_auth(&client(), "PLAIN").unwrap();
        assert!(matches!(
            exchange.step(Some(&data)).await,
            AuthResult::Success { .. }
        ));
        // A finished exchange accepts no more steps
        assert_eq!(
            exchange.step(Some(&data)).await,
            AuthResult::Failed(ReasonCode::ProtocolError)
        );
    }

    #[tokio::test]
    async fn test_credential_validator_success() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert_eq!(
            validator
                .authenticate(
                    &ClientInfo::new("c", None),
                    &Credentials::new("user", b"pass")
                )
                .await,
            Decision::Allow
        );
    }

    #[tokio::test]
    async fn test_credential_validator_wrong_password() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert_eq!(
            validator
                .authenticate(
                    &ClientInfo::new("c", None),
                    &Credentials::new("user", b"wrong")
                )
                .await,
            Decision::Deny
        );
    }

    #[tokio::test]
    async fn test_credential_validator_leaves_unknown_users_to_the_next_plugin() {
        let mut creds = HashMap::new();
        creds.insert("user".to_string(), "pass".to_string());
        let validator = SimpleCredentialValidator::new(creds);

        assert_eq!(
            validator
                .authenticate(
                    &ClientInfo::new("c", None),
                    &Credentials::new("unknown", b"pass")
                )
                .await,
            Decision::Defer
        );
    }

    #[test]
    fn test_passwords_match_only_when_equal() {
        assert!(same_password(b"secret", b"secret"));
        assert!(same_password(b"", b""));
        assert!(!same_password(b"secret", b"secreT"));
        assert!(!same_password(b"secret", b"secret!"));
        assert!(!same_password(b"secret", b"secre"));
        assert!(!same_password(b"", b"x"));
    }
}
